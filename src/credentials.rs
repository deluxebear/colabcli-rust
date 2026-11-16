use crate::{
    Error, Result,
    auth::OAuthCredentials,
    models::{ProfileId, Secret},
    storage::Storage,
};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
#[cfg(all(
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
use std::{
    sync::{atomic::Ordering, mpsc},
    time::Instant,
};

pub trait CredentialBackend: Send + Sync {
    fn get(&self, username: &str) -> Result<Option<Secret>>;
    fn set(&self, username: &str, secret: &Secret) -> Result<()>;
    fn delete(&self, username: &str) -> Result<()>;
}

/// Bounded caller wait. OS keyring operations may still finish after a timeout.
pub struct SystemKeyring {
    timeout: Duration,
    busy: Arc<AtomicBool>,
}
impl Default for SystemKeyring {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl SystemKeyring {
    pub fn with_timeout(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(Error::CredentialBackend("等待时间必须在 1–300 秒内"));
        }
        Ok(Self {
            timeout,
            busy: Arc::new(AtomicBool::new(false)),
        })
    }
}
#[cfg(all(
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
impl SystemKeyring {
    fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let deadline = Instant::now() + self.timeout;
        while self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            if Instant::now() >= deadline {
                return Err(Error::CredentialTimeout(self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let busy = self.busy.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = action();
            busy.store(false, Ordering::Release);
            let _ = tx.send(result);
        });
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| Error::CredentialTimeout(self.timeout.as_secs()))?
    }
}
#[cfg(all(
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
impl CredentialBackend for SystemKeyring {
    fn get(&self, username: &str) -> Result<Option<Secret>> {
        let username = username.to_owned();
        self.run(move || {
            let entry =
                keyring::Entry::new("google-colab-cli", &username).map_err(keyring_error)?;
            match entry.get_password() {
                Ok(value) => Ok(Some(Secret::new(value))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(keyring_error(error)),
            }
        })
    }
    fn set(&self, username: &str, secret: &Secret) -> Result<()> {
        let (username, secret) = (username.to_owned(), secret.clone());
        self.run(move || {
            keyring::Entry::new("google-colab-cli", &username)
                .and_then(|e| e.set_password(secret.expose()))
                .map_err(keyring_error)
        })
    }
    fn delete(&self, username: &str) -> Result<()> {
        let username = username.to_owned();
        self.run(move || {
            let entry =
                keyring::Entry::new("google-colab-cli", &username).map_err(keyring_error)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keyring_error(error)),
            }
        })
    }
}
#[cfg(all(
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
fn keyring_error(error: keyring::Error) -> Error {
    // Do not format keyring errors: BadEncoding/Ambiguous may contain credential material.
    let reason = match error {
        keyring::Error::NoStorageAccess(_) => "系统凭据库未解锁或拒绝访问",
        keyring::Error::PlatformFailure(_) => "操作系统凭据服务返回错误",
        keyring::Error::Ambiguous(_) => "凭据条目不唯一",
        keyring::Error::BadEncoding(_) => "凭据编码无效",
        _ => "凭据库请求失败",
    };
    Error::CredentialBackend(reason)
}
#[cfg(not(all(
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
)))]
impl CredentialBackend for SystemKeyring {
    fn get(&self, _: &str) -> Result<Option<Secret>> {
        let _ = (&self.timeout, &self.busy);
        Err(Error::CredentialBackend(
            "此构建未启用原生凭据库；请使用 --token-storage file 登录并运行命令",
        ))
    }
    fn set(&self, _: &str, _: &Secret) -> Result<()> {
        Err(Error::CredentialBackend("此构建未启用原生凭据库"))
    }
    fn delete(&self, _: &str) -> Result<()> {
        Err(Error::CredentialBackend("此构建未启用原生凭据库"))
    }
}

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum, PartialEq, Eq)]
pub enum TokenStorageMode {
    #[default]
    Auto,
    Keyring,
    File,
}

