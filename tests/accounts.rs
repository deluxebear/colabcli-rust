mod common;
use colabcli_rust::{
    Error,
    accounts::lookup,
    context::{self, AccountSource, Environment},
    credentials::{CredentialBackend, TokenStorageMode, TokenStore},
    models::ProfileId,
};
use common::*;
use serde_json::json;
use std::{fs, sync::atomic::Ordering};

#[test]
fn profile_names_enforce_documented_rules() {
    for name in ["", "..", "a..b", "/tmp", "x/y", "x\\y", "中文", "-bad"] {
        assert!(ProfileId::new(name).is_err(), "{name}");
    }
    assert!(ProfileId::new("a".repeat(65)).is_err());
    for name in ["default", "Work.Test-1", "a@b.com", "a_b"] {
        assert!(ProfileId::new(name).is_ok());
    }
}
#[test]
fn email_lookup_is_case_insensitive_but_duplicate_emails_are_ambiguous() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts
        .add(id("work"), account("Test@Example.invalid"))
        .unwrap();
    assert_eq!(
        lookup(&f.accounts.registry().unwrap(), " test@example.invalid ").unwrap(),
        id("work")
    );
    f.accounts
        .add(id("other"), account("test@example.invalid"))
        .unwrap();
    assert!(matches!(
        lookup(&f.accounts.registry().unwrap(), "TEST@example.invalid"),
        Err(Error::Ambiguous(_))
    ));
    assert_eq!(
        lookup(&f.accounts.registry().unwrap(), "work").unwrap(),
        id("work")
    );
}
#[test]
fn account_resolution_honors_flag_env_session_and_active() {
    let f = Fixture::new(TokenStorageMode::File);
    let work = f.add("work");
    let other = f.add("other");
    f.storage.save_session(&work, session("s")).unwrap();
    let env = Environment {
        account: Some("other".into()),
        ..Environment::default()
    };
    let selected = context::resolve(&f.accounts, &env, Some("default"), Some("s"), true).unwrap();
    assert_eq!(selected.source, AccountSource::Flag);
    assert_eq!(
        context::resolve(&f.accounts, &env, None, Some("s"), true)
            .unwrap()
            .profile,
        other
    );
    assert_eq!(
        context::resolve(&f.accounts, &Environment::default(), None, Some("s"), true)
            .unwrap()
            .profile,
        work
    );
    assert_eq!(
        context::resolve(&f.accounts, &Environment::default(), None, Some("s"), false)
            .unwrap()
            .profile,
        id("default")
    );
}
#[test]
fn active_same_name_wins_and_other_multiple_owners_are_rejected() {
    let f = Fixture::new(TokenStorageMode::File);
    let a = f.add("a");
    let b = f.add("b");
    for profile in [&a, &b] {
        f.storage.save_session(profile, session("s")).unwrap();
    }
    assert!(matches!(
        context::resolve(&f.accounts, &Environment::default(), None, Some("s"), true),
        Err(Error::Ambiguous(_))
    ));
    f.accounts.use_account("b").unwrap();
    assert_eq!(
        context::resolve(&f.accounts, &Environment::default(), None, Some("s"), true)
            .unwrap()
            .profile,
        b
    );
}
#[test]
fn drive_resolution_separates_consent_and_mount_with_all_priorities() {
    let f = Fixture::new(TokenStorageMode::File);
    let compute = f.add("compute");
    let drive = f.add("drive");
    let envdrive = f.add("envdrive");
    f.accounts.set_drive(None, Some(&drive)).unwrap();
    let env = Environment::default();
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, None, false).unwrap(),
        drive
    );
    f.storage
        .write(
            &f.storage
                .profile_path(&compute, "dfs_credentials.json")
                .unwrap(),
            &json!({}),
        )
        .unwrap();
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, None, false).unwrap(),
        compute
    );
    f.accounts.set_drive(Some(&compute), Some(&drive)).unwrap();
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, None, false).unwrap(),
        drive
    );
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, None, true).unwrap(),
        compute
    );
    let env = Environment {
        drive_account: Some(envdrive.to_string()),
        ..env
    };
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, None, false).unwrap(),
        envdrive
    );
    assert_eq!(
        context::resolve_drive(&f.accounts, &env, &compute, Some("drive"), false).unwrap(),
        drive
    );
}
#[test]
fn rename_migrates_tokens_pending_and_self_references() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let old = f.add("old");
    let new = id("new");
    f.tokens.save(&old, &token()).unwrap();
    f.accounts.use_account("old").unwrap();
    f.accounts.set_drive(Some(&old), Some(&old)).unwrap();
    f.accounts.set_drive(None, Some(&old)).unwrap();
    f.storage.save_session(&old, session("s")).unwrap();
    f.storage
        .write(
            &f.storage.profile_path(&old, "oauth_pending.json").unwrap(),
            &json!({"account":"old","test":true}),
        )
        .unwrap();
    f.accounts.rename(&old, &new).unwrap();
    let r = f.accounts.registry().unwrap();
    assert_eq!(r.active, new);
    assert_eq!(r.default_drive_account, Some(new.clone()));
    assert_eq!(r.accounts[&new].drive_account, Some(new.clone()));
    assert!(f.tokens.load(&new).unwrap().is_some());
    assert!(f.backend.get(&TokenStore::slot(&old)).unwrap().is_none());
    assert!(f.storage.sessions(&new).unwrap().contains_key("s"));
    let pending: serde_json::Value = f
        .storage
        .read(&f.storage.profile_path(&new, "oauth_pending.json").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(pending["account"], "new");
}
#[test]
fn interrupted_keyring_rename_is_recoverable_without_losing_source() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let old = f.add("old");
    f.tokens.save(&old, &token()).unwrap();
    f.backend.fail_set.store(true, Ordering::SeqCst);
    assert!(f.accounts.rename(&old, &id("new")).is_err());
    assert!(f.backend.get(&TokenStore::slot(&old)).unwrap().is_some());
    assert!(matches!(
        f.accounts.registry(),
        Err(Error::MigrationPending)
    ));
    f.backend.fail_set.store(false, Ordering::SeqCst);
    f.accounts.recover().unwrap();
    assert!(
        f.accounts
            .registry()
            .unwrap()
            .accounts
            .contains_key(&id("new"))
    );
    assert!(f.tokens.load(&id("new")).unwrap().is_some());
}
#[test]
fn removal_refuses_sessions_and_managed_plans_and_does_not_stop_vms() {
    let f = Fixture::new(TokenStorageMode::File);
    let work = f.add("work");
    f.storage.save_session(&work, session("s")).unwrap();
    assert!(matches!(
        f.accounts.remove(&work, false),
        Err(Error::AccountInUse)
    ));
    f.accounts.remove(&work, true).unwrap();
    assert!(!f.accounts.registry().unwrap().accounts.contains_key(&work));
    let managed = f.add("managed");
    f.storage
        .write(
            &f.storage
                .profile_path(&managed, "managed/plan.json")
                .unwrap(),
            &json!({"desired_state":"running"}),
        )
        .unwrap();
    assert!(matches!(
        f.accounts.remove(&managed, true),
        Err(Error::AccountInUse)
    ));
}
#[test]
fn last_account_removal_does_not_reimport_legacy_data() {
    let f = Fixture::new(TokenStorageMode::File);
    fs::write(
        f.temp.path().join("sessions.json"),
        "{\"legacy\":{\"name\":\"legacy\",\"token\":\"t\",\"url\":\"u\",\"endpoint\":\"e\"}}",
    )
    .unwrap();
    f.accounts.remove(&id("default"), true).unwrap();
    f.accounts.bootstrap().unwrap();
    assert!(!f.accounts.registry().unwrap().accounts[&id("default")].migrated_legacy);
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
}
#[test]
fn auto_fallback_remains_authoritative_until_explicit_keyring_write() {
    let f = Fixture::new(TokenStorageMode::Auto);
    f.backend.fail_set.store(true, Ordering::SeqCst);
    f.tokens.save(&id("default"), &token()).unwrap();
    assert!(
        f.storage
            .profile_path(&id("default"), "oauth_token.json")
            .unwrap()
            .exists()
    );
    f.backend.fail_set.store(false, Ordering::SeqCst);
    let mut new = token();
    new.token = colabcli_rust::models::Secret::new("new-access");
    f.tokens.save(&id("default"), &new).unwrap();
    assert!(
        f.storage
            .profile_path(&id("default"), "oauth_token.json")
            .unwrap()
            .exists()
    );
    let explicit = TokenStore::new(
        f.storage.clone(),
        f.backend.clone(),
        TokenStorageMode::Keyring,
    );
    explicit.save(&id("default"), &new).unwrap();
    assert!(
        !f.storage
            .profile_path(&id("default"), "oauth_token.json")
            .unwrap()
            .exists()
    );
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .token
            .expose(),
        "new-access"
    );
}
#[test]
fn token_migration_preserves_legacy_slot_and_file() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let raw = colabcli_rust::models::Secret::new(serde_json::to_string(&token()).unwrap());
    f.backend.set("oauth_token", &raw).unwrap();
    assert!(f.tokens.migrate_legacy().unwrap());
    assert!(!f.tokens.migrate_legacy().unwrap());
    assert!(f.backend.get("oauth_token").unwrap().is_some());
    assert!(f.tokens.load(&id("default")).unwrap().is_some());
}
#[test]
fn keyring_cleanup_failure_is_visible_and_can_be_repaired() {
    let f = Fixture::new(TokenStorageMode::File);
    let work = f.add("work");
    f.backend
        .set(
            &TokenStore::slot(&work),
            &colabcli_rust::models::Secret::new("orphan"),
        )
        .unwrap();
    f.backend.fail_delete.store(true, Ordering::SeqCst);
    assert!(f.accounts.remove(&work, true).is_err());
    f.backend.fail_delete.store(false, Ordering::SeqCst);
    f.accounts.recover().unwrap();
    assert!(f.backend.get(&TokenStore::slot(&work)).unwrap().is_none());
}
#[test]
fn adc_path_selection_uses_snapshot_without_changing_environment() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts
        .update(|r| {
            r.accounts
                .get_mut(&id("default"))
                .unwrap()
                .adc_credentials_file = Some("/profile/adc.json".into());
            Ok(())
        })
        .unwrap();
    let before = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
    let env = Environment {
        adc_file: Some("/snapshot/adc.json".into()),
        ..Environment::default()
    };
    assert_eq!(
        context::resolve(&f.accounts, &env, None, None, false)
            .unwrap()
            .adc_file
            .unwrap(),
        std::path::PathBuf::from("/snapshot/adc.json")
    );
    assert_eq!(std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"), before);
}

