//! AWS federation signs a GetCallerIdentity request; it never sends that request to AWS STS.
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use ring::hmac;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use url::Url;
use zeroize::Zeroizing;
const QUERY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
const VERIFY: &str =
    "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
#[derive(Debug, Clone, Default)]
pub struct AwsEnvironment {
    pub region: Option<String>,
    pub access_key: Option<Secret>,
    pub secret_key: Option<Secret>,
    pub session_token: Option<Secret>,
}
impl AwsEnvironment {
    pub fn capture() -> Self {
        fn text(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|s| !s.is_empty())
        }
        Self {
            region: text("AWS_REGION").or_else(|| text("AWS_DEFAULT_REGION")),
            access_key: text("AWS_ACCESS_KEY_ID").map(Secret::new),
            secret_key: text("AWS_SECRET_ACCESS_KEY").map(Secret::new),
            session_token: text("AWS_SESSION_TOKEN").map(Secret::new),
        }
    }
}
#[derive(Deserialize)]
struct Config {
    environment_id: String,
    regional_cred_verification_url: String,
    region_url: Option<String>,
    url: Option<String>,
    imdsv2_session_token_url: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Credentials {
    access_key_id: Secret,
    secret_access_key: Secret,
    token: Option<Secret>,
    expiration: Option<String>,
    code: Option<String>,
}
pub(super) struct Aws {
    env: AwsEnvironment,
    verify: String,
    region: Option<Url>,
    credentials: Option<Url>,
    session: Option<Url>,
    audience: String,
    client: reqwest::blocking::Client,
    cancellation: Option<Arc<AtomicUsize>>,
}
fn metadata(raw: &str, path: &str) -> Result<Url> {
    let uri = Url::parse(raw).map_err(|_| Error::OAuth("AWS metadata URL 无效"))?;
    let loopback = matches!(uri.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if uri.scheme() != "http"
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
        || uri.path().trim_end_matches('/') != path
        || (!loopback
            && (!matches!(uri.host_str(), Some("169.254.169.254" | "[fd00:ec2::254]"))
                || uri.port().is_some()))
    {
        return Err(Error::OAuth(
            "AWS metadata 仅接受 EC2 link-local 或 loopback 的固定路径",
        ));
    }
    Ok(uri)
}
fn valid_region(value: &str) -> bool {
    let parts: Vec<_> = value.split('-').collect();
    value.len() <= 64
        && parts.len() >= 3
        && parts
            .last()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        && parts[..parts.len() - 1].iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
fn valid_secret(value: &Secret) -> bool {
    !value.is_empty()
        && value.expose().len() <= 16384
        && value.expose().bytes().all(|b| (33..=126).contains(&b))
}
fn validate_keys(keys: &Credentials) -> Result<()> {
    if !valid_secret(&keys.access_key_id)
        || !keys
            .access_key_id
            .expose()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric())
        || !valid_secret(&keys.secret_access_key)
        || keys.token.as_ref().is_some_and(|s| !valid_secret(s))
    {
        return Err(Error::OAuth("AWS 安全凭据字段无效"));
    }
    Ok(())
}
impl Aws {
    pub(super) fn load(
        source: Value,
        env: AwsEnvironment,
        audience: &str,
        cancellation: Option<Arc<AtomicUsize>>,
    ) -> Result<Self> {
        if ["file", "executable", "certificate", "format", "headers"]
            .iter()
            .any(|key| source.get(*key).is_some())
        {
            return Err(Error::OAuth("AWS 来源不能混用其他主体来源"));
        }
        let config: Config =
            serde_json::from_value(source).map_err(|_| Error::OAuth("AWS ADC 来源字段无效"))?;
        if config.environment_id != "aws1" {
            return Err(Error::OAuth("不支持的 AWS ADC environment_id"));
        }
        let china = VERIFY.replace("amazonaws.com?", "amazonaws.com.cn?");
        if config.regional_cred_verification_url != VERIFY
            && config.regional_cred_verification_url != china
        {
            return Err(Error::OAuth(
                "AWS 验证 URL 必须为区域 STS GetCallerIdentity 模板",
            ));
        }
        if env.region.as_deref().is_some_and(|s| !valid_region(s)) {
            return Err(Error::OAuth("AWS region 无效"));
        }
        let region = config
            .region_url
            .as_deref()
            .map(|s| metadata(s, "/latest/meta-data/placement/availability-zone"))
            .transpose()?;
        let credentials = config
            .url
            .as_deref()
            .map(|s| metadata(s, "/latest/meta-data/iam/security-credentials"))
            .transpose()?;
        let session = config
            .imdsv2_session_token_url
            .as_deref()
            .map(|s| metadata(s, "/latest/api/token"))
            .transpose()?;
        let endpoints: Vec<_> = [region.as_ref(), credentials.as_ref(), session.as_ref()]
            .into_iter()
            .flatten()
            .collect();
        if endpoints.windows(2).any(|p| p[0].origin() != p[1].origin()) {
            return Err(Error::OAuth("AWS metadata 端点必须同源"));
        }
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| Error::OAuthTransport)?;
        Ok(Self {
            env,
            verify: config.regional_cred_verification_url,
            region,
            credentials,
            session,
            audience: audience.into(),
            client,
            cancellation,
        })
    }
    fn cancelled(&self) -> Result<()> {
        let signal = self
            .cancellation
            .as_ref()
            .map_or(0, |f| f.load(Ordering::SeqCst));
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        Ok(())
    }
    fn request(
        &self,
        uri: &Url,
        session: Option<&Secret>,
        put: bool,
    ) -> Result<Zeroizing<Vec<u8>>> {
        self.cancelled()?;
        let mut request = if put {
            self.client
                .put(uri.clone())
                .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
        } else {
            self.client.get(uri.clone())
        };
        if let Some(token) = session {
            request = request.header("X-aws-ec2-metadata-token", token.expose());
        }
        let response = request.send().map_err(|_| Error::OAuthTransport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("AWS metadata 响应过大"));
        }
        self.cancelled()?;
        Ok(body)
    }
    pub(super) fn subject(&self, now: DateTime<Utc>) -> Result<Secret> {
        self.cancelled()?;
        let started = Instant::now();
        let env_keys = match (&self.env.access_key, &self.env.secret_key) {
            (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
                Some(Credentials {
                    access_key_id: access.clone(),
                    secret_access_key: secret.clone(),
                    token: self.env.session_token.clone(),
                    expiration: None,
                    code: None,
                })
            }
            _ => None,
        };
        if let Some(keys) = &env_keys {
            validate_keys(keys)?;
        }
        let needs_metadata = self.env.region.is_none() || env_keys.is_none();
        let session = if needs_metadata {
            self.session
                .as_ref()
                .map(|uri| {
                    let bytes = self.request(uri, None, true)?;
                    let value = std::str::from_utf8(&bytes)
                        .map_err(|_| Error::OAuth("AWS IMDSv2 token 无效"))?;
                    let token = Secret::new(value);
                    if !valid_secret(&token) {
                        return Err(Error::OAuth("AWS IMDSv2 token 无效"));
                    }
                    Ok(token)
                })
                .transpose()?
        } else {
            None
        };
        let region = if let Some(region) = &self.env.region {
            region.clone()
        } else {
            let uri = self
                .region
                .as_ref()
                .ok_or(Error::OAuth("AWS 缺少 region 和 region_url"))?;
            let body = self.request(uri, session.as_ref(), false)?;
            let zone = std::str::from_utf8(&body)
                .map_err(|_| Error::OAuth("AWS availability zone 无效"))?
                .trim();
            let Some(last) = zone.bytes().last() else {
                return Err(Error::OAuth("AWS availability zone 为空"));
            };
            if !last.is_ascii_lowercase() {
                return Err(Error::OAuth("AWS availability zone 后缀无效"));
            }
            let region = &zone[..zone.len() - 1];
            if !valid_region(region) {
                return Err(Error::OAuth("AWS availability zone region 无效"));
            }
            region.to_string()
        };
        let keys = if let Some(keys) = env_keys {
            keys
        } else {
            let uri = self
                .credentials
                .as_ref()
                .ok_or(Error::OAuth("AWS 缺少环境凭据和 metadata url"))?;
            let body = self.request(uri, session.as_ref(), false)?;
            let role = std::str::from_utf8(&body)
                .map_err(|_| Error::OAuth("AWS role 名无效"))?
                .trim();
            if role.is_empty()
                || matches!(role, "." | "..")
                || role.len() > 64
                || !role
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+=,.@_-".contains(&b))
            {
                return Err(Error::OAuth("AWS role 名无效"));
            }
            let mut target = uri.clone();
            target.set_path(&format!("{}/{}", uri.path().trim_end_matches('/'), role));
            let body = self.request(&target, session.as_ref(), false)?;
            let keys: Credentials = serde_json::from_slice(&body)
                .map_err(|_| Error::OAuth("AWS metadata 安全凭据 JSON 无效"))?;
            if keys.code.as_deref().is_some_and(|s| s != "Success") {
                return Err(Error::OAuth("AWS metadata 安全凭据失败"));
            }
            validate_keys(&keys)?;
            keys
        };
        let observed = now
            .checked_add_signed(
                chrono::Duration::from_std(started.elapsed())
                    .map_err(|_| Error::OAuth("AWS 签名时间无效"))?,
            )
            .ok_or(Error::OAuth("AWS 签名时间无效"))?;
        if let Some(expiration) = &keys.expiration {
            let expiry = DateTime::parse_from_rfc3339(expiration)
                .map_err(|_| Error::OAuth("AWS 凭据到期时间无效"))?;
            if expiry <= observed {
                return Err(Error::OAuth("AWS 凭据已过期"));
            }
        }
        self.cancelled()?;
        sign(&self.verify, &region, &self.audience, &keys, observed)
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn mac(key: &[u8], data: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
            .as_ref()
            .to_vec(),
    )
}
fn sign(
    template: &str,
    region: &str,
    audience: &str,
    keys: &Credentials,
    now: DateTime<Utc>,
) -> Result<Secret> {
    validate_keys(keys)?;
    let raw = template.replace("{region}", region);
    let uri = Url::parse(&raw).map_err(|_| Error::OAuth("AWS 区域 STS URL 无效"))?;
    let host = uri.host_str().ok_or(Error::OAuth("AWS STS host 无效"))?;
    let mut headers = BTreeMap::new();
    headers.insert("host", host.to_string());
    headers.insert("x-amz-date", now.format("%Y%m%dT%H%M%SZ").to_string());
    headers.insert("x-goog-cloud-target-resource", audience.into());
    if let Some(token) = &keys.token {
        headers.insert("x-amz-security-token", token.expose().to_string());
    }
    let signed = headers.keys().copied().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let canonical = Zeroizing::new(format!(
        "POST\n/\n{QUERY}\n{canonical_headers}\n{signed}\n{}",
        hex(&Sha256::digest([]))
    ));
    let date = now.format("%Y%m%d").to_string();
    let scope = format!("{date}/{region}/sts/aws4_request");
    let message = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        headers["x-amz-date"],
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let seed = Zeroizing::new(format!("AWS4{}", keys.secret_access_key.expose()));
    let a = mac(seed.as_bytes(), &date);
    let b = mac(&a, region);
    let c = mac(&b, "sts");
    let key = mac(&c, "aws4_request");
    let signature = hex(&mac(&key, &message));
    headers.insert(
        "Authorization",
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            keys.access_key_id.expose()
        ),
    );
    let headers: Vec<_> = headers
        .into_iter()
        .map(|(k, v)| json!({"key":k,"value":v}))
        .collect();
    let body = Zeroizing::new(json!({"url":raw,"method":"POST","headers":headers}).to_string());
    // Python urllib.quote keeps '/' unescaped; the outer STS form escapes it again.
    const QUOTE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'/');
    Ok(Secret::new(
        percent_encoding::utf8_percent_encode(&body, QUOTE).to_string(),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signatures_match_python_stdlib_and_google_auth_without_network() {
        let probe: serde_json::Value =
            serde_json::from_str(include_str!("../../../research/aws-signing-probe.json")).unwrap();
        for case in probe["cases"].as_array().unwrap() {
            let keys = super::Credentials {
                access_key_id: crate::models::Secret::new(case["access_key"].as_str().unwrap()),
                secret_access_key: crate::models::Secret::new(case["secret_key"].as_str().unwrap()),
                token: case["session_token"]
                    .as_str()
                    .map(crate::models::Secret::new),
                expiration: None,
                code: None,
            };
            let token = super::sign(
                case["template"].as_str().unwrap(),
                case["region"].as_str().unwrap(),
                case["audience"].as_str().unwrap(),
                &keys,
                chrono::DateTime::parse_from_rfc3339(case["instant"].as_str().unwrap())
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            )
            .unwrap();
            let raw = percent_encoding::percent_decode_str(token.expose())
                .decode_utf8()
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(value["url"], case["url"]);
            assert_eq!(value["method"], "POST");
            let headers: std::collections::BTreeMap<_, _> = value["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| (h["key"].as_str().unwrap(), h["value"].as_str().unwrap()))
                .collect();
            assert_eq!(headers["Authorization"], case["authorization"]);
            for (key, value) in case["headers"].as_object().unwrap() {
                assert_eq!(headers[key.as_str()], value.as_str().unwrap());
            }
        }
    }
}
