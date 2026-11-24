mod adc;
mod control;
mod default_client;
pub use control::ControlCredentials;
pub(crate) mod quota;
use crate::{
    Error, Result,
    credentials::TokenStore,
    error::io,
    models::{ProfileId, Secret},
    storage::Storage,
};
pub use adc::{
    AdcProvider, AwsEnvironment, CertificateOptions, MetadataMtlsOptions, MetadataOptions,
    RequestCredentials,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{fmt, fs, io::Read, path::Path, time::Duration};
use url::Url;

pub const PUBLIC_SCOPES: [&str; 6] = [
    "openid",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/colaboratory",
    "https://www.googleapis.com/auth/drive.file",
];
pub const REMOTE_REDIRECT: &str = "https://sdk.cloud.google.com/applicationdefaultauthcode.html";
fn token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    #[serde(default)]
    pub token: Secret,
    #[serde(default)]
    pub refresh_token: Option<Secret>,
    #[serde(default = "token_uri")]
    pub token_uri: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<Secret>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_expiry")]
    pub expiry: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("expiry", &self.expiry)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}
fn deserialize_expiry<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<DateTime<Utc>>, D::Error> {
    let Some(raw) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(&raw)
        .map(|d| Some(d.with_timezone(&Utc)))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|d| Some(d.and_utc()))
        })
        .map_err(|_| serde::de::Error::custom("invalid credential expiry"))
}
impl OAuthCredentials {
    pub fn valid_at(&self, now: DateTime<Utc>) -> bool {
        !self.token.is_empty()
            && self
                .expiry
                .is_some_and(|t| t > now + chrono::Duration::seconds(60))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<Secret>,
    pub auth_uri: Url,
    pub token_uri: Url,
}
impl OAuthClient {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw: Value = serde_json::from_slice(&fs::read(path).map_err(|e| io(path, e))?)
            .map_err(|_| Error::InvalidData(path.into()))?;
        let inner = raw
            .get("installed")
            .or_else(|| raw.get("web"))
            .unwrap_or(&raw);
        let client: Self =
            serde_json::from_value(inner.clone()).map_err(|_| Error::InvalidData(path.into()))?;
        client.validate()?;
        Ok(client)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.client_id.trim().is_empty() {
            return Err(Error::OAuth("缺少 client_id"));
        }
        validate_endpoint(&self.auth_uri)?;
        validate_endpoint(&self.token_uri)
    }
}
pub fn validate_endpoint(url: &Url) -> Result<()> {
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(Error::OAuth("端点必须使用 HTTPS；仅 loopback 允许 HTTP"));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAuthorization {
    #[serde(default = "pending_version")]
    pub version: u32,
    pub account: ProfileId,
    pub client_config: OAuthClient,
    pub scopes: Vec<String>,
    pub redirect_uri: Url,
    pub state: Secret,
    pub code_verifier: Secret,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub exchange_started: bool,
    #[serde(default)]
    pub result: Option<OAuthCredentials>,
}
fn pending_version() -> u32 {
    1
}
impl PendingAuthorization {
    pub fn authorization_url(&self, email_hint: Option<&str>) -> Url {
        let mut url = self.client_config.auth_uri.clone();
        let challenge =
            URL_SAFE_NO_PAD.encode(Sha256::digest(self.code_verifier.expose().as_bytes()));
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_config.client_id)
            .append_pair("redirect_uri", self.redirect_uri.as_str())
            .append_pair("response_type", "code")
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", self.state.expose())
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
        if self.redirect_uri.as_str() == REMOTE_REDIRECT {
            url.query_pairs_mut().append_pair("token_usage", "remote");
        }
        if let Some(email) = email_hint {
            url.query_pairs_mut().append_pair("login_hint", email);
        }
        url
    }
    pub fn callback_code(&self, callback: &Url) -> Result<Secret> {
        if callback.scheme() != self.redirect_uri.scheme()
            || callback.host_str() != self.redirect_uri.host_str()
            || callback.port_or_known_default() != self.redirect_uri.port_or_known_default()
            || callback.path() != self.redirect_uri.path()
            || callback.fragment().is_some()
            || !callback.username().is_empty()
            || callback.password().is_some()
        {
            return Err(Error::OAuth("授权回调地址不匹配"));
        }
        let pairs: Vec<_> = callback.query_pairs().collect();
        let states: Vec<_> = pairs.iter().filter(|(k, _)| k == "state").collect();
        let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
        if states.len() != 1
            || !constant_time_equal(states[0].1.as_bytes(), self.state.expose().as_bytes())
        {
            return Err(Error::OAuth("OAuth state 不匹配"));
        }
        if pairs.iter().any(|(k, _)| k == "error") {
            return Err(Error::OAuth("授权被拒绝"));
        }
        if codes.len() != 1 || codes[0].1.is_empty() {
            return Err(Error::OAuth("回调缺少唯一 code"));
        }
        Ok(Secret::new(codes[0].1.as_ref()))
    }
}
fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.ct_eq(b))
}
pub(crate) fn random_secret() -> Secret {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    Secret::new(URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Clone)]
pub struct TokenClient {
    client: reqwest::blocking::Client,
    extra_roots: Vec<reqwest::Certificate>,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Secret,
    #[serde(default)]
    refresh_token: Option<Secret>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
}
impl TokenClient {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| Error::OAuthTransport)?;
        Ok(Self {
            client,
            extra_roots: Vec::new(),
        })
    }
    fn with_identity(&self, identity: reqwest::Identity) -> Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .identity(identity);
        for root in &self.extra_roots {
            builder = builder.add_root_certificate(root.clone());
        }
        Ok(Self {
            client: builder.build().map_err(|_| Error::OAuthTransport)?,
            extra_roots: self.extra_roots.clone(),
        })
    }
    fn request(&self, uri: &str, form: &[(String, String)]) -> Result<TokenResponse> {
        let uri = Url::parse(uri).map_err(|_| Error::OAuth("token URI 无效"))?;
        validate_endpoint(&uri)?;
        let response = self
            .client
            .post(uri)
            .form(form)
            .send()
            .map_err(|_| Error::OAuthTransport)?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut body = zeroize::Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("token 响应过大"));
        }
        let response: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| Error::OAuth("token 响应无效"))?;
        if response.access_token.is_empty()
            || response.expires_in.is_some_and(|n| n <= 0)
            || response
                .token_type
                .as_deref()
                .is_some_and(|s| !s.eq_ignore_ascii_case("bearer"))
        {
            return Err(Error::OAuth("token 响应字段无效"));
        }
        Ok(response)
    }
    pub fn refresh(&self, old: &OAuthCredentials, now: DateTime<Utc>) -> Result<OAuthCredentials> {
        self.refresh_with_scope(old, now)
            .map(|(credentials, _)| credentials)
    }
    pub(crate) fn refresh_with_scope(
        &self,
        old: &OAuthCredentials,
        now: DateTime<Utc>,
    ) -> Result<(OAuthCredentials, bool)> {
        let refresh = old
            .refresh_token
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or(Error::NeedsAuth)?;
        if old.client_id.is_empty() {
            return Err(Error::OAuth("缺少 client_id"));
        }
        let mut form = vec![
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), refresh.expose().into()),
            ("client_id".into(), old.client_id.clone()),
        ];
        if let Some(secret) = &old.client_secret {
            form.push(("client_secret".into(), secret.expose().into()));
        }
        // Do not expand the previously granted scope set on refresh.
        let response = self.request(&old.token_uri, &form)?;
        let confirmed = response.scope.is_some();
        let mut new = old.clone();
        new.token = response.access_token;
        if let Some(refresh) = response.refresh_token {
            new.refresh_token = Some(refresh);
        }
        if let Some(scope) = response.scope {
            new.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        new.expiry = expiry(now, response.expires_in)?;
        Ok((new, confirmed))
    }
    pub(crate) fn exchange(
        &self,
        pending: &PendingAuthorization,
        code: &Secret,
        now: DateTime<Utc>,
    ) -> Result<OAuthCredentials> {
        self.exchange_with_scope(pending, code, now)
            .map(|(credentials, _)| credentials)
    }
    pub(crate) fn exchange_with_scope(
        &self,
        pending: &PendingAuthorization,
        code: &Secret,
        now: DateTime<Utc>,
    ) -> Result<(OAuthCredentials, bool)> {
        if code.is_empty() {
            return Err(Error::OAuth("授权码为空"));
        }
        let mut form = vec![
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), code.expose().into()),
            ("client_id".into(), pending.client_config.client_id.clone()),
            ("redirect_uri".into(), pending.redirect_uri.to_string()),
            (
                "code_verifier".into(),
                pending.code_verifier.expose().into(),
            ),
        ];
        if let Some(secret) = &pending.client_config.client_secret {
            form.push(("client_secret".into(), secret.expose().into()));
        }
        let response = self.request(pending.client_config.token_uri.as_str(), &form)?;
        if response.refresh_token.as_ref().is_none_or(Secret::is_empty) {
            return Err(Error::OAuth("缺少 refresh_token，请重新同意离线访问"));
        }
        let confirmed = response.scope.is_some();
        Ok((
            OAuthCredentials {
                token: response.access_token,
                refresh_token: response.refresh_token,
                token_uri: pending.client_config.token_uri.to_string(),
                client_id: pending.client_config.client_id.clone(),
                client_secret: pending.client_config.client_secret.clone(),
                scopes: response
                    .scope
                    .map(|s| s.split_whitespace().map(str::to_owned).collect())
                    .unwrap_or_else(|| pending.scopes.clone()),
                expiry: expiry(now, response.expires_in)?,
                extra: Map::new(),
            },
            confirmed,
        ))
    }
}
fn expiry(now: DateTime<Utc>, seconds: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    seconds
        .map(|n| {
            now.checked_add_signed(
                chrono::Duration::try_seconds(n).ok_or(Error::OAuth("到期时间越界"))?,
            )
            .ok_or(Error::OAuth("到期时间越界"))
        })
        .transpose()
}

