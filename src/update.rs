//! Native release manifests and verified, recoverable Unix installation.
pub mod daily;
mod store;
use crate::{Error, Result, error::io};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use url::{Host, Url};

const MAX_BINARY: u64 = 256 * 1024 * 1024;
#[derive(Clone, Deserialize)]
struct Artifact {
    url: Url,
    sha256: String,
    size: u64,
}
#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    package: String,
    version: String,
    artifacts: BTreeMap<String, Artifact>,
}
#[derive(Serialize)]
pub struct Plan {
    pub current_version: String,
    pub latest_version: String,
    pub target: String,
    pub update_available: bool,
    #[serde(skip)]
    artifact: Artifact,
}
#[derive(Serialize)]
pub struct InstallResult {
    pub status: &'static str,
    pub version: String,
    pub cleanup_pending: bool,
}
pub struct UpdateClient {
    client: reqwest::blocking::Client,
}
fn loopback(uri: &Url) -> bool {
    match uri.host() {
        Some(Host::Domain("localhost" | "localhost.")) => true,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => {
            address.is_loopback() || address.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}
fn validate_url(uri: &Url, query: bool) -> Result<()> {
    if !uri.username().is_empty()
        || uri.password().is_some()
        || uri.fragment().is_some()
        || (!query && uri.query().is_some())
        || !(uri.scheme() == "https" || (uri.scheme() == "http" && loopback(uri)))
    {
        return Err(Error::Update(
            "发行 URL 需要 HTTPS，禁止认证信息、fragment 或初始 query；仅 loopback 允许 HTTP",
        ));
    }
    Ok(())
}
fn redirect_allowed(uri: &Url, previous: &[Url]) -> bool {
    let local_destination = loopback(uri);
    previous.len() <= 5
        && validate_url(uri, true).is_ok()
        && previous.iter().all(|source| {
            (!local_destination || loopback(source))
                && (uri.scheme() != "http" || source.scheme() == "http")
        })
}

fn validate_artifact_url(manifest: &Url, artifact: &Url) -> Result<()> {
    validate_url(artifact, false)?;
    if !loopback(manifest) && loopback(artifact) {
        return Err(Error::Update("公开发行清单不能指向 loopback 发行物"));
    }
    Ok(())
}

pub fn current_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        _ => Err(Error::Update(
            "当前原生更新仅支持 Linux GNU/macOS 的 x86_64 与 aarch64",
        )),
    }
}
fn version(raw: &str) -> Result<Version> {
    Version::parse(raw).map_err(|_| Error::Update("版本必须符合 SemVer"))
}
impl UpdateClient {
    pub fn new(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(3600) {
            return Err(Error::Update("请求期限必须为 1–3600 秒"));
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if redirect_allowed(attempt.url(), attempt.previous()) {
                    attempt.follow()
                } else {
                    attempt.error("更新重定向不安全或过多")
                }
            }))
            .build()
            .map_err(|_| Error::UpdateTransport)?;
        Ok(Self { client })
    }
    pub fn check(&self, uri: &Url, current: &str) -> Result<Plan> {
        validate_url(uri, false)?;
        let target = current_target()?;
        let current = version(current)?;
        let response = self
            .client
            .get(uri.clone())
            .send()
            .map_err(|_| Error::UpdateTransport)?;
        if !response.status().is_success() {
            return Err(Error::UpdateStatus(response.status().as_u16()));
        }
        let manifest_url = response.url().clone();
        let mut body = Vec::new();
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::UpdateTransport)?;
        if body.len() > 65536 {
            return Err(Error::Update("发行清单过大"));
        }
        let manifest: Manifest =
            serde_json::from_slice(&body).map_err(|_| Error::Update("发行清单无效"))?;
        if manifest.schema_version != 1 || manifest.package != "colabcli-rust" {
            return Err(Error::Update("发行清单 schema 或 package 不匹配"));
        }
        let latest = version(&manifest.version)?;
        let artifact = manifest
            .artifacts
            .get(target)
            .ok_or(Error::Update("清单不包含当前平台发行物"))?
            .clone();
        validate_artifact_url(&manifest_url, &artifact.url)?;
        if artifact.size == 0
            || artifact.size > MAX_BINARY
            || artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Update("发行物长度或 SHA-256 无效"));
        }
        Ok(Plan {
            current_version: current.to_string(),
            latest_version: latest.to_string(),
            target: target.into(),
            update_available: latest.cmp_precedence(&current).is_gt(),
            artifact,
        })
    }
    fn download(&self, artifact: &Artifact, path: &Path) -> Result<()> {
        let response = self
            .client
            .get(artifact.url.clone())
            .send()
            .map_err(|_| Error::UpdateTransport)?;
        if !response.status().is_success() {
            return Err(Error::UpdateStatus(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|size| size != artifact.size)
        {
            return Err(Error::Update("发行物 Content-Length 不匹配"));
        }
        let mut file = File::create(path).map_err(|e| io(path, e))?;
        let copied = std::io::copy(&mut response.take(artifact.size + 1), &mut file)
            .map_err(|_| Error::UpdateTransport)?;
        if copied != artifact.size {
            return Err(Error::Update("发行物长度不匹配"));
        }
        file.sync_all().map_err(|e| io(path, e))?;
        drop(file);
        if fingerprint(path)?.0 != artifact.sha256.to_ascii_lowercase() {
            return Err(Error::Update("发行物 SHA-256 不匹配"));
        }
        validate_image(path)?;
        store::executable(path, 0o755)?;
        Ok(())
    }
    pub fn install(&self, plan: &Plan, target: &Path) -> Result<InstallResult> {
        if plan.target != current_target()?
            || plan.update_available
                != version(&plan.latest_version)?
                    .cmp_precedence(&version(&plan.current_version)?)
                    .is_gt()
        {
            return Err(Error::Update("更新计划不一致"));
        }
        let target = store::canonical_target(target)?;
        if !plan.update_available {
            store::reject_pending(&target)?;
            return Ok(InstallResult {
                status: "up_to_date",
                version: plan.current_version.clone(),
                cleanup_pending: false,
            });
        }
        let store = store::Store::open(&target, true)?;
        let _guard = store.lock()?;
        let previous = store.load()?;
        if previous
            .as_ref()
            .is_some_and(|state| state.phase != store::Phase::Committed)
        {
            return Err(Error::UpdatePending);
        }
        if let Some(state) = &previous {
            store.validate_current(state)?;
        }
        validate_image(&target)?;
        let current = probe_version(&target)?;
        if current
            .cmp_precedence(&version(&plan.current_version)?)
            .is_ne()
        {
            return Err(Error::Update("安装目标版本与运行中的 CLI 不匹配"));
        }
        let old = fingerprint(&target)?;
        let stage = tempfile::Builder::new()
            .prefix("txn-")
            .tempdir_in(store.root())
            .map_err(|e| io(store.root(), e))?;
        let payload = stage.path().join("payload");
        self.download(&plan.artifact, &payload)?;
        if probe_version(&payload)? != version(&plan.latest_version)? {
            return Err(Error::Update("发行物版本自检与清单不匹配"));
        }
        if fingerprint(&target)? != old {
            return Err(Error::Update("下载期间安装文件已变化；拒绝覆盖"));
        }
        store::copy_sync(&target, &stage.path().join("previous"))?;
        fs::set_permissions(
            &payload,
            fs::metadata(&target)
                .map_err(|e| io(&target, e))?
                .permissions(),
        )
        .map_err(|e| io(&payload, e))?;
        File::open(&payload)
            .and_then(|file| file.sync_all())
            .map_err(|e| io(&payload, e))?;
        if fingerprint(&target)? != old {
            return Err(Error::Update("准备替换期间安装文件已变化；拒绝覆盖"));
        }
        let state = store.prepare(stage, plan, old, &previous)?;
        if fs::rename(store.generation(&state)?.join("payload"), &target).is_err() {
            if fingerprint(&target).ok().as_ref()
                == Some(&(state.previous_sha256.clone(), state.previous_size))
            {
                let _ = store.abort_prepared(&state);
            }
            return Err(Error::Update(
                "原子替换失败；原文件保留，请用 update --status 核查",
            ));
        }
        if store::sync_parent(&target).is_err() {
            return Err(Error::UpdatePending);
        }
        let mut committed = state;
        committed.phase = store::Phase::Committed;
        store.save(&committed).map_err(|_| Error::UpdatePending)?;
        let cleanup_pending = store.prune_previous(&committed, &previous).is_err();
        Ok(InstallResult {
            status: "installed",
            version: plan.latest_version.clone(),
            cleanup_pending,
        })
    }
}
// All image/version checks happen on local, checksum-verified files. No shell or
// package manager participates in installation.
fn fingerprint(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).map_err(|e| io(path, e))?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| io(path, e))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_BINARY {
            return Err(Error::Update("安装文件超过大小上限"));
        }
        hash.update(&buffer[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}
fn validate_image(path: &Path) -> Result<()> {
    let mut head = [0u8; 64];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .map_err(|_| Error::Update("发行物不是支持的原生可执行文件"))?;
    let supported = if cfg!(target_os = "linux") {
        head[..4] == *b"\x7fELF"
            && head[4] == 2
            && head[5] == 1
            && u16::from_le_bytes([head[18], head[19]])
                == if cfg!(target_arch = "aarch64") {
                    183
                } else {
                    62
                }
    } else if cfg!(target_os = "macos") {
        head[..4] == [0xcf, 0xfa, 0xed, 0xfe]
            && u32::from_le_bytes(head[4..8].try_into().unwrap())
                == if cfg!(target_arch = "aarch64") {
                    0x0100000c
                } else {
                    0x01000007
                }
    } else {
        false
    };
    if !supported {
        return Err(Error::Update("发行物格式或架构不匹配"));
    }
    Ok(())
}
fn executable_busy(error: &std::io::Error) -> bool {
    #[cfg(target_os = "linux")]
    {
        error.raw_os_error() == Some(libc::ETXTBSY)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = error;
        false
    }
}
fn probe_version(path: &Path) -> Result<Version> {
    let output =
        tempfile::NamedTempFile::new().map_err(|_| Error::Update("不能建立版本自检输出"))?;
    let mut command = Command::new(path);
    command
        .arg("version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(
            output
                .reopen()
                .map_err(|_| Error::Update("不能建立版本自检输出"))?,
        )
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            // A concurrent Linux fork can briefly inherit a writable handle
            // until its own exec closes it, even if the writer has closed it.
            Err(error) if executable_busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err(Error::Update("原生版本自检无法启动")),
        }
    };
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {}
            Err(_) => break false,
        }
        if Instant::now() >= deadline
            || output
                .as_file()
                .metadata()
                .map_or(true, |meta| meta.len() > 4096)
        {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !outcome {
        #[cfg(unix)]
        {
            // This unique process group belongs to the owned probe.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::Update("原生版本自检失败或超时"));
    }
    let mut bytes = Vec::new();
    output
        .reopen()
        .and_then(|file| file.take(4097).read_to_end(&mut bytes))
        .map_err(|_| Error::Update("原生版本自检输出无效"))?;
    if bytes.len() > 4096 {
        return Err(Error::Update("原生版本自检输出过大"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Update("原生版本自检输出无效"))?;
    version(
        text.trim()
            .strip_prefix("Version: ")
            .ok_or(Error::Update("原生版本自检输出无效"))?,
    )
}
pub fn status(target: &Path) -> Result<serde_json::Value> {
    store::status(target)
}
pub fn rollback(target: &Path) -> Result<InstallResult> {
    store::rollback(target)
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn public_release_requests_stay_out_of_loopback() {
        let public = Url::parse("https://releases.example.invalid/manifest.json").unwrap();
        let signed = Url::parse("https://assets.example.invalid/image?signature=fixture").unwrap();
        let local = Url::parse("http://127.0.0.1/manifest.json").unwrap();
        assert!(redirect_allowed(&signed, std::slice::from_ref(&public)));
        assert!(redirect_allowed(&public, std::slice::from_ref(&local)));
        for host in [
            "localhost",
            "localhost.",
            "127.0.0.2",
            "[::1]",
            "[::ffff:7f00:1]",
        ] {
            let destination = Url::parse(&format!("https://{host}/image")).unwrap();
            assert!(loopback(&destination));
            assert!(!redirect_allowed(
                &destination,
                std::slice::from_ref(&public)
            ));
            assert!(!redirect_allowed(
                &destination,
                &[local.clone(), public.clone()]
            ));
            assert!(validate_artifact_url(&public, &destination).is_err());
            assert!(validate_artifact_url(&local, &destination).is_ok());
        }
    }

    #[test]
    fn loopback_https_accepts_same_transport_but_cannot_downgrade() {
        let https = Url::parse("https://localhost/manifest.json").unwrap();
        let http = Url::parse("http://localhost/image").unwrap();
        assert!(redirect_allowed(&https, std::slice::from_ref(&https)));
        assert!(redirect_allowed(&https, std::slice::from_ref(&http)));
        assert!(!redirect_allowed(&http, std::slice::from_ref(&https)));
        assert!(redirect_allowed(&http, std::slice::from_ref(&http)));
    }
}
