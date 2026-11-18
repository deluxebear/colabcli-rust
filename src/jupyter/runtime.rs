//! Explicit REST/kernel/socket lifetimes. Dropping a connection never deletes a kernel or VM.
use super::{
    codec::{self, Frame, KernelMessage},
    output::{Output, OutputChange, OutputCollector},
};
use crate::{
    Error, Result,
    models::{Secret, SessionState},
};
use reqwest::{Method, blocking::Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};
use url::Url;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KernelModel {
    pub id: String,
    #[serde(default)]
    pub execution_state: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone)]
pub struct KernelRest {
    client: Client,
    base: Url,
    proxy: Secret,
    pub timeout: Duration,
}
impl KernelRest {
    pub fn new(state: &SessionState, timeout: Duration) -> Result<Self> {
        let base =
            Url::parse(&state.url).map_err(|_| Error::KernelProtocol("无效的 runtime URL"))?;
        crate::auth::validate_endpoint(&base)?;
        if timeout.is_zero() || base.query().is_some() || state.token.is_empty() {
            return Err(Error::KernelProtocol("无效的 runtime 配置"));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|_| Error::RuntimeTransport)?;
        Ok(Self {
            client,
            base,
            proxy: state.token.clone(),
            timeout,
        })
    }
    fn url(&self, id: Option<&str>, action: Option<&str>) -> Result<Url> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| Error::KernelProtocol("无效的 runtime URL"))?;
        path.pop_if_empty().push("api").push("kernels");
        if let Some(id) = id {
            if id.is_empty() || id == "." || id == ".." || id.chars().any(char::is_control) {
                return Err(Error::KernelProtocol("无效的 kernel ID"));
            }
            path.push(id);
        }
        if let Some(action) = action {
            path.push(action);
        }
        drop(path);
        Ok(url)
    }
    fn request(
        &self,
        method: Method,
        id: Option<&str>,
        action: Option<&str>,
        body: Option<Value>,
    ) -> Result<Option<Value>> {
        let empty_post = method == Method::POST && body.is_none();
        let mut request = self
            .client
            .request(method, self.url(id, action)?)
            .bearer_auth(self.proxy.expose())
            .header("X-Colab-Runtime-Proxy-Token", self.proxy.expose())
            .header("X-Colab-Client-Agent", "colab-cli")
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        } else if empty_post {
            request = request.header(reqwest::header::CONTENT_LENGTH, "0");
        }
        let response = request.send().map_err(|_| Error::RuntimeTransport)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::RuntimeStatus(status));
        }
        let mut bytes = Vec::new();
        response
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::RuntimeTransport)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::KernelProtocol("kernel 响应超过 4 MiB"));
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::KernelProtocol("无效的 kernel 响应"))
    }
    fn model(value: Option<Value>) -> Result<KernelModel> {
        let model: KernelModel =
            serde_json::from_value(value.ok_or(Error::KernelProtocol("kernel model 为空"))?)
                .map_err(|_| Error::KernelProtocol("无效的 kernel model"))?;
        if model.id.is_empty()
            || model.id == "."
            || model.id == ".."
            || model.id.chars().any(char::is_control)
        {
            return Err(Error::KernelProtocol("kernel ID 为空"));
        }
        Ok(model)
    }
    pub fn list(&self) -> Result<Vec<KernelModel>> {
        let value = self
            .request(Method::GET, None, None, None)?
            .ok_or(Error::KernelProtocol("kernels 列表为空"))?;
        let models: Vec<KernelModel> = serde_json::from_value(value)
            .map_err(|_| Error::KernelProtocol("无效的 kernels 列表"))?;
        if models.iter().any(|m| m.id.is_empty()) {
            return Err(Error::KernelProtocol("kernel ID 为空"));
        }
        Ok(models)
    }
    pub fn create(&self) -> Result<KernelModel> {
        Self::model(self.request(
            Method::POST,
            None,
            None,
            Some(json!({"name":"python3","path":"content"})),
        )?)
    }
    pub fn get(&self, id: &str) -> Result<KernelModel> {
        let model = Self::model(self.request(Method::GET, Some(id), None, None)?)?;
        if model.id != id {
            return Err(Error::KernelProtocol("响应 kernel ID 与请求不匹配"));
        }
        Ok(model)
    }
    pub fn restart(&self, id: &str) -> Result<()> {
        self.request(Method::POST, Some(id), Some("restart"), None)?;
        Ok(())
    }
    pub fn interrupt(&self, id: &str) -> Result<()> {
        self.request(Method::POST, Some(id), Some("interrupt"), None)?;
        Ok(())
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        match self.request(Method::DELETE, Some(id), None, None) {
            Ok(_) | Err(Error::RuntimeStatus(404)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ExecutionTimeout {
    Idle(Option<Duration>),
    Deadline(Option<Duration>),
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Succeeded,
    Failed,
    Unknown,
}
#[derive(Debug, Serialize)]
pub struct ExecutionResult {
    pub msg_id: String,
    pub status: ExecutionStatus,
    pub outputs: Vec<Output>,
    pub reply: Option<Value>,
    pub error: Option<String>,
    /// Classifies the local wait limit separately from other unknown results.
    #[serde(skip)]
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_error: Option<String>,
}
impl ExecutionResult {
    pub fn succeeded(&self) -> bool {
        self.status == ExecutionStatus::Succeeded
    }
}

/// A request for user input. Values returned by the handler are never added to execution history.
#[derive(Clone)]
pub struct InputRequest {
    pub id: String,
    pub prompt: String,
    pub password: bool,
}

/// Called repeatedly while input is pending. Implementations must not block: None keeps
/// socket reads, PING/PONG, cancellation and execution deadlines active.
#[derive(Clone)]
pub struct ColabRequest {
    pub id: String,
    pub colab_msg_id: Value,
    pub auth_type: String,
}
enum PendingInput {
    Standard(InputRequest),
    Colab(ColabRequest),
}

pub trait InputHandler {
    fn bind_endpoint(&mut self, _endpoint: &str) -> Result<()> {
        Ok(())
    }
    fn poll_colab(&mut self, _request: &ColabRequest) -> Result<bool> {
        Err(Error::KernelProtocol("未配置 Colab 专用请求处理器"))
    }
    fn poll(&mut self, request: &InputRequest) -> Result<Option<String>>;
}
impl<F: FnMut(&InputRequest) -> Result<Option<String>>> InputHandler for F {
    fn poll(&mut self, request: &InputRequest) -> Result<Option<String>> {
        self(request)
    }
}

pub struct KernelRuntime {
    socket: WebSocket<MaybeTlsStream<crate::websocket_transport::Stream>>,
    session: String,
    usable: bool,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    heartbeat_interval: Duration,
    last_ping: Instant,
    interrupt: Option<(KernelRest, String, Duration)>,
}
impl KernelRuntime {
    /// Persist the kernel ID before this call; a failed WS handshake must not cause another kernel POST.
    pub fn connect(rest: &KernelRest, kernel_id: &str, session: &str) -> Result<Self> {
        Self::connect_with_proxy(
            rest,
            kernel_id,
            session,
            &crate::websocket_transport::ProxyEnvironment::capture(),
        )
    }
    pub fn connect_with_proxy(
        rest: &KernelRest,
        kernel_id: &str,
        session: &str,
        proxy: &crate::websocket_transport::ProxyEnvironment,
    ) -> Result<Self> {
        if session.is_empty() {
            return Err(Error::KernelProtocol("Jupyter session ID 为空"));
        }
        let mut url = rest.url(Some(kernel_id), Some("channels"))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| Error::KernelProtocol("无效的 WS URL"))?;
        url.query_pairs_mut()
            .append_pair("session_id", session)
            .append_pair("token", rest.proxy.expose());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::KernelProtocol("无效的 WS 请求"))?;
        for (name, value) in [
            ("Authorization", format!("Bearer {}", rest.proxy.expose())),
            ("X-Colab-Runtime-Proxy-Token", rest.proxy.expose().into()),
            ("X-Colab-Client-Agent", "colab-cli".into()),
        ] {
            request.headers_mut().insert(
                tungstenite::http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value
                    .parse()
                    .map_err(|_| Error::KernelProtocol("无效的 proxy header"))?,
            );
        }
        let deadline = Instant::now()
            .checked_add(rest.timeout)
            .ok_or(Error::KernelProtocol("连接期限溢出"))?;
        let stream = crate::websocket_transport::connect(&url, deadline, proxy)?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(codec::MAX_MESSAGE_BYTES))
            .max_frame_size(Some(codec::MAX_MESSAGE_BYTES))
            .max_write_buffer_size(codec::MAX_MESSAGE_BYTES + 256 * 1024);
        let (mut socket, _) =
            tungstenite::client_tls_with_config(request, stream, Some(config), None)
                .map_err(|_| Error::WebSocketTransport)?;
        set_timeouts(socket.get_mut(), Duration::from_millis(100), rest.timeout)?;
        Ok(Self {
            socket,
            session: session.into(),
            usable: true,
            cancellation: None,
            heartbeat_interval: Duration::from_secs(60),
            last_ping: Instant::now(),
            interrupt: None,
        })
    }
    /// WebSocket protocol traffic only; this does not renew a VM allocation.
    pub fn set_heartbeat_interval(&mut self, interval: Duration) -> Result<()> {
        if interval.is_zero() {
            return Err(Error::KernelProtocol("心跳间隔必须大于零"));
        }
        self.heartbeat_interval = interval;
        Ok(())
    }
    fn heartbeat(&mut self) -> Result<()> {
        if self.last_ping.elapsed() >= self.heartbeat_interval {
            self.socket
                .send(Message::Ping(b"colab-cli".to_vec().into()))
                .map_err(|_| Error::WebSocketTransport)?;
            self.last_ping = Instant::now();
        }
        Ok(())
    }
    /// Opt in to interrupting one execution on SIGINT. REST success alone is not
    /// completion: execute_reply AND idle must still arrive within the grace period.
    pub fn interrupt_on_sigint(
        &mut self,
        rest: KernelRest,
        kernel_id: String,
        grace: Duration,
    ) -> Result<()> {
        if grace.is_zero() || Instant::now().checked_add(grace).is_none() {
            return Err(Error::KernelProtocol("无效的中断确认期限"));
        }
        self.interrupt = Some((rest, kernel_id, grace));
        Ok(())
    }
    /// Service an idle socket once (bounded by the 100 ms read timeout). Never
    /// replay code or expose late outputs as the next execution's output.
    pub fn poll_idle(&mut self) -> Result<()> {
        let result = self.poll_idle_inner();
        if result.is_err() {
            self.usable = false;
        }
        result
    }
    fn poll_idle_inner(&mut self) -> Result<()> {
        if !self.usable {
            return Err(Error::WebSocketTransport);
        }
        self.heartbeat()?;
        let incoming = match self.socket.read() {
            Ok(Message::Text(text)) => codec::decode_text(&text)?,
            Ok(Message::Binary(bytes)) => codec::decode_binary(&bytes)?,
            Ok(Message::Ping(_)) => {
                return self.socket.flush().map_err(|_| Error::WebSocketTransport);
            }
            Ok(Message::Close(_)) => return Err(Error::WebSocketTransport),
            Ok(_) => return Ok(()),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            Err(_) => return Err(Error::WebSocketTransport),
        };
        if incoming.kind() == "status"
            && [Some("dead"), Some("restarting")]
                .contains(&incoming.content()["execution_state"].as_str())
        {
            return Err(Error::KernelProtocol("kernel 已停止或正在重启"));
        }
        Ok(())
    }
    pub fn execute(
        &mut self,
        code: &str,
        timeout: ExecutionTimeout,
        output_hook: impl FnMut(&OutputChange) -> Result<()>,
    ) -> Result<ExecutionResult> {
        self.execute_with_checkpoint(code, timeout, |_| Ok(()), output_hook)
    }
    pub fn set_cancellation(
        &mut self,
        flag: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) {
        self.cancellation = flag;
    }
    pub fn execute_with_checkpoint(
        &mut self,
        code: &str,
        timeout: ExecutionTimeout,
        before_send: impl FnOnce(&str) -> Result<()>,
        output_hook: impl FnMut(&OutputChange) -> Result<()>,
    ) -> Result<ExecutionResult> {
        self.execute_with_input(code, timeout, before_send, output_hook, None)
    }
    pub fn execute_with_input(
        &mut self,
        code: &str,
        timeout: ExecutionTimeout,
        before_send: impl FnOnce(&str) -> Result<()>,
        mut output_hook: impl FnMut(&OutputChange) -> Result<()>,
        mut input: Option<&mut dyn InputHandler>,
    ) -> Result<ExecutionResult> {
        if !self.usable {
            return Err(Error::KernelProtocol(
                "此连接的上次执行结果未知；禁止重用连接自动重放",
            ));
        }
        if self
            .cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst) != 0)
        {
            return Err(Error::Run("收到终止信号；没有发送新的执行请求"));
        }
        let message = KernelMessage::new(
            "execute_request",
            "shell",
            &self.session,
            json!({"code":code,"silent":false,"store_history":true,
            "user_expressions":{},"allow_stdin":input.is_some(),"stop_on_error":true}),
        );
        let id = message.id().to_owned();
        let frame = codec::encode(&message)?;
        let mut outputs = OutputCollector::default();
        let mut reply = None;
        let mut idle = false;
        let mut pending_input: Option<(PendingInput, Value)> = None;
        let started = Instant::now();
        let mut activity = started;
        let (limit, total) = match timeout {
            ExecutionTimeout::Idle(limit) => (limit, false),
            ExecutionTimeout::Deadline(limit) => (limit, true),
        };
        if limit.is_some_and(|d| started.checked_add(d).is_none()) {
            return Err(Error::KernelProtocol("执行期限溢出"));
        }
        let outbound = match frame {
            Frame::Text(text) => Message::Text(text.into()),
            Frame::Binary(bytes) => Message::Binary(bytes.into()),
        };
        before_send(&id)?;
        let mut failure = if self.socket.send(outbound).is_err() {
            Some((
                ExecutionStatus::Unknown,
                "发送执行请求失败，结果未知".to_string(),
            ))
        } else {
            None
        };
        let mut interrupted = None;
        let mut timed_out = false;
        while failure.is_none() && !(idle && reply.is_some()) {
            use std::sync::atomic::Ordering::SeqCst;
            let signal = self
                .cancellation
                .as_ref()
                .map_or(0, |flag| flag.load(SeqCst));
            if signal != 0 {
                if let (true, true, Some((rest, kernel, grace))) = (
                    signal == signal_hook::consts::SIGINT as usize,
                    interrupted.is_none(),
                    self.interrupt.as_ref(),
                ) {
                    // Clear only the observed signal; do not overwrite a newer TERM/HUP.
                    let cleared = self
                        .cancellation
                        .as_ref()
                        .unwrap()
                        .compare_exchange(signal, 0, SeqCst, SeqCst)
                        .is_ok();
                    if !cleared || rest.interrupt(kernel).is_err() {
                        failure = Some((
                            ExecutionStatus::Unknown,
                            "中断请求未确认；远端执行结果未知".into(),
                        ));
                        break;
                    }
                    interrupted = Some((Instant::now(), *grace));
                    pending_input = None;
                } else {
                    failure = Some((
                        ExecutionStatus::Unknown,
                        "收到终止信号；尚未确认远端执行结束".into(),
                    ));
                    break;
                }
            }
            if interrupted.is_some_and(|(start, grace)| start.elapsed() >= grace) {
                failure = Some((
                    ExecutionStatus::Unknown,
                    "中断后未收到执行完成确认；结果未知".into(),
                ));
                break;
            }
            if self.heartbeat().is_err() {
                failure = Some((ExecutionStatus::Unknown, "WS 心跳发送失败；结果未知".into()));
                break;
            }
            if limit.is_some_and(|limit| {
                if total {
                    started.elapsed() >= limit
                } else {
                    activity.elapsed() >= limit
                }
            }) {
                timed_out = true;
                failure = Some((
                    ExecutionStatus::Unknown,
                    "等待本次执行消息超时；未取消远端执行".into(),
                ));
                break;
            }
            if let Some((request, parent)) = &pending_input {
                let handler = input.as_deref_mut().unwrap();
                let polled = match request {
                    PendingInput::Standard(request) => {
                        handler.poll(request).map(|value| value.map(Value::String))
                    }
                    PendingInput::Colab(request) => handler.poll_colab(request).map(|ready| {
                        ready.then(
                            || json!({"type":"colab_reply","colab_msg_id":request.colab_msg_id}),
                        )
                    }),
                };
                match polled {
                    Ok(Some(value)) => {
                        let mut response = KernelMessage::new(
                            "input_reply",
                            "stdin",
                            &self.session,
                            json!({"value":value}),
                        );
                        response.envelope["parent_header"] = parent.clone();
                        let sent = match codec::encode(&response) {
                            Ok(Frame::Text(text)) => {
                                self.socket.send(Message::Text(text.into())).is_ok()
                            }
                            Ok(Frame::Binary(bytes)) => {
                                self.socket.send(Message::Binary(bytes.into())).is_ok()
                            }
                            Err(_) => false,
                        };
                        if !sent {
                            failure = Some((
                                ExecutionStatus::Unknown,
                                "stdin 回复发送失败，结果未知".into(),
                            ));
                            break;
                        }
                        pending_input = None;
                        activity = Instant::now();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        // Never format arbitrary handler errors: they may contain private input.
                        let message = match request {
                            PendingInput::Colab(_) => match error {
                                Error::InteractionRequired => {
                                    "Colab 授权需要交互终端；未回复内核".into()
                                }
                                Error::ControlStatus(status) => {
                                    format!("Colab 凭据传播请求失败 (HTTP {status})；未回复内核")
                                }
                                _ => "Colab 凭据传播未完成；未回复内核".into(),
                            },
                            PendingInput::Standard(_) => {
                                if matches!(error, Error::InteractionRequired) {
                                    "远端输入需要交互终端；未确认远端执行结束".into()
                                } else {
                                    "读取交互输入失败；未确认远端执行结束".into()
                                }
                            }
                        };
                        failure = Some((ExecutionStatus::Unknown, message));
                        break;
                    }
                }
            }
            let incoming = match self.socket.read() {
                Ok(Message::Text(text)) => codec::decode_text(&text),
                Ok(Message::Binary(bytes)) => codec::decode_binary(&bytes),
                Ok(Message::Ping(_)) => {
                    if self.socket.flush().is_err() {
                        failure = Some((ExecutionStatus::Unknown, "WS PONG 发送失败".into()));
                    }
                    continue;
                }
                Ok(Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => {
                    failure = Some((ExecutionStatus::Unknown, "执行完成前连接关闭".into()));
                    break;
                }
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(_) => {
                    failure = Some((ExecutionStatus::Unknown, "执行完成前连接丢失".into()));
                    break;
                }
            };
            let incoming = match incoming {
                Ok(message) => message,
                Err(error) => {
                    failure = Some((ExecutionStatus::Unknown, error.to_string()));
                    break;
                }
            };
            if incoming.kind() == "status"
                && incoming.channel() == "iopub"
                && [Some("dead"), Some("restarting")]
                    .contains(&incoming.content()["execution_state"].as_str())
            {
                // A broadcast does not acknowledge this request or its side effects.
                // Keep the durable replay guard until explicit recovery.
                failure = Some((
                    ExecutionStatus::Unknown,
                    "kernel 已停止或正在重启；尚未确认本次执行结果".into(),
                ));
                break;
            }
            if incoming.parent() != Some(id.as_str()) {
                continue;
            }
            if incoming.channel() == "iopub" {
                if incoming.kind() == "colab_request" {
                    let auth_type = incoming.content()["request"]["authType"].as_str();
                    let colab_id = &incoming.envelope["metadata"]["colab_msg_id"];
                    if input.is_none()
                        || pending_input.is_some()
                        || idle
                        || auth_type.is_none()
                        || !(colab_id.as_u64().is_some()
                            || colab_id.as_str().is_some_and(|id| !id.is_empty()))
                    {
                        failure = Some((
                            ExecutionStatus::Unknown,
                            "Colab 专用请求无效或未配置处理器".into(),
                        ));
                        break;
                    }
                    pending_input = Some((
                        PendingInput::Colab(ColabRequest {
                            id: incoming.id().to_owned(),
                            colab_msg_id: colab_id.clone(),
                            auth_type: auth_type.unwrap().into(),
                        }),
                        incoming.envelope["header"].clone(),
                    ));
                    activity = Instant::now();
                    continue;
                }
                if !idle {
                    activity = Instant::now();
                }
                match outputs.apply(&incoming) {
                    Ok(changes) => {
                        for change in &changes {
                            if let Err(error) = output_hook(change) {
                                failure = Some((ExecutionStatus::Unknown, error.to_string()));
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        failure = Some((ExecutionStatus::Unknown, error.to_string()));
                        break;
                    }
                }
                if !idle
                    && incoming.kind() == "status"
                    && incoming.content()["execution_state"] == "idle"
                {
                    idle = true;
                    activity = Instant::now();
                }
            } else if incoming.channel() == "shell" && incoming.kind() == "execute_reply" {
                if ![Some("ok"), Some("error"), Some("aborted")]
                    .contains(&incoming.content()["status"].as_str())
                {
                    failure = Some((
                        ExecutionStatus::Unknown,
                        "无效的 execute_reply status".into(),
                    ));
                    break;
                }
                reply = Some(incoming.content().clone());
            } else if incoming.channel() == "stdin" && incoming.kind() == "input_request" {
                if input.is_none() {
                    failure = Some((
                        ExecutionStatus::Unknown,
                        "非交互执行收到 stdin 请求；未自动回复".into(),
                    ));
                    break;
                }
                let prompt = incoming.content()["prompt"].as_str();
                let password = incoming.content()["password"].as_bool();
                if pending_input.is_some() || prompt.is_none() || password.is_none() || idle {
                    failure = Some((ExecutionStatus::Unknown, "无效或重叠的 stdin 请求".into()));
                    break;
                }
                pending_input = Some((
                    PendingInput::Standard(InputRequest {
                        id: incoming.id().to_owned(),
                        prompt: prompt.unwrap().to_owned(),
                        password: password.unwrap(),
                    }),
                    incoming.envelope["header"].clone(),
                ));
                activity = Instant::now();
            }
        }
        if failure.is_none() && pending_input.is_some() {
            failure = Some((
                ExecutionStatus::Unknown,
                "执行结束时仍有未回复的 stdin 请求".into(),
            ));
        }
        if failure.is_none() {
            for change in outputs.synthesize_error(reply.as_ref().unwrap())? {
                output_hook(&change)?;
            }
        }
        let (status, error) = match failure {
            Some((status, error)) => {
                self.usable = false;
                (status, Some(error))
            }
            None => (
                if reply.as_ref().unwrap()["status"] == "ok" {
                    ExecutionStatus::Succeeded
                } else {
                    ExecutionStatus::Failed
                },
                None,
            ),
        };
        Ok(ExecutionResult {
            msg_id: id,
            status,
            outputs: outputs.outputs,
            reply,
            error,
            timed_out,
            history_error: None,
        })
    }
    pub fn close(&mut self) {
        self.usable = false;
        let _ = self.socket.close(None);
    }
}
impl Drop for KernelRuntime {
    fn drop(&mut self) {
        self.close();
    }
}
fn set_timeouts(
    stream: &mut MaybeTlsStream<crate::websocket_transport::Stream>,
    read: Duration,
    write: Duration,
) -> Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &mut stream.sock,
        _ => return Err(Error::WebSocketTransport),
    };
    tcp.set_read_timeout(Some(read))
        .and_then(|_| tcp.set_write_timeout(Some(write)))
        .map_err(|_| Error::WebSocketTransport)
}
