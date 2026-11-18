use super::{OAuthCredentials, PUBLIC_SCOPES, TokenClient, expiry, token_uri, validate_endpoint};
use crate::{Error, Result, error::io, models::Secret};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};
use url::Url;
use zeroize::{Zeroize, Zeroizing};
mod aws;
mod certificate;
pub use certificate::CertificateOptions;
mod executable;
mod external;
pub use aws::AwsEnvironment;
mod impersonation;
mod metadata;
pub use metadata::{MetadataMtlsOptions, MetadataOptions};
#[cfg(test)]
mod test_tls;
mod workforce;
use impersonation::Impersonation;

/// A provider owns one source and one cache; no process-wide ADC identity or env mutation.
pub struct AdcProvider {
    path: Option<PathBuf>,
    required: bool,
    metadata: MetadataOptions,
    aws: AwsEnvironment,
    certificate: Box<CertificateOptions>,
    allow_executables: bool,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    client: TokenClient,
    cache: Mutex<Option<LoadedSource>>,
    quota_override: Option<String>,
}
/// A coherent snapshot: the bearer and request quota belong to the same source.
/// No Debug/Serialize implementation: callers must not log credentials.
pub struct RequestCredentials {
    pub token: Secret,
    pub quota_project_id: Option<String>,
}
struct LoadedSource {
    source: Source,
    quota_project_id: Option<String>,
}
enum Source {
    Metadata(Box<metadata::Metadata>),
    Workforce(Box<workforce::Workforce>),
    User(OAuthCredentials),
    Service {
        account: Box<ServiceAccount>,
        token: Option<OAuthCredentials>,
    },
    External {
        account: Box<external::External>,
        token: Option<OAuthCredentials>,
    },
    Impersonated {
        account: Box<Impersonation>,
        token: Option<OAuthCredentials>,
    },
}
struct ServiceAccount {
    email: String,
    key_id: Option<String>,
    key: RsaKeyPair,
    token_uri: String,
}
#[derive(Deserialize)]
struct ServiceAccountFile {
    client_email: String,
    private_key: Secret,
    #[serde(default)]
    private_key_id: Option<String>,
    #[serde(default = "token_uri")]
    token_uri: String,
    #[serde(default)]
    universe_domain: Option<String>,
    #[serde(default)]
    subject: Option<String>,
}
impl ServiceAccount {
    fn load(value: Value) -> Result<Self> {
        let file: ServiceAccountFile = serde_json::from_value(value)
            .map_err(|_| Error::OAuth("service_account ADC 字段无效"))?;
        if file.client_email.len() > 320
            || file
                .client_email
                .split_once('@')
                .is_none_or(|(user, domain)| {
                    user.is_empty() || domain.is_empty() || domain.contains('@')
                })
            || file
                .client_email
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || file
                .private_key_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(Error::OAuth("service_account ADC 身份字段无效"));
        }
        if file.subject.is_some()
            || file
                .universe_domain
                .as_deref()
                .is_some_and(|s| s != "googleapis.com")
        {
            return Err(Error::OAuth(
                "service_account ADC 暂不支持 subject 或其他 universe",
            ));
        }
        let uri = Url::parse(&file.token_uri).map_err(|_| Error::OAuth("token URI 无效"))?;
        validate_endpoint(&uri)?;
        if uri.query().is_some() {
            return Err(Error::OAuth("service_account token URI 不接受 query"));
        }
        // Google's downloaded keys are unencrypted PKCS#8 PEM. Reject extra PEM
        // blocks, encrypted/PKCS#1 material and malformed DER before any request.
        let pem = file.private_key.expose().trim();
        let body = pem
            .strip_prefix("-----BEGIN PRIVATE KEY-----")
            .and_then(|s| s.strip_suffix("-----END PRIVATE KEY-----"))
            .ok_or(Error::OAuth("service_account 需要 PKCS#8 RSA 私钥"))?;
        let encoded = Zeroizing::new(
            body.chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect::<String>(),
        );
        let der = Zeroizing::new(
            STANDARD
                .decode(encoded.as_bytes())
                .map_err(|_| Error::OAuth("service_account 私钥编码无效"))?,
        );
        let key = RsaKeyPair::from_pkcs8(&der)
            .map_err(|_| Error::OAuth("service_account RSA 私钥无效"))?;
        Ok(Self {
            email: file.client_email,
            key_id: file.private_key_id,
            key,
            token_uri: file.token_uri,
        })
    }
    fn assertion(&self, now: DateTime<Utc>) -> Result<Secret> {
        let issued = now.timestamp();
        let expires = issued
            .checked_add(3600)
            .filter(|_| issued >= 0)
            .ok_or(Error::OAuth("service_account JWT 时间无效"))?;
        let mut header = json!({"alg":"RS256","typ":"JWT"});
        if let Some(id) = &self.key_id {
            header["kid"] = json!(id);
        }
        let claims = json!({"iss":self.email,"scope":PUBLIC_SCOPES.join(" "),
            "aud":"https://oauth2.googleapis.com/token","iat":issued,"exp":expires});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| Error::OAuth("service_account JWT 签名失败"))?;
        Ok(Secret::new(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
    fn exchange(&self, client: &TokenClient, now: DateTime<Utc>) -> Result<OAuthCredentials> {
        let assertion = self.assertion(now)?;
        let mut form = vec![
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            ),
            ("assertion".into(), assertion.expose().into()),
        ];
        let response = client.request(&self.token_uri, &form);
        form[1].1.zeroize();
        let response = response?;
        if response.expires_in.is_none() {
            return Err(Error::OAuth("service_account token 缺少到期时间"));
        }
        Ok(OAuthCredentials {
            token: response.access_token,
            refresh_token: None,
            token_uri: self.token_uri.clone(),
            client_id: self.email.clone(),
            client_secret: None,
            scopes: response
                .scope
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_else(|| PUBLIC_SCOPES.iter().map(|s| (*s).into()).collect()),
            expiry: expiry(now, response.expires_in)?,
            extra: Map::new(),
        })
    }
}
impl Source {
    fn load(
        path: &Path,
        allow_executables: bool,
        aws: AwsEnvironment,
        cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        quota_override: Option<&str>,
        certificate: &CertificateOptions,
    ) -> Result<LoadedSource> {
        let file = fs::File::open(path).map_err(|e| io(path, e))?;
        let mut raw = Zeroizing::new(Vec::new());
        file.take(65537)
            .read_to_end(&mut raw)
            .map_err(|e| io(path, e))?;
        if raw.len() > 65536 {
            return Err(Error::OAuth("ADC 文件过大"));
        }
        let mut value: Value =
            serde_json::from_slice(&raw).map_err(|_| Error::InvalidData(path.into()))?;
        let quota_project_id = super::quota::select(&value, quota_override)?;
        // External-account IAM calls use the effective source quota. Do not
        // alter nested impersonation source credentials or the file on disk.
        if let Some(object) = value.as_object_mut() {
            object.insert("quota_project_id".into(), json!(quota_project_id));
        }
        Ok(LoadedSource {
            source: Self::from_value(
                value,
                path,
                false,
                allow_executables,
                aws,
                cancellation,
                certificate,
            )?,
            quota_project_id,
        })
    }
    fn from_value(
        mut value: Value,
        path: &Path,
        embedded: bool,
        allow_executables: bool,
        aws: AwsEnvironment,
        cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        certificate: &CertificateOptions,
    ) -> Result<Self> {
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("missing")
        {
            "authorized_user" => {
                // User ADC may encode scopes as a space-separated string.
                if let Some(scopes) = value.get("scopes").and_then(Value::as_str) {
                    value["scopes"] = json!(scopes.split_whitespace().collect::<Vec<_>>());
                }
                Ok(Self::User(
                    serde_json::from_value(value).map_err(|_| Error::InvalidData(path.into()))?,
                ))
            }
            "service_account" => Ok(Self::Service {
                account: Box::new(ServiceAccount::load(value)?),
                token: None,
            }),
            "external_account_authorized_user" => Ok(Self::Workforce(Box::new(
                workforce::Workforce::load(value)?,
            ))),
            "external_account" if !embedded => Ok(Self::External {
                account: Box::new(external::External::load(
                    value,
                    path,
                    allow_executables,
                    aws,
                    cancellation,
                    certificate,
                )?),
                token: None,
            }),
            "impersonated_service_account" if !embedded => Ok(Self::Impersonated {
                account: Box::new(Impersonation::load(value, path)?),
                token: None,
            }),
            "impersonated_service_account" => {
                Err(Error::OAuth("ADC 暂不支持递归 impersonation 来源"))
            }
            kind => Err(Error::UnsupportedAdc(match kind {
                "missing" | "external_account" => kind.into(),
                _ => "unknown".into(),
            })),
        }
    }
    fn access_token(
        &mut self,
        client: &TokenClient,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<Secret> {
        match self {
            Self::Metadata(source) => source.access_token(now, force),
            Self::Workforce(source) => source.access_token(client, now, force),
            Self::User(token) => {
                if force || !token.valid_at(now) {
                    *token = client.refresh(token, now)?;
                }
                Ok(token.token.clone())
            }
            Self::Service { account, token } => {
                if force || token.as_ref().is_none_or(|token| !token.valid_at(now)) {
                    *token = Some(account.exchange(client, now)?);
                }
                Ok(token.as_ref().unwrap().token.clone())
            }
            Self::External { account, token } => {
                if force || token.as_ref().is_none_or(|token| !token.valid_at(now)) {
                    *token = Some(account.exchange(client, now, force)?);
                }
                Ok(token.as_ref().unwrap().token.clone())
            }
            Self::Impersonated { account, token } => {
                if force || token.as_ref().is_none_or(|token| !token.valid_at(now)) {
                    *token = Some(account.exchange(client, now)?);
                }
                Ok(token.as_ref().unwrap().token.clone())
            }
        }
    }
}
impl AdcProvider {
    pub fn new(path: PathBuf, client: TokenClient) -> Self {
        Self {
            path: Some(path),
            required: true,
            metadata: MetadataOptions::default(),
            aws: AwsEnvironment::default(),
            certificate: Box::default(),
            allow_executables: false,
            cancellation: None,
            client,
            cache: Mutex::new(None),
            quota_override: None,
        }
    }
    pub fn from_context(context: &crate::context::AccountContext, client: TokenClient) -> Self {
        Self {
            path: context.adc_file.clone(),
            required: context.adc_file_required,
            metadata: context.metadata.clone(),
            aws: context.aws.clone(),
            certificate: Box::new(context.certificate.clone()),
            allow_executables: context.allow_executables,
            cancellation: context.cancellation.clone(),
            client,
            cache: Mutex::new(None),
            quota_override: context.quota_project.clone(),
        }
    }
    fn load(&self) -> Result<LoadedSource> {
        if let Some(path) = &self.path {
            match Source::load(
                path,
                self.allow_executables,
                self.aws.clone(),
                self.cancellation.clone(),
                self.quota_override.as_deref(),
                &self.certificate,
            ) {
                Err(Error::Io { source, .. })
                    if !self.required && source.kind() == std::io::ErrorKind::NotFound => {}
                result => return result,
            }
        }
        let quota_project_id = super::quota::select(&Value::Null, self.quota_override.as_deref())?;
        Ok(LoadedSource {
            source: Source::Metadata(Box::new(metadata::Metadata::discover(&self.metadata)?)),
            quota_project_id,
        })
    }
    pub fn access_token(&self, now: DateTime<Utc>) -> Result<Secret> {
        self.credentials_inner(now, false)
            .map(|credentials| credentials.token)
    }
    pub fn refresh_access_token(&self, now: DateTime<Utc>) -> Result<Secret> {
        self.credentials_inner(now, true)
            .map(|credentials| credentials.token)
    }
    pub fn request_credentials(&self, now: DateTime<Utc>) -> Result<RequestCredentials> {
        self.credentials_inner(now, false)
    }
    pub fn refresh_request_credentials(&self, now: DateTime<Utc>) -> Result<RequestCredentials> {
        self.credentials_inner(now, true)
    }
    fn cancelled(&self) -> Result<()> {
        let signal = self
            .cancellation
            .as_ref()
            .map_or(0, |flag| flag.load(std::sync::atomic::Ordering::SeqCst));
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        Ok(())
    }
    fn credentials_inner(&self, now: DateTime<Utc>, force: bool) -> Result<RequestCredentials> {
        self.cancelled()?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Error::OAuth("ADC 缓存锁失效"))?;
        if cache.is_none() {
            *cache = Some(self.load()?);
        }
        self.cancelled()?;
        let loaded = cache.as_mut().unwrap();
        let token = loaded.source.access_token(&self.client, now, force)?;
        let quota_project_id = loaded.quota_project_id.clone();
        self.cancelled()?;
        Ok(RequestCredentials {
            token,
            quota_project_id,
        })
    }
}
