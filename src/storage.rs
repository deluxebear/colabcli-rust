use crate::{
    Error, Result,
    error::io,
    models::{ProfileId, SessionState, Sessions},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
    lock_timeout: Duration,
}
impl Storage {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root).map_err(|e| io(root, e))?;
        let root = fs::canonicalize(root).map_err(|e| io(root, e))?;
        private_dir(&root)?;
        Ok(Self {
            root,
            lock_timeout: Duration::from_secs(5),
        })
    }
    /// Open an existing root without creating directories or changing permissions.
    /// Callers such as diagnostics use only read/path operations on this handle.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(root.as_ref()).map_err(|e| io(root.as_ref(), e))?;
        if !root.is_dir() {
            return Err(Error::UnsafePath(root));
        }
        Ok(Self {
            root,
            lock_timeout: Duration::from_secs(5),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }
    pub fn path(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::UnsafePath(relative.into()));
        }
        let mut path = self.root.clone();
        for part in relative.components() {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => return Err(Error::UnsafePath(path)),
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(io(&path, e)),
            }
        }
        Ok(path)
    }
    pub fn profile_path(&self, id: &ProfileId, file: &str) -> Result<PathBuf> {
        self.path(Path::new("accounts").join(id.as_str()).join(file))
    }
    /// Stable lock files live outside profile directories, so rename cannot replace the lock inode.
    pub fn profile_lock(&self, id: &ProfileId) -> Result<FileLock> {
        self.lock(&self.path(Path::new("locks").join(format!("{id}.lifecycle.lock")))?)
    }
    pub fn ensure_parent(&self, path: &Path) -> Result<()> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| Error::UnsafePath(path.into()))?;
        self.path(relative)?;
        let parent = path
            .parent()
            .ok_or_else(|| Error::UnsafePath(path.into()))?;
        let mut dir = self.root.clone();
        for component in parent.strip_prefix(&self.root).unwrap().components() {
            dir.push(component);
            private_dir(&dir)?;
        }
        Ok(())
    }
    pub fn read<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        self.check(path)?;
        match fs::read(path) {
            Ok(mut data) => {
                let result =
                    serde_json::from_slice(&data).map_err(|_| Error::InvalidData(path.into()));
                use zeroize::Zeroize;
                data.zeroize();
                result.map(Some)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(path, e)),
        }
    }
    pub fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        self.check(path)?;
        self.ensure_parent(path)?;
        let mut data =
            serde_json::to_vec_pretty(value).map_err(|_| Error::InvalidData(path.into()))?;
        let result = (|| {
            let parent = path.parent().unwrap();
            let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| io(parent, e))?;
            private_file(temp.path())?;
            temp.write_all(&data).map_err(|e| io(path, e))?;
            temp.write_all(b"\n").map_err(|e| io(path, e))?;
            temp.as_file().sync_all().map_err(|e| io(path, e))?;
            temp.persist(path).map_err(|e| io(path, e.error))?;
            #[cfg(unix)]
            File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| io(parent, e))?;
            Ok(())
        })();
        use zeroize::Zeroize;
        data.zeroize();
        result
    }
    pub fn remove(&self, path: &Path) -> Result<()> {
        self.check(path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(path, e)),
        }
    }
    fn check(&self, path: &Path) -> Result<()> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| Error::UnsafePath(path.into()))?;
        self.path(rel).map(|_| ())
    }
    pub fn lock(&self, path: &Path) -> Result<FileLock> {
        self.check(path)?;
        self.ensure_parent(path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|e| io(path, e))?;
        private_file(path)?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(FileLock(file, path.to_owned())),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(Error::LockTimeout(path.into()));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(io(path, e)),
            }
        }
    }
    pub fn sessions(&self, id: &ProfileId) -> Result<Sessions> {
        Ok(self
            .read(&self.profile_path(id, "sessions.json")?)?
            .unwrap_or_default())
    }
    pub fn settings(&self) -> Result<crate::models::Settings> {
        Ok(self.read(&self.path("settings.json")?)?.unwrap_or_default())
    }
    pub fn update_settings(
        &self,
        change: impl FnOnce(&mut crate::models::Settings) -> Result<()>,
    ) -> Result<()> {
        let _lock = self.lock(&self.path("settings.json.rust.lock")?)?;
        let mut settings = self.settings()?;
        change(&mut settings)?;
        self.write(&self.path("settings.json")?, &settings)
    }
    pub fn save_session(&self, id: &ProfileId, session: SessionState) -> Result<()> {
        let _lifecycle = self.profile_lock(id)?;
        if self.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let registry: crate::models::Registry = self
            .read(&self.path("accounts.json")?)?
            .ok_or_else(|| Error::AccountMissing(id.to_string()))?;
        registry.validate()?;
        if !registry.accounts.contains_key(id) {
            return Err(Error::AccountMissing(id.to_string()));
        }
        let path = self.profile_path(id, "sessions.json")?;
        let _lock = self.lock(&self.profile_path(id, "sessions.json.rust.lock")?)?;
        let mut sessions = self.sessions(id)?;
        sessions.insert(session.name.clone(), session);
        self.write(&path, &sessions)
    }
}
pub struct FileLock(File, PathBuf);
impl FileLock {
    pub(crate) fn protects_profile(&self, storage: &Storage, id: &ProfileId) -> Result<bool> {
        Ok(self.1 == storage.path(Path::new("locks").join(format!("{id}.lifecycle.lock")))?)
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn private_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(Error::UnsafePath(path.into()));
        }
    }
    fs::create_dir_all(path).map_err(|e| io(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| io(path, e))?;
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| io(path, e))?;
    }
    Ok(())
}