#[derive(Clone)]
pub struct OAuthService {
    storage: Storage,
    tokens: TokenStore,
    client: TokenClient,
}
impl OAuthService {
    fn guard_account(&self, account: &ProfileId) -> Result<crate::storage::FileLock> {
        let guard = self.storage.profile_lock(account)?;
        self.validate_account(account)?;
        Ok(guard)
    }
    fn validate_account(&self, account: &ProfileId) -> Result<()> {
        if self.storage.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let registry: crate::models::Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or_else(|| Error::AccountMissing(account.to_string()))?;
        registry.validate()?;
        if !registry.accounts.contains_key(account) {
            return Err(Error::AccountMissing(account.to_string()));
        }
        Ok(())
    }
    pub fn new(storage: Storage, tokens: TokenStore, client: TokenClient) -> Self {
        Self {
            storage,
            tokens,
            client,
        }
    }
    pub fn begin(
        &self,
        account: &ProfileId,
        client: OAuthClient,
        redirect: Url,
        email: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Url> {
        let _lifecycle = self.guard_account(account)?;
        client.validate()?;
        validate_endpoint(&redirect)?;
        if client.auth_uri.query().is_some() || redirect.query().is_some() {
            return Err(Error::OAuth("授权端点与 redirect 不得包含预设 query"));
        }
        let _lock = self
            .storage
            .lock(&self.storage.profile_path(account, "oauth_flow.rust.lock")?)?;
        let pending = PendingAuthorization {
            version: 1,
            account: account.clone(),
            client_config: client,
            scopes: PUBLIC_SCOPES.iter().map(|s| s.to_string()).collect(),
            redirect_uri: redirect,
            state: random_secret(),
            code_verifier: random_secret(),
            created_at: now,
            exchange_started: false,
            result: None,
        };
        let url = pending.authorization_url(email);
        self.storage.write(
            &self.storage.profile_path(account, "oauth_pending.json")?,
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
    ) -> Result<()> {
        let _lifecycle = self.guard_account(account)?;
        let _lock = self
            .storage
            .lock(&self.storage.profile_path(account, "oauth_flow.rust.lock")?)?;
        let path = self.storage.profile_path(account, "oauth_pending.json")?;
        let mut pending: PendingAuthorization =
            self.storage.read(&path)?.ok_or(Error::NeedsAuth)?;
        if pending.version != 1 {
            return Err(Error::Schema(pending.version));
        }
        if &pending.account != account {
            return Err(Error::OAuth("pending 账户不匹配"));
        }
        if pending.result.is_none() {
            if now - pending.created_at > chrono::Duration::minutes(15)
                || pending.created_at > now + chrono::Duration::minutes(1)
            {
                return Err(Error::OAuth("授权请求已过期"));
            }
            if pending.exchange_started {
                return Err(Error::OAuth("授权码交换结果未知，请开始新的授权"));
            }
            let code = match (code, callback) {
                (None, Some(callback)) => pending.callback_code(&callback)?,
                (Some(code), None) if !code.is_empty() => code,
                _ => return Err(Error::OAuth("提供唯一的 code 或 callback URL")),
            };
            pending.exchange_started = true;
            self.storage.write(&path, &pending)?;
            pending.result = Some(self.client.exchange(&pending, &code, now)?);
            // Stage a successful exchange before keyring writes; failed persistence is resumable.
            self.storage.write(&path, &pending)?;
        }
        let _refresh = self.storage.lock(
            &self
                .storage
                .profile_path(account, "oauth_refresh.rust.lock")?,
        )?;
        self.tokens
            .save(account, pending.result.as_ref().unwrap())?;
        self.storage.remove(
            &self
                .storage
                .profile_path(account, "oauth_refresh_pending.json")?,
        )?;
        self.storage.remove(&path)
    }
    pub fn access_token(&self, account: &ProfileId, now: DateTime<Utc>) -> Result<Secret> {
        self.access_token_inner(account, now, false)
    }
    /// Explicit identity diagnostics require a fresh provider response even if cached.
    pub fn refresh_access_token(&self, account: &ProfileId, now: DateTime<Utc>) -> Result<Secret> {
        self.access_token_inner(account, now, true)
    }
    fn access_token_inner(
        &self,
        account: &ProfileId,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<Secret> {
        self.oauth_credentials(account, now, force, None)
            .map(|credentials| credentials.token)
    }
    fn oauth_credentials(
        &self,
        account: &ProfileId,
        now: DateTime<Utc>,
        force: bool,
        guard: Option<&crate::storage::FileLock>,
    ) -> Result<OAuthCredentials> {
        let _lifecycle = match guard {
            Some(guard) if guard.protects_profile(&self.storage, account)? => {
                self.validate_account(account)?;
                None
            }
            Some(_) => return Err(Error::OAuth("凭据刷新账户锁不匹配")),
            None => Some(self.guard_account(account)?),
        };
        let _lock = self.storage.lock(
            &self
                .storage
                .profile_path(account, "oauth_refresh.rust.lock")?,
        )?;
        let staged = self
            .storage
            .profile_path(account, "oauth_refresh_pending.json")?;
        if let Some(token) = self.storage.read::<OAuthCredentials>(&staged)? {
            self.tokens.save(account, &token)?;
            self.storage.remove(&staged)?;
        }
        let mut token = self.tokens.load(account)?;
        if token.is_none() && account == &ProfileId::default_profile() {
            let registry: crate::models::Registry = self
                .storage
                .read(&self.storage.path("accounts.json")?)?
                .ok_or(Error::NeedsAuth)?;
            if registry
                .accounts
                .get(account)
                .is_some_and(|a| a.migrated_legacy)
            {
                self.tokens.migrate_legacy()?;
                token = self.tokens.load(account)?;
            }
        }
        let mut token = token.ok_or(Error::NeedsAuth)?;
        if force || !token.valid_at(now) {
            token = self.client.refresh(&token, now)?;
            self.storage.write(&staged, &token)?;
            self.tokens.save(account, &token)?;
            self.storage.remove(&staged)?;
        }
        Ok(token)
    }
}
