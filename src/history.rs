//! Account-scoped JSONL history with recoverable append intent and credential-aware serialization.
use crate::{
    Error, Result, contents,
    error::io,
    models::{ProfileId, Registry},
    storage::{FileLock, Storage},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const MAX_EVENT: usize = 48 * 1024 * 1024;
const MAX_QUERY: usize = 64 * 1024 * 1024;
#[derive(Clone)]
pub struct History {
    storage: Storage,
    profile: ProfileId,
}
#[derive(Serialize, Deserialize)]
struct AppendIntent {
    offset: u64,
    line: String,
}
#[derive(Default)]
pub struct HistoryQuery<'a> {
    pub event_type: Option<&'a str>,
    pub lines: Option<usize>,
}
impl History {
    pub fn new(storage: Storage, profile: ProfileId) -> Self {
        Self { storage, profile }
    }
    fn guard(&self) -> Result<FileLock> {
        let guard = self.storage.profile_lock(&self.profile)?;
        if self.storage.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let registry: Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or_else(|| Error::AccountMissing(self.profile.to_string()))?;
        registry.validate()?;
        if !registry.accounts.contains_key(&self.profile) {
            return Err(Error::AccountMissing(self.profile.to_string()));
        }
        Ok(guard)
    }
    fn path(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(Error::History("无效的历史会话名称"));
        }
        self.storage.profile_path(
            &self.profile,
            &format!(
                "history/rust-v1/{:x}.jsonl",
                Sha256::digest(name.as_bytes())
            ),
        )
    }
    fn adjacent(&self, path: &Path, suffix: &str) -> Result<PathBuf> {
        let relative = path
            .strip_prefix(self.storage.root())
            .map_err(|_| Error::UnsafePath(path.into()))?;
        self.storage
            .path(format!("{}{suffix}", relative.to_string_lossy()))
    }
    pub fn append(
        &self,
        name: &str,
        endpoint: Option<&str>,
        kind: &str,
        data: Value,
        event_id: Option<&str>,
    ) -> Result<()> {
        let _guard = self.guard()?;
        self.append_locked(name, endpoint, kind, data, event_id)
    }
    /// Caller holds the profile lifecycle lock, e.g. while committing assign/unassign.
    pub(crate) fn append_locked(
        &self,
        name: &str,
        endpoint: Option<&str>,
        kind: &str,
        mut data: Value,
        event_id: Option<&str>,
    ) -> Result<()> {
        if !data.is_object() || kind.is_empty() {
            return Err(Error::History("事件数据必须为对象"));
        }
        let path = self.path(name)?;
        let _lock = self.storage.lock(&self.adjacent(&path, ".rust.lock")?)?;
        self.recover(&path)?;
        if let Some(id) = event_id {
            let mut found = false;
            scan(&path, |event| {
                if event["event_id"].as_str() == Some(id) {
                    found = true;
                }
                Ok(())
            })?;
            if found {
                return Ok(());
            }
        }
        let object = data.as_object_mut().unwrap();
        for key in [
            "timestamp",
            "event_type",
            "event_id",
            "account",
            "session",
            "endpoint",
        ] {
            object.remove(key);
        }
        object.insert("timestamp".into(), json!(chrono::Utc::now().to_rfc3339()));
        object.insert("event_type".into(), json!(kind));
        object.insert(
            "event_id".into(),
            json!(
                event_id
                    .map(str::to_owned)
                    .unwrap_or_else(crate::jupyter::codec::message_id)
            ),
        );
        object.insert("account".into(), json!(self.profile));
        object.insert("session".into(), json!(name));
        if let Some(endpoint) = endpoint {
            object.insert("endpoint".into(), json!(endpoint));
        }
        if kind == "input_reply" {
            object.insert("value".into(), json!("[REDACTED]"));
        }
        redact(&mut data);
        let mut line =
            serde_json::to_string(&data).map_err(|_| Error::History("历史序列化失败"))?;
        line.push('\n');
        if line.len() > MAX_EVENT {
            return Err(Error::History("单条历史超过 48 MiB"));
        }
        let offset = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(io(&path, e)),
        };
        if offset > 0 {
            let mut file = File::open(&path).map_err(|e| io(&path, e))?;
            file.seek(SeekFrom::End(-1)).map_err(|e| io(&path, e))?;
            let mut last = [0];
            file.read_exact(&mut last).map_err(|e| io(&path, e))?;
            if last[0] != b'\n' {
                return Err(Error::InvalidData(path));
            }
        }
        self.storage.write(
            &self.adjacent(&path, ".append.json")?,
            &AppendIntent { offset, line },
        )?;
        self.recover(&path)
    }
    fn recover(&self, path: &Path) -> Result<()> {
        let pending = self.adjacent(path, ".append.json")?;
        let Some(intent) = self.storage.read::<AppendIntent>(&pending)? else {
            return Ok(());
        };
        if intent.line.len() > MAX_EVENT
            || !intent.line.ends_with('\n')
            || serde_json::from_str::<Value>(intent.line.trim_end())
                .ok()
                .is_none_or(|v| !v.is_object())
        {
            return Err(Error::InvalidData(pending));
        }
        self.storage.ensure_parent(path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|e| io(path, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|e| io(path, e))?;
        }
        let length = file.metadata().map_err(|e| io(path, e))?.len();
        if length < intent.offset || length - intent.offset > intent.line.len() as u64 {
            return Err(Error::InvalidData(path.into()));
        }
        file.seek(SeekFrom::Start(intent.offset))
            .map_err(|e| io(path, e))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).map_err(|e| io(path, e))?;
        if !intent.line.as_bytes().starts_with(&tail) {
            return Err(Error::InvalidData(path.into()));
        }
        if tail.len() != intent.line.len() {
            file.set_len(intent.offset).map_err(|e| io(path, e))?;
            file.seek(SeekFrom::Start(intent.offset))
                .map_err(|e| io(path, e))?;
            file.write_all(intent.line.as_bytes())
                .map_err(|e| io(path, e))?;
        }
        file.sync_all().map_err(|e| io(path, e))?;
        #[cfg(unix)]
        File::open(path.parent().unwrap())
            .and_then(|dir| dir.sync_all())
            .map_err(|e| io(path, e))?;
        self.storage.remove(&pending)
    }
    fn paths(&self) -> Result<Vec<(PathBuf, Option<String>)>> {
        let mut paths = BTreeSet::new();
        for (relative, legacy) in [("history", true), ("history/rust-v1", false)] {
            let dir = self.storage.profile_path(&self.profile, relative)?;
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
                let entry = entry.map_err(|e| io(&dir, e))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let filename = if name.ends_with(".jsonl") {
                    name
                } else if !legacy && name.ends_with(".jsonl.append.json") {
                    name.strip_suffix(".append.json").unwrap()
                } else {
                    continue;
                };
                let path = self
                    .storage
                    .profile_path(&self.profile, &format!("{relative}/{filename}"))?;
                paths.insert((
                    path,
                    legacy.then(|| filename.trim_end_matches(".jsonl").into()),
                ));
                if paths.len() > 10000 {
                    return Err(Error::History("历史文件数量超过 10000"));
                }
            }
        }
        Ok(paths.into_iter().collect())
    }
    pub fn sessions(&self) -> Result<Vec<String>> {
        let _guard = self.guard()?;
        let mut names = BTreeSet::new();
        for (path, legacy) in self.paths()? {
            let _lock = self.storage.lock(&self.adjacent(&path, ".rust.lock")?)?;
            self.recover(&path)?;
            if let Some(name) = legacy {
                names.insert(name);
            } else {
                let mut first = None;
                scan(&path, |event| {
                    if first.is_none() {
                        first = event["session"].as_str().map(str::to_owned);
                    }
                    Ok(())
                })?;
                if let Some(name) = first {
                    names.insert(name);
                }
            }
        }
        Ok(names.into_iter().collect())
    }
    pub fn has_session(&self, name: &str) -> Result<bool> {
        if self.path(name)?.exists() || self.adjacent(&self.path(name)?, ".append.json")?.exists() {
            return Ok(true);
        }
        if safe_legacy_name(name) {
            return Ok(self
                .storage
                .profile_path(&self.profile, &format!("history/{name}.jsonl"))?
                .is_file());
        }
        Ok(false)
    }
    pub fn read(&self, name: &str, query: HistoryQuery<'_>) -> Result<Vec<Value>> {
        self.read_skipping(name, query, 0)
    }

    /// Skip a prefix of events matching the type filter before collecting results.
    pub fn read_skipping(
        &self,
        name: &str,
        query: HistoryQuery<'_>,
        mut skip: usize,
    ) -> Result<Vec<Value>> {
        let _guard = self.guard()?;
        let mut paths = Vec::new();
        if safe_legacy_name(name) {
            let path = self
                .storage
                .profile_path(&self.profile, &format!("history/{name}.jsonl"))?;
            if path.exists() {
                paths.push(path);
            }
        }
        paths.push(self.path(name)?);
        let mut events = VecDeque::new();
        let mut bytes = 0usize;
        for path in paths {
            let _lock = self.storage.lock(&self.adjacent(&path, ".rust.lock")?)?;
            self.recover(&path)?;
            scan(&path, |mut event| {
                if query
                    .event_type
                    .is_some_and(|kind| event["event_type"].as_str() != Some(kind))
                {
                    return Ok(());
                }
                if skip > 0 {
                    skip -= 1;
                    return Ok(());
                }
                event["account"] = json!(self.profile);
                event["session"] = json!(name);
                redact(&mut event);
                let size = serde_json::to_vec(&event)
                    .map_err(|_| Error::History("历史编码失败"))?
                    .len();
                events.push_back((event, size));
                bytes = bytes.saturating_add(size);
                if let Some(lines) = query.lines {
                    while events.len() > lines {
                        bytes -= events.pop_front().unwrap().1;
                    }
                }
                if bytes > MAX_QUERY {
                    return Err(Error::History("历史查询超过 64 MiB；请用 --lines 限制"));
                }
                Ok(())
            })?;
        }
        Ok(events.into_iter().map(|(event, _)| event).collect())
    }
}
fn safe_legacy_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}
fn scan(path: &Path, mut callback: impl FnMut(Value) -> Result<()>) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(path, e)),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|e| io(path, e))?;
        if available.is_empty() {
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        if line.len().saturating_add(take) > MAX_EVENT {
            return Err(Error::History("历史记录超过 48 MiB"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") {
            if line.iter().any(|b| !b.is_ascii_whitespace()) {
                let event: Value =
                    serde_json::from_slice(&line).map_err(|_| Error::InvalidData(path.into()))?;
                if !event.is_object() {
                    return Err(Error::InvalidData(path.into()));
                }
                callback(event)?;
            }
            line.clear();
        }
    }
    if !line.is_empty() {
        return Err(Error::InvalidData(path.into()));
    }
    Ok(())
}
fn secret_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace('-', "_").as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "proxy_token"
            | "runtime_proxy_token"
            | "colab_runtime_proxy_token"
            | "xsrf_token"
            | "client_secret"
            | "authorization"
            | "password"
            | "secret"
            | "api_key"
            | "client_config"
    )
}
pub(crate) fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("event_type").and_then(Value::as_str) == Some("input_reply") {
                map.insert("value".into(), json!("[REDACTED]"));
            }
            for (key, value) in map {
                if secret_key(key) {
                    *value = json!("[REDACTED]");
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact(value);
            }
        }
        Value::String(text) => {
            if let Ok(mut url) = url::Url::parse(text) {
                if ["http", "https", "ws", "wss"].contains(&url.scheme()) {
                    let mut changed = !url.username().is_empty();
                    let _ = url.set_username("");
                    if url.password().is_some() {
                        let _ = url.set_password(None);
                        changed = true;
                    }
                    let query: Vec<_> = url
                        .query_pairs()
                        .map(|(key, value)| {
                            if secret_key(&key) {
                                changed = true;
                                (key.into_owned(), "[REDACTED]".into())
                            } else {
                                (key.into_owned(), value.into_owned())
                            }
                        })
                        .collect();
                    if changed {
                        if url.query().is_some() {
                            url.query_pairs_mut().clear().extend_pairs(query);
                        }
                        *text = url.to_string();
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn export(events: &[Value], name: &str, path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut safe = events.to_vec();
    for event in &mut safe {
        redact(event);
    }
    let bytes = match extension.as_str() {
        "jsonl" => {
            let mut bytes = Vec::new();
            for event in &safe {
                serde_json::to_writer(&mut bytes, event)
                    .map_err(|_| Error::History("导出编码失败"))?;
                bytes.push(b'\n');
            }
            bytes
        }
        "ipynb" => serde_json::to_vec_pretty(&notebook(&safe, name))
            .map_err(|_| Error::History("notebook 导出失败"))?,
        "md" | "txt" => render_text(&safe, name, extension == "md").into_bytes(),
        _ => return Err(Error::History("导出扩展名必须为 .ipynb/.md/.txt/.jsonl")),
    };
    contents::write_atomic(path, &bytes)
}
fn notebook(events: &[Value], name: &str) -> Value {
    let markdown = |source: String| json!({"cell_type":"markdown","id":crate::jupyter::codec::message_id(),"metadata":{},"source":source});
    let mut cells = vec![markdown(format!("# Colab Session: {name}"))];
    for event in events {
        if event["event_type"] == "execution" {
            let mut buffers = serde_json::Map::new();
            let outputs = event["outputs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .filter_map(|(index, mut output)| {
                    let data = output.as_object_mut()?;
                    data.remove("transient");
                    if let Some(raw) = data.remove("buffers") {
                        buffers.insert(index.to_string(), raw);
                    }
                    Some(output)
                })
                .collect::<Vec<_>>();
            cells.push(json!({"cell_type":"code","id":crate::jupyter::codec::message_id(),"metadata":{"colab_cli":{"status":event["status"],"msg_id":event["msg_id"],"timestamp":event["timestamp"]},"colab_cli_output_buffers":buffers},
                "source":event.get("code").cloned().unwrap_or(json!("")),"execution_count":event.get("execution_count").cloned().unwrap_or(Value::Null),"outputs":outputs}));
        } else {
            cells.push(markdown(
                serde_json::to_string_pretty(event).unwrap_or_default(),
            ));
        }
    }
    json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":"python3","display_name":"Python 3 (Colab)","language":"python"}},"cells":cells})
}
fn render_text(events: &[Value], name: &str, markdown: bool) -> String {
    let mut text = format!(
        "{}Colab Session: {name}\n\n",
        if markdown { "# " } else { "" }
    );
    for event in events {
        text.push_str(&format!(
            "[{}] {}\n",
            event["timestamp"].as_str().unwrap_or(""),
            event["event_type"].as_str().unwrap_or("unknown")
        ));
        if event["event_type"] == "execution" {
            let code = event["code"].as_str().unwrap_or("");
            if markdown {
                fenced(&mut text, code, "python");
            } else {
                text.push_str(code);
                text.push('\n');
            }
            for output in event["outputs"].as_array().into_iter().flatten() {
                let content = output["text"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| output["data"]["text/plain"].as_str().map(str::to_owned))
                    .or_else(|| {
                        output["traceback"].as_array().map(|lines| {
                            lines
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                    });
                if let Some(content) = content {
                    if markdown {
                        fenced(&mut text, &content, "");
                    } else {
                        text.push_str(&content);
                        text.push('\n');
                    }
                }
                if markdown {
                    use base64::{Engine, engine::general_purpose::STANDARD};
                    for mime in ["image/png", "image/jpeg"] {
                        if let Some(encoded) = output["data"][mime].as_str() {
                            // Re-encode validated bytes so image contents cannot inject Markdown syntax.
                            if let Ok(bytes) = STANDARD.decode(encoded) {
                                text.push_str(&format!(
                                    "![output](data:{mime};base64,{})\n\n",
                                    STANDARD.encode(bytes)
                                ));
                            }
                        }
                    }
                }
            }
        } else {
            text.push_str(&serde_json::to_string(event).unwrap_or_default());
            text.push('\n');
        }
        text.push('\n');
    }
    text
}
fn fenced(text: &mut String, body: &str, language: &str) {
    let max = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(max.max(2) + 1);
    text.push_str(&format!("{fence}{language}\n{body}\n{fence}\n\n"));
}