#[test]
fn unavailable_keyring_never_becomes_missing_credentials_during_rename() {
    let f = Fixture::new(TokenStorageMode::Auto);
    let old = f.add("old");
    f.tokens.save(&old, &token()).unwrap();
    f.backend.fail_get.store(true, Ordering::SeqCst);
    assert!(f.accounts.rename(&old, &id("new")).is_err());
    f.backend.fail_get.store(false, Ordering::SeqCst);
    assert!(f.tokens.load(&old).unwrap().is_some());
    assert!(!f.storage.path("account-transaction.json").unwrap().exists());
    f.accounts.rename(&old, &id("new")).unwrap();
    assert!(f.tokens.load(&id("new")).unwrap().is_some());
}

#[test]
fn renaming_default_adopts_its_legacy_keyring_first() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    f.backend
        .set(
            "oauth_token",
            &colabcli_rust::models::Secret::new(serde_json::to_string(&token()).unwrap()),
        )
        .unwrap();
    f.accounts.rename(&id("default"), &id("personal")).unwrap();
    assert!(f.tokens.load(&id("personal")).unwrap().is_some());
    assert!(f.backend.get("oauth_token").unwrap().is_some());
}

#[test]
fn rename_recovery_preserves_credentials_when_storage_mode_changes() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let old = f.add("old");
    f.tokens.save(&old, &token()).unwrap();
    f.backend.fail_set.store(true, Ordering::SeqCst);
    assert!(f.accounts.rename(&old, &id("new")).is_err());
    f.backend.fail_set.store(false, Ordering::SeqCst);
    let tokens = TokenStore::new(f.storage.clone(), f.backend.clone(), TokenStorageMode::File);
    let accounts = colabcli_rust::accounts::Accounts::new(f.storage.clone(), tokens.clone());
    accounts.recover().unwrap();
    assert_eq!(
        tokens.load(&id("new")).unwrap().unwrap().token.expose(),
        "fixture-access"
    );
    assert!(f.backend.get(&TokenStore::slot(&old)).unwrap().is_none());
}

#[test]
fn case_only_profile_collisions_are_rejected_on_all_platforms() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("Work");
    assert!(matches!(
        f.accounts.add(id("work"), Default::default()),
        Err(Error::AccountExists(_))
    ));
    assert_eq!(f.accounts.registry().unwrap().accounts.len(), 2);
}
