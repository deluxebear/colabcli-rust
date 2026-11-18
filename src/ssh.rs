//! SSH uses its own WebSocket endpoint and system OpenSSH, never kernel channels.
use crate::{Error, Result, models::SessionState, sessions::SessionService};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};
use url::Url;

const MAX_KEY_BYTES: u64 = 64 * 1024;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    Command,
    Proxy,
}
impl Mode {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Command => "cmd",
            Self::Proxy => "proxy",
        }
    }
}

pub fn expand_identity(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if path == Path::new("~") {
        return home
            .map(Path::to_owned)
            .ok_or(Error::Ssh("无法确定用户目录"));
    }
    if let Ok(tail) = path.strip_prefix("~") {
        return Ok(home.ok_or(Error::Ssh("无法确定用户目录"))?.join(tail));
    }
    Ok(path.to_owned())
}
fn read_key(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| Error::Ssh("无法读取 SSH 公钥文件"))?
        .take(MAX_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Ssh("无法读取 SSH 公钥文件"))?;
    if bytes.len() as u64 > MAX_KEY_BYTES {
        return Err(Error::Ssh("SSH 公钥文件超过 64 KiB"));
    }
    let key = String::from_utf8(bytes).map_err(|_| Error::Ssh("SSH 公钥必须为 UTF-8"))?;
    Ok(key.trim().into())
}
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() as u64 > MAX_KEY_BYTES || key.chars().any(char::is_control) {
        return Err(Error::Ssh("SSH 公钥必须为非空单行文本"));
    }
    Ok(())
}

/// Resolve only the header key; private key contents are never read or stored by Rust.
/// A keygen child inherits the terminal (for passphrases) but never stdout/stderr.
pub fn resolve_pubkey(
    identity: Option<&Path>,
    home: Option<&Path>,
    timeout: Duration,
    cancellation: &AtomicUsize,
) -> Result<String> {
    let Some(identity) = identity else {
        let dir = home
            .ok_or(Error::Ssh("无法确定用户目录；请指定 --identity"))?
            .join(".ssh");
        for name in ["id_ed25519.pub", "id_ecdsa.pub"] {
            let path = dir.join(name);
            if path.exists() {
                let key = read_key(&path)?;
                validate_key(&key)?;
                return Ok(key);
            }
        }
        return Err(Error::Ssh(
            "没有 SSH 公钥；请运行 ssh-keygen -t ed25519 或指定 --identity",
        ));
    };
    let expanded = expand_identity(identity, home)?;
    let (public, private) = if expanded.extension().is_some_and(|e| e == "pub") {
        (expanded.clone(), expanded.with_extension(""))
    } else {
        (expanded.with_extension("pub"), expanded.clone())
    };
    if public.exists() {
        // An empty explicit .pub falls back to deriving the private key, as upstream does.
        let key = read_key(&public)?;
        if !key.is_empty() {
            validate_key(&key)?;
            return Ok(key);
        }
    }
    let target = if private.exists() { private } else { expanded };
    if !target.is_file() {
        return Err(Error::Ssh("--identity 文件不存在"));
    }
    // Temporary output avoids a full pipe blocking the child. Its size is
    // monitored while running and reads are capped at 64 KiB. It
    // contains only a public key and is removed on every controlled return.
    let mut output = tempfile::tempfile().map_err(|_| Error::Ssh("无法准备公钥导出"))?;
    let mut child = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(&target)
        .stdout(
            output
                .try_clone()
                .map_err(|_| Error::Ssh("无法准备公钥导出"))?,
        )
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Ssh("无法启动 ssh-keygen"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Ssh("等待 ssh-keygen 失败"));
            }
        }
        if cancellation.load(Ordering::SeqCst) != 0 || started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Ssh("公钥导出已取消或超时"));
        }
        if output.metadata().map_or(true, |m| m.len() > MAX_KEY_BYTES) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Ssh("公钥导出超过限额"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(Error::Ssh("ssh-keygen 无法导出公钥"));
    }
    use std::io::{Seek, SeekFrom};
    output
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::Ssh("无法读取导出的公钥"))?;
    let mut bytes = Vec::new();
    output
        .take(MAX_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Ssh("无法读取导出的公钥"))?;
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Ssh("导出的公钥不是 UTF-8"))?
        .trim();
    validate_key(key)?;
    Ok(key.into())
}

