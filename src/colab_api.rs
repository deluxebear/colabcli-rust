//! Colab control-plane wire contract. OAuth and runtime proxy credentials stay separate.
use crate::{Error, Result, models::Secret};
use reqwest::{Method, blocking::Client};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{io::Read, sync::Arc, time::Duration};
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
        let accelerator = tpu.or(gpu).unwrap_or("NONE").to_ascii_uppercase();
        let valid = if tpu.is_some() {
            ["V5E1", "V6E1"].contains(&accelerator.as_str())
        } else if gpu.is_some() {
            ["T4", "L4", "G4", "H100", "A100"].contains(&accelerator.as_str())
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

pub(crate) struct AssignFailure {
    pub error: Error,
    pub definitive_refusal: bool,
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

/// Contains a user-facing authorization URL; deliberately has no Debug/Serialize implementation.
#[derive(Deserialize)]
pub struct CredentialsPropagation {
    pub success: bool,
    pub unauthorized_redirect_uri: Option<Url>,
}

#[derive(Clone)]
pub struct ColabApi {
    client: Client,
    config: ApiConfig,
    bearer: Secret,
    quota_project: Option<reqwest::header::HeaderValue>,
    credentials: Option<Arc<crate::auth::ControlCredentials>>,
    account_guard: Option<Arc<crate::storage::FileLock>>,
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
            quota_project: None,
            credentials: None,
            account_guard: None,
        })
    }
    pub fn with_credentials_source(
        config: ApiConfig,
        source: crate::auth::ControlCredentials,
    ) -> Result<Self> {
        config.validate()?;
        let credentials = source.credentials(None, None)?;
        let mut api = Self::new(config, credentials.token)?
            .with_quota_project(credentials.quota_project_id.as_deref())?;
        api.credentials = Some(Arc::new(source));
        Ok(api)
    }
    /// Runtime-only work can use an already valid account-bound proxy token.
    /// The first control-plane request still acquires real credentials and
    /// propagates failures; the empty placeholder is never a fallback bearer.
    pub fn with_deferred_credentials_source(
        config: ApiConfig,
        source: crate::auth::ControlCredentials,
    ) -> Result<Self> {
        let mut api = Self::new(config, Secret::default())?;
        api.credentials = Some(Arc::new(source));
        Ok(api)
    }
    pub(crate) fn under_account_guard(&self, guard: &Arc<crate::storage::FileLock>) -> Self {
        let mut api = self.clone();
        api.account_guard = Some(Arc::clone(guard));
        api
    }
    pub fn with_quota_project(mut self, project: Option<&str>) -> Result<Self> {
        if self.credentials.is_some() {
            return Err(Error::ControlProtocol("动态凭据的配额项目由来源管理"));
        }
        self.quota_project = project
            .filter(|value| !value.is_empty())
            .map(crate::auth::quota::header)
            .transpose()?;
        Ok(self)
    }
    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        xsrf: Option<&Secret>,
    ) -> Result<Vec<u8>> {
        self.request_body(method, path, query, xsrf, None)
    }
    fn request_body(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        xsrf: Option<&Secret>,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<Vec<u8>> {
        let credentials = self
            .credentials
            .as_ref()
            .map(|source| source.credentials(None, self.account_guard.as_deref()))
            .transpose()?;
        let bearer = credentials
            .as_ref()
            .map_or(&self.bearer, |credentials| &credentials.token);
        let quota = match &credentials {
            Some(credentials) => credentials
                .quota_project_id
                .as_deref()
                .map(crate::auth::quota::header)
                .transpose()?,
            None => self.quota_project.clone(),
        };
        let result = self.send_request(
            &method,
            path,
            query,
            xsrf,
            body.as_ref(),
            bearer,
            quota.as_ref(),
        );
        // An allocation GET can itself return a VM. Retry only documented
        // inventory/usage reads; never replay allocation, release or propagation.
        if let (Err(Error::ControlStatus(401)), Some(source), true) = (
            &result,
            &self.credentials,
            method == Method::GET && matches!(path, "/tun/m/assignments" | "/tun/m/ccu-info"),
        ) {
            let credentials = source.credentials(Some(bearer), self.account_guard.as_deref())?;
            let quota = credentials
                .quota_project_id
                .as_deref()
                .map(crate::auth::quota::header)
                .transpose()?;
            return self.send_request(
                &method,
                path,
                query,
                xsrf,
                body.as_ref(),
                &credentials.token,
                quota.as_ref(),
            );
        }
        result
    }
    #[allow(clippy::too_many_arguments)]
    fn send_request(
        &self,
        method: &Method,
        path: &str,
        query: &[(&str, &str)],
        xsrf: Option<&Secret>,
        body: Option<&(String, Vec<u8>)>,
        bearer: &Secret,
        quota: Option<&reqwest::header::HeaderValue>,
    ) -> Result<Vec<u8>> {
        let empty_post = method == Method::POST && body.is_none();
        let url = self
            .config
            .base_url
            .join(path)
            .map_err(|_| Error::ControlProtocol("无效的控制面路径"))?;
        let mut request = self
            .client
            .request(method.clone(), url)
            .bearer_auth(bearer.expose())
            .header("Accept", "application/json")
            .header("X-Colab-Client-Agent", "colab-cli")
            .query(&[("authuser", "0")])
            .query(query);
        if let Some(project) = quota {
            request = request.header("x-goog-user-project", project.clone());
        }
        if let Some((content_type, bytes)) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes.clone());
        }
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
    fn credentials_path(&self, endpoint: &str) -> Result<String> {
        if endpoint.is_empty()
            || endpoint == "."
            || endpoint == ".."
            || endpoint.chars().any(char::is_control)
        {
            return Err(Error::ControlProtocol("无效的凭据传播 endpoint"));
        }
        let mut url = self
            .config
            .base_url
            .join("/tun/m/credentials-propagation/")
            .unwrap();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push(endpoint);
        Ok(url.path().to_owned())
    }
    pub fn prepare_credentials(&self, endpoint: &str) -> Result<Secret> {
        let response: UnassignToken = Self::decode(&self.request(
            Method::GET,
            &self.credentials_path(endpoint)?,
            &[
                ("authtype", "dfs_ephemeral"),
                ("version", "2"),
                ("dryrun", "true"),
                ("propagate", "true"),
                ("record", "false"),
            ],
            None,
        )?)?;
        if response.token.is_empty() {
            return Err(Error::ControlProtocol("凭据传播 token 为空"));
        }
        Ok(response.token)
    }
    pub fn propagate_credentials(
        &self,
        endpoint: &str,
        token: &Secret,
        dry_run: bool,
    ) -> Result<CredentialsPropagation> {
        if token.is_empty() {
            return Err(Error::ControlProtocol("凭据传播 token 为空"));
        }
        let boundary = format!("colab-{}", notebook_identity().replace('.', ""));
        let bytes = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file_id\"\r\n\r\nempty.ipynb\r\n--{boundary}--\r\n").into_bytes();
        let outcome: CredentialsPropagation = Self::decode(&self.request_body(
            Method::POST,
            &self.credentials_path(endpoint)?,
            &[
                ("authtype", "dfs_ephemeral"),
                ("version", "2"),
                ("dryrun", if dry_run { "true" } else { "false" }),
                ("propagate", "true"),
                ("record", "false"),
            ],
            Some(token),
            Some((format!("multipart/form-data; boundary={boundary}"), bytes)),
        )?)?;
        if !outcome.success && !dry_run {
            return Err(Error::ControlProtocol("服务端未确认凭据传播成功"));
        }
        Ok(outcome)
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
    fn assign_once(
        &self,
        nbh: &str,
        options: &RuntimeOptions,
        before_post: impl FnOnce() -> Result<()>,
    ) -> Result<Assignment> {
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
            before_post()?;
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
        self.assign_checkpointed(nbh, options, false, || Ok(()))
            .map_err(|failure| failure.error)
    }
    pub(crate) fn assign_checkpointed(
        &self,
        nbh: &str,
        options: &RuntimeOptions,
        mut prior_post: bool,
        mut before_post: impl FnMut() -> Result<()>,
    ) -> std::result::Result<Assignment, AssignFailure> {
        for attempt in 0..=self.config.retries {
            let mut posting = false;
            match self.assign_once(nbh, options, || {
                before_post()?;
                posting = true;
                Ok(())
            }) {
                Err(error)
                    if attempt < self.config.retries
                        && matches!(
                            error,
                            Error::ControlStatus(502..=504)
                                | Error::ControlTransport { retryable: true }
                        ) =>
                {
                    prior_post |= posting;
                    std::thread::sleep(self.config.backoff.saturating_mul(1 << attempt));
                }
                result => {
                    // A rejection answers the latest request only. An earlier POST without
                    // a confirmed outcome can still have created the assignment.
                    return result.map_err(|error| AssignFailure {
                        definitive_refusal: !prior_post
                            && matches!(error, Error::ControlStatus(400 | 412)),
                        error,
                    });
                }
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
