//! Independent DriveFS credentials. Never substitute the compute OAuth identity.
use crate::{
    Error, Result,
    auth::{
        OAuthClient, OAuthCredentials, PendingAuthorization, TokenClient, random_secret,
        validate_endpoint,
    },
    models::{ProfileId, Registry, Secret},
    storage::{FileLock, Storage},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, time::Duration};
use url::Url;

/// Opaque compare-and-swap identity. Never includes an access token or raw secret.
pub fn remote_fingerprint(credentials: &DriveCredentials) -> String {
    let mut hash = Sha256::new();
    let source = credentials
        .oauth
        .extra
        .get("drive_scope_source")
        .and_then(Value::as_str)
        .unwrap_or("");
    for value in [
        "drivefs-remote-v1",
        credentials.flavor.as_str(),
        credentials.oauth.client_id.as_str(),
        credentials
            .oauth
            .client_secret
            .as_ref()
            .map(Secret::expose)
            .unwrap_or(""),
        credentials
            .oauth
            .refresh_token
            .as_ref()
            .map(Secret::expose)
            .unwrap_or(""),
        credentials.oauth.token_uri.as_str(),
        credentials.user_id.as_deref().unwrap_or(""),
        credentials.email.as_deref().unwrap_or(""),
        source,
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    for scope in &credentials.oauth.scopes {
        hash.update((scope.len() as u64).to_be_bytes());
        hash.update(scope.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[derive(Debug, Serialize)]
pub struct RemoteAdoption {
    pub receipt: String,
    pub updated: bool,
}

#[derive(Deserialize)]
struct RemoteSnapshot {
    version: u32,
    client_id: String,
    #[serde(default)]
    client_secret: Option<Secret>,
    refresh_token: Secret,
    token_uri: String,
    user_id: String,
    #[serde(default)]
    email: String,
    scopes: Vec<String>,
    #[serde(default)]
    scope_confirmed: bool,
}

pub const DRIVEFS_CLIENT_ID: &str =
    "947318989803-k88lapdik9bledfml8rr69ic6d3rdv57.apps.googleusercontent.com";
pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
pub const MINIMAL_SCOPES: [&str; 3] = [
    "openid",
    "https://www.googleapis.com/auth/userinfo.email",
    DRIVE_SCOPE,
];
pub const FULL_SCOPES: [&str; 8] = [
    "email",
    "https://www.googleapis.com/auth/client_channel",
    "https://www.googleapis.com/auth/docs.test",
    DRIVE_SCOPE,
    "https://www.googleapis.com/auth/drive.activity.readonly",
    "https://www.googleapis.com/auth/drive.photos.readonly",
    "https://www.googleapis.com/auth/experimentsandconfigs",
    "https://www.googleapis.com/auth/peopleapi.readonly",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct DriveCredentials {
    pub flavor: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(flatten)]
    pub oauth: OAuthCredentials,
}
impl std::fmt::Debug for DriveCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveCredentials")
            .field("flavor", &self.flavor)
            .field(
                "has_refresh_token",
                &self
                    .oauth
                    .refresh_token
                    .as_ref()
                    .is_some_and(|s| !s.is_empty()),
            )
            .finish_non_exhaustive()
    }
}
impl DriveCredentials {
    pub fn validate(&self) -> Result<()> {
        if !["sdk", "dfs", "custom", "imported"].contains(&self.flavor.as_str()) {
            return Err(Error::OAuth("未知 Drive 凭据 flavor"));
        }
        if self.oauth.client_id.trim().is_empty()
            || self
                .oauth
                .refresh_token
                .as_ref()
                .is_none_or(Secret::is_empty)
        {
            return Err(Error::OAuth(
                "Drive 凭据需要匹配的 client_id 和 refresh_token",
            ));
        }
        let token_uri =
            Url::parse(&self.oauth.token_uri).map_err(|_| Error::OAuth("Drive token URI 无效"))?;
        validate_endpoint(&token_uri)?;
        if token_uri.query().is_some() {
            return Err(Error::OAuth("Drive token URI 不得包含 query"));
        }
        validate_scopes(&self.oauth.scopes)?;
        validate_identity(self.user_id.as_deref(), self.email.as_deref())
    }
    pub fn validate_for_mount(&self, direct_refresh: bool) -> Result<()> {
        self.validate()?;
        if !self.oauth.scopes.iter().any(|s| s == DRIVE_SCOPE) {
            return Err(Error::OAuth(
                "DriveFS 挂载需要完整 drive scope；drive.file 不能替代",
            ));
        }
        if self.user_id.as_deref().is_none_or(str::is_empty) {
            return Err(Error::OAuth("DriveFS 缺少 Gaia user_id"));
        }
        if direct_refresh && (self.flavor != "dfs" || self.oauth.client_id != DRIVEFS_CLIENT_ID) {
            return Err(Error::OAuth(
                "rt 模式需要 DriveFS client 配套凭据；其他来源请用 shim",
            ));
        }
        Ok(())
    }
    /// Only recognized, non-secret metadata is displayed. Unknown fields survive
    /// storage but are not assumed safe to print.
    pub fn summary(&self) -> Value {
        json!({"flavor":self.flavor,"user_id":self.user_id,"email":self.email,"created_at":self.created_at,
            "client_id":self.oauth.client_id,"token_uri":self.oauth.token_uri,"scopes":self.oauth.scopes,"scope_source":self.oauth.extra.get("drive_scope_source").and_then(Value::as_str).filter(|s|["token_response","configured_unverified","requested_unverified"].contains(s)),
            "refresh_token":self.oauth.refresh_token.as_ref().filter(|s|!s.is_empty()).map(|_|"<set>"),
            "client_secret":self.oauth.client_secret.as_ref().filter(|s|!s.is_empty()).map(|_|"<set>"),
            "access_token":(!self.oauth.token.is_empty()).then_some("<set>"),"expiry":self.oauth.expiry})
    }
}
fn validate_identity(uid: Option<&str>, email: Option<&str>) -> Result<()> {
    if uid.is_some_and(|s| {
        s.is_empty() || s.len() > 256 || s.chars().any(|c| c.is_whitespace() || c.is_control())
    }) || email.is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
    {
        return Err(Error::OAuth("Drive 身份字段无效"));
    }
    Ok(())
}
fn validate_scopes(scopes: &[String]) -> Result<()> {
    if scopes.len() > 64
        || scopes.iter().any(|s| {
            s.is_empty() || s.len() > 2048 || s.chars().any(|c| c.is_whitespace() || c.is_control())
        })
    {
        return Err(Error::OAuth("Drive scope 列表无效"));
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    flavor: String,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(flatten)]
    flow: PendingAuthorization,
}
#[derive(Serialize, Deserialize)]
struct RefreshPending {
    version: u32,
    account: ProfileId,
    credentials: DriveCredentials,
}
#[derive(Clone)]
pub struct DriveCredentialService {
    storage: Storage,
    client: TokenClient,
    userinfo: Url,
}
impl DriveCredentialService {
    pub fn new(storage: Storage, client: TokenClient) -> Self {
        Self {
            storage,
            client,
            userinfo: Url::parse("https://www.googleapis.com/oauth2/v3/userinfo").unwrap(),
        }
    }
    /// Primarily supports authorized deployments and deterministic loopback tests.
    pub fn with_userinfo_endpoint(mut self, endpoint: Url) -> Result<Self> {
        validate_endpoint(&endpoint)?;
        if endpoint.query().is_some() {
            return Err(Error::OAuth("userinfo 端点不得包含 query"));
        }
        self.userinfo = endpoint;
        Ok(self)
    }
    fn guard(&self, account: &ProfileId) -> Result<FileLock> {
        let guard = self.storage.profile_lock(account)?;
        if self.storage.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let registry: Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or_else(|| Error::AccountMissing(account.to_string()))?;
        registry.validate()?;
        if !registry.accounts.contains_key(account) {
            return Err(Error::AccountMissing(account.to_string()));
        }
        Ok(guard)
    }
    fn load_locked(&self, account: &ProfileId) -> Result<Option<DriveCredentials>> {
        let value = self.storage.read::<DriveCredentials>(
            &self.storage.profile_path(account, "dfs_credentials.json")?,
        )?;
        if let Some(value) = &value {
            value.validate()?;
        }
        Ok(value)
    }
    pub fn load(&self, account: &ProfileId) -> Result<Option<DriveCredentials>> {
        let _guard = self.guard(account)?;
        self.recover_locked(account)?;
        self.load_locked(account)
    }
    /// Resolve an explicit mount source without refreshing it. Persist before
    /// dispatch so remote rotation can use the same CAS/recovery protocol as consent.
    /// The source digest preserves an adopted token across retries with unchanged
    /// CI input; stale environment values must not undo a successful rotation.
    pub fn configure_mount(
        &self,
        account: &ProfileId,
        supplied: Option<&DriveCredentials>,
        user_id: Option<&str>,
        email: Option<&str>,
        direct_refresh: bool,
        persist: bool,
    ) -> Result<DriveCredentials> {
        const SOURCE: &str = "drive_mount_source_fingerprint";
        let _guard = self.guard(account)?;
        self.recover_locked(account)?;
        let existing = self.load_locked(account)?;
        let mut current = if let Some(supplied) = supplied {
            let mut candidate = supplied.clone();
            if let Some(value) = user_id {
                candidate.user_id = Some(value.into());
            }
            if let Some(value) = email {
                candidate.email = Some(value.into());
            }
            // Injected scopes are configuration, not a fresh provider response.
            candidate
                .oauth
                .extra
                .insert("drive_scope_source".into(), json!("configured_unverified"));
            candidate.validate_for_mount(direct_refresh)?;
            let fingerprint = remote_fingerprint(&candidate);
            if let Some(saved) = existing.as_ref().filter(|saved| {
                saved.oauth.extra.get(SOURCE).and_then(Value::as_str) == Some(&fingerprint)
            }) {
                saved.clone()
            } else {
                if let Some(saved) = &existing {
                    for (key, value) in &saved.oauth.extra {
                        if key != SOURCE && key != "drive_scope_source" {
                            candidate
                                .oauth
                                .extra
                                .entry(key.clone())
                                .or_insert_with(|| value.clone());
                        }
                    }
                }
                candidate.oauth.token = Secret::default();
                candidate.oauth.expiry = None;
                candidate
                    .oauth
                    .extra
                    .insert(SOURCE.into(), json!(fingerprint));
                candidate
            }
        } else {
            let mut current = existing.clone().ok_or(Error::NeedsAuth)?;
            let before = remote_fingerprint(&current);
            if let Some(value) = user_id {
                current.user_id = Some(value.into());
            }
            if let Some(value) = email {
                current.email = Some(value.into());
            }
            if before != remote_fingerprint(&current) {
                current.oauth.extra.remove(SOURCE);
            }
            current
        };
        current.validate_for_mount(direct_refresh)?;
        let changed = existing.as_ref().is_none_or(|saved| {
            remote_fingerprint(saved) != remote_fingerprint(&current)
                || saved.oauth.extra.get(SOURCE) != current.oauth.extra.get(SOURCE)
        });
        if changed {
            self.ensure_unleased_locked(account)?;
            if persist {
                // Source changes invalidate a cached access token even if only
                // the selected identity changed. Preserve unrelated metadata.
                current.oauth.token = Secret::default();
                current.oauth.expiry = None;
                self.stage_locked(account, &current)?;
                self.recover_locked(account)?;
            }
        }
        Ok(current)
    }
    fn leases_locked(&self, account: &ProfileId) -> Result<Vec<crate::drivefs::lease::Lease>> {
        let directory = self.storage.profile_path(account, "managed")?;
        if !directory.exists() {
            return Ok(vec![]);
        }
        let mut leases = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: directory.clone(),
                source,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("drivefs-") && name.ends_with(".json") {
                let lease: crate::drivefs::lease::Lease = self
                    .storage
                    .read(&entry.path())?
                    .ok_or(Error::OAuth("DriveFS 租约读取失败"))?;
                lease.validate()?;
                if lease.drive_account != *account
                    || name != format!("drivefs-{}.json", lease.request.operation_id)
                {
                    return Err(Error::OAuth("DriveFS 租约归属不匹配"));
                }
                leases.push(lease);
            }
        }
        Ok(leases)
    }
    fn ensure_unleased_locked(&self, account: &ProfileId) -> Result<()> {
        if !self.leases_locked(account)?.is_empty() {
            return Err(Error::OAuth(
                "Drive 凭据正在被远端挂载使用；请先卸载或释放对应 VM",
            ));
        }
        Ok(())
    }
    pub fn acquire_lease(&self, lease: &crate::drivefs::lease::Lease) -> Result<DriveCredentials> {
        lease.validate()?;
        let _registry = self
            .storage
            .lock(&self.storage.path("accounts.json.lock")?)?;
        let _guard = self.guard(&lease.drive_account)?;
        let registry: Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or(Error::AccountMissing(lease.compute_account.to_string()))?;
        if !registry.accounts.contains_key(&lease.compute_account)
            || !self
                .storage
                .sessions(&lease.compute_account)?
                .values()
                .any(|state| state.name == lease.session && state.endpoint == lease.endpoint)
        {
            return Err(Error::OAuth("DriveFS 租约的计算账户或 VM 已变化"));
        }
        self.recover_locked(&lease.drive_account)?;
        let current = self
            .load_locked(&lease.drive_account)?
            .ok_or(Error::NeedsAuth)?;
        if remote_fingerprint(&current) != lease.fingerprint {
            return Err(Error::OAuth("Drive 凭据已改变；没有向 VM 发送旧凭据"));
        }
        let path = self
            .storage
            .profile_path(&lease.drive_account, &lease.file())?;
        if let Some(existing) = self.storage.read::<crate::drivefs::lease::Lease>(&path)? {
            if existing != *lease {
                return Err(Error::OAuth("DriveFS 租约已存在且归属不同"));
            }
        } else {
            self.storage.write(&path, lease)?;
        }
        Ok(current)
    }
    pub fn release_lease(&self, lease: &crate::drivefs::lease::Lease) -> Result<()> {
        lease.validate()?;
        let _guard = match self.guard(&lease.drive_account) {
            Ok(guard) => guard,
            Err(Error::AccountMissing(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let path = self
            .storage
            .profile_path(&lease.drive_account, &lease.file())?;
        if let Some(existing) = self.storage.read::<crate::drivefs::lease::Lease>(&path)? {
            if existing != *lease {
                return Err(Error::OAuth("拒绝清除其他操作的 DriveFS 租约"));
            }
            self.storage.remove(&path)?;
        }
        Ok(())
    }
    /// Call only after confirmed VM release/absence. Each Drive account lock is
    /// acquired separately, outside the compute account's lifecycle lock.
    pub(crate) fn release_endpoint_leases(
        &self,
        compute: &ProfileId,
        endpoint: &str,
    ) -> Result<()> {
        let registry: Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or(Error::AccountMissing(compute.to_string()))?;
        registry.validate()?;
        for account in registry.accounts.keys() {
            let _guard = match self.guard(account) {
                Ok(guard) => guard,
                Err(Error::AccountMissing(_)) => continue,
                Err(error) => return Err(error),
            };
            for lease in self.leases_locked(account)? {
                if &lease.compute_account == compute && lease.endpoint == endpoint {
                    self.storage
                        .remove(&self.storage.profile_path(account, &lease.file())?)?;
                }
            }
        }
        Ok(())
    }
    pub fn leases(&self, account: &ProfileId) -> Result<Vec<crate::drivefs::lease::Lease>> {
        let _guard = self.guard(account)?;
        self.leases_locked(account)
    }
    /// Adopt only frozen remote credentials bound to the existing account/client.
    /// The caller must retain the remote file until this durable operation returns
    /// its receipt. Raw bytes must never be logged and should be zeroized by caller.
    pub fn adopt_remote(
        &self,
        account: &ProfileId,
        expected: &str,
        raw: &[u8],
    ) -> Result<RemoteAdoption> {
        if expected.len() != 64
            || !expected.bytes().all(|b| b.is_ascii_hexdigit())
            || raw.len() > 65536
        {
            return Err(Error::OAuth("DriveFS 凭据回收参数无效"));
        }
        let snapshot: RemoteSnapshot =
            serde_json::from_slice(raw).map_err(|_| Error::OAuth("DriveFS 远端凭据格式无效"))?;
        if snapshot.version != 1 {
            return Err(Error::Schema(snapshot.version));
        }
        let _guard = self.guard(account)?;
        self.recover_locked(account)?;
        let current = self.load_locked(account)?.ok_or(Error::NeedsAuth)?;
        fn secret(value: Option<&Secret>) -> &str {
            value.map(Secret::expose).unwrap_or("")
        }
        if snapshot.client_id != current.oauth.client_id
            || secret(snapshot.client_secret.as_ref())
                != secret(current.oauth.client_secret.as_ref())
            || snapshot.token_uri != current.oauth.token_uri
            || Some(snapshot.user_id.as_str()) != current.user_id.as_deref()
            || snapshot.email != current.email.as_deref().unwrap_or("")
        {
            return Err(Error::OAuth("DriveFS 远端凭据与本地账户或 client 不匹配"));
        }
        let mut updated = current.clone();
        updated.oauth.refresh_token = Some(snapshot.refresh_token);
        updated.oauth.scopes = snapshot.scopes;
        if snapshot.scope_confirmed {
            updated
                .oauth
                .extra
                .insert("drive_scope_source".into(), json!("token_response"));
        } else if updated.oauth.scopes != current.oauth.scopes {
            updated
                .oauth
                .extra
                .insert("drive_scope_source".into(), json!("configured_unverified"));
        }
        updated.validate()?;
        let before = remote_fingerprint(&current);
        let after = remote_fingerprint(&updated);
        if before != expected && before != after {
            return Err(Error::OAuth(
                "Drive 凭据已被其他操作更新；保留远端凭据等待核查",
            ));
        }
        let changed = before != after;
        if changed {
            updated.oauth.token = Secret::default();
            updated.oauth.expiry = None;
            self.stage_locked(account, &updated)?;
            self.recover_locked(account)?;
        }
        Ok(RemoteAdoption {
            receipt: format!("{:x}", Sha256::digest(raw)),
            updated: changed,
        })
    }
    pub fn clear(&self, account: &ProfileId) -> Result<bool> {
        let _guard = self.guard(account)?;
        self.ensure_unleased_locked(account)?;
        let path = self.storage.profile_path(account, "dfs_credentials.json")?;
        let existed = path.exists();
        // Clear staged credentials BEFORE the main file: an interrupted clear may
        // leave old credentials, but cannot resurrect deleted ones on refresh.
        self.storage
            .remove(&self.storage.profile_path(account, "dfs_pending.json")?)?;
        self.storage.remove(
            &self
                .storage
                .profile_path(account, "dfs_refresh_pending.json")?,
        )?;
        self.storage.remove(&path)?;
        Ok(existed)
    }
    fn userinfo(&self, token: &Secret) -> Result<(String, Option<String>)> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| Error::OAuthTransport)?;
        let response = client
            .get(self.userinfo.clone())
            .bearer_auth(token.expose())
            .send()
            .map_err(|_| Error::OAuthTransport)?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        response
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::OAuthTransport)?;
        if bytes.len() > 65536 {
            return Err(Error::OAuth("Drive userinfo 响应过大"));
        }
        let parsed = serde_json::from_slice::<Value>(&bytes);
        use zeroize::Zeroize;
        bytes.zeroize();
        let value = parsed.map_err(|_| Error::OAuth("Drive userinfo 响应无效"))?;
        let uid = value["sub"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(Error::OAuth("Drive userinfo 缺少 sub"))?
            .to_owned();
        let email = match value.get("email") {
            Some(Value::String(email)) => Some(email.clone()),
            Some(Value::Null) | None => None,
            _ => return Err(Error::OAuth("Drive userinfo email 类型无效")),
        };
        validate_identity(Some(&uid), email.as_deref())?;
        Ok((uid, email))
    }
    fn identify(&self, credentials: &mut DriveCredentials) -> Result<()> {
        // Explicit overrides permit headless imports when userinfo is unavailable.
        // Do not overwrite an already recorded identity merely to mint a token.
        if credentials.user_id.as_deref().is_none_or(str::is_empty) {
            let (uid, email) = self.userinfo(&credentials.oauth.token)?;
            credentials.user_id = Some(uid);
            if credentials.email.is_none() {
                credentials.email = email;
            }
        }
        Ok(())
    }
    fn stage_locked(&self, account: &ProfileId, credentials: &DriveCredentials) -> Result<()> {
        self.storage.write(
            &self
                .storage
                .profile_path(account, "dfs_refresh_pending.json")?,
            &RefreshPending {
                version: 1,
                account: account.clone(),
                credentials: credentials.clone(),
            },
        )
    }
    fn recover_locked(&self, account: &ProfileId) -> Result<()> {
        let path = self
            .storage
            .profile_path(account, "dfs_refresh_pending.json")?;
        if let Some(pending) = self.storage.read::<RefreshPending>(&path)? {
            if pending.version != 1 {
                return Err(Error::Schema(pending.version));
            }
            if &pending.account != account {
                return Err(Error::OAuth("Drive 刷新恢复账户不匹配"));
            }
            pending.credentials.validate()?;
            self.storage.write(
                &self.storage.profile_path(account, "dfs_credentials.json")?,
                &pending.credentials,
            )?;
            self.storage.remove(&path)?;
        }
        Ok(())
    }
    pub fn import(
        &self,
        account: &ProfileId,
        mut credentials: DriveCredentials,
        now: DateTime<Utc>,
    ) -> Result<DriveCredentials> {
        credentials.validate()?;
        let _guard = self.guard(account)?;
        self.ensure_unleased_locked(account)?;
        self.recover_locked(account)?;
        let (mut refreshed, confirmed) = self.client.refresh_with_scope(&credentials.oauth, now)?;
        if confirmed || refreshed.extra.get("drive_scope_source").is_none() {
            refreshed.extra.insert(
                "drive_scope_source".into(),
                json!(if confirmed {
                    "token_response"
                } else {
                    "configured_unverified"
                }),
            );
        }
        credentials.oauth = refreshed;
        // Save a rotated refresh token before a later userinfo/persistence error.
        self.stage_locked(account, &credentials)?;
        self.identify(&mut credentials)?;
        self.stage_locked(account, &credentials)?;
        self.recover_locked(account)?;
        self.storage
            .remove(&self.storage.profile_path(account, "dfs_pending.json")?)?;
        Ok(credentials)
    }
    pub fn token(
        &self,
        account: &ProfileId,
        whoami: bool,
        now: DateTime<Utc>,
    ) -> Result<DriveCredentials> {
        let _guard = self.guard(account)?;
        self.ensure_unleased_locked(account)?;
        self.recover_locked(account)?;
        let mut credentials = self
            .load_locked(account)?
            .ok_or(Error::OAuth("没有 Drive 凭据；请先 consent 或 import"))?;
        let (mut refreshed, confirmed) = self.client.refresh_with_scope(&credentials.oauth, now)?;
        if confirmed || refreshed.extra.get("drive_scope_source").is_none() {
            refreshed.extra.insert(
                "drive_scope_source".into(),
                json!(if confirmed {
                    "token_response"
                } else {
                    "configured_unverified"
                }),
            );
        }
        credentials.oauth = refreshed;
        self.stage_locked(account, &credentials)?;
        if whoami {
            let (uid, email) = self.userinfo(&credentials.oauth.token)?;
            credentials.user_id = Some(uid);
            credentials.email = email;
        }
        self.stage_locked(account, &credentials)?;
        self.recover_locked(account)?;
        Ok(credentials)
    }
    pub fn begin(
        &self,
        account: &ProfileId,
        request: ConsentRequest,
        now: DateTime<Utc>,
    ) -> Result<Url> {
        let _guard = self.guard(account)?;
        self.ensure_unleased_locked(account)?;
        request.client.validate()?;
        validate_endpoint(&request.redirect)?;
        validate_scopes(&request.scopes)?;
        validate_identity(request.user_id.as_deref(), request.email.as_deref())?;
        if !["sdk", "dfs", "custom"].contains(&request.flavor.as_str())
            || request.scopes.is_empty()
            || request.client.token_uri.query().is_some()
            || request.client.auth_uri.query().is_some()
            || request.redirect.query().is_some()
        {
            return Err(Error::OAuth("Drive 授权配置无效"));
        }
        if request.flavor == "dfs"
            && !(request.redirect.scheme() == "http"
                && matches!(
                    request.redirect.host_str(),
                    Some("127.0.0.1" | "localhost" | "[::1]")
                ))
        {
            return Err(Error::OAuth("dfs 授权需要 localhost callback"));
        }
        let pending = Pending {
            flavor: request.flavor,
            user_id: request.user_id,
            email: request.email,
            flow: PendingAuthorization {
                version: 1,
                account: account.clone(),
                client_config: request.client,
                scopes: request.scopes,
                redirect_uri: request.redirect,
                state: random_secret(),
                code_verifier: random_secret(),
                created_at: now,
                exchange_started: false,
                result: None,
            },
        };
        let mut url = pending
            .flow
            .authorization_url(request.login_hint.as_deref());
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .filter(|(key, _)| key != "token_usage")
            .collect();
        url.query_pairs_mut().clear().extend_pairs(pairs);
        if pending.flavor == "sdk" {
            url.query_pairs_mut().append_pair("token_usage", "remote");
        }
        if request.include_granted_scopes {
            url.query_pairs_mut()
                .append_pair("include_granted_scopes", "true");
        }
        self.storage.write(
            &self.storage.profile_path(account, "dfs_pending.json")?,
            &pending,
        )?;
        Ok(url)
    }
    pub fn finish(
        &self,
        account: &ProfileId,
        code: Option<Secret>,
        callback: Option<Url>,
        now: DateTime<Utc>,
    ) -> Result<DriveCredentials> {
        let _guard = self.guard(account)?;
        self.ensure_unleased_locked(account)?;
        let path = self.storage.profile_path(account, "dfs_pending.json")?;
        let mut pending: Pending = self
            .storage
            .read(&path)?
            .ok_or(Error::OAuth("没有待完成的 Drive 授权"))?;
        if pending.flow.version != 1 {
            return Err(Error::Schema(pending.flow.version));
        }
        if &pending.flow.account != account {
            return Err(Error::OAuth("Drive pending 账户不匹配"));
        }
        if pending.flow.result.is_none() {
            if now - pending.flow.created_at > chrono::Duration::minutes(15)
                || pending.flow.created_at > now + chrono::Duration::minutes(1)
            {
                return Err(Error::OAuth("Drive 授权请求已过期"));
            }
            if pending.flow.exchange_started {
                return Err(Error::OAuth("Drive 授权码交换结果未知；请开始新的授权"));
            }
            let code = match (code, callback) {
                (Some(code), None) if !code.is_empty() => code,
                (None, Some(callback)) => pending.flow.callback_code(&callback)?,
                _ => return Err(Error::OAuth("提供唯一的 code 或 callback URL")),
            };
            pending.flow.client_config.validate()?;
            validate_endpoint(&pending.flow.redirect_uri)?;
            validate_scopes(&pending.flow.scopes)?;
            pending.flow.exchange_started = true;
            self.storage.write(&path, &pending)?;
            let (mut credentials, confirmed) =
                self.client.exchange_with_scope(&pending.flow, &code, now)?;
            credentials.extra.insert(
                "drive_scope_source".into(),
                json!(if confirmed {
                    "token_response"
                } else {
                    "requested_unverified"
                }),
            );
            pending.flow.result = Some(credentials);
            self.storage.write(&path, &pending)?;
        }
        let mut credentials = DriveCredentials {
            flavor: pending.flavor,
            user_id: pending.user_id,
            email: pending.email,
            created_at: Some(now.to_rfc3339()),
            oauth: pending.flow.result.unwrap(),
        };
        credentials.validate()?;
        self.identify(&mut credentials)?;
        self.stage_locked(account, &credentials)?;
        self.recover_locked(account)?;
        self.storage.remove(&path)?;
        Ok(credentials)
    }
}
pub struct ConsentRequest {
    pub flavor: String,
    pub client: OAuthClient,
    pub redirect: Url,
    pub scopes: Vec<String>,
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub login_hint: Option<String>,
    pub include_granted_scopes: bool,
}
/// Build imported credentials without ever formatting token material in errors.
pub struct ImportRequest {
    pub refresh_token: Secret,
    pub client_id: String,
    pub client_secret: Option<Secret>,
    pub token_uri: String,
    pub scopes: Vec<String>,
    pub user_id: Option<String>,
    pub email: Option<String>,
}
pub fn imported(request: ImportRequest, now: DateTime<Utc>) -> Result<DriveCredentials> {
    let ImportRequest {
        refresh_token,
        client_id,
        client_secret,
        token_uri,
        scopes,
        user_id,
        email,
    } = request;
    let credentials = DriveCredentials {
        flavor: "imported".into(),
        user_id,
        email,
        created_at: Some(now.to_rfc3339()),
        oauth: OAuthCredentials {
            token: Secret::default(),
            refresh_token: Some(refresh_token),
            token_uri,
            client_id,
            client_secret,
            scopes,
            expiry: None,
            extra: Map::new(),
        },
    };
    credentials.validate()?;
    Ok(credentials)
}
