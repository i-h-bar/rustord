mod data;
pub mod utils;

use crate::adapters::services::scryfall::data::ScryfallData;
use crate::adapters::services::scryfall::data::card::ScryfallCard;
use crate::adapters::services::scryfall::data::symbols::ScryfallSymbol;
#[cfg(feature = "local-dev")]
use crate::domain::utils::bulk_cache;
use crate::ports::emoji::{EmojiImage, EmojiMetaData, SetEmoji, SymbolEmoji};
use crate::ports::source::CardSource;
use async_trait::async_trait;
use cards_sdk::{CardInfo, Set};
use data::set::ScryfallSet;
use flate2::read::GzDecoder;
use futures::future;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use normalise::normalise_emoji_name;
use reqwest::{Client, Response};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Read;
use std::num::NonZeroU32;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

fn check_if_rate_limited(resp: &Response) {
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        log::error!("Rate limited by Scryfall");
        std::process::exit(1);
    }
}

#[derive(Error, Debug)]
enum ScryfallError {
    #[error("Http error {0}")]
    HTTPError(#[from] reqwest::Error),
    #[error("Error parsing response from scryfall")]
    ParseError,
}

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;
type ScryfallResult<T> = Result<T, ScryfallError>;

const DEFAULT_SET_ICON_URL: &str = "https://svgs.scryfall.io/sets/default.svg";

#[derive(serde::Deserialize)]
struct BulkDataEntry {
    #[serde(rename = "type")]
    data_type: String,
    jsonl_download_uri: String,
}

// Scryfall bulk downloads are gzip-compressed JSON Lines (one card object per line),
// not a single JSON array.
fn parse_bulk_cards(bytes: &[u8]) -> ScryfallResult<Vec<ScryfallCard>> {
    let mut decompressed = String::new();
    GzDecoder::new(bytes)
        .read_to_string(&mut decompressed)
        .map_err(|e| {
            log::warn!("Failed to decompress bulk card data: {e}");
            ScryfallError::ParseError
        })?;

    decompressed
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|e| {
                log::warn!("Failed to parse bulk card data: {e}");
                ScryfallError::ParseError
            })
        })
        .collect()
}

pub struct Scryfall {
    base_url: String,
    client: Client,
    sets: RwLock<HashMap<Uuid, ScryfallSet>>,
    low_limiter: Arc<Limiter>,
    high_limiter: Arc<Limiter>,
}

impl Default for Scryfall {
    fn default() -> Self {
        let low_quota = Quota::per_second(NonZeroU32::new(2).unwrap());
        let high_quota = Quota::per_second(NonZeroU32::new(10).unwrap());
        Self {
            base_url: String::default(),
            client: Client::default(),
            sets: RwLock::default(),
            low_limiter: Arc::new(RateLimiter::direct(low_quota)),
            high_limiter: Arc::new(RateLimiter::direct(high_quota)),
        }
    }
}

impl Scryfall {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(env::var("USER_AGENT").expect("USER_AGENT wasn't in env vars"))
            .build()
            .expect("Failure to creating reqwest client");

