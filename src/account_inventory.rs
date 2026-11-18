//! Offline account projections. Never open a native credential backend or refresh tokens.
use crate::{Cli, print_json};
use colabcli_rust::{
    Result,
    accounts::Accounts,
    context::{self, AccountSource, Environment},
    credentials::TokenStorageMode,
    models::{Account, AuthProvider, ProfileId},
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn auth(
    accounts: &Accounts,
    env: &Environment,
    cli: &Cli,
    id: &ProfileId,
    meta: &Account,
) -> Result<Value> {
    if meta.auth_provider == AuthProvider::Adc {
        let configured = meta
            .adc_credentials_file
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| env.adc_file.clone());
        let expand = |path: &std::path::Path| {
            path.strip_prefix("~")
                .ok()
                .and_then(|tail| env.home.as_ref().map(|home| home.join(tail)))
                .unwrap_or_else(|| path.to_path_buf())
        };
        if let Some(path) = configured.as_ref().filter(|path| expand(path).is_file()) {
            return Ok(json!({"provider":"adc","ready":true,"storage":"adc-file","detail":path}));
        }
        if meta.adc_credentials_file.is_none() {
            if let Some(path) = env
                .home
                .as_ref()
                .map(|home| home.join(".config/gcloud/application_default_credentials.json"))
                .filter(|path| path.is_file())
            {
                return Ok(
                    json!({"provider":"adc","ready":true,"storage":"gcloud-adc","detail":path}),
                );
            }
        }
        return Ok(
            json!({"provider":"adc","ready":false,"storage":"adc","detail":"missing ADC file"}),
        );
    }
    let path = accounts.storage.profile_path(id, "oauth_token.json")?;
    // This is only presence/readiness, as in the extension. It does not validate or use credentials.
    let present = accounts
        .storage
        .read::<Value>(&path)
        .ok()
        .flatten()
        .is_some_and(|value| {
            ["refresh_token", "token"].iter().any(|key| {
                value
                    .get(key)
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            })
        });
    Ok(if present {
        json!({"provider":"oauth2","ready":true,"storage":"file","detail":path})
    } else if cli.token_storage == TokenStorageMode::File {
        json!({"provider":"oauth2","ready":false,"storage":"none","detail":"no token file"})
    } else {
        // Native keyrings can prompt even for reads. Unknown is preferable to a false negative.
        json!({"provider":"oauth2","ready":null,"storage":"keyring (not inspected)","detail":"native credentials not inspected offline"})
    })
}

pub fn list(cli: &Cli, accounts: &Accounts, env: &Environment, as_json: bool) -> Result<()> {
    let (registry, order) = accounts.registry_with_order()?;
    let mut rows = Vec::new();
    for id in &order {
        let meta = &registry.accounts[id];
        let (drive, source) = context::resolve_drive_with_source(accounts, env, id, None, false)?;
        let ready = accounts
            .storage
            .profile_path(&drive, "dfs_credentials.json")?
            .exists();
        let state = if ready { "ok" } else { "missing" };
        let label = if &drive == id {
            state.into()
        } else if source.starts_with("bound:") {
            format!("->{drive} ({state})")
        } else {
            format!("default:{drive} ({state})")
        };
        rows.push(json!({"account":id,"active":id == &registry.active,"email":meta.email,"user_id":meta.user_id,
            "auth":auth(accounts, env, cli, id, meta)?,"drive":{"account":drive,"source":source,"ready":ready,"label":label},
            "sessions":accounts.storage.sessions(id)?.keys().collect::<Vec<_>>()}));
    }
    if as_json {
        print_json(
            &json!({"active":registry.active,"default_drive_account":registry.default_drive_account,"accounts":rows}),
        )?;
    } else {
        for row in rows {
            let auth = &row["auth"];
            let state = match auth["ready"].as_bool() {
                Some(true) => "ok",
                Some(false) => "missing",
                None => "?",
            };
            let storage = if auth["ready"] == false {
                &auth["provider"]
            } else {
                &auth["storage"]
            };
            let auth_label = format!("{state} ({})", storage.as_str().unwrap_or(""));
            let names: Vec<_> = row["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            let sessions = if names.is_empty() {
                "0".into()
            } else {
                format!("{} ({})", names.len(), names.join(", "))
            };
            println!(
                "{} {:<14} | email: {:<26} | colab: {:<22} | drive: {:<18} | sessions: {}",
                if row["active"] == true { "*" } else { " " },
                row["account"].as_str().unwrap(),
                row["email"].as_str().unwrap_or("-"),
                auth_label,
                row["drive"]["label"].as_str().unwrap(),
                sessions
            );
        }
    }
    Ok(())
}

pub fn current(cli: &Cli, accounts: &Accounts, env: &Environment, as_json: bool) -> Result<()> {
    let selected = context::resolve(accounts, env, cli.account.as_deref(), None, false)?;
    if !as_json {
        println!("{}", selected.profile);
        return Ok(());
    }
    let id = &selected.profile;
    let registry = accounts.registry()?;
    let meta = &registry.accounts[id];
    let (drive, source) = context::resolve_drive_with_source(accounts, env, id, None, false)?;
    let account_source = match selected.source {
        AccountSource::Flag => "flag",
        AccountSource::Environment => "env",
        AccountSource::Active => "active",
        AccountSource::Session => "session",
    };
    print_json(
        &json!({"account":id,"source":account_source,"email":meta.email,"user_id":meta.user_id,
        "auth_provider":meta.auth_provider,"drive_account":drive,"drive_source":source,
        "sessions_path":accounts.storage.profile_path(id,"sessions.json")?,"history_dir":accounts.storage.profile_path(id,"history")?,
        "dfs_credentials_path":accounts.storage.profile_path(&drive,"dfs_credentials.json")?,
        "oauth_token_file":accounts.storage.profile_path(id,"oauth_token.json")?,"auth":auth(accounts,env,cli,id,meta)?}),
    )
}
