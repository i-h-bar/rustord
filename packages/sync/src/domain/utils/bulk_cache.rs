use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_AGE: Duration = Duration::from_hours(168);

#[must_use]
pub fn find_cached() -> Option<PathBuf> {
    let mut found = None;

    for entry in std::fs::read_dir(".").ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().into_owned();

        if !name.starts_with("default-cards-") || !name.ends_with(".jsonl.gz") {
            continue;
        }

        if let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) {
            match modified.elapsed() {
                Ok(age) if age < MAX_AGE => found = Some(path),
                _ => {
                    log::info!("Deleting stale bulk cache: {name}");
                    std::fs::remove_file(&path).ok();
                }
            }
        }
    }

    found
}

pub async fn load(path: &Path) -> Option<Vec<u8>> {
    log::info!("Loading bulk data from cache: {}", path.display());
    tokio::fs::read(path)
        .await
        .map_err(|e| log::warn!("Failed to read cache: {e}"))
        .ok()
}

pub async fn save(bytes: &[u8]) {
    let now = time::OffsetDateTime::now_utc();
    let filename = format!(
        "default-cards-{:04}{:02}{:02}{:02}{:02}{:02}+0000.jsonl.gz",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    );
    log::info!("Caching bulk data to {filename}");
    if let Err(e) = tokio::fs::write(&filename, bytes).await {
        log::warn!("Failed to save bulk cache: {e}");
    }
}
