//! File/URL federation. Source retrieval, STS and optional IAM each have explicit boundaries.
use super::impersonation::Target;
use super::{OAuthCredentials, PUBLIC_SCOPES, TokenClient, expiry, validate_endpoint};
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;
use zeroize::{Zeroize, Zeroizing};
const ACCESS: &str = "urn:ietf:params:oauth:token-type:access_token";
const CLOUD: &str = "https://www.googleapis.com/auth/cloud-platform";
pub(super) struct External {
    quota_project: Option<String>,
    audience: String,
    kind: String,
    uri: Url,
    subject: Subject,
    format: Format,
    target: Option<Target>,
    project: Option<String>,
    client_auth: Option<(String, Secret)>,
    sts: Option<OAuthCredentials>,
}
enum Subject {
    Certificate(super::certificate::Certificate),
    Aws(Box<super::aws::Aws>),
    Executable(super::executable::Executable),
    File(PathBuf),
    Url {
        uri: Url,
        headers: HeaderMap,
        client: reqwest::blocking::Client,
    },
}
enum Format {
    Text,
    Json(String),
}
#[derive(Deserialize)]
struct File {
    audience: String,
    subject_token_type: String,
    token_url: String,
    credential_source: Value,
    #[serde(default)]
    service_account_impersonation_url: Option<String>,
    #[serde(default)]
    service_account_impersonation: Option<ImpersonationOptions>,
    #[serde(default)]
    workforce_pool_user_project: Option<String>,
    #[serde(default)]
    universe_domain: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<Secret>,
}
#[derive(Deserialize)]
struct ImpersonationOptions {
    #[serde(default = "lifetime")]
    token_lifetime_seconds: u64,
}
fn lifetime() -> u64 {
    3600
}
fn segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
pub(super) fn audience(raw: &str) -> Result<bool> {
    if raw.len() > 2048 {
        return Err(Error::OAuth("external ADC audience 无效"));
    }
    let tail = raw
        .strip_prefix("//iam.googleapis.com/")
        .ok_or(Error::OAuth("external ADC audience 无效"))?;
    let p: Vec<_> = tail.split('/').collect();
    let workload = match p.as_slice() {
        [
            "projects",
            number,
            "locations",
            location,
            "workloadIdentityPools",
            pool,
            "providers",
            provider,
        ] => {
            !number.is_empty()
                && number.bytes().all(|c| c.is_ascii_digit())
                && [location, pool, provider].iter().all(|s| segment(s))
        }
        [
            "locations",
            location,
            "workforcePools",
            pool,
            "providers",
            provider,
        ] => {
            if [location, pool, provider].iter().all(|s| segment(s)) {
                return Ok(true);
            }
            false
        }
        _ => false,
    };
    if workload {
        Ok(false)
    } else {
        Err(Error::OAuth("external ADC audience 无效"))
    }
}
impl External {
    pub(super) fn load(
        value: Value,
        path: &Path,
        allow_executables: bool,
        aws: super::AwsEnvironment,
        cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        certificate: &super::CertificateOptions,
    ) -> Result<Self> {
        let quota_project = crate::auth::quota::select(&value, None)?;
        let file: File = serde_json::from_value(value)
            .map_err(|_| Error::OAuth("external_account ADC 字段无效"))?;
        let workforce = audience(&file.audience)?;
        if file
            .universe_domain
            .as_deref()
            .is_some_and(|s| s != "googleapis.com")
        {
            return Err(Error::OAuth("external ADC 暂不支持其他 universe"));
        }
        // Match google-auth: a nonempty client ID enables HTTP Basic;
        // a missing secret becomes empty, and a secret without an ID is unused.
        let client_auth = file
            .client_id
            .filter(|id| !id.is_empty())
            .map(|id| (id, file.client_secret.unwrap_or_default()));
        if !matches!(
            file.subject_token_type.as_str(),
            "urn:ietf:params:oauth:token-type:jwt"
                | "urn:ietf:params:oauth:token-type:idToken"
                | "urn:ietf:params:oauth:token-type:id_token"
                | "urn:ietf:params:oauth:token-type:saml2"
                | "urn:ietf:params:aws:token-type:aws4_request"
                | "urn:ietf:params:oauth:token-type:mtls"
        ) {
            return Err(Error::OAuth("external ADC 主体令牌类型不受支持"));
        }
        let uri = Url::parse(&file.token_url).map_err(|_| Error::OAuth("STS URL 无效"))?;
        validate_endpoint(&uri)?;
        let loopback = matches!(uri.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if uri.query().is_some()
            || uri.path() != "/v1/token"
            || (!loopback
                && (!matches!(
                    uri.host_str(),
                    Some("sts.googleapis.com" | "sts.mtls.googleapis.com")
                ) || uri.port().is_some_and(|p| p != 443)))
        {
            return Err(Error::OAuth("STS 仅接受 Google v1/token 或 loopback 端点"));
        }
        if file
            .workforce_pool_user_project
            .as_ref()
            .is_some_and(|s| !workforce || s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()))
        {
            return Err(Error::OAuth(
                "workforce user project 必须为 workforce 的项目编号",
            ));
        }
        if file.service_account_impersonation.is_some()
            && file.service_account_impersonation_url.is_none()
        {
            return Err(Error::OAuth("impersonation options 缺少目标 URL"));
        }
        let target = file
            .service_account_impersonation_url
            .as_deref()
            .map(|raw| {
                Target::new(
                    raw,
                    vec![],
                    file.service_account_impersonation
                        .as_ref()
                        .map_or(3600, |o| o.token_lifetime_seconds),
                )
            })
            .transpose()?;
        let source = file
            .credential_source
            .as_object()
            .ok_or(Error::OAuth("external credential_source 无效"))?;
        let is_certificate = source.contains_key("certificate");
        if is_certificate != (file.subject_token_type == "urn:ietf:params:oauth:token-type:mtls") {
            return Err(Error::OAuth("证书来源与 mtls 主体类型不匹配"));
        }
        if is_certificate
            && (workforce
                || uri.scheme() != "https"
                || ["file", "url", "executable", "environment_id"]
                    .iter()
                    .any(|key| source.contains_key(*key)))
        {
            return Err(Error::OAuth("证书来源需要独立 workload 配置和 HTTPS STS"));
        }
        let format = match source.get("format") {
            None => Format::Text,
            Some(value) => match value.get("type").and_then(Value::as_str) {
                Some("text") => Format::Text,
                Some("json") => Format::Json(
                    value
                        .get("subject_token_field_name")
                        .and_then(Value::as_str)
                        .filter(|s| {
                            !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                        })
                        .ok_or(Error::OAuth("主体 JSON 缺少字段名"))?
                        .into(),
                ),
                _ => return Err(Error::OAuth("主体令牌 format 无效")),
            },
        };
        let is_aws = file.subject_token_type == "urn:ietf:params:aws:token-type:aws4_request";
        if is_aws != source.contains_key("environment_id") || (is_aws && workforce) {
            return Err(Error::OAuth("AWS 主体类型与来源或 audience 不匹配"));
        }
        let subject = if is_certificate {
            Subject::Certificate(super::certificate::Certificate::load(
                source["certificate"].clone(),
                certificate,
            )?)
        } else if is_aws {
            Subject::Aws(Box::new(super::aws::Aws::load(
                file.credential_source.clone(),
                aws,
                &file.audience,
                cancellation,
            )?))
        } else if let Some(executable) = source.get("executable") {
            if ["file", "url", "format"]
                .iter()
                .any(|key| source.contains_key(*key))
            {
                return Err(Error::OAuth("可执行 ADC 来源不能混用 file/url/format"));
            }
            Subject::Executable(super::executable::Executable::load(
                executable.clone(),
                allow_executables,
                &file.audience,
                &file.subject_token_type,
                target.as_ref().map(Target::identifier),
                cancellation,
            )?)
        } else if let Some(file) = source.get("file") {
            let file = file
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error::OAuth("主体文件路径无效"))?;
            let file = Path::new(file);
            let file = if file.is_absolute() {
                file.into()
            } else {
                path.parent().unwrap_or(Path::new(".")).join(file)
            };
            Subject::File(file)
        } else {
            let raw = source
                .get("url")
                .and_then(Value::as_str)
                .ok_or(Error::OAuth("external ADC 需要 file 或 url 来源"))?;
            let uri = Url::parse(raw).map_err(|_| Error::OAuth("主体 URL 无效"))?;
            let headers: BTreeMap<String, Secret> = source
                .get("headers")
                .map(|h| {
                    serde_json::from_value(h.clone()).map_err(|_| Error::OAuth("主体 headers 无效"))
                })
                .transpose()?
                .unwrap_or_default();
            if headers.len() > 32 {
                return Err(Error::OAuth("主体 headers 过多"));
            }
            let mut map = HeaderMap::new();
            let mut total = 0;
            for (key, value) in headers {
                total += key.len() + value.expose().len();
                let key = HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| Error::OAuth("主体 header 名无效"))?;
                if matches!(
                    key.as_str(),
                    "host"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                        | "proxy-authorization"
                ) || total > 16384
                {
                    return Err(Error::OAuth("主体 header 不受支持或过大"));
                }
                let mut value = HeaderValue::from_str(value.expose())
                    .map_err(|_| Error::OAuth("主体 header 值无效"))?;
                value.set_sensitive(true);
                if map.insert(key, value).is_some() {
                    return Err(Error::OAuth("主体 header 名重复"));
                }
            }
            let azure = uri.scheme() == "http"
                && uri.host_str() == Some("169.254.169.254")
                && uri.port().is_none()
                && uri.path() == "/metadata/identity/oauth2/token"
                && map
                    .get("metadata")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            if azure {
                if !uri.username().is_empty()
                    || uri.password().is_some()
                    || uri.fragment().is_some()
                {
                    return Err(Error::OAuth("Azure 主体 URL 无效"));
                }
            } else {
                validate_endpoint(&uri)?;
            }
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|_| Error::OAuthTransport)?;
            Subject::Url {
                uri,
                headers: map,
                client,
            }
        };
        Ok(Self {
            quota_project,
            audience: file.audience,
            kind: file.subject_token_type,
            uri,
            subject,
            format,
            target,
            project: file.workforce_pool_user_project,
            client_auth,
            sts: None,
        })
    }
    fn subject(&self, now: DateTime<Utc>) -> Result<Secret> {
        let mut body = Zeroizing::new(Vec::new());
        match &self.subject {
            Subject::Certificate(_) => return Err(Error::OAuth("证书请求缺少 TLS 快照")),
            Subject::Aws(aws) => return aws.subject(now),
            Subject::Executable(executable) => return executable.subject(now),
            Subject::File(path) => {
                let mut options = fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = options
                    .open(path)
                    .map_err(|_| Error::OAuth("无法读取主体令牌文件"))?;
                if !file
                    .metadata()
                    .map_err(|_| Error::OAuth("无法检查主体令牌文件"))?
                    .is_file()
                {
                    return Err(Error::OAuth("主体令牌文件必须为普通文件"));
                }
                file.take(65537)
                    .read_to_end(&mut body)
                    .map_err(|_| Error::OAuth("无法读取主体令牌文件"))?;
            }
            Subject::Url {
                uri,
                headers,
                client,
            } => {
                let response = client
                    .get(uri.clone())
                    .headers(headers.clone())
                    .send()
                    .map_err(|_| Error::OAuthTransport)?;
                if !response.status().is_success() {
                    return Err(Error::OAuthStatus(response.status().as_u16()));
                }
                response
                    .take(65537)
                    .read_to_end(&mut body)
                    .map_err(|_| Error::OAuthTransport)?;
            }
        }
        if body.len() > 65536 {
            return Err(Error::OAuth("主体令牌来源响应过大"));
        }
        let token = match &self.format {
            Format::Text => Secret::new(
                std::str::from_utf8(&body)
                    .map_err(|_| Error::OAuth("主体令牌不是 UTF-8"))?
                    .trim(),
            ),
            Format::Json(field) => {
                let mut value: Value =
                    serde_json::from_slice(&body).map_err(|_| Error::OAuth("主体 JSON 无效"))?;
                match value.get_mut(field).map(Value::take) {
                    Some(Value::String(token)) => Secret::new(token),
                    _ => return Err(Error::OAuth("主体 JSON 字段缺失或不是字符串")),
                }
            }
        };
        if token.expose().trim().is_empty() {
            return Err(Error::OAuth("主体令牌为空"));
        }
        Ok(token)
    }
    fn exchange_sts(
        &self,
        client: &TokenClient,
        now: DateTime<Utc>,
        subject: Option<Secret>,
    ) -> Result<OAuthCredentials> {
        let subject = match subject {
            Some(subject) => subject,
            None => self.subject(now)?,
        };
        let scopes = if self.target.is_some() {
            CLOUD.into()
        } else {
            PUBLIC_SCOPES.join(" ")
        };
        let mut form: Vec<(String, String)> = vec![
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:token-exchange".into(),
            ),
            ("audience".into(), self.audience.clone()),
            ("requested_token_type".into(), ACCESS.into()),
            ("subject_token_type".into(), self.kind.clone()),
            ("subject_token".into(), subject.expose().into()),
            ("scope".into(), scopes),
        ];
        if let Some(project) = self.project.as_ref().filter(|_| self.client_auth.is_none()) {
            // google-auth STS quotes serialized options before the outer form encoding.
            let options = json!({"userProject":project}).to_string();
            form.push((
                "options".into(),
                percent_encoding::utf8_percent_encode(&options, percent_encoding::NON_ALPHANUMERIC)
                    .to_string(),
            ));
        }
        let mut request = client.client.post(self.uri.clone());
        if let Some((id, secret)) = &self.client_auth {
            request = request.basic_auth(id, Some(secret.expose()));
        }
        let response = request.form(&form).send();
        form[4].1.zeroize();
        let response = response.map_err(|_| Error::OAuthTransport)?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("STS 响应过大"));
        }
        #[derive(Deserialize)]
        struct Response {
            access_token: Secret,
            token_type: String,
            issued_token_type: String,
            expires_in: i64,
        }
        let response: Response =
            serde_json::from_slice(&body).map_err(|_| Error::OAuth("STS 响应无效"))?;
        if response.access_token.is_empty()
            || response
                .access_token
                .expose()
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || !response.token_type.eq_ignore_ascii_case("bearer")
            || response.issued_token_type != ACCESS
            || response.expires_in <= 0
        {
            return Err(Error::OAuth("STS token 或类型/到期时间无效"));
        }
        Ok(OAuthCredentials {
            token: response.access_token,
            refresh_token: None,
            token_uri: self.uri.to_string(),
            client_id: self.audience.clone(),
            client_secret: None,
            scopes: if self.target.is_some() {
                vec![CLOUD.into()]
            } else {
                PUBLIC_SCOPES.iter().map(|s| (*s).into()).collect()
            },
            expiry: expiry(now, Some(response.expires_in))?,
            extra: Map::new(),
        })
    }
    pub(super) fn exchange(
        &mut self,
        client: &TokenClient,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<OAuthCredentials> {
        let material = match &self.subject {
            Subject::Certificate(certificate) => Some(certificate.material()?),
            _ => None,
        };
        let (tls_client, subject) = if let Some(material) = material {
            (
                Some(client.with_identity(material.identity)?),
                Some(material.subject),
            )
        } else {
            (None, None)
        };
        let client = tls_client.as_ref().unwrap_or(client);
        if (force && self.target.is_none())
            || self.sts.as_ref().is_none_or(|token| !token.valid_at(now))
        {
            self.sts = Some(self.exchange_sts(client, now, subject)?);
        }
        let sts = self.sts.as_ref().ok_or(Error::OAuth("缺少 STS token"))?;
        if let Some(target) = &self.target {
            target.exchange(client, &sts.token, self.quota_project.as_deref(), now)
        } else {
            Ok(sts.clone())
        }
    }
}