        Self {
            base_url: "https://api.scryfall.com".into(),
            client,
            ..Self::default()
        }
    }

    async fn fetch_sets_from_api(&self) -> ScryfallResult<Vec<ScryfallSet>> {
        let url = format!("{}/sets", self.base_url);
        let response = self.get::<ScryfallSet>(&url).await?;
        Ok(response.data)
    }

    async fn recent_sets(&self) -> ScryfallResult<Vec<ScryfallSet>> {
        let today = time::OffsetDateTime::now_utc().date();
        let threshold = today - time::Duration::days(7);

        self.fetch_sets_from_api().await.map(|sets| {
            sets.into_iter()
                .filter(|s| s.released_at >= threshold && s.card_count > 0)
                .collect()
        })
    }

    async fn fetch_bulk_cards(&self) -> ScryfallResult<Vec<ScryfallCard>> {
        #[cfg(feature = "local-dev")]
        if let Some(path) = bulk_cache::find_cached() {
            if let Some(bytes) = bulk_cache::load(&path).await {
                return parse_bulk_cards(&bytes);
            }
        }

        let url = format!("{}/bulk-data", self.base_url);
        let manifest = self.get::<BulkDataEntry>(&url).await?;

        let entry = manifest
            .data
            .into_iter()
            .find(|e| e.data_type == "default_cards")
            .ok_or(ScryfallError::ParseError)?;

        log::info!("Downloading bulk card data");
        let resp = self
            .get_resp(&entry.jsonl_download_uri, &self.low_limiter)
            .await?;

        let bytes = resp.bytes().await.map_err(|e| {
            log::warn!("Failed to download bulk data: {e}");
            ScryfallError::ParseError
        })?;

        #[cfg(feature = "local-dev")]
        bulk_cache::save(&bytes).await;

        parse_bulk_cards(&bytes)
    }

    async fn get_resp(&self, url: &str, limiter: &Limiter) -> ScryfallResult<Response> {
        limiter.until_ready().await;
        let resp = self.client.get(url).send().await.map_err(|why| {
            log::warn!("Error getting data from scryfall: {why}");
            ScryfallError::HTTPError(why)
        })?;

        check_if_rate_limited(&resp);

        Ok(resp)
    }

    async fn get_text(&self, url: &str, limiter: &Limiter) -> ScryfallResult<String> {
        let resp = self.get_resp(url, limiter).await?;

        resp.text().await.map_err(|err| {
            log::warn!("Error parsing data from scryfall: {err}");
            ScryfallError::ParseError
        })
    }

    async fn get<T>(&self, url: &str) -> ScryfallResult<ScryfallData<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let resp = self.get_resp(url, &self.low_limiter).await?;

        resp.json().await.map_err(|err| {
            log::warn!("Error parsing data from scryfall: {err}");
            ScryfallError::ParseError
        })
    }
}

#[async_trait]
impl CardSource for Scryfall {
    async fn get_recent_sets(&self) -> Vec<Set> {
        if !self.sets.read().await.is_empty() {
            return self.sets.read().await.values().map(Into::into).collect();
        }

        let Ok(sets) = self.recent_sets().await else {
            return Vec::new();
        };
        self.sets
            .write()
            .await
            .extend(sets.into_iter().map(|set| (set.id, set)));

        self.sets.read().await.values().map(Into::into).collect()
    }

    async fn get_all_sets(&self) -> Vec<Set> {
        let Ok(sets) = self.fetch_sets_from_api().await else {
            return Vec::new();
        };
        self.sets
            .write()
            .await
            .extend(sets.into_iter().map(|set| (set.id, set)));

        self.sets.read().await.values().map(Into::into).collect()
    }

