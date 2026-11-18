//! Runtime Contents API. This client never receives or sends a control-plane Bearer token.
use crate::{
    Error, Result,
    error::io,
    models::{Secret, SessionState},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Method, blocking::Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;
mod json_document;

/// Whole-file v1 limit; uploads and decoded downloads are bounded independently of JSON size.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 48 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub content: Value,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
impl Entry {
    pub fn listing(&self) -> Result<Vec<Entry>> {
        if self.kind != "directory" {
            return Ok(vec![self.clone()]);
        }
        let mut entries: Vec<Entry> = serde_json::from_value(self.content.clone())
            .map_err(|_| Error::ContentsProtocol("目录 content 必须为条目数组"))?;
        entries.sort_by(|a, b| {
            (a.kind != "directory", &a.name).cmp(&(b.kind != "directory", &b.name))
        });
        Ok(entries)
    }
}

pub struct ContentsClient {
    client: Client,
    base: Url,
    proxy: Secret,
}
struct ContentsResponse {
    entry: Entry,
    json_document: Option<String>,
}
impl ContentsClient {
    pub fn new(session: &SessionState, timeout: Duration) -> Result<Self> {
        let base =
            Url::parse(&session.url).map_err(|_| Error::ContentsProtocol("无效的 runtime URL"))?;
        crate::auth::validate_endpoint(&base)?;
        if base.query().is_some() || timeout.is_zero() || session.token.is_empty() {
            return Err(Error::ContentsProtocol("无效的 runtime 配置或凭据"));
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
            proxy: session.token.clone(),
        })
    }
    fn url(&self, path: &str) -> Result<Url> {
        validate_remote_path(path, false)?;
        let mut url = self.base.clone();
        // Append, preserving any runtime base path. Push encodes each segment exactly once.
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::ContentsProtocol("无效的 runtime URL"))?;
        segments.pop_if_empty().push("api").push("contents");
        for part in path.trim_matches('/').split('/') {
            segments.push(part);
        }
        drop(segments);
        url.query_pairs_mut()
            .append_pair("authuser", "0")
            .append_pair("colab-runtime-proxy-token", self.proxy.expose());
        Ok(url)
    }
    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        download: bool,
    ) -> Result<Option<ContentsResponse>> {
        let mut url = self.url(path)?;
        if download {
            url.query_pairs_mut().append_pair("content", "1");
        }
        let delete = method == Method::DELETE;
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|_| Error::RuntimeTransport)?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(Error::RemoteMissing);
        }
        if !(200..300).contains(&status) {
            return Err(Error::RuntimeStatus(status));
        }
        if delete {
            return Ok(None);
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| Error::RuntimeTransport)?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(Error::FileTooLarge(MAX_RESPONSE_BYTES));
        }
        let (entry, json_document) = json_document::decode_entry(&body, download)?;
        if !["directory", "file", "notebook"].contains(&entry.kind.as_str()) {
            return Err(Error::ContentsProtocol("未知的 Contents 条目类型"));
        }
        Ok(Some(ContentsResponse {
            entry,
            json_document,
        }))
    }
    pub fn list(&self, path: &str) -> Result<Vec<Entry>> {
        self.request(Method::GET, path, None, false)?
            .unwrap()
            .entry
            .listing()
    }
    pub fn upload_bytes(&self, remote: &str, bytes: &[u8]) -> Result<Entry> {
        validate_remote_path(remote, true)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::FileTooLarge(MAX_FILE_BYTES));
        }
        let path = remote.trim_matches('/');
        let body = json!({"name":path.rsplit('/').next().unwrap(),"path":path,"type":"file",
            "format":"base64","content":STANDARD.encode(bytes),"chunk":1});
        Ok(self
            .request(Method::PUT, path, Some(body), false)?
            .unwrap()
            .entry)
    }
    pub fn upload(&self, local: &Path, remote: &str) -> Result<Entry> {
        validate_remote_path(remote, true)?;
        self.upload_bytes(remote, &read_local_file(local)?)
    }
    pub fn download_bytes(&self, remote: &str) -> Result<Vec<u8>> {
        validate_remote_path(remote, true)?;
        let response = self.request(Method::GET, remote, None, true)?.unwrap();
        let entry = response.entry;
        if entry.kind == "directory" {
            return Err(Error::RemoteDirectory);
        }
        let bytes = match entry.format.as_deref() {
            Some("base64") => {
                // Jupyter wraps binary content at 76 columns and adds a trailing newline.
                let encoded: Vec<u8> = entry
                    .content
                    .as_str()
                    .ok_or(Error::ContentsProtocol("base64 content 必须为字符串"))?
                    .bytes()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect();
                STANDARD
                    .decode(encoded)
                    .map_err(|_| Error::ContentsProtocol("无效的 base64 content"))?
            }
            Some("json") => {
                json_document::render(response.json_document.as_deref().unwrap_or("null"))?
            }
            Some("text") | None => entry
                .content
                .as_str()
                .ok_or(Error::ContentsProtocol("text content 必须为字符串"))?
                .as_bytes()
                .to_vec(),
            Some(_) => return Err(Error::ContentsProtocol("未知的文件 format")),
        };
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::FileTooLarge(MAX_FILE_BYTES));
        }
        Ok(bytes)
    }
    pub fn download(&self, remote: &str, local: &Path) -> Result<()> {
        validate_download_target(local)?;
        let bytes = self.download_bytes(remote)?;
        write_atomic(local, &bytes)
    }
    pub fn remove(&self, remote: &str) -> Result<()> {
        validate_remote_path(remote, true)?;
        self.request(Method::DELETE, remote, None, false)?;
        Ok(())
    }
}

