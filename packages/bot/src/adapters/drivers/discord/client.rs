use crate::domain::app::App;
use crate::ports::drivers::client::Client;
use crate::ports::services::cache::Cache;
use crate::ports::services::card_store::CardStore;
use crate::ports::services::image_store::ImageStore;
use crate::ports::services::spoiler_subscription::SpoilerSubscription;
use async_trait::async_trait;
use cards_sdk::SpoilerQueue;
use serenity::all::GatewayIntents;
use serenity::Client as DiscordClient;
use std::env;

pub struct Discord(DiscordClient);

impl Discord {
    #[allow(clippy::missing_panics_doc)]
    pub async fn new<IS, CS, C, Sub>(app: App<IS, CS, C, Sub>) -> Self
    where
        IS: ImageStore + Send + Sync + 'static,
        CS: CardStore + SpoilerQueue + Send + Sync + 'static,
        C: Cache + Send + Sync + 'static,
        Sub: SpoilerSubscription + Send + Sync + 'static,
    {
        let token = env::var("BOT_TOKEN").expect("Bot token wasn't in env vars");
        let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES;

        let client = DiscordClient::builder(&token, intents)
            .event_handler(app)
            .await
            .expect("Error creating client");

        Self(client)
    }
}

#[async_trait]
impl Client for Discord {
    async fn run(&mut self) {
        if let Err(why) = self.0.start().await {
            log::error!("Error starting client - {why:?}");
        }
    }
}
