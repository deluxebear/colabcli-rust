//! IAM target exchange shared by user/service and federated credential sources.
use super::{OAuthCredentials, PUBLIC_SCOPES, Source, TokenClient, validate_endpoint};
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{io::Read, path::Path};
use url::Url;
use zeroize::Zeroizing;

pub(super) struct Impersonation {
    source: Box<Source>,
    target: Target,
    source_quota_project: Option<String>,
}
pub(super) struct Target {
    uri: Url,
    name: String,
    delegates: Vec<String>,
    lifetime: u64,
}
#[derive(Deserialize)]
struct File {
    source_credentials: Value,
    service_account_impersonation_url: String,
    #[serde(default)]
    delegates: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    access_token: Secret,
    expire_time: String,
}
fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 320
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"@._-+".contains(&c))
        && (value.bytes().all(|c| c.is_ascii_digit())
            || value
                .split_once('@')
                .is_some_and(|(u, d)| !u.is_empty() && !d.is_empty() && !d.contains('@')))
}
impl Target {
    pub(super) fn new(raw: &str, delegates: Vec<String>, lifetime: u64) -> Result<Self> {
        let uri = Url::parse(raw).map_err(|_| Error::OAuth("impersonation URL 无效"))?;
        validate_endpoint(&uri)?;
        let loopback = matches!(uri.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if uri.query().is_some()
            || (!loopback
                && (!matches!(
                    uri.host_str(),
                    Some("iamcredentials.googleapis.com" | "iamcredentials.mtls.googleapis.com")
                ) || uri.port().is_some_and(|p| p != 443)))
        {
            return Err(Error::OAuth(
                "impersonation 仅接受 Google IAM Credentials 或 loopback 端点",
            ));
        }
        let name = uri
            .path()
            .strip_prefix("/v1/projects/-/serviceAccounts/")
            .and_then(|s| s.strip_suffix(":generateAccessToken"))
            .ok_or(Error::OAuth("impersonation 资源路径无效"))?;
        let name = percent_decode_str(name)
            .decode_utf8()
            .map_err(|_| Error::OAuth("impersonation 目标标识无效"))?
            .into_owned();
        if !valid_identifier(&name)
            || delegates.iter().any(|d| {
                d.strip_prefix("projects/-/serviceAccounts/")
                    .is_none_or(|s| !valid_identifier(s))
            })
            || !(600..=43200).contains(&lifetime)
        {
            return Err(Error::OAuth(
                "impersonation 目标、delegates 或 lifetime 无效",
            ));
        }
        Ok(Self {
            uri,
            name,
            delegates,
            lifetime,
        })
    }
    pub(super) fn identifier(&self) -> &str {
        &self.name
    }
    pub(super) fn exchange(
        &self,
        client: &TokenClient,
        bearer: &Secret,
        quota_project: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<OAuthCredentials> {
        let mut payload = json!({"scope":PUBLIC_SCOPES,"lifetime":format!("{}s",self.lifetime)});
        if !self.delegates.is_empty() {
            payload["delegates"] = json!(self.delegates);
        }
        let mut request = client
            .client
            .post(self.uri.clone())
            .bearer_auth(bearer.expose())
            .json(&payload);
        if let Some(project) = quota_project {
            request = request.header("x-goog-user-project", crate::auth::quota::header(project)?);
        }
        let response = request.send().map_err(|_| Error::OAuthTransport)?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("impersonation 响应过大"));
        }
        let response: Response =
            serde_json::from_slice(&body).map_err(|_| Error::OAuth("impersonation 响应无效"))?;
        let expiry = DateTime::parse_from_rfc3339(&response.expire_time)
            .map_err(|_| Error::OAuth("impersonation 到期时间无效"))?
            .with_timezone(&Utc);
        if response.access_token.is_empty()
            || response
                .access_token
                .expose()
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || expiry <= now
        {
            return Err(Error::OAuth("impersonation token 或到期时间无效"));
        }
        Ok(OAuthCredentials {
            token: response.access_token,
            refresh_token: None,
            token_uri: self.uri.to_string(),
            client_id: self.name.clone(),
            client_secret: None,
            scopes: PUBLIC_SCOPES.iter().map(|s| (*s).into()).collect(),
            expiry: Some(expiry),
            extra: Map::new(),
        })
    }
}
impl Impersonation {
    pub(super) fn load(value: Value, path: &Path) -> Result<Self> {
        let file: File = serde_json::from_value(value)
            .map_err(|_| Error::OAuth("impersonation ADC 字段无效"))?;
        let target = Target::new(
            &file.service_account_impersonation_url,
            file.delegates.unwrap_or_default(),
            3600,
        )?;
        let source_quota_project = crate::auth::quota::select(&file.source_credentials, None)?;
        let source = Source::from_value(
            file.source_credentials,
            path,
            true,
            false,
            super::AwsEnvironment::default(),
            None,
            &super::CertificateOptions::default(),
        )?;
        Ok(Self {
            source: Box::new(source),
            target,
            source_quota_project,
        })
    }
    pub(super) fn exchange(
        &mut self,
        client: &TokenClient,
        now: DateTime<Utc>,
    ) -> Result<OAuthCredentials> {
        let bearer = self.source.access_token(client, now, false)?;
        self.target
            .exchange(client, &bearer, self.source_quota_project.as_deref(), now)
    }
}