    async fn fetch_all_cards(&self) -> Vec<CardInfo> {
        match self.fetch_bulk_cards().await {
            Ok(cards) => {
                log::info!("Processing {} bulk cards", cards.len());

                #[cfg(feature = "local-dev")]
                let pb = {
                    let bar = indicatif::ProgressBar::new(cards.len() as u64);
                    bar.set_style(
                        indicatif::ProgressStyle::with_template(
                            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} processed ({eta})",
                        )
                        .unwrap()
                        .progress_chars("=>-"),
                    );
                    bar
                };

                let result: Vec<CardInfo> = cards
                    .into_iter()
                    .inspect(|_| {
                        #[cfg(feature = "local-dev")]
                        pb.inc(1);
                    })
                    .filter_map(ScryfallCard::into_storage_records)
                    .flatten()
                    .collect();

                #[cfg(feature = "local-dev")]
                pb.finish_with_message("done");

                result
            }
            Err(e) => {
                log::error!("Failed to fetch bulk cards: {e}");
                vec![]
            }
        }
    }

    async fn fetch_cards_for_sets(&self, sets: &[Set]) -> Vec<CardInfo> {
        let mut scryfall_cards: Vec<ScryfallCard> = Vec::new();
        log::info!("Fetching {} sets", sets.len());

        'set: for set in sets {
            log::info!("Fetching cards in {}", set.name);
            let mut url = Some(format!(
                "{}/cards/search?q=e:{}",
                self.base_url, set.abbreviation
            ));
            while let Some(ref next_page) = url {
                let Ok(response) = self.get(next_page).await else {
                    continue 'set;
                };
                scryfall_cards.extend(response.data);
                url = response.next_page;
            }
        }

        scryfall_cards
            .into_iter()
            .filter_map(ScryfallCard::into_storage_records)
            .flatten()
            .collect()
    }

    async fn fetch_missing_set_symbols(&self, current: &[EmojiMetaData]) -> Vec<SetEmoji> {
        let current_sets: HashSet<&str> = current.iter().map(|e| e.name.as_str()).collect();

        let mut emojis: Vec<SetEmoji> = future::join_all(
            self.sets
                .read()
                .await
                .values()
                .filter(|s| {
                    !s.icon_svg_uri
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .ends_with("default.svg")
                })
                .filter(|s| !current_sets.contains(normalise_emoji_name(&s.abbreviation).as_str()))
                .collect::<Vec<&ScryfallSet>>()
                .iter()
                .map(|s| async {
                    let data = self
                        .get_text(&s.icon_svg_uri, &self.high_limiter)
                        .await
                        .ok()?;

                    Some(SetEmoji {
                        name: s.abbreviation.clone(),
                        image: EmojiImage(data),
                    })
                }),
        )
        .await
        .into_iter()
        .flatten()
        .collect();

        if !current_sets.contains("default")
            && let Ok(data) = self
                .get_text(DEFAULT_SET_ICON_URL, &self.high_limiter)
                .await
        {
            emojis.push(SetEmoji {
                name: "default".to_string(),
                image: EmojiImage(data),
            });
        }

        emojis
    }

    async fn download_image(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.get_resp(url, &self.high_limiter).await.ok()?;

        match resp.bytes().await {
            Ok(bytes) => Some(bytes.into()),
            Err(why) => {
                log::warn!("Error downloading image from {url}: {why}");
                None
            }
        }
    }

    async fn fetch_missing_card_symbols(&self, current: &[EmojiMetaData]) -> Vec<SymbolEmoji> {
        let current_symbols: HashSet<&str> = current.iter().map(|e| e.name.as_str()).collect();
        let Ok(response) = self
            .get::<ScryfallSymbol>(&format!("{}/symbology", self.base_url))
            .await
        else {
            return vec![];
        };

        future::join_all(
            response
                .data
                .into_iter()
                .filter(|s| !current_symbols.contains(normalise_emoji_name(&s.symbol).as_str()))
                .collect::<Vec<ScryfallSymbol>>()
                .iter()
                .map(|s| async {
                    let data = self.get_text(&s.svg_uri, &self.high_limiter).await.ok()?;

                    Some(SymbolEmoji::new(&s.symbol, EmojiImage(data)))
                }),
        )
        .await
        .into_iter()
        .flatten()
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    const BULK_SAMPLE_JSONL: &str = include_str!("test_fixtures/bulk_sample.jsonl");

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn bulk_data_entry_deserializes_current_scryfall_schema() {
        let manifest_json = r#"{
            "object": "list",
            "has_more": false,
            "data": [
                {
                    "object": "bulk_data",
                    "type": "default_cards",
                    "jsonl_download_uri": "https://data.scryfall.io/default-cards/default-cards.jsonl.gz"
                }
            ]
        }"#;

        let manifest: ScryfallData<BulkDataEntry> = serde_json::from_str(manifest_json).unwrap();
        let entry = &manifest.data[0];

        assert_eq!(entry.data_type, "default_cards");
        assert_eq!(
            entry.jsonl_download_uri,
            "https://data.scryfall.io/default-cards/default-cards.jsonl.gz"
        );
    }

    #[test]
    fn parse_bulk_cards_reads_gzipped_jsonl() {
        let compressed = gzip(BULK_SAMPLE_JSONL.as_bytes());

        let cards = parse_bulk_cards(&compressed).unwrap();

        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "Forest");
        assert_eq!(cards[1].name, "Fury Sliver");
    }
}
