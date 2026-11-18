use crate::{Error, Result};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fmt};
use zeroize::Zeroize;

/// Serialized only for deliberate credential persistence; Debug is always redacted.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProfileId(String);
impl ProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || value.contains("..")
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b))
        {
            return Err(Error::InvalidProfile(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn default_profile() -> Self {
        Self("default".into())
    }
}
impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AuthProvider {
    #[default]
    Oauth2,
    Adc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub dfs_email: Option<String>,
    pub auth_provider: AuthProvider,
    pub adc_credentials_file: Option<String>,
    pub drive_account: Option<ProfileId>,
    pub migrated_legacy: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for Account {
    fn default() -> Self {
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
        Self {
            email: None,
            user_id: None,
            dfs_email: None,
            auth_provider: AuthProvider::Oauth2,
            adc_credentials_file: None,
            drive_account: None,
            migrated_legacy: false,
            created_at: now.clone(),
            updated_at: now,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "one")]
    pub version: u32,
    pub active: ProfileId,
    #[serde(default)]
    pub default_drive_account: Option<ProfileId>,
    pub accounts: IndexMap<ProfileId, Account>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
fn one() -> u32 {
    1
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            active: ProfileId::default_profile(),
            default_drive_account: None,
            accounts: IndexMap::from([(
                ProfileId::default_profile(),
                Account {
                    migrated_legacy: true,
                    ..Account::default()
                },
            )]),
            extra: Map::new(),
        }
    }
}
impl Registry {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Schema(self.version));
        }
        if !self.accounts.contains_key(&self.active) {
            return Err(Error::AccountMissing(self.active.to_string()));
        }
        let mut names = std::collections::BTreeSet::new();
        for id in self.accounts.keys() {
            if !names.insert(id.as_str().to_ascii_lowercase()) {
                return Err(Error::Ambiguous(
                    "账户名称在忽略大小写的文件系统上发生冲突".into(),
                ));
            }
        }
        for id in self.default_drive_account.iter().chain(
            self.accounts
                .values()
                .filter_map(|a| a.drive_account.as_ref()),
        ) {
            if !self.accounts.contains_key(id) {
                return Err(Error::AccountMissing(id.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub name: String,
    pub token: Secret,
    pub url: String,
    pub endpoint: String,
    #[serde(default)]
    pub token_expires_at: Option<DateTime<Utc>>,
    #[serde(default = "default_variant")]
    pub variant: String,
    #[serde(default = "default_accelerator")]
    pub accelerator: String,
    #[serde(default = "default_shape")]
    pub machine_shape: String,
    #[serde(default)]
    pub kernel_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_execution: Option<(String, Option<String>, String)>,
    #[serde(default)]
    pub running: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
fn default_variant() -> String {
    "DEFAULT".into()
}
fn default_accelerator() -> String {
    "NONE".into()
}
fn default_shape() -> String {
    "STANDARD".into()
}
pub type Sessions = BTreeMap<String, SessionState>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub update_url: String,
    pub last_check: Option<DateTime<Utc>>,
    pub enable_update_check: bool,
    pub latest_version: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            update_url: "https://pypi.org/pypi/google-colab-cli/json".into(),
            last_check: None,
            enable_update_check: true,
            latest_version: None,
            extra: Map::new(),
        }
    }
}