#[derive(Clone)]
pub struct TokenStore {
    storage: Storage,
    backend: Arc<dyn CredentialBackend>,
    mode: TokenStorageMode,
}
impl TokenStore {
    pub fn new(
        storage: Storage,
        backend: Arc<dyn CredentialBackend>,
        mode: TokenStorageMode,
    ) -> Self {
        Self {
            storage,
            backend,
            mode,
        }
    }
    pub fn slot(id: &ProfileId) -> String {
        format!("oauth_token:{id}")
    }
    pub fn load(&self, id: &ProfileId) -> Result<Option<OAuthCredentials>> {
        let file = self.storage.profile_path(id, "oauth_token.json")?;
        // A fallback file is authoritative until a successful keyring write removes it.
        if self.mode != TokenStorageMode::Keyring {
            if let Some(value) = self.storage.read(&file)? {
                return Ok(Some(value));
            }
            if self.mode == TokenStorageMode::File {
                return Ok(None);
            }
        }
        match self.backend.get(&Self::slot(id)) {
            Ok(Some(raw)) => serde_json::from_str(raw.expose())
                .map(Some)
                .map_err(|_| Error::InvalidData(file)),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Lifecycle recovery must inspect both backends even if the CLI mode changed after a crash.
    pub fn load_for_migration(&self, id: &ProfileId) -> Result<Option<OAuthCredentials>> {
        let file = self.storage.profile_path(id, "oauth_token.json")?;
        if let Some(value) = self.storage.read(&file)? {
            return Ok(Some(value));
        }
        match self.backend.get(&Self::slot(id))? {
            Some(raw) => serde_json::from_str(raw.expose())
                .map(Some)
                .map_err(|_| Error::InvalidData(file)),
            None => Ok(None),
        }
    }
    pub fn save(&self, id: &ProfileId, token: &OAuthCredentials) -> Result<()> {
        let _lock = self
            .storage
            .lock(&self.storage.profile_path(id, "oauth_token.rust.lock")?)?;
        let path = self.storage.profile_path(id, "oauth_token.json")?;
        let fallback = self
            .storage
            .profile_path(id, "oauth_keyring_fallback.json")?;
        let sticky_fallback = self.mode == TokenStorageMode::Auto && fallback.exists();
        if self.mode != TokenStorageMode::File && !sticky_fallback {
            let raw = Secret::new(
                serde_json::to_string(token).map_err(|_| Error::InvalidData(path.clone()))?,
            );
            match self.backend.set(&Self::slot(id), &raw) {
                Ok(()) => {
                    self.storage.remove(&fallback)?;
                    return self.storage.remove(&path);
                }
                Err(e) if self.mode == TokenStorageMode::Keyring => return Err(e),
                Err(_) => {
                    // A timed-out OS write may finish later. Auto must keep its file authoritative
                    // across processes until the user explicitly chooses a keyring write.
                    self.storage
                        .write(&fallback, &serde_json::json!({"file_authoritative":true}))?;
                }
            }
        }
        self.storage.write(&path, token)
    }
    /// Clear both backends even in file mode. Fail visibly if keyring cleanup is unknown.
    pub fn delete(&self, id: &ProfileId) -> Result<()> {
        self.backend.delete(&Self::slot(id))?;
        self.storage
            .remove(&self.storage.profile_path(id, "oauth_token.json")?)
    }
    pub fn migrate_legacy(&self) -> Result<bool> {
        let id = ProfileId::default_profile();
        if self.load(&id)?.is_some() {
            return Ok(false);
        }
        let old = self.storage.path("token.json")?;
        let token = if let Some(token) = self.storage.read::<OAuthCredentials>(&old)? {
            Some(token)
        } else if self.mode == TokenStorageMode::File {
            None
        } else {
            match self.backend.get("oauth_token") {
                Ok(Some(raw)) => {
                    Some(serde_json::from_str(raw.expose()).map_err(|_| Error::InvalidData(old))?)
                }
                Ok(None) => None,
                Err(e) if self.mode == TokenStorageMode::Keyring => return Err(e),
                Err(_) => None,
            }
        };
        if let Some(token) = token {
            self.save(&id, &token)?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(all(
    test,
    feature = "native-keyring",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use super::*;
    #[test]
    fn timed_out_worker_blocks_new_operations_until_it_finishes() {
        let backend = SystemKeyring {
            timeout: Duration::from_millis(20),
            busy: Arc::new(AtomicBool::new(false)),
        };
        let (release_tx, release_rx) = mpsc::channel();
        assert!(matches!(
            backend.run(move || {
                release_rx.recv().unwrap();
                Ok(())
            }),
            Err(Error::CredentialTimeout(_))
        ));
        assert!(
            backend
                .run(|| -> Result<()> { panic!("must not start while old operation is pending") })
                .is_err()
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while backend.busy.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(backend.run(|| Ok(42)).unwrap(), 42);
    }
    #[test]
    fn native_error_mapping_never_formats_secret_bytes_or_ambiguous_credentials() {
        let error = keyring_error(keyring::Error::BadEncoding(
            b"fixture-sensitive-value".to_vec(),
        ));
        assert!(matches!(error, Error::CredentialBackend("凭据编码无效")));
        assert!(!error.to_string().contains("fixture-sensitive-value"));
        assert!(matches!(
            keyring_error(keyring::Error::Ambiguous(Vec::new())),
            Error::CredentialBackend("凭据条目不唯一")
        ));
    }
}
