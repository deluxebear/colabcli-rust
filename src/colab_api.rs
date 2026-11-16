//! Colab control-plane wire contract. OAuth and runtime proxy credentials stay separate.
use crate::{Error, Result, models::Secret};
use reqwest::{Method, blocking::Client};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{io::Read, time::Duration};
use url::Url;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub base_url: Url,
    pub timeout: Duration,
    pub retries: u32,
    pub backoff: Duration,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            base_url: Url::parse("https://colab.research.google.com").unwrap(),
            timeout: Duration::from_secs(20),
            retries: 3,
            backoff: Duration::from_secs(10),
        }
    }
}
impl ApiConfig {
    pub fn validate(&self) -> Result<()> {
        crate::auth::validate_endpoint(&self.base_url)?;
        if self.timeout.is_zero()
            || self.retries > 10
            || self.backoff > Duration::from_secs(60)
            || self.base_url.query().is_some()
        {
            return Err(Error::ControlProtocol("无效的请求或重试配置"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptions {
    pub accelerator: String,
    pub high_mem: bool,
}
impl RuntimeOptions {
    pub fn new(gpu: Option<&str>, tpu: Option<&str>, high_mem: bool) -> Result<Self> {
        if gpu.is_some() && tpu.is_some() {
            return Err(Error::ControlProtocol("--gpu 与 --tpu 不能同时使用"));
        }
        let accelerator = gpu.or(tpu).unwrap_or("NONE").to_ascii_uppercase();
        let valid = if gpu.is_some() {
            ["T4", "L4", "G4", "H100", "A100"].contains(&accelerator.as_str())
        } else if tpu.is_some() {
            ["V5E1", "V6E1"].contains(&accelerator.as_str())
        } else {
            true
        };
        if !valid {
            return Err(Error::ControlProtocol("不支持的硬件型号"));
        }
        Ok(Self {
            accelerator,
            high_mem,
        })
    }
    pub fn variant(&self) -> &'static str {
        match self.accelerator.as_str() {
            "NONE" => "DEFAULT",
            "V5E1" | "V6E1" => "TPU",
            _ => "GPU",
        }
    }
    pub fn shape(&self) -> Option<&'static str> {
        (self.high_mem && !["L4", "V5E1", "V6E1"].contains(&self.accelerator.as_str()))
            .then_some("hm")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyInfo {
    pub token: Secret,
    pub token_expires_in_seconds: i64,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub endpoint: String,
    pub runtime_proxy_info: ProxyInfo,
    #[serde(default)]
    pub accelerator: Option<String>,
    #[serde(default)]
    pub variant: Option<i64>,
    #[serde(default)]
    pub machine_shape: Option<i64>,
}
impl Assignment {
    fn validate(&self) -> Result<()> {
        if self.endpoint.is_empty()
            || self.endpoint == "."
            || self.endpoint == ".."
            || self.runtime_proxy_info.token.is_empty()
            || self.runtime_proxy_info.token_expires_in_seconds < 0
        {
            return Err(Error::ControlProtocol("无效的会话凭据"));
        }
        let url = Url::parse(&self.runtime_proxy_info.url)
            .map_err(|_| Error::ControlProtocol("无效的 runtime URL"))?;
        crate::auth::validate_endpoint(&url)
            .map_err(|_| Error::ControlProtocol("不安全的 runtime URL"))
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub current_balance: f64,
    pub consumption_rate_hourly: f64,
    pub assignments_count: u64,
}
#[derive(Deserialize)]
struct Preparation {
    #[serde(rename = "acc")]
    _acc: String,
    #[serde(rename = "nbh")]
    _nbh: String,
    token: Secret,
    variant: String,
}
#[derive(Deserialize)]
struct UnassignToken {
    token: Secret,
}

pub struct ColabApi {
    client: Client,
    config: ApiConfig,
    bearer: Secret,
}
impl ColabApi {
    pub fn new(config: ApiConfig, bearer: Secret) -> Result<Self> {
        config.validate()?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(config.timeout)
            .build()
            .map_err(|_| Error::ControlTransport { retryable: false })?;
        Ok(Self {
            client,
            config,
            bearer,
        })
    }
    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        xsrf: Option<&Secret>,
    ) -> Result<Vec<u8>> {
        let empty_post = method == Method::POST;
        let url = self
            .config
            .base_url
            .join(path)
            .map_err(|_| Error::ControlProtocol("无效的控制面路径"))?;
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(self.bearer.expose())
            .header("Accept", "application/json")
            .header("X-Colab-Client-Agent", "colab-cli")
            .query(&[("authuser", "0")])
            .query(query);
        if empty_post {
            // These POSTs have no body. Strict HTTP frontends require an explicit length
            // even though the HTTP client can otherwise omit framing for an empty request.
            request = request.header(reqwest::header::CONTENT_LENGTH, "0");
        }
        if let Some(token) = xsrf {
            request = request.header("X-Goog-Colab-Token", token.expose());
        }
        let response = request.send().map_err(|e| Error::ControlTransport {
            retryable: e.is_timeout() && !e.is_connect(),
        })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::ControlStatus(status));
        }
        let mut body = Vec::new();
        response
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut body)
            .map_err(|e| Error::ControlTransport {
                retryable: matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) || e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
                    .is_some_and(reqwest::Error::is_timeout),
            })?;
        if body.len() > 4 * 1024 * 1024 {
            return Err(Error::ControlProtocol("响应超过 4 MiB"));
        }
        Ok(body)
    }
    fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
        let body = body.strip_prefix(b")]}'\n").unwrap_or(body);
        serde_json::from_slice(body).map_err(|_| Error::ControlProtocol("无效的控制面响应"))
    }
    pub fn assignments(&self) -> Result<Vec<Assignment>> {
        #[derive(Deserialize)]
        struct Listed {
            assignments: Vec<Assignment>,
        }
        let listed: Listed =
            Self::decode(&self.request(Method::GET, "/tun/m/assignments", &[], None)?)?;
        let mut endpoints = std::collections::BTreeSet::new();
        for assignment in &listed.assignments {
            assignment.validate()?;
            if assignment.accelerator.is_none()
                || assignment.variant.is_none()
                || assignment.machine_shape.is_none()
                || !endpoints.insert(&assignment.endpoint)
            {
                return Err(Error::ControlProtocol("不完整或重复的 assignment"));
            }
        }
        Ok(listed.assignments)
    }
    pub fn usage(&self) -> Result<Usage> {
        Self::decode(&self.request(Method::GET, "/tun/m/ccu-info", &[], None)?)
    }
    fn assign_once(&self, nbh: &str, options: &RuntimeOptions) -> Result<Assignment> {
        let mut query = vec![
            ("nbh", nbh),
            ("variant", options.variant()),
            ("accelerator", &options.accelerator),
        ];
        if let Some(shape) = options.shape() {
            query.push(("shape", shape));
        }
        let body = self.request(Method::GET, "/tun/m/assign", &query, None)?;
        let value: serde_json::Value = Self::decode(&body)?;
        let assignment = if value.get("runtimeProxyInfo").is_some() {
            Self::decode::<Assignment>(&body)?
        } else {
            let prep: Preparation = Self::decode(&body)?;
            // Upstream consumes only the token. acc/nbh/variant are server metadata,
            // not a contractual echo of the requested hardware or notebook identity.
            if prep.token.is_empty() {
                return Err(Error::ControlProtocol("分配准备 token 为空"));
            }
            if !["DEFAULT", "GPU", "TPU"].contains(&prep.variant.as_str()) {
                return Err(Error::ControlProtocol("无效的分配准备 variant"));
            }
            let response: Assignment = Self::decode(&self.request(
                Method::POST,
                "/tun/m/assign",
                &query,
                Some(&prep.token),
            )?)?;
            if response.accelerator.is_none() || response.variant.is_none() {
                return Err(Error::ControlProtocol("不完整的分配响应"));
            }
            response
        };
        assignment.validate()?;
        Ok(assignment)
    }
    pub fn assign(&self, nbh: &str, options: &RuntimeOptions) -> Result<Assignment> {
        for attempt in 0..=self.config.retries {
            match self.assign_once(nbh, options) {
                Err(error)
                    if attempt < self.config.retries
                        && matches!(
                            error,
                            Error::ControlStatus(502..=504)
                                | Error::ControlTransport { retryable: true }
                        ) =>
                {
                    std::thread::sleep(self.config.backoff.saturating_mul(1 << attempt));
                }
                result => return result,
            }
        }
        unreachable!()
    }
    pub fn unassign(&self, endpoint: &str) -> Result<()> {
        if endpoint.is_empty() || endpoint == "." || endpoint == ".." {
            return Err(Error::ControlProtocol("无效的 endpoint"));
        }
        // Encode the endpoint as one path segment, including literal slashes and percent signs.
        let mut url = self.config.base_url.join("/tun/m/unassign/").unwrap();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push(endpoint);
        let path = url.path();
        let token: UnassignToken = Self::decode(&self.request(Method::GET, path, &[], None)?)?;
        if token.token.is_empty() {
            return Err(Error::ControlProtocol("释放 token 为空"));
        }
        self.request(Method::POST, path, &[], Some(&token.token))?;
        Ok(())
    }
}

/// UUID v4 text with hyphens replaced and padding to 44 characters (not base64).
pub fn notebook_identity() -> String {
    use rand::RngCore;
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}_{}_{}_{}_{}........",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
