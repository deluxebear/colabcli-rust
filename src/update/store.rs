use super::{InstallResult, Plan, fingerprint, version};
use crate::{
    Error, Result,
    error::io,
    storage::{FileLock, Storage},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Prepared,
    Committed,
    RollingBack,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Journal {
    schema_version: u32,
    target: PathBuf,
    generation: String,
    pub phase: Phase,
    pub previous_sha256: String,
    pub previous_size: u64,
    installed_sha256: String,
    installed_size: u64,
    previous_version: String,
    installed_version: String,
}
pub(super) struct Store {
    storage: Storage,
    target: PathBuf,
}
pub(super) fn canonical_target(target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or(Error::Update("安装路径缺少文件名"))?;
    let parent = target
        .parent()
        .filter(|s| !s.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let target = fs::canonicalize(parent)
        .map_err(|e| io(parent, e))?
        .join(name);
    if target.to_str().is_none() {
        return Err(Error::Update("安装路径必须是 UTF-8"));
    }
    let meta = fs::symlink_metadata(&target).map_err(|e| io(&target, e))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Update("安装目标必须是已有普通文件，不能为符号链接"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = fs::metadata(target.parent().unwrap()).map_err(|e| io(&target, e))?;
        if parent.uid() != unsafe { libc::geteuid() } || parent.mode() & 0o022 != 0 {
            return Err(Error::Update(
                "安装父目录必须由当前用户拥有且不向其他用户开放写入",
            ));
        }
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o6022 != 0
            || meta.mode() & 0o111 == 0
        {
            return Err(Error::Update(
                "安装目标必须是当前用户拥有的非特权可执行文件",
            ));
        }
    }
    Ok(target)
}
fn directory(path: &Path, create: bool) -> Result<()> {
    if create {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if let Err(error) = builder.create(path) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(io(path, error));
            }
        }
    }
    let meta = fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::UnsafePath(path.into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(Error::Update(
                "更新记录目录必须由当前用户拥有且不对其他用户开放",
            ));
        }
    }
    Ok(())
}
fn area(target: &Path) -> (PathBuf, PathBuf) {
    let parent = target.parent().unwrap().join(".colab-updates");
    // The canonical path is local metadata, not an identity or credential.
    let child = parent.join(format!(
        "{:x}",
        Sha256::digest(target.as_os_str().as_encoded_bytes())
    ));
    (parent, child)
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::UnsafePath(path.into()));
    }
    let mut data = Vec::new();
    File::open(path)
        .and_then(|file| file.take(65537).read_to_end(&mut data))
        .map_err(|e| io(path, e))?;
    if data.len() > 65536 {
        return Err(Error::Update("更新记录过大"));
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| Error::Update("更新记录无效"))
}
pub(super) fn executable(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
            .map_err(|e| io(path, e))?;
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|e| io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
pub(super) fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().ok_or(Error::Update("文件缺少父目录"))?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| io(parent, e))?;
    }
    Ok(())
}
pub(super) fn copy_sync(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).map_err(|e| io(from, e))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::UnsafePath(from.into()));
    }
    if let Ok(meta) = fs::symlink_metadata(to) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(Error::UnsafePath(to.into()));
        }
    }
    fs::copy(from, to).map_err(|e| io(to, e))?;
    File::open(to)
        .and_then(|file| file.sync_all())
        .map_err(|e| io(to, e))?;
    Ok(())
}
impl Store {
    pub(super) fn open(target: &Path, create: bool) -> Result<Self> {
        let (parent, root) = area(target);
        directory(&parent, create)?;
        directory(&root, create)?;
        let storage = if create {
            Storage::new(&root)?
        } else {
            Storage::open_existing(&root)?
        };
        Ok(Self {
            storage,
            target: target.into(),
        })
    }
    pub(super) fn root(&self) -> &Path {
        self.storage.root()
    }
    pub(super) fn lock(&self) -> Result<FileLock> {
        self.storage.lock(&self.storage.path("operation.lock")?)
    }
    fn basic(&self, state: &Journal) -> Result<()> {
        if state.schema_version != 1
            || state.target != self.target
            || !state.generation.starts_with("txn-")
            || state.generation.len() > 64
            || !state
                .generation
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || [&state.previous_sha256, &state.installed_sha256]
                .iter()
                .any(|hash| hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()))
            || state.previous_size == 0
            || state.previous_size > super::MAX_BINARY
            || state.installed_size == 0
            || state.installed_size > super::MAX_BINARY
        {
            return Err(Error::Update("更新记录归属或字段无效"));
        }
        version(&state.previous_version)?;
        version(&state.installed_version)?;
        Ok(())
    }
    pub(super) fn generation(&self, state: &Journal) -> Result<PathBuf> {
        self.basic(state)?;
        self.storage.path(&state.generation)
    }
    fn verify_record(&self, state: &Journal) -> Result<()> {
        let generation = self.generation(state)?;
        directory(&generation, false)?;
        let mut record: Journal =
            read(&generation.join("record.json"))?.ok_or(Error::Update("缺少更新归属记录"))?;
        record.phase = state.phase.clone();
        if &record != state {
            return Err(Error::Update("更新记录与其归属记录不匹配"));
        }
        Ok(())
    }
    pub(super) fn load(&self) -> Result<Option<Journal>> {
        let state: Option<Journal> = read(&self.storage.path("state.json")?)?;
        if let Some(state) = &state {
            self.verify_record(state)?;
        }
        Ok(state)
    }
    pub(super) fn save(&self, state: &Journal) -> Result<()> {
        self.basic(state)?;
        self.storage.write(&self.storage.path("state.json")?, state)
    }
    fn backup(&self, state: &Journal) -> Result<PathBuf> {
        self.verify_record(state)?;
        let path = self.generation(state)?.join("previous");
        let meta = fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || fingerprint(&path)? != (state.previous_sha256.clone(), state.previous_size)
        {
            return Err(Error::Update("旧版本备份缺失或校验失败"));
        }
        Ok(path)
    }
    pub(super) fn validate_current(&self, state: &Journal) -> Result<()> {
        self.backup(state)?;
        if fingerprint(&self.target)? != (state.installed_sha256.clone(), state.installed_size) {
            return Err(Error::Update("安装文件已被其他操作改变；拒绝覆盖更新记录"));
        }
        Ok(())
    }
    pub(super) fn prepare(
        &self,
        stage: tempfile::TempDir,
        plan: &Plan,
        old: (String, u64),
        previous: &Option<Journal>,
    ) -> Result<Journal> {
        if fingerprint(&stage.path().join("previous"))? != old {
            return Err(Error::Update("旧版本复制期间发生变化；拒绝替换"));
        }
        let generation = stage
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .ok_or(Error::Update("暂存名称无效"))?
            .to_owned();
        let state = Journal {
            schema_version: 1,
            target: self.target.clone(),
            generation,
            phase: Phase::Prepared,
            previous_sha256: old.0,
            previous_size: old.1,
            installed_sha256: plan.artifact.sha256.to_ascii_lowercase(),
            installed_size: plan.artifact.size,
            previous_version: plan.current_version.clone(),
            installed_version: plan.latest_version.clone(),
        };
        self.storage
            .write(&stage.path().join("record.json"), &state)?;
        if let Some(previous) = previous {
            self.storage
                .write(&stage.path().join("prior-state.json"), previous)?;
        }
        sync_parent(&stage.path().join("record.json"))?;
        // Once published, the generation survives all errors until explicit recovery.
        let durable = stage.keep();
        if let Err(error) = self.save(&state) {
            if self.load().ok().flatten().as_ref() != Some(&state) {
                let _ = fs::remove_dir_all(&durable);
            }
            return Err(error);
        }
        Ok(state)
    }
    fn cleanup_generation(&self, state: &Journal) -> Result<()> {
        let path = self.generation(state)?;
        if !path.try_exists().map_err(|e| io(&path, e))? {
            return Ok(());
        }
        self.verify_record(state)?;
        for entry in fs::read_dir(&path).map_err(|e| io(&path, e))? {
            let entry = entry.map_err(|e| io(&path, e))?;
            let name = entry.file_name();
            let meta = entry.file_type().map_err(|e| io(entry.path(), e))?;
            if !meta.is_file()
                || !matches!(
                    name.to_str(),
                    Some("record.json" | "previous" | "payload" | "prior-state.json" | "restore")
                )
            {
                return Err(Error::Update("暂存目录含未知文件；保留现场"));
            }
        }
        fs::remove_dir_all(&path).map_err(|e| io(&path, e))?;
        sync_parent(&path)
    }
    fn prior(&self, state: &Journal) -> Result<Option<Journal>> {
        let prior: Option<Journal> = read(&self.generation(state)?.join("prior-state.json"))?;
        if let Some(prior) = &prior {
            self.basic(prior)?;
            if prior.phase != Phase::Committed
                || prior.generation == state.generation
                || prior.installed_sha256 != state.previous_sha256
            {
                return Err(Error::Update("前次更新归属无效"));
            }
        }
        Ok(prior)
    }
    pub(super) fn abort_prepared(&self, state: &Journal) -> Result<()> {
        self.backup(state)?;
        if fingerprint(&self.target)? != (state.previous_sha256.clone(), state.previous_size) {
            return Err(Error::UpdatePending);
        }
        match self.prior(state)? {
            Some(prior) => self.save(&prior)?,
            None => self.storage.remove(&self.storage.path("state.json")?)?,
        }
        self.cleanup_generation(state)
    }
    pub(super) fn prune_previous(&self, state: &Journal, previous: &Option<Journal>) -> Result<()> {
        if let Some(previous) = previous {
            self.cleanup_generation(previous)?;
        }
        self.storage
            .remove(&self.generation(state)?.join("prior-state.json"))
    }
}
pub(super) fn reject_pending(target: &Path) -> Result<()> {
    if !area(target).1.try_exists().map_err(|e| io(target, e))? {
        return Ok(());
    }
    if Store::open(target, false)?
        .load()?
        .is_some_and(|state| state.phase != Phase::Committed)
    {
        return Err(Error::UpdatePending);
    }
    Ok(())
}
pub(super) fn status(target: &Path) -> Result<serde_json::Value> {
    let target = canonical_target(target)?;
    let root = area(&target).1;
    if !root.try_exists().map_err(|e| io(&root, e))? {
        return Ok(
            serde_json::json!({"snapshot":"read_only_non_atomic","target":target,"state":"unmanaged","rollback_available":false}),
        );
    }
    let store = Store::open(&target, false)?;
    let state = store.load()?;
    let mut staging = 0;
    for entry in fs::read_dir(store.root()).map_err(|e| io(store.root(), e))? {
        let entry = entry.map_err(|e| io(store.root(), e))?;
        if entry.file_name().to_str().is_some_and(|name| {
            name.starts_with("txn-") && state.as_ref().is_none_or(|state| name != state.generation)
        }) {
            let kind = entry.file_type().map_err(|e| io(entry.path(), e))?;
            if !kind.is_dir() {
                return Err(Error::UnsafePath(entry.path()));
            }
            staging += 1;
        }
    }
    let Some(state) = state else {
        return Ok(
            serde_json::json!({"snapshot":"read_only_non_atomic","target":target,"state":"unmanaged","rollback_available":false,"staging_generations":staging}),
        );
    };
    let actual = fingerprint(&target)?;
    let current = if actual == (state.installed_sha256.clone(), state.installed_size) {
        "installed"
    } else if actual == (state.previous_sha256.clone(), state.previous_size) {
        "previous"
    } else {
        "changed"
    };
    Ok(
        serde_json::json!({"snapshot":"read_only_non_atomic","target":target,"state":state.phase,"current":current,"previous_version":state.previous_version,"installed_version":state.installed_version,
        "rollback_available":store.backup(&state).is_ok() && current!="changed","cleanup_pending":store.generation(&state)?.join("prior-state.json").exists(),"staging_generations":staging}),
    )
}
pub(super) fn rollback(target: &Path) -> Result<InstallResult> {
    let target = canonical_target(target)?;
    let store = Store::open(&target, false)?;
    let _guard = store.lock()?;
    let mut state = store.load()?.ok_or(Error::Update("没有可回滚的原生更新"))?;
    let previous = store.backup(&state)?;
    let actual = fingerprint(&target)?;
    if actual != (state.installed_sha256.clone(), state.installed_size)
        && actual != (state.previous_sha256.clone(), state.previous_size)
    {
        return Err(Error::Update("安装文件已变化；拒绝覆盖回滚"));
    }
    state.phase = Phase::RollingBack;
    store.save(&state)?;
    if actual != (state.previous_sha256.clone(), state.previous_size) {
        let restore = store.generation(&state)?.join("restore");
        copy_sync(&previous, &restore)?;
        if fingerprint(&restore)? != (state.previous_sha256.clone(), state.previous_size) {
            return Err(Error::Update("回滚复制校验失败；保留更新记录"));
        }
        fs::rename(&restore, &target).map_err(|e| io(&target, e))?;
    }
    sync_parent(&target).map_err(|_| Error::UpdatePending)?;
    let prior = store.prior(&state)?;
    // Rollback consumes the one retained version; it never chains into an older update.
    store
        .storage
        .remove(&store.storage.path("state.json")?)
        .map_err(|_| Error::UpdatePending)?;
    let cleanup_pending =
        store.prune_previous(&state, &prior).is_err() || store.cleanup_generation(&state).is_err();
    Ok(InstallResult {
        status: "rolled_back",
        version: state.previous_version,
        cleanup_pending,
    })
}
