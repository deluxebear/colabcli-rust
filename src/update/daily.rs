//! Best-effort native release notices, separate from Python's PyPI cache.
use super::{UpdateClient, version};
use crate::{Result, storage::Storage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use url::Url;

const CACHE: &str = "native_update_cache";
const DAY: i64 = 86_400;

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    source: String,
    checked_at: Option<DateTime<Utc>>,
    latest_version: Option<String>,
}

/// Returns a newer version for the caller to announce. Never installs anything.
/// Network/cache/lock failures are deliberately silent and cannot fail a command.
/// Call only for interactive commands whose output permits unsolicited notices.
pub fn notice(
    storage: &Storage,
    source: &Url,
    current: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    check(storage, source, current, now).ok().flatten()
}

fn check(
    storage: &Storage,
    source: &Url,
    current: &str,
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    super::validate_url(source, false)?;
    let current = version(current)?;
    let target = super::current_target()?;
    let key = format!("{:x}", Sha256::digest(format!("{target}\n{source}")));
    // Do not let another CLI's daily fetch hold up the user's command.
    let storage = storage.clone().with_lock_timeout(Duration::from_millis(25));
    let _lock = storage.lock(&storage.path("native-update-check.rust.lock")?)?;
    let settings = storage.settings()?;
    if !settings.enable_update_check {
        return Ok(None);
    }
    let mut cache: Cache = settings
        .extra
        .get(CACHE)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .filter(|cache: &Cache| cache.source == key)
        .unwrap_or_default();
    let elapsed = cache.checked_at.map(|time| (now - time).num_seconds());
    if !elapsed.is_some_and(|age| (0..DAY).contains(&age)) {
        cache.source = key;
        // Failed checks are throttled too, to avoid repeated delays offline.
        cache.checked_at = Some(now);
        if let Ok(plan) =
            UpdateClient::new(Duration::from_secs(2))?.check(source, &current.to_string())
        {
            let observed = version(&plan.latest_version)?;
            if cache
                .latest_version
                .as_deref()
                .and_then(|s| version(s).ok())
                .is_none_or(|old| observed.cmp_precedence(&old).is_gt())
            {
                cache.latest_version = Some(plan.latest_version);
            }
        }
        storage.update_settings(|settings| {
            // Respect a concurrent user disabling checks while HTTP was pending.
            if settings.enable_update_check {
                settings.extra.insert(
                    CACHE.into(),
                    serde_json::to_value(&cache)
                        .map_err(|_| crate::Error::Update("更新缓存序列化失败"))?,
                );
            }
            Ok(())
        })?;
    }
    if !storage.settings()?.enable_update_check {
        return Ok(None);
    }
    Ok(cache.latest_version.filter(|latest| {
        version(latest).is_ok_and(|latest| latest.cmp_precedence(&current).is_gt())
    }))
}
