use crate::{
    Error, Result,
    accounts::{Accounts, lookup},
    models::{AuthProvider, ProfileId},
};
use std::path::PathBuf;

/// Snapshot once at process startup; authentication never mutates process environment.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub account: Option<String>,
    pub drive_account: Option<String>,
    pub adc_file: Option<PathBuf>,
    pub gcloud_directory: Option<PathBuf>,
    pub home: Option<PathBuf>,
}
impl Environment {
    pub fn capture() -> Self {
        fn nonblank(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        Self {
            account: nonblank("COLAB_ACCOUNT"),
            drive_account: nonblank("COLAB_DRIVE_ACCOUNT")
                .or_else(|| nonblank("COLAB_DFS_ACCOUNT")),
            adc_file: nonblank("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from),
            gcloud_directory: nonblank("CLOUDSDK_CONFIG").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    Flag,
    Environment,
    Session,
    Active,
}
#[derive(Debug, Clone)]
pub struct AccountContext {
    pub profile: ProfileId,
    pub source: AccountSource,
    pub provider: AuthProvider,
    pub adc_file: Option<PathBuf>,
}
pub fn resolve(
    accounts: &Accounts,
    env: &Environment,
    explicit: Option<&str>,
    session: Option<&str>,
    allow_routing: bool,
) -> Result<AccountContext> {
    let r = accounts.registry()?;
    let (profile, source) = if let Some(selector) = explicit {
        (lookup(&r, selector)?, AccountSource::Flag)
    } else if let Some(selector) = env.account.as_deref().filter(|s| !s.trim().is_empty()) {
        (lookup(&r, selector)?, AccountSource::Environment)
    } else if let Some(name) = session.filter(|_| allow_routing) {
        if crate::sessions::owns_name(&accounts.storage, &r.active, name)? {
            (r.active.clone(), AccountSource::Session)
        } else {
            let mut owners = Vec::new();
            for id in r.accounts.keys() {
                if crate::sessions::owns_name(&accounts.storage, id, name)? {
                    owners.push(id.clone());
                }
            }
            match owners.len() {
                0 => (r.active.clone(), AccountSource::Active),
                1 => (owners.remove(0), AccountSource::Session),
                _ => return Err(Error::Ambiguous(name.into())),
            }
        }
    } else {
        (r.active.clone(), AccountSource::Active)
    };
    let account = &r.accounts[&profile];
    let adc_file = env
        .adc_file
        .clone()
        .or_else(|| account.adc_credentials_file.as_ref().map(PathBuf::from))
        .or_else(|| {
            env.gcloud_directory
                .as_ref()
                .map(|p| p.join("application_default_credentials.json"))
        })
        .or_else(|| {
            env.home
                .as_ref()
                .map(|p| p.join(".config/gcloud/application_default_credentials.json"))
        });
    Ok(AccountContext {
        profile,
        source,
        provider: account.auth_provider,
        adc_file,
    })
}
pub fn resolve_drive(
    accounts: &Accounts,
    env: &Environment,
    compute: &ProfileId,
    explicit: Option<&str>,
    consent: bool,
) -> Result<ProfileId> {
    let r = accounts.registry()?;
    let account = r
        .accounts
        .get(compute)
        .ok_or_else(|| Error::AccountMissing(compute.to_string()))?;
    if let Some(selector) = explicit.or(env.drive_account.as_deref()) {
        return lookup(&r, selector);
    }
    if consent {
        return Ok(compute.clone());
    }
    if let Some(id) = &account.drive_account {
        return Ok(id.clone());
    }
    if accounts
        .storage
        .profile_path(compute, "dfs_credentials.json")?
        .exists()
    {
        return Ok(compute.clone());
    }
    Ok(r.default_drive_account.unwrap_or_else(|| compute.clone()))
}
