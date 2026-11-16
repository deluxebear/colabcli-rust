//! Bounded account inventory: one account's authentication/network failure stays local to its row.
use crate::{Error, Result, models::ProfileId};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AccountResult<T> {
    pub account: ProfileId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
/// At most four account workers run concurrently. Output order follows the input order.
pub fn collect<T: Send>(
    profiles: &[ProfileId],
    query: impl Fn(&ProfileId) -> Result<T> + Sync,
) -> Vec<AccountResult<T>> {
    let mut rows = Vec::with_capacity(profiles.len());
    for batch in profiles.chunks(4) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|profile| {
                    let query = &query;
                    (profile, scope.spawn(move || query(profile)))
                })
                .collect();
            for (profile, handle) in handles {
                let result = handle.join().unwrap_or(Err(Error::InventoryWorker));
                rows.push(match result {
                    Ok(result) => AccountResult {
                        account: profile.clone(),
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => AccountResult {
                        account: profile.clone(),
                        result: None,
                        error: Some(error.to_string()),
                    },
                });
            }
        });
    }
    rows
}
