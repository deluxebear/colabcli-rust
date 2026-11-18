mod common;
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    credentials::{CredentialBackend, TokenStorageMode, TokenStore},
    models::Secret,
};
use common::*;
use serde_json::json;
use std::sync::Arc;

struct ForbiddenBackend;
impl CredentialBackend for ForbiddenBackend {
    fn get(&self, _: &str) -> Result<Option<Secret>> {
        panic!("file lifecycle read the keyring")
    }
    fn set(&self, _: &str, _: &Secret) -> Result<()> {
        panic!("file lifecycle wrote the keyring")
    }
    fn delete(&self, _: &str) -> Result<()> {
        panic!("file lifecycle deleted from the keyring")
    }
}

#[test]
fn file_rename_and_remove_never_call_native_backend_or_change_other_accounts() {
    let f = Fixture::new(TokenStorageMode::File);
    let old = f.add("old");
    let other = f.add("other");
    f.tokens.save(&old, &token()).unwrap();
    f.tokens.save(&other, &token()).unwrap();
    let other_file = f.storage.profile_path(&other, "oauth_token.json").unwrap();
    let before = std::fs::read(&other_file).unwrap();
    let tokens = TokenStore::new(
        f.storage.clone(),
        Arc::new(ForbiddenBackend),
        TokenStorageMode::File,
    );
    let accounts = Accounts::new(f.storage.clone(), tokens.clone());
    accounts.rename(&old, &id("new")).unwrap();
    assert_eq!(
        tokens.load(&id("new")).unwrap().unwrap().token.expose(),
        "fixture-access"
    );
    accounts.remove(&id("new"), true).unwrap();
    assert_eq!(std::fs::read(other_file).unwrap(), before);
    assert!(!f.storage.path("account-transaction.json").unwrap().exists());
}

#[test]
#[cfg(unix)]
fn file_rename_recovery_remains_file_only_after_storage_mode_changes() {
    let f = Fixture::new(TokenStorageMode::File);
    let old = f.add("old");
    f.tokens.save(&old, &token()).unwrap();
    let source = f.storage.profile_path(&old, "blocked").unwrap();
    std::os::unix::fs::symlink(f.temp.path().join("outside"), &source).unwrap();
    let tokens = TokenStore::new(
        f.storage.clone(),
        Arc::new(ForbiddenBackend),
        TokenStorageMode::File,
    );
    let accounts = Accounts::new(f.storage.clone(), tokens);
    assert!(matches!(
        accounts.rename(&old, &id("new")),
        Err(Error::UnsafePath(_))
    ));
    let journal: serde_json::Value = f
        .storage
        .read(&f.storage.path("account-transaction.json").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(journal["credentials"], "file");
    assert!(
        f.storage
            .profile_path(&old, "oauth_token.json")
            .unwrap()
            .exists()
    );
    std::fs::remove_file(source).unwrap();
    let tokens = TokenStore::new(
        f.storage.clone(),
        Arc::new(ForbiddenBackend),
        TokenStorageMode::Keyring,
    );
    let accounts = Accounts::new(f.storage.clone(), tokens);
    accounts.recover().unwrap();
    assert!(
        accounts
            .registry()
            .unwrap()
            .accounts
            .contains_key(&id("new"))
    );
    let recovered: colabcli_rust::auth::OAuthCredentials = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("new"), "oauth_token.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(recovered.token.expose(), "fixture-access");
    assert!(!f.storage.path("account-transaction.json").unwrap().exists());
}

#[test]
fn old_journals_without_credential_policy_keep_cross_backend_cleanup() {
    let f = Fixture::new(TokenStorageMode::File);
    let old = f.add("old");
    let other = f.add("other");
    f.backend
        .set(&TokenStore::slot(&old), &Secret::new("old-secret"))
        .unwrap();
    f.backend
        .set(&TokenStore::slot(&other), &Secret::new("other-secret"))
        .unwrap();
    let mut after = f.accounts.registry().unwrap();
    after.accounts.shift_remove(&old);
    f.storage
        .write(
            &f.storage.path("account-transaction.json").unwrap(),
            &json!({"operation":"Remove","id":old,"after":after}),
        )
        .unwrap();
    f.accounts.recover().unwrap();
    assert!(f.backend.get(&TokenStore::slot(&old)).unwrap().is_none());
    assert_eq!(
        f.backend
            .get(&TokenStore::slot(&other))
            .unwrap()
            .unwrap()
            .expose(),
        "other-secret"
    );
}

#[test]
fn account_writes_preserve_order_and_remove_active_selects_first_remaining() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts.add(id("z-first"), Default::default()).unwrap();
    f.accounts.add(id("a-last"), Default::default()).unwrap();
    let mut registry = f.accounts.registry().unwrap();
    registry.accounts.move_index(1, 0);
    f.storage
        .write(&f.storage.path("accounts.json").unwrap(), &registry)
        .unwrap();
    f.accounts.use_account("z-first").unwrap();
    f.accounts.set_drive(None, Some(&id("z-first"))).unwrap();
    f.accounts.rename(&id("z-first"), &id("m-renamed")).unwrap();
    let registry = f.accounts.registry().unwrap();
    assert_eq!(
        registry
            .accounts
            .keys()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["m-renamed", "default", "a-last"]
    );
    assert_eq!(registry.default_drive_account, Some(id("m-renamed")));
    f.accounts.remove(&id("m-renamed"), true).unwrap();
    let registry = f.accounts.registry().unwrap();
    assert_eq!(registry.active, id("default"));
    assert_eq!(
        registry
            .accounts
            .keys()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["default", "a-last"]
    );
    assert!(registry.default_drive_account.is_none());
}
