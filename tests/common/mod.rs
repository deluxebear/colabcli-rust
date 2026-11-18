#![allow(dead_code)]
pub mod interrupted_stream;
pub mod service_account;
use chrono::{DateTime, Utc};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::OAuthCredentials,
    credentials::{CredentialBackend, TokenStorageMode, TokenStore},
    models::{Account, ProfileId, Secret, SessionState},
    storage::Storage,
};
use serde_json::{Map, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub struct MemoryBackend {
    pub values: Mutex<BTreeMap<String, Secret>>,
    pub fail_get: AtomicBool,
    pub fail_set: AtomicBool,
    pub fail_delete: AtomicBool,
}
impl CredentialBackend for MemoryBackend {
    fn get(&self, key: &str) -> Result<Option<Secret>> {
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(Error::CredentialUnavailable);
        }
        Ok(self.values.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> Result<()> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(Error::CredentialUnavailable);
        }
        self.values
            .lock()
            .unwrap()
            .insert(key.into(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(Error::CredentialUnavailable);
        }
        self.values.lock().unwrap().remove(key);
        Ok(())
    }
}
pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub storage: Storage,
    pub backend: Arc<MemoryBackend>,
    pub tokens: TokenStore,
    pub accounts: Accounts,
}
impl Fixture {
    pub fn new(mode: TokenStorageMode) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::new(temp.path()).unwrap();
        let backend = Arc::new(MemoryBackend::default());
        let tokens = TokenStore::new(storage.clone(), backend.clone(), mode);
        let accounts = Accounts::new(storage.clone(), tokens.clone());
        accounts.bootstrap().unwrap();
        Self {
            temp,
            storage,
            backend,
            tokens,
            accounts,
        }
    }
    pub fn add(&self, name: &str) -> ProfileId {
        let id = id(name);
        self.accounts.add(id.clone(), Account::default()).unwrap();
        id
    }
}
pub fn id(name: &str) -> ProfileId {
    ProfileId::new(name).unwrap()
}
pub fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
pub fn token() -> OAuthCredentials {
    serde_json::from_value(json!({"token":"fixture-access","refresh_token":"fixture-refresh","client_id":"fixture-client",
        "client_secret":"fixture-secret","expiry":"2026-11-16T00:00:00Z","scopes":["openid"]})).unwrap()
}
pub fn session(name: &str) -> SessionState {
    serde_json::from_value(json!({"name":name,"endpoint":format!("ep-{name}"),"token":"fixture-proxy","url":"https://runtime.invalid"})).unwrap()
}
pub fn account(email: &str) -> Account {
    Account {
        email: Some(email.into()),
        extra: Map::new(),
        ..Account::default()
    }
}
