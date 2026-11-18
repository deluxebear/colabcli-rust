//! Explicitly enabled, noninteractive external credential plugins.
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(super) struct Executable {
    args: Vec<String>,
    timeout: Duration,
    output: Option<PathBuf>,
    audience: String,
    kind: String,
    email: Option<String>,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}
#[derive(Deserialize)]
struct Config {
    command: String,
    timeout_millis: Option<u64>,
    output_file: Option<String>,
    interactive_timeout_millis: Option<Value>,
}
#[derive(Deserialize)]
struct Response {
    version: u64,
    success: bool,
    token_type: Option<String>,
    id_token: Option<Secret>,
    saml_response: Option<Secret>,
    expiration_time: Option<i64>,
    code: Option<String>,
    message: Option<Secret>,
}
enum Parsed {
    Token(Secret),
    Expired,
}
fn oidc(kind: &str) -> bool {
    matches!(
        kind,
        "urn:ietf:params:oauth:token-type:jwt"
            | "urn:ietf:params:oauth:token-type:id_token"
            | "urn:ietf:params:oauth:token-type:idToken"
    )
}
// Match Python shlex.split(posix=True, comments=False), without evaluating shell syntax.
fn split(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut started = false;
    for ch in command.chars() {
        if escape {
            if quote == Some('"') && ch != '"' && ch != '\\' {
                word.push('\\');
            }
            word.push(ch);
            escape = false;
            started = true;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escape = true,
                _ => word.push(ch),
            },
            None => match ch {
                '\\' => {
                    escape = true;
                    started = true;
                }
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                ' ' | '\t' | '\r' | '\n' => {
                    if started {
                        args.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            },
            _ => unreachable!(),
        }
    }
    if quote.is_some() || escape {
        return None;
    }
    if started {
        args.push(word);
    }
    Some(args)
}

impl Executable {
    pub(super) fn load(
        value: Value,
        allowed: bool,
        audience: &str,
        kind: &str,
        email: Option<&str>,
        cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Result<Self> {
        if !allowed {
            return Err(Error::OAuth(
                "可执行 ADC 来源需要 GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1",
            ));
        }
        #[cfg(not(unix))]
        {
            let _ = (value, audience, kind, email);
            return Err(Error::OAuth("可执行 ADC 来源当前仅支持 Unix"));
        }
        #[cfg(unix)]
        {
            let config: Config =
                serde_json::from_value(value).map_err(|_| Error::OAuth("可执行 ADC 配置无效"))?;
            if config.interactive_timeout_millis.is_some() {
                return Err(Error::OAuth("ADC 暂不支持交互式可执行来源"));
            }
            let args = split(&config.command)
                .filter(|a| {
                    !a.is_empty()
                        && Path::new(&a[0]).is_absolute()
                        && a.iter().all(|s| !s.contains('\0'))
                })
                .ok_or(Error::OAuth("ADC 命令需要绝对程序路径和有效引用"))?;
            let millis = config.timeout_millis.unwrap_or(30000);
            if !(5000..=120000).contains(&millis) {
                return Err(Error::OAuth("可执行 ADC timeout_millis 必须为 5000–120000"));
            }
            let output = config
                .output_file
                .map(|s| {
                    let p = PathBuf::from(s);
                    if !p.is_absolute() || p.as_os_str().is_empty() {
                        Err(Error::OAuth("ADC output_file 需要绝对路径"))
                    } else {
                        Ok(p)
                    }
                })
                .transpose()?;
            Ok(Self {
                args,
                timeout: Duration::from_millis(millis),
                output,
                audience: audience.into(),
                kind: kind.into(),
                email: email.map(str::to_string),
                cancellation,
            })
        }
    }
    fn parse(&self, raw: &[u8], now: DateTime<Utc>, cached: bool) -> Result<Parsed> {
        let value: Response = serde_json::from_slice(raw)
            .map_err(|_| Error::OAuth("可执行 ADC 输出不是有效 JSON"))?;
        if value.version != 1 {
            return Err(Error::OAuth("可执行 ADC 输出版本不支持"));
        }
        if !value.success {
            if value.code.as_ref().is_none_or(|s| s.is_empty()) || value.message.is_none() {
                return Err(Error::OAuth("可执行 ADC 错误输出缺少字段"));
            }
            return Err(Error::OAuth("可执行 ADC 来源拒绝授权"));
        }
        let kind = value
            .token_type
            .ok_or(Error::OAuth("可执行 ADC 输出缺少 token_type"))?;
        let token = if oidc(&self.kind) && oidc(&kind) {
            value.id_token
        } else if self.kind == "urn:ietf:params:oauth:token-type:saml2" && kind == self.kind {
            value.saml_response
        } else {
            return Err(Error::OAuth("可执行 ADC 输出令牌类型不匹配"));
        }
        .ok_or(Error::OAuth("可执行 ADC 输出缺少主体令牌"))?;
        if token.expose().trim().is_empty() {
            return Err(Error::OAuth("可执行 ADC 主体令牌为空"));
        }
        if let Some(expiration) = value.expiration_time {
            let expiration = DateTime::from_timestamp(expiration, 0)
                .ok_or(Error::OAuth("可执行 ADC 到期时间无效"))?;
            if expiration <= now {
                return Ok(Parsed::Expired);
            }
        } else if cached {
            return Ok(Parsed::Expired);
        } else if self.output.is_some() {
            return Err(Error::OAuth("配置 output_file 时需要 expiration_time"));
        }
        Ok(Parsed::Token(token))
    }
    fn file(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(path) = &self.output else {
            return Ok(None);
        };
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = match options.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::OAuth("无法读取 ADC output_file")),
        };
        if !file
            .metadata()
            .map_err(|_| Error::OAuth("无法检查 ADC output_file"))?
            .is_file()
        {
            return Err(Error::OAuth("ADC output_file 必须为普通文件"));
        }
        let mut raw = Zeroizing::new(Vec::new());
        file.take(65537)
            .read_to_end(&mut raw)
            .map_err(|_| Error::OAuth("无法读取 ADC output_file"))?;
        if raw.len() > 65536 {
            return Err(Error::OAuth("ADC output_file 过大"));
        }
        Ok(Some(raw))
    }
    pub(super) fn subject(&self, now: DateTime<Utc>) -> Result<Secret> {
        let started = Instant::now();
        let observed_now = || -> Result<DateTime<Utc>> {
            let elapsed = chrono::Duration::from_std(started.elapsed())
                .map_err(|_| Error::OAuth("可执行 ADC 校验时间无效"))?;
            now.checked_add_signed(elapsed)
                .ok_or(Error::OAuth("可执行 ADC 校验时间无效"))
        };
        let signal = self
            .cancellation
            .as_ref()
            .map_or(0, |flag| flag.load(Ordering::SeqCst));
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        if let Some(raw) = self.file()? {
            if let Parsed::Token(token) = self.parse(&raw, observed_now()?, true)? {
                return Ok(token);
            }
        }
        let raw = self.run()?;
        let raw = if raw.is_empty() {
            self.file()?.ok_or(Error::OAuth("可执行 ADC 未返回输出"))?
        } else {
            raw
        };
        match self.parse(&raw, observed_now()?, false)? {
            Parsed::Token(token) => Ok(token),
            Parsed::Expired => Err(Error::OAuth("可执行 ADC 返回已过期令牌")),
        }
    }
    #[cfg(not(unix))]
    fn run(&self) -> Result<Zeroizing<Vec<u8>>> {
        Err(Error::OAuth("可执行 ADC 来源当前仅支持 Unix"))
    }
    #[cfg(unix)]
    fn run(&self) -> Result<Zeroizing<Vec<u8>>> {
        use std::{
            io::ErrorKind,
            os::{fd::AsRawFd, unix::process::CommandExt},
        };
        let signals = Signals::install()?;
        let flag = self.cancellation.as_ref().unwrap_or(&signals.flag);
        let mut command = Command::new(&self.args[0]);
        command
            .args(&self.args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .env("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE", &self.audience)
            .env("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE", &self.kind)
            .env("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "0")
            .env("GOOGLE_EXTERNAL_ACCOUNT_REVOKE", "0")
            .env("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "1")
            .env(
                "GOOGLE_EXTERNAL_ACCOUNT_ID",
                self.email.as_deref().unwrap_or(""),
            )
            .env_remove("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL")
            .env_remove("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE");
        if let Some(email) = &self.email {
            command.env("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL", email);
        }
        if let Some(output) = &self.output {
            command.env("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", output);
        }
        let signal = flag.load(Ordering::SeqCst);
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        let mut running = Running {
            child: command
                .spawn()
                .map_err(|_| Error::OAuth("无法启动可执行 ADC 来源"))?,
            finished: false,
        };
        let mut stdout = running
            .child
            .stdout
            .take()
            .ok_or(Error::OAuth("无法读取可执行 ADC 输出"))?;
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(Error::OAuth("无法配置 ADC 输出管道"));
        }
        let deadline = Instant::now() + self.timeout;
        let mut raw = Zeroizing::new(Vec::new());
        let mut closed = false;
        loop {
            let signal = flag.load(Ordering::SeqCst);
            if signal != 0 {
                return Err(Error::CliExit(128 + signal as i32));
            }
            if Instant::now() >= deadline {
                return Err(Error::OAuth("可执行 ADC 来源超时"));
            }
            let mut chunk = [0u8; 4096];
            match stdout.read(&mut chunk) {
                Ok(0) => closed = true,
                Ok(n) => {
                    raw.extend_from_slice(&chunk[..n]);
                    if raw.len() > 65536 {
                        return Err(Error::OAuth("可执行 ADC 输出过大"));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::OAuth("无法读取可执行 ADC 输出")),
            }
            // WNOWAIT keeps our leader PID reserved until group cleanup, preventing PID reuse.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    running.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } < 0
            {
                if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::OAuth("无法检查 ADC 来源进程"));
            }
            if unsafe { info.si_pid() } != 0 {
                let status = running.finish()?;
                if !status.success() {
                    return Err(Error::OAuth("可执行 ADC 来源未成功退出"));
                }
                // Kill remaining descendants before draining the bounded nonblocking pipe.
                loop {
                    let signal = flag.load(Ordering::SeqCst);
                    if signal != 0 {
                        return Err(Error::CliExit(128 + signal as i32));
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::OAuth("可执行 ADC 输出管道未关闭"));
                    }
                    match stdout.read(&mut chunk) {
                        Ok(0) => return Ok(raw),
                        Ok(n) => {
                            raw.extend_from_slice(&chunk[..n]);
                            if raw.len() > 65536 {
                                return Err(Error::OAuth("可执行 ADC 输出过大"));
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return Err(Error::OAuth("可执行 ADC 输出管道未关闭"));
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return Err(Error::OAuth("无法读取可执行 ADC 输出")),
                    }
                }
            }
            if closed {
                std::thread::sleep(Duration::from_millis(10));
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
#[cfg(unix)]
struct Running {
    child: Child,
    finished: bool,
}
#[cfg(unix)]
impl Running {
    fn finish(&mut self) -> Result<std::process::ExitStatus> {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let result = self
            .child
            .wait()
            .map_err(|_| Error::OAuth("无法回收 ADC 来源进程"));
        self.finished = true;
        result
    }
}
#[cfg(unix)]
impl Drop for Running {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish();
        }
    }
}
#[cfg(unix)]
struct Signals {
    flag: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    handlers: Vec<signal_hook::SigId>,
}
#[cfg(unix)]
impl Signals {
    fn install() -> Result<Self> {
        let mut guard = Self {
            flag: Default::default(),
            handlers: Vec::new(),
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            guard.handlers.push(
                signal_hook::flag::register_usize(signal, guard.flag.clone(), signal as usize)
                    .map_err(|_| Error::OAuth("无法注册 ADC 来源信号处理器"))?,
            );
        }
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for Signals {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_splitting_matches_independent_python_stdlib_probe() {
        let probe: serde_json::Value = serde_json::from_str(include_str!(
            "../../../research/executable-command-probe.json"
        ))
        .unwrap();
        for case in probe["cases"].as_array().unwrap() {
            let expected: Option<Vec<String>> =
                serde_json::from_value(case["expected"].clone()).unwrap();
            assert_eq!(
                super::split(case["command"].as_str().unwrap()),
                expected,
                "{}",
                case["command"]
            );
        }
    }
}
