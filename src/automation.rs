//! Colab dfs_ephemeral requests use the selected compute account, never runtime proxy credentials.
use crate::{
    Error, Result,
    colab_api::CredentialsPropagation,
    jupyter::{ColabRequest, InputHandler, InputRequest},
    models::Secret,
    sessions::SessionService,
};
use std::sync::mpsc::{self, Receiver, TryRecvError};

enum Phase {
    Idle,
    Checking(Receiver<Result<(Secret, CredentialsPropagation)>>),
    Consent { token: Secret, prompt: InputRequest },
    Propagating(Receiver<Result<()>>),
    Complete,
    Failed,
}

pub struct AutomationInput<'a> {
    service: SessionService,
    endpoint: Option<String>,
    input: &'a mut dyn InputHandler,
    request_id: Option<String>,
    phase: Phase,
    audit: Option<String>,
}
impl<'a> AutomationInput<'a> {
    pub fn new(service: &SessionService, input: &'a mut dyn InputHandler) -> Self {
        Self {
            service: service.clone(),
            endpoint: None,
            input,
            request_id: None,
            phase: Phase::Idle,
            audit: None,
        }
    }
    pub fn with_history(mut self, name: String) -> Self {
        self.audit = Some(name);
        self
    }
    fn event(&self, kind: &str, data: serde_json::Value) -> Result<()> {
        if let Some(name) = &self.audit {
            self.service
                .record_event(name, self.endpoint.as_deref(), kind, data, None)?;
        }
        Ok(())
    }
    fn propagate(&self, token: Secret) -> Result<Phase> {
        let api = self.service.control_api();
        let endpoint = self
            .endpoint
            .clone()
            .ok_or(Error::KernelProtocol("凭据传播尚未绑定会话"))?;
        Ok(Phase::Propagating(worker(move || {
            api.propagate_credentials(&endpoint, &token, false)?;
            Ok(())
        })?))
    }
}
fn worker<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Receiver<Result<T>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("colab-credentials".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|_| Error::KernelProtocol("无法启动凭据传播请求"))?;
    Ok(receiver)
}
impl InputHandler for AutomationInput<'_> {
    fn bind_endpoint(&mut self, endpoint: &str) -> Result<()> {
        if self.request_id.is_some() {
            return Err(Error::KernelProtocol("不能更换进行中授权的会话"));
        }
        self.endpoint = Some(endpoint.to_owned());
        self.input.bind_endpoint(endpoint)
    }
    fn poll(&mut self, request: &InputRequest) -> Result<Option<String>> {
        self.input.poll(request)
    }
    fn poll_colab(&mut self, request: &ColabRequest) -> Result<bool> {
        if request.auth_type != "dfs_ephemeral" {
            return Err(Error::KernelProtocol("不支持的 Colab 授权类型"));
        }
        if self.request_id.as_deref() != Some(&request.id) {
            if !matches!(self.phase, Phase::Idle | Phase::Complete) {
                return Err(Error::KernelProtocol("不能重叠执行凭据传播"));
            }
            self.event(
                "colab_request",
                serde_json::json!({"type":"dfs_ephemeral","colab_msg_id":request.colab_msg_id}),
            )?;
            self.request_id = Some(request.id.clone());
            let api = self.service.control_api();
            let endpoint = self
                .endpoint
                .clone()
                .ok_or(Error::KernelProtocol("凭据传播尚未绑定会话"))?;
            self.phase = Phase::Checking(worker(move || {
                let token = api.prepare_credentials(&endpoint)?;
                let outcome = api.propagate_credentials(&endpoint, &token, true)?;
                Ok((token, outcome))
            })?);
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Failed);
        self.phase = match phase {
            Phase::Checking(receiver) => match receiver.try_recv() {
                Ok(result) => {
                    let (token, outcome) = result?;
                    if outcome.success {
                        self.propagate(token)?
                    } else {
                        let url = outcome
                            .unauthorized_redirect_uri
                            .ok_or(Error::ControlProtocol("缺少凭据授权 URL"))?;
                        if url.scheme() != "https"
                            || url.host_str().is_none()
                            || !url.username().is_empty()
                            || url.password().is_some()
                        {
                            return Err(Error::ControlProtocol("不安全的凭据授权 URL"));
                        }
                        self.event(
                            "drive_auth_needed",
                            serde_json::json!({"reason":"consent_required"}),
                        )?;
                        Phase::Consent {
                            token,
                            prompt: InputRequest {
                                id: format!("colab-consent-{}", request.id),
                                password: false,
                                prompt: format!(
                                    "\n[colab] Open this authorization URL: {url}\nPress Enter after you have granted access... "
                                ),
                            },
                        }
                    }
                }
                Err(TryRecvError::Empty) => Phase::Checking(receiver),
                Err(_) => return Err(Error::KernelProtocol("凭据传播工作线程未返回结果")),
            },
            Phase::Consent { token, prompt } => {
                if self.input.poll(&prompt)?.is_some() {
                    self.propagate(token)?
                } else {
                    Phase::Consent { token, prompt }
                }
            }
            Phase::Propagating(receiver) => match receiver.try_recv() {
                Ok(result) => {
                    result?;
                    self.event("drive_auth_success", serde_json::json!({}))?;
                    Phase::Complete
                }
                Err(TryRecvError::Empty) => Phase::Propagating(receiver),
                Err(_) => return Err(Error::KernelProtocol("凭据传播工作线程未返回结果")),
            },
            Phase::Complete => Phase::Complete,
            _ => return Err(Error::KernelProtocol("凭据传播状态不可复用")),
        };
        Ok(matches!(self.phase, Phase::Complete))
    }
}

pub fn auth_code() -> &'static str {
    "import os\nos.environ['USE_AUTH_EPHEM'] = '0'\nfrom google.colab import auth\nauth.authenticate_user()"
}
pub fn mount_code(path: &str) -> Result<String> {
    if path.is_empty() || path.contains('\0') || !path.starts_with('/') {
        return Err(Error::KernelProtocol(
            "Drive 挂载点必须为不含 NUL 的绝对路径",
        ));
    }
    let literal =
        serde_json::to_string(path).map_err(|_| Error::KernelProtocol("Drive 挂载点编码失败"))?;
    // Load JSON as data: JSON escapes such as \u000a are not interpolated into Python source.
    let encoded = serde_json::to_string(&literal)
        .map_err(|_| Error::KernelProtocol("Drive 挂载点编码失败"))?;
    Ok(format!(
        "from google.colab import drive\ndrive.mount(__import__('json').loads({encoded}))"
    ))
}