pub fn validate_remote_path(path: &str, mutation_or_file: bool) -> Result<()> {
    let normalized = path.trim_matches('/');
    if path.chars().any(char::is_control)
        || path.contains('\\')
        || normalized
            .split('/')
            .any(|part| part == "." || part == "..")
        || (mutation_or_file && normalized.is_empty())
    {
        return Err(Error::ContentsProtocol(
            "远端路径无效；禁止根目录修改、控制字符和 . / .. 分段",
        ));
    }
    Ok(())
}
pub fn read_local_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).map_err(|e| io(path, e))?;
    if !metadata.is_file() {
        return Err(Error::ContentsProtocol("本地上传源必须为普通文件"));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(Error::FileTooLarge(MAX_FILE_BYTES));
    }
    let file = fs::File::open(path).map_err(|e| io(path, e))?;
    if !file.metadata().map_err(|e| io(path, e))?.is_file() {
        return Err(Error::ContentsProtocol("本地上传源必须为普通文件"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io(path, e))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::FileTooLarge(MAX_FILE_BYTES));
    }
    Ok(bytes)
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
pub fn validate_download_target(path: &Path) -> Result<()> {
    if path.file_name().is_none() {
        return Err(Error::ContentsProtocol("本地目标必须是文件路径"));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(Error::ContentsProtocol("本地目标不能为目录或符号链接"));
        }
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(io(path, error)),
    }
    if !parent(path).is_dir() {
        return Err(Error::ContentsProtocol("本地目标父目录不存在"));
    }
    Ok(())
}
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    validate_download_target(path)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent(path)).map_err(|e| io(path, e))?;
    temp.write_all(bytes).map_err(|e| io(path, e))?;
    temp.as_file().sync_all().map_err(|e| io(path, e))?;
    temp.persist(path).map_err(|e| io(path, e.error))?;
    #[cfg(unix)]
    fs::File::open(parent(path))
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io(path, e))?;
    Ok(())
}

/// Only a confirmed 404 starts an empty file; all other download errors abort before the editor.
/// The editor callback is injectable so cancellation and no-change behavior can be tested offline.
pub fn edit_with(
    client: &ContentsClient,
    remote: &str,
    editor: impl FnOnce(&Path) -> Result<()>,
) -> Result<bool> {
    edit_with_access(
        remote,
        || client.download_bytes(remote),
        |bytes| client.upload_bytes(remote, bytes).map(|_| ()),
        editor,
    )
}

pub fn edit_with_access(
    remote: &str,
    download: impl FnOnce() -> Result<Vec<u8>>,
    upload: impl FnOnce(&[u8]) -> Result<()>,
    editor: impl FnOnce(&Path) -> Result<()>,
) -> Result<bool> {
    validate_remote_path(remote, true)?;
    let before = match download() {
        Ok(bytes) => bytes,
        Err(Error::RemoteMissing) => Vec::new(),
        Err(error) => return Err(error),
    };
    let suffix = Path::new(remote)
        .extension()
        .and_then(|s| s.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    let directory =
        tempfile::tempdir().map_err(|e| io(PathBuf::from("temporary editor directory"), e))?;
    let file = directory.path().join(format!("edit{suffix}"));
    write_atomic(&file, &before)?;
    editor(&file)?;
    let after = read_local_file(&file)?;
    if after == before {
        return Ok(false);
    }
    if let Err(source) = upload(&after) {
        let _retained_directory = directory.keep();
        return Err(Error::EditPending {
            path: file,
            source: Box::new(source),
        });
    }
    Ok(true)
}
