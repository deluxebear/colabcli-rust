use crate::{
    Error, Result,
    accounts::{Accounts, lookup},
    models::{AuthProvider, ProfileId, Registry},
};
use std::path::PathBuf;

/// Snapshot once at process startup; authentication never mutates process environment.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub account: Option<String>,
    pub drive_account: Option<String>,
    pub adc_file: Option<PathBuf>,
    pub quota_project: Option<String>,
    pub gcloud_directory: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub metadata: crate::auth::MetadataOptions,
    pub allow_executables: bool,
    pub aws: crate::auth::AwsEnvironment,
    pub certificate: crate::auth::CertificateOptions,
}
impl Environment {
    pub fn capture() -> Self {
        fn nonblank(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        Self {
            account: nonblank("COLAB_ACCOUNT").map(|s| s.trim().to_owned()),
            drive_account: nonblank("COLAB_DRIVE_ACCOUNT")
                .or_else(|| nonblank("COLAB_DFS_ACCOUNT"))
                .map(|s| s.trim().to_owned()),
            adc_file: nonblank("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from),
            // Preserve malformed/non-UTF8 overrides as invalid input instead of
            // silently billing against the file's lower-priority project.
            quota_project: std::env::var_os("GOOGLE_CLOUD_QUOTA_PROJECT")
                .map(|value| value.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty()),
            gcloud_directory: nonblank("CLOUDSDK_CONFIG").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            metadata: crate::auth::MetadataOptions::capture(),
            aws: crate::auth::AwsEnvironment::capture(),
            certificate: crate::auth::CertificateOptions::capture(),
            allow_executables: nonblank("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").as_deref()
                == Some("1"),
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
    pub quota_project: Option<String>,
    pub adc_file_required: bool,
    pub cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    pub metadata: crate::auth::MetadataOptions,
    pub allow_executables: bool,
    pub aws: crate::auth::AwsEnvironment,
    pub certificate: crate::auth::CertificateOptions,
}
pub fn resolve(
    accounts: &Accounts,
    env: &Environment,
    explicit: Option<&str>,
    session: Option<&str>,
    allow_routing: bool,
) -> Result<AccountContext> {
    let r = accounts.registry()?;
    let (profile, source) = if let Some(selector) = explicit.filter(|s| !s.is_empty()) {
        (lookup(&r, selector)?, AccountSource::Flag)
    } else if let Some(selector) = env
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (lookup(&r, selector)?, AccountSource::Environment)
    } else if let Some(name) = session.filter(|_| allow_routing) {
        match named_owner(&r, name, |id| {
            crate::sessions::owns_name(&accounts.storage, id, name)
        })? {
            Some(id) if id != r.active => (id, AccountSource::Session),
            _ => (r.active.clone(), AccountSource::Active),
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
        quota_project: env.quota_project.clone(),
        adc_file_required: env.adc_file.is_some() || account.adc_credentials_file.is_some(),
        cancellation: None,
        metadata: env.metadata.clone(),
        allow_executables: env.allow_executables,
        aws: env.aws.clone(),
        certificate: env.certificate.clone(),
    })
}

fn named_owner(
    registry: &Registry,
    name: &str,
    contains: impl Fn(&ProfileId) -> Result<bool>,
) -> Result<Option<ProfileId>> {
    if contains(&registry.active)? {
        return Ok(Some(registry.active.clone()));
    }
    let mut owners = Vec::new();
    for profile in registry
        .accounts
        .keys()
        .filter(|id| *id != &registry.active)
    {
        if contains(profile)? {
            owners.push(profile);
        }
    }
    match owners.as_slice() {
        [] => Ok(None),
        [owner] => Ok(Some((*owner).clone())),
        _ => Err(Error::SessionAccountAmbiguous {
            session: name.into(),
            accounts: owners
                .iter()
                .map(|owner| owner.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Resolve offline history against recorded sessions first, then retained history.
/// The second return value indicates a route to a non-active session owner.
pub fn resolve_history(
    accounts: &Accounts,
    env: &Environment,
    explicit: Option<&str>,
    session: Option<&str>,
) -> Result<(ProfileId, bool)> {
    let registry = accounts.registry()?;
    if let Some(selector) = explicit.filter(|s| !s.is_empty()).or_else(|| {
        env.account
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }) {
        return Ok((lookup(&registry, selector)?, false));
    }
    if let Some(name) = session {
        if let Some(profile) = named_owner(&registry, name, |id| {
            Ok(accounts.storage.sessions(id)?.contains_key(name))
        })? {
            let routed = profile != registry.active;
            return Ok((profile, routed));
        }
        if let Some(profile) = named_owner(&registry, name, |id| {
            crate::history::History::new(accounts.storage.clone(), id.clone()).has_session(name)
        })? {
            return Ok((profile, false));
        }
    }
    Ok((registry.active, false))
}
pub fn resolve_drive(
    accounts: &Accounts,
    env: &Environment,
    compute: &ProfileId,
    explicit: Option<&str>,
    consent: bool,
) -> Result<ProfileId> {
    Ok(resolve_drive_with_source(accounts, env, compute, explicit, consent)?.0)
}

pub fn resolve_drive_with_source(
    accounts: &Accounts,
    env: &Environment,
    compute: &ProfileId,
    explicit: Option<&str>,
    consent: bool,
) -> Result<(ProfileId, String)> {
    let r = accounts.registry()?;
    let account = r
        .accounts
        .get(compute)
        .ok_or_else(|| Error::AccountMissing(compute.to_string()))?;
    if let Some(selector) = explicit {
        return Ok((lookup(&r, selector)?, "flag".into()));
    }
    if let Some(selector) = env.drive_account.as_deref() {
        return Ok((lookup(&r, selector)?, "env".into()));
    }
    if consent {
        return Ok((compute.clone(), "self".into()));
    }
    if let Some(id) = &account.drive_account {
        return Ok((id.clone(), format!("bound:{compute}")));
    }
    if accounts
        .storage
        .profile_path(compute, "dfs_credentials.json")?
        .exists()
    {
        return Ok((compute.clone(), "self".into()));
    }
    Ok(match r.default_drive_account {
        Some(id) => (id, "default".into()),
        None => (compute.clone(), "self".into()),
    })
}