fn ws_url(state: &SessionState, mode: Mode) -> Result<Url> {
    let mut url = Url::parse(&state.url).map_err(|_| Error::Ssh("无效的 runtime URL"))?;
    crate::auth::validate_endpoint(&url)?;
    if state.token.is_empty() || url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Ssh("无效的 runtime 配置"));
    }
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| Error::Ssh("无效的 SSH WebSocket URL"))?;
    url.set_path("/colab/ssh");
    url.query_pairs_mut()
        .append_pair("colab-runtime-proxy-token", state.token.expose())
        .append_pair("colab-client-agent", "colab-cli")
        .append_pair("colab-client-agent", &format!("ssh-mode-{}", mode.tag()));
    Ok(url)
}

/// Only known body predicates affect the error. Arbitrary server text, URLs and
/// tokens never become terminal output, including unexpected 3xx responses.
pub fn handshake_error(status: u16, body: &[u8]) -> Error {
    let body = &body[..body.len().min(4096)];
    let text = String::from_utf8_lossy(body);
    let reason = match status {
        400 if text.contains("missing pubkey") => "服务器要求公钥 header；请检查客户端版本".into(),
        400 if text.contains("unsupported key type") => {
            "公钥类型不受支持；请使用 Ed25519 或 ECDSA，服务器不接受 RSA".into()
        }
        400 => "服务器拒绝公钥；请用 ssh-keygen -y -f 检查密钥".into(),
        401 => "runtime proxy token 无效或已过期".into(),
        403 => "runtime proxy token 无权访问 SSH".into(),
        404 => "runtime 未开放 /colab/ssh；SSH 需要在分配时启用".into(),
        429 => {
            let value: Option<serde_json::Value> = serde_json::from_slice(body).ok();
            let limit = value
                .as_ref()
                .filter(|v| v["error"] == "already-active-session")
                .and_then(|v| v["limit"].as_u64())
                .filter(|n| (1..=5).contains(n));
            match limit {
                Some(n) => format!("SSH 连接数已达到当前限制（{n}/{n}）；请关闭其他连接后重试"),
                None => "已有 SSH 连接；请关闭其他连接后重试".into(),
            }
        }
        502 => "runtime 的 sshd 不可达；请检查会话状态".into(),
        _ => "服务器拒绝 SSH WebSocket 握手".into(),
    };
    Error::SshHandshake { status, reason }
}

type Socket = WebSocket<MaybeTlsStream<crate::websocket_transport::Stream>>;
fn tcp(socket: &mut Socket) -> Result<&TcpStream> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => Ok(stream.tcp()),
        MaybeTlsStream::Rustls(tls) => Ok(tls.sock.tcp()),
        _ => Err(Error::Ssh("不支持的 TLS 流")),
    }
}
pub fn connect(
    state: &SessionState,
    pubkey: &str,
    mode: Mode,
    timeout: Duration,
) -> Result<Socket> {
    connect_with_proxy(
        state,
        pubkey,
        mode,
        timeout,
        &crate::websocket_transport::ProxyEnvironment::capture(),
    )
}

pub fn connect_with_proxy(
    state: &SessionState,
    pubkey: &str,
    mode: Mode,
    timeout: Duration,
    proxy: &crate::websocket_transport::ProxyEnvironment,
) -> Result<Socket> {
    validate_key(pubkey)?;
    if timeout.is_zero() {
        return Err(Error::Ssh("连接期限必须大于零"));
    }
    let url = ws_url(state, mode)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| Error::Ssh("无效的 SSH 握手"))?;
    request.headers_mut().insert(
        "X-Colab-Ssh-Pubkey",
        pubkey
            .parse()
            .map_err(|_| Error::Ssh("无效的公钥 header"))?,
    );
    request
        .headers_mut()
        .insert("X-Colab-Client-Agent", "colab-cli".parse().unwrap());
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(Error::Ssh("连接期限溢出"))?;
    let stream = crate::websocket_transport::connect(&url, deadline, proxy)?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_FRAME_BYTES + 256 * 1024);
    let (mut socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|error| match error {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
                handshake_error(
                    response.status().as_u16(),
                    response.body().as_deref().unwrap_or_default(),
                )
            }
            _ => Error::Ssh("SSH WebSocket 握手或传输失败"),
        })?;
    tcp(&mut socket)?
        .set_nonblocking(true)
        .map_err(|_| Error::Ssh("无法设置非阻塞 SSH 连接"))?;
    Ok(socket)
}
fn blocked(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
fn socket_blocked(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if blocked(e))
}

