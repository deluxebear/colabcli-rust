//! Metadata discovery is the final ADC source, never a fallback from an explicit broken file.
use super::{OAuthCredentials, PUBLIC_SCOPES, expiry};
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Map;
use std::{io::Read, path::PathBuf, time::Duration};
use url::Url;
use zeroize::Zeroizing;

#[derive(Debug, Clone)]
pub struct MetadataOptions {
    pub host: String,
    pub probe_host: String,
    pub disabled: bool,
    pub timeout_seconds: u64,
    pub mtls: Box<MetadataMtlsOptions>,
}

#[cfg(test)]
mod tests;
#[derive(Debug, Clone)]
pub struct MetadataMtlsOptions {
    pub mode: String,
    pub root_cert: PathBuf,
    pub client_bundle: PathBuf,
}
impl Default for MetadataMtlsOptions {
    fn default() -> Self {
        let directory = if cfg!(windows) {
            PathBuf::from("C:/ProgramData/Google/ComputeEngine")
        } else {
            PathBuf::from("/run/google-mds-mtls")
        };
        Self {
            mode: "none".into(),
            root_cert: directory.join(if cfg!(windows) {
                "mds-mtls-root.crt"
            } else {
                "root.crt"
            }),
            client_bundle: directory.join(if cfg!(windows) {
                "mds-mtls-client.key"
            } else {
                "client.key"
            }),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum MtlsMode {
    None,
    Strict,
    Default,
}
impl MetadataMtlsOptions {
    fn mode(&self) -> Result<MtlsMode> {
        match self.mode.to_ascii_lowercase().as_str() {
            "none" => Ok(MtlsMode::None),
            "strict" => Ok(MtlsMode::Strict),
            "default" => Ok(MtlsMode::Default),
            _ => Err(Error::OAuth(
                "GCE_METADATA_MTLS_MODE 必须为 none、strict 或 default",
            )),
        }
    }
    fn enabled(&self, mode: MtlsMode) -> Result<bool> {
        if mode == MtlsMode::None {
            return Ok(false);
        }
        let exists = self.root_cert.exists() && self.client_bundle.exists();
        if mode == MtlsMode::Strict && !exists {
            return Err(Error::OAuth("strict metadata mTLS 缺少证书材料"));
        }
        Ok(exists)
    }
    fn client(&self) -> Result<reqwest::blocking::Client> {
        let (identity, _) =
            super::certificate::read_identity(&self.client_bundle, &self.client_bundle)?;
        let roots = super::certificate::certificates(&super::certificate::read(&self.root_cert)?)?;
        let mut builder = builder().identity(identity);
        for root in roots {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_der(&root)
                    .map_err(|_| Error::OAuth("metadata 根证书无效"))?,
            );
        }
        builder.build().map_err(|_| Error::OAuthTransport)
    }
}
fn builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(20))
}
impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            host: "metadata.google.internal".into(),
            probe_host: "169.254.169.254".into(),
            disabled: false,
            timeout_seconds: 3,
            mtls: Box::default(),
        }
    }
}
impl MetadataOptions {
    /// Called only by Environment::capture; providers consume this immutable snapshot.
    pub fn capture() -> Self {
        fn nonblank(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        let default = Self::default();
        Self {
            host: nonblank("GCE_METADATA_HOST")
                .or_else(|| nonblank("GCE_METADATA_ROOT"))
                .unwrap_or(default.host),
            probe_host: nonblank("GCE_METADATA_IP").unwrap_or(default.probe_host),
            disabled: nonblank("NO_GCE_CHECK").as_deref() == Some("true"),
            timeout_seconds: nonblank("GCE_METADATA_TIMEOUT")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| (1..=30).contains(n))
                .unwrap_or(3),
            mtls: Box::new(MetadataMtlsOptions {
                mode: std::env::var_os("GCE_METADATA_MTLS_MODE")
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "none".into()),
                ..MetadataMtlsOptions::default()
            }),
        }
    }
}
fn origin(host: &str) -> Result<Url> {
    let raw = format!("http://{host}/");
    let uri = Url::parse(&raw).map_err(|_| Error::OAuth("metadata host 无效"))?;
    if host.is_empty()
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || host.contains(['/', '\\', '?', '#', '@'])
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.path() != "/"
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        return Err(Error::OAuth(
            "metadata host 必须为主机名或带端口的 authority",
        ));
    }
    Ok(uri)
}
fn https_uri(uri: &Url, authority: &str) -> Result<Url> {
    // HTTP parsing removes an explicit :80. Reparse the original authority
    // instead of changing scheme on the normalized HTTP URL.
    let mut result = Url::parse(&format!("https://{authority}/"))
        .map_err(|_| Error::OAuth("metadata TLS URL 无效"))?;
    result.set_path(uri.path());
    result.set_query(uri.query());
    Ok(result)
}
fn google(response: &reqwest::blocking::Response) -> bool {
    response
        .headers()
        .get("metadata-flavor")
        .is_some_and(|v| v == "Google")
}
pub(super) struct Metadata {
    client: reqwest::blocking::Client,
    token_uri: Url,
    tls_token_uri: Url,
    cached: Option<OAuthCredentials>,
    mtls: Box<MetadataMtlsOptions>,
    mode: MtlsMode,
}
impl Metadata {
    fn get(
        &self,
        uri: Url,
        tls_uri: Url,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response> {
        if self.mtls.enabled(self.mode)? {
            // Material/load failures are not network failures and never downgrade.
            let client = self.mtls.client()?;
            let response = client
                .get(tls_uri)
                .header("Metadata-Flavor", "Google")
                .timeout(timeout)
                .send();
            match response {
                Ok(response)
                    if self.mode == MtlsMode::Default
                        && (response.status().is_client_error()
                            || response.status().is_server_error()) =>
                {
                    drop(response)
                }
                Ok(response) => return Ok(response),
                Err(_) if self.mode == MtlsMode::Strict => return Err(Error::OAuthTransport),
                Err(_) => {}
            }
        }
        self.client
            .get(uri)
            .header("Metadata-Flavor", "Google")
            .timeout(timeout)
            .send()
            .map_err(|_| Error::OAuthTransport)
    }
    pub(super) fn discover(options: &MetadataOptions) -> Result<Self> {
        if options.disabled {
            return Err(Error::NeedsAuth);
        }
        if !(1..=30).contains(&options.timeout_seconds) {
            return Err(Error::OAuth("metadata 探测期限无效"));
        }
        let mode = options.mtls.mode()?;
        if mode == MtlsMode::Strict
            && !matches!(
                options.host.as_str(),
                "metadata.google.internal" | "169.254.169.254"
            )
        {
            return Err(Error::OAuth(
                "strict metadata mTLS 只接受默认 metadata 主机",
            ));
        }
        options.mtls.enabled(mode)?;
        let probe = origin(&options.probe_host)?;
        let mut token_uri = origin(&options.host)?
            .join("computeMetadata/v1/instance/service-accounts/default/token")
            .map_err(|_| Error::OAuth("metadata token URL 无效"))?;
        token_uri
            .query_pairs_mut()
            .append_pair("scopes", &PUBLIC_SCOPES.join(","));
        let tls_token_uri = https_uri(&token_uri, &options.host)?;
        let tls_probe_uri = https_uri(&probe, &options.probe_host)?;
        let client = builder().build().map_err(|_| Error::OAuthTransport)?;
        let metadata = Self {
            client,
            token_uri,
            tls_token_uri,
            cached: None,
            mtls: options.mtls.clone(),
            mode,
        };
        let response = metadata
            .get(
                probe,
                tls_probe_uri,
                Duration::from_secs(options.timeout_seconds),
            )
            .map_err(|error| {
                if matches!(error, Error::OAuthTransport) {
                    Error::NeedsAuth
                } else {
                    error
                }
            })?;
        if response.status() != reqwest::StatusCode::OK || !google(&response) {
            return Err(Error::NeedsAuth);
        }
        Ok(metadata)
    }
    pub(super) fn access_token(&mut self, now: DateTime<Utc>, force: bool) -> Result<Secret> {
        if !force && self.cached.as_ref().is_some_and(|t| t.valid_at(now)) {
            return Ok(self.cached.as_ref().unwrap().token.clone());
        }
        let response = self.get(
            self.token_uri.clone(),
            self.tls_token_uri.clone(),
            Duration::from_secs(20),
        )?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        if !google(&response) {
            return Err(Error::OAuth("metadata 响应缺少 Google 标记"));
        }
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("metadata token 响应过大"));
        }
        #[derive(Deserialize)]
        struct Response {
            access_token: Secret,
            expires_in: i64,
            token_type: String,
        }
        let value: Response =
            serde_json::from_slice(&body).map_err(|_| Error::OAuth("metadata token 响应无效"))?;
        if value.access_token.is_empty()
            || value
                .access_token
                .expose()
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || !value.token_type.eq_ignore_ascii_case("bearer")
            || value.expires_in <= 0
        {
            return Err(Error::OAuth("metadata token 字段无效"));
        }
        let expiry = expiry(now, Some(value.expires_in))?;
        self.cached = Some(OAuthCredentials {
            token: value.access_token,
            refresh_token: None,
            token_uri: self.token_uri.to_string(),
            client_id: String::new(),
            client_secret: None,
            scopes: vec![],
            expiry,
            extra: Map::new(),
        });
        Ok(self.cached.as_ref().unwrap().token.clone())
    }
}
