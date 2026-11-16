use crate::{
    Error, Result,
    credentials::TokenStore,
    error::io,
    models::{Account, ProfileId, Registry},
    storage::Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Accounts {
    pub storage: Storage,
    pub tokens: TokenStore,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation")]
enum Transaction {
    Rename {
        old: ProfileId,
        new: ProfileId,
        after: Registry,
    },
    Remove {
        id: ProfileId,
        after: Registry,
    },
}
impl Accounts {
    pub fn new(storage: Storage, tokens: TokenStore) -> Self {
        Self { storage, tokens }
    }
    fn registry_path(&self) -> Result<PathBuf> {
        self.storage.path("accounts.json")
    }
    fn journal_path(&self) -> Result<PathBuf> {
        self.storage.path("account-transaction.json")
    }
    fn lock(&self) -> Result<crate::storage::FileLock> {
        self.storage.lock(&self.storage.path("accounts.json.lock")?)
    }
    fn load_raw(&self) -> Result<Registry> {
        let registry: Registry = self
            .storage
            .read(&self.registry_path()?)?
            .unwrap_or_default();
        registry.validate()?;
        Ok(registry)
    }
    pub fn registry(&self) -> Result<Registry> {
        if self.journal_path()?.exists() {
            return Err(Error::MigrationPending);
        }
        self.load_raw()
    }
    pub fn bootstrap(&self) -> Result<()> {
        let _lock = self.lock()?;
        if self.journal_path()?.exists() {
            return Err(Error::MigrationPending);
        }
        let path = self.registry_path()?;
        let registry = self.load_raw()?;
        let id = ProfileId::default_profile();
        if registry
            .accounts
            .get(&id)
            .is_some_and(|a| a.migrated_legacy)
        {
            let _lifecycle = self.storage.profile_lock(&id)?;
            for name in [
                "sessions.json",
                "history",
                "dfs_credentials.json",
                "token.json",
            ] {
                let source = self.storage.path(name)?;
                let dest_name = if name == "token.json" {
                    "oauth_token.json"
                } else {
                    name
                };
                let destination = self.storage.profile_path(&id, dest_name)?;
                if source.exists() {
                    self.copy_missing(&source, &destination)?;
                }
            }
        }
        if !path.exists() {
            self.storage.write(&path, &registry)?;
        }
        Ok(())
    }
    fn copy_missing(&self, source: &Path, destination: &Path) -> Result<()> {
        if fs::symlink_metadata(source)
            .map_err(|e| io(source, e))?
            .file_type()
            .is_symlink()
        {
            return Err(Error::UnsafePath(source.into()));
        }
        if source.is_dir() {
            self.storage
                .ensure_parent(&destination.join(".placeholder"))?;
            for entry in fs::read_dir(source).map_err(|e| io(source, e))? {
                let entry = entry.map_err(|e| io(source, e))?;
                self.copy_missing(&entry.path(), &destination.join(entry.file_name()))?;
            }
        } else if !fs::metadata(source).map_err(|e| io(source, e))?.is_file() {
            return Err(Error::UnsafePath(source.into()));
        } else if !destination.exists() {
            // Legacy inputs are JSON/JSONL, preserve exact bytes using an owner-only temp file.
            self.storage.ensure_parent(destination)?;
            let mut temp = tempfile::NamedTempFile::new_in(destination.parent().unwrap())
                .map_err(|e| io(destination, e))?;
            let mut input = fs::File::open(source).map_err(|e| io(source, e))?;
            std::io::copy(&mut input, &mut temp).map_err(|e| io(destination, e))?;
            temp.as_file().sync_all().map_err(|e| io(destination, e))?;
            temp.persist_noclobber(destination)
                .map_err(|e| io(destination, e.error))?;
        }
        Ok(())
    }
    pub fn add(&self, id: ProfileId, account: Account) -> Result<()> {
        self.update(|r| {
            if r.accounts
                .keys()
                .any(|existing| existing.as_str().eq_ignore_ascii_case(id.as_str()))
            {
                return Err(Error::AccountExists(id.to_string()));
            }
            r.accounts.insert(id, account);
            Ok(())
        })
    }
    pub fn use_account(&self, selector: &str) -> Result<()> {
        self.update(|r| {
            r.active = lookup(r, selector)?;
            Ok(())
        })
    }
    pub fn set_drive(&self, compute: Option<&ProfileId>, drive: Option<&ProfileId>) -> Result<()> {
        self.update(|r| {
            if let Some(d) = drive {
                if !r.accounts.contains_key(d) {
                    return Err(Error::AccountMissing(d.to_string()));
                }
            }
            match compute {
                Some(id) => {
                    r.accounts
                        .get_mut(id)
                        .ok_or_else(|| Error::AccountMissing(id.to_string()))?
                        .drive_account = drive.cloned()
                }
                None => r.default_drive_account = drive.cloned(),
            }
            Ok(())
        })
    }
    pub fn update(&self, change: impl FnOnce(&mut Registry) -> Result<()>) -> Result<()> {
        let _lock = self.lock()?;
        if self.journal_path()?.exists() {
            return Err(Error::MigrationPending);
        }
        let mut r = self.load_raw()?;
        change(&mut r)?;
        r.validate()?;
        self.storage.write(&self.registry_path()?, &r)
    }
    pub fn rename(&self, old: &ProfileId, new: &ProfileId) -> Result<()> {
        if old == new {
            return if self.registry()?.accounts.contains_key(old) {
                Ok(())
            } else {
                Err(Error::AccountMissing(old.to_string()))
            };
        }
        let _lock = self.lock()?;
        if self.journal_path()?.exists() {
            return Err(Error::MigrationPending);
        }
        let mut r = self.load_raw()?;
        if r.accounts.contains_key(new)
            || self
                .storage
                .path(Path::new("accounts").join(new.as_str()))?
                .exists()
        {
            return Err(Error::AccountExists(new.to_string()));
        }
        let _old_lifecycle = self.storage.profile_lock(old)?;
        let _new_lifecycle = self.storage.profile_lock(new)?;
        let account = r
            .accounts
            .remove(old)
            .ok_or_else(|| Error::AccountMissing(old.to_string()))?;
        if self.managed(old)? {
            return Err(Error::AccountInUse);
        }
        if self.tokens.load_for_migration(new)?.is_some() {
            return Err(Error::AccountExists(new.to_string()));
        }
        if old == &ProfileId::default_profile() && account.migrated_legacy {
            self.tokens.migrate_legacy()?;
        }
        r.accounts.insert(new.clone(), account);
        for a in r.accounts.values_mut() {
            if a.drive_account.as_ref() == Some(old) {
                a.drive_account = Some(new.clone());
            }
        }
        if &r.active == old {
            r.active = new.clone();
        }
        if r.default_drive_account.as_ref() == Some(old) {
            r.default_drive_account = Some(new.clone());
        }
        r.validate()?;
        self.storage.write(
            &self.journal_path()?,
            &Transaction::Rename {
                old: old.clone(),
                new: new.clone(),
                after: r,
            },
        )?;
        self.recover_locked()
    }
    pub fn remove(&self, id: &ProfileId, force: bool) -> Result<()> {
        let _lock = self.lock()?;
        let _lifecycle = self.storage.profile_lock(id)?;
        if self.journal_path()?.exists() {
            return Err(Error::MigrationPending);
        }
        let mut r = self.load_raw()?;
        if !r.accounts.contains_key(id) {
            return Err(Error::AccountMissing(id.to_string()));
        }
        if self.managed(id)? || (!force && !self.storage.sessions(id)?.is_empty()) {
            return Err(Error::AccountInUse);
        }
        r.accounts.remove(id);
        for a in r.accounts.values_mut() {
            if a.drive_account.as_ref() == Some(id) {
                a.drive_account = None;
            }
        }
        if r.default_drive_account.as_ref() == Some(id) {
            r.default_drive_account = None;
        }
        if r.accounts.is_empty() {
            r.active = ProfileId::default_profile();
            r.accounts.insert(r.active.clone(), Account::default());
        } else if &r.active == id {
            r.active = r.accounts.keys().next().unwrap().clone();
        }
        self.storage.write(
            &self.journal_path()?,
            &Transaction::Remove {
                id: id.clone(),
                after: r,
            },
        )?;
        self.recover_locked()
    }
    fn managed(&self, id: &ProfileId) -> Result<bool> {
        if self
            .storage
            .profile_path(id, "control_operation.json")?
            .exists()
        {
            return Ok(true);
        }
        let runs = self.storage.profile_path(id, "runs")?;
        if runs.exists()
            && fs::read_dir(&runs)
                .map_err(|e| io(&runs, e))?
                .next()
                .is_some()
        {
            return Ok(true);
        }
        let path = self.storage.profile_path(id, "managed")?;
        if !path.exists() {
            return Ok(false);
        }
        Ok(fs::read_dir(&path)
            .map_err(|e| io(&path, e))?
            .next()
            .is_some())
    }
    pub fn recover(&self) -> Result<()> {
        let _lock = self.lock()?;
        let transaction = self.storage.read::<Transaction>(&self.journal_path()?)?;
        let mut guards = Vec::new();
        match transaction {
            Some(Transaction::Rename { old, new, .. }) => {
                guards.push(self.storage.profile_lock(&old)?);
                guards.push(self.storage.profile_lock(&new)?);
            }
            Some(Transaction::Remove { id, .. }) => guards.push(self.storage.profile_lock(&id)?),
            None => (),
        }
        self.recover_locked()
    }
    fn recover_locked(&self) -> Result<()> {
        let Some(transaction) = self.storage.read::<Transaction>(&self.journal_path()?)? else {
            return Ok(());
        };
        match transaction {
            Transaction::Rename { old, new, after } => {
                after.validate()?;
                let token = match self.tokens.load_for_migration(&old)? {
                    Some(token) => Some(token),
                    None => self.tokens.load_for_migration(&new)?,
                };
                if let Some(token) = token {
                    self.tokens.save(&new, &token)?;
                }
                let source = self
                    .storage
                    .path(Path::new("accounts").join(old.as_str()))?;
                let destination = self
                    .storage
                    .path(Path::new("accounts").join(new.as_str()))?;
                if source.exists() {
                    // save(new) may have created a token file; merge it without overwriting source data.
                    self.copy_missing(&source, &destination)?;
                }
                for name in ["oauth_pending.json", "dfs_pending.json"] {
                    let path = self.storage.profile_path(&new, name)?;
                    if let Some(mut pending) = self.storage.read::<Value>(&path)? {
                        if let Some(obj) = pending.as_object_mut() {
                            obj.insert("account".into(), Value::String(new.to_string()));
                        }
                        self.storage.write(&path, &pending)?;
                    }
                }
                self.storage.write(&self.registry_path()?, &after)?;
                self.tokens.delete(&old)?;
                if source.exists() {
                    fs::remove_dir_all(&source).map_err(|e| io(&source, e))?;
                }
            }
            Transaction::Remove { id, after } => {
                after.validate()?;
                self.tokens.delete(&id)?;
                let dir = self.storage.path(Path::new("accounts").join(id.as_str()))?;
                if dir.exists() {
                    fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
                }
                self.storage.write(&self.registry_path()?, &after)?;
            }
        }
        self.storage.remove(&self.journal_path()?)
    }
}

pub fn lookup(registry: &Registry, selector: &str) -> Result<ProfileId> {
    if let Some(id) = registry.accounts.keys().find(|id| id.as_str() == selector) {
        return Ok(id.clone());
    }
    let mut matches = registry
        .accounts
        .iter()
        .filter(|(_, a)| {
            a.email
                .as_ref()
                .is_some_and(|email| email.trim().eq_ignore_ascii_case(selector.trim()))
        })
        .map(|(id, _)| id.clone());
    let first = matches
        .next()
        .ok_or_else(|| Error::AccountMissing(selector.into()))?;
    if matches.next().is_some() {
        return Err(Error::Ambiguous(selector.into()));
    }
    Ok(first)
}