/// Both I/O endpoints MUST be nonblocking. At most one WS message is held for
/// stdout; stalled output backpressures reads without blocking signal handling.
/// Stdin EOF closes the whole WS, matching upstream (there is no SSH half-close).
pub fn bridge(
    mut socket: Socket,
    input: &mut dyn Read,
    output: &mut dyn Write,
    cancellation: &AtomicUsize,
) -> Result<i32> {
    let mut pending = Vec::new();
    let mut offset = 0;
    let mut last_ping = Instant::now();
    let mut stdin_closed = false;
    let result = (|| loop {
        let signal = cancellation.load(Ordering::SeqCst);
        if signal != 0 {
            return Ok(128 + signal as i32);
        }
        let mut activity = false;
        if offset < pending.len() {
            match output.write(&pending[offset..]) {
                Ok(0) => return Err(Error::Ssh("SSH stdout 已关闭")),
                Ok(n) => {
                    offset += n;
                    activity = true;
                }
                Err(e) if blocked(&e) => {}
                Err(_) => return Err(Error::Ssh("SSH stdout 写入失败")),
            }
        }
        if offset == pending.len() {
            pending.clear();
            offset = 0;
            if stdin_closed {
                return Ok(0);
            }
            match socket.read() {
                Ok(Message::Binary(bytes)) => {
                    pending = bytes.to_vec();
                    activity = true;
                }
                Ok(Message::Text(text)) => {
                    pending = text.as_bytes().to_vec();
                    activity = true;
                }
                Ok(Message::Close(_)) => {
                    let _ = socket.flush();
                    return Ok(0);
                }
                Ok(_) => activity = true,
                Err(e) if socket_blocked(&e) => {}
                Err(_) => return Err(Error::Ssh("SSH WebSocket 传输中断")),
            }
        }
        let mut write_blocked = match socket.flush() {
            Ok(()) => false,
            Err(e) if socket_blocked(&e) => true,
            Err(_) => return Err(Error::Ssh("SSH WebSocket 写入失败")),
        };
        if !write_blocked && last_ping.elapsed() >= Duration::from_secs(60) {
            match socket.send(Message::Ping(b"colab-cli".to_vec().into())) {
                Ok(()) => {}
                Err(e) if socket_blocked(&e) => write_blocked = true,
                Err(_) => return Err(Error::Ssh("SSH 心跳发送失败")),
            }
            last_ping = Instant::now();
        }
        if !write_blocked && !stdin_closed {
            let mut bytes = [0; 8192];
            match input.read(&mut bytes) {
                Ok(0) => {
                    stdin_closed = true;
                }
                Ok(n) => {
                    // Tungstenite retains a queued frame on WouldBlock; never
                    // resend the bytes, or a blocked socket would duplicate SSH data.
                    match socket.send(Message::Binary(bytes[..n].to_vec().into())) {
                        Ok(()) => {}
                        Err(e) if socket_blocked(&e) => {}
                        Err(_) => return Err(Error::Ssh("SSH stdin 转发失败")),
                    }
                    activity = true;
                }
                Err(e) if blocked(&e) => {}
                Err(_) => return Err(Error::Ssh("SSH stdin 读取失败")),
            }
        }
        if !activity {
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    let _ = socket.close(None);
    let _ = socket.flush();
    result
}

/// Quote for the shell used by OpenSSH ProxyCommand (not Rust Command args).
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
pub fn proxy_command(arguments: &[String]) -> Result<String> {
    if arguments.iter().any(|s| s.contains('\0')) {
        return Err(Error::Ssh("ProxyCommand 参数包含 NUL"));
    }
    // OpenSSH expands percent tokens BEFORE invoking the shell.
    Ok(arguments
        .iter()
        .map(|s| shell_quote(&s.replace('%', "%%")))
        .collect::<Vec<_>>()
        .join(" "))
}
pub fn openssh_args(
    proxy: &str,
    identity: Option<&Path>,
    home: Option<&Path>,
    command: &[String],
    tty: bool,
) -> Result<Vec<OsString>> {
    if command.iter().any(|s| s.contains('\0')) {
        return Err(Error::Ssh("远端命令包含 NUL"));
    }
    let mut args: Vec<OsString> = [
        "-o",
        &format!("ProxyCommand={proxy}"),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        if cfg!(windows) {
            "UserKnownHostsFile=NUL"
        } else {
            "UserKnownHostsFile=/dev/null"
        },
        "-o",
        "LogLevel=ERROR",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if let Some(path) = identity {
        let mut path = expand_identity(path, home)?;
        if path.extension().is_some_and(|e| e == "pub") {
            let private = path.with_extension("");
            if private.exists() {
                path = private;
            }
        }
        args.push("-i".into());
        args.push(path.into_os_string());
    }
    args.push(
        if command.is_empty() || tty {
            "-t"
        } else {
            "-T"
        }
        .into(),
    );
    args.push("root@colab-runtime".into());
    let remote = match command {
        [] => "exec ${SHELL:-/bin/bash} -l".into(),
        [line] => line.clone(),
        words => words
            .iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" "),
    };
    args.push(format!("cd /content 2>/dev/null; {remote}").into());
    Ok(args)
}
pub fn run_openssh(arguments: &[OsString], cancellation: &AtomicUsize) -> Result<i32> {
    if cancellation.load(Ordering::SeqCst) != 0 {
        return Err(Error::Ssh("已取消；未启动 OpenSSH"));
    }
    let mut child = Command::new("ssh")
        .args(arguments)
        .spawn()
        .map_err(|_| Error::Ssh("无法启动系统 ssh；请安装 OpenSSH"))?;
    let mut cancelling = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let signal = cancellation.load(Ordering::SeqCst);
                if signal != 0 {
                    return Ok(128 + signal as i32);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    return Ok(status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
                }
                #[cfg(not(unix))]
                return Ok(status.code().unwrap_or(1));
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Ssh("等待 OpenSSH 子进程失败"));
            }
        }
        let signal = cancellation.load(Ordering::SeqCst);
        if signal != 0 {
            if cancelling.is_none() {
                #[cfg(unix)]
                // SAFETY: child is owned, still running and has not been reaped.
                unsafe {
                    libc::kill(child.id() as i32, signal as i32);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                cancelling = Some(Instant::now());
            }
            if cancelling.is_some_and(|start| start.elapsed() >= Duration::from_secs(1)) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(128 + signal as i32);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub struct Outcome {
    pub action: Result<i32>,
    pub cleanup: Option<Error>,
}
/// Install this scope immediately after selecting/creating a VM. Key resolution,
/// handshake and child startup all belong INSIDE action, not before it.
pub fn with_cleanup(
    service: &SessionService,
    state: &SessionState,
    remove: bool,
    action: impl FnOnce() -> Result<i32>,
) -> Outcome {
    with_cleanup_progress(service, state, remove, action, |_| {})
}
#[derive(Clone, Copy)]
pub enum CleanupProgress {
    Releasing,
    Released,
}
pub fn with_cleanup_progress(
    service: &SessionService,
    state: &SessionState,
    remove: bool,
    action: impl FnOnce() -> Result<i32>,
    mut progress: impl FnMut(CleanupProgress),
) -> Outcome {
    let action = action();
    let cleanup = if remove {
        progress(CleanupProgress::Releasing);
        match service.stop_if_endpoint(&state.name, Some(&state.endpoint)) {
            Ok(()) | Err(Error::SessionMissing(_)) => {
                progress(CleanupProgress::Released);
                None
            }
            Err(error) => Some(error),
        }
    } else {
        None
    };
    Outcome { action, cleanup }
}

#[cfg(unix)]
pub struct StdioGuard {
    pub input: fs::File,
    pub output: fs::File,
    original: [(i32, i32); 2],
}
#[cfg(unix)]
impl StdioGuard {
    pub fn open() -> Result<Self> {
        use std::os::fd::FromRawFd;
        let mut original = [(0, -1), (1, -1)];
        for (fd, flags) in &mut original {
            // SAFETY: querying standard descriptor flags does not transfer ownership.
            *flags = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
            if *flags < 0 {
                return Err(Error::Ssh("无法访问 SSH stdio"));
            }
        }
        // SAFETY: dup returns independent descriptor ownership, sharing I/O flags.
        let input = unsafe { libc::dup(0) };
        let output = unsafe { libc::dup(1) };
        if input < 0 || output < 0 {
            if input >= 0 {
                unsafe {
                    libc::close(input);
                }
            }
            if output >= 0 {
                unsafe {
                    libc::close(output);
                }
            }
            return Err(Error::Ssh("无法复制 SSH stdio"));
        }
        // SAFETY: the duplicated descriptors are uniquely owned here.
        let guard = Self {
            input: unsafe { fs::File::from_raw_fd(input) },
            output: unsafe { fs::File::from_raw_fd(output) },
            original,
        };
        for (fd, flags) in guard.original {
            // SAFETY: standard descriptors are borrowed for the guard lifetime.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(Error::Ssh("无法设置非阻塞 SSH stdio"));
            }
        }
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for StdioGuard {
    fn drop(&mut self) {
        for (fd, flags) in self.original {
            // SAFETY: restores flags of borrowed standard descriptors; never closes them.
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags);
            }
        }
    }
}
