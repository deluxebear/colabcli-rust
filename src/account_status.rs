//! Account inventory preserves partial results and never starts interactive login.
use crate::{Cli, authenticated_service, print_json};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    colab_api::ApiConfig,
    context::{self, Environment},
    credentials::TokenStore,
    inventory,
    models::{AuthProvider, ProfileId, Registry},
    storage::Storage,
};
use serde::Serialize;

#[derive(Serialize)]
struct Row {
    account: ProfileId,
    active: bool,
    email: Option<String>,
    auth_provider: AuthProvider,
    drive_account: Option<ProfileId>,
    // Presence only, matching the extension; not proof of usable credentials.
    drive_ready: bool,
    usage: Option<Usage>,
    assignments: Vec<Assignment>,
    error: Option<String>,
}
#[derive(Serialize)]
struct Usage {
    balance: f64,
    rate_hourly: f64,
    assignments_count: u64,
}
#[derive(Serialize)]
struct Assignment {
    name: String,
    endpoint: String,
    hardware: String,
    accelerator: String,
    shape: String,
    machine_shape: String,
    variant: String,
}
fn empty(registry: &Registry, profile: &ProfileId, cli: &Cli) -> Row {
    let meta = &registry.accounts[profile];
    Row {
        account: profile.clone(),
        active: registry.active == *profile,
        email: meta.email.clone(),
        auth_provider: cli.auth.unwrap_or(meta.auth_provider),
        drive_account: None,
        drive_ready: false,
        usage: None,
        assignments: Vec::new(),
        error: None,
    }
}
#[derive(Clone, Copy)]
pub enum Kind {
    Status,
    Sessions,
    Usage,
}

#[derive(Clone, Copy)]
pub struct Query {
    pub kind: Kind,
    pub all: bool,
    pub json: bool,
}

fn collect(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    config: &ApiConfig,
    query: Query,
) -> Result<Vec<Row>> {
    let Query { kind, all, .. } = query;
    let signals = env
        .allow_executables
        .then(colabcli_rust::runs::SignalGuard::install)
        .transpose()?;
    let registry = accounts.registry()?;
    let profiles: Vec<_> = if all {
        registry.accounts.keys().cloned().collect()
    } else {
        vec![context::resolve(accounts, env, cli.account.as_deref(), None, false)?.profile]
    };
    let rows = inventory::collect(&profiles, |profile| {
        let mut row = empty(&registry, profile, cli);
        let mut probe = || -> Result<()> {
            let drive = context::resolve_drive(accounts, env, profile, None, false)?;
            row.drive_ready = storage
                .profile_path(&drive, "dfs_credentials.json")?
                .exists();
            row.drive_account = Some(drive);
            let mut selected =
                context::resolve(accounts, env, Some(profile.as_str()), None, false)?;
            selected.cancellation = signals.as_ref().map(|guard| guard.flag.clone());
            let service = authenticated_service(cli, storage, tokens, &selected, config)?;
            if !matches!(kind, Kind::Sessions) {
                let usage = service.usage()?;
                row.usage = Some(Usage {
                    balance: usage.current_balance,
                    rate_hourly: usage.consumption_rate_hourly,
                    assignments_count: usage.assignments_count,
                });
            }
            if !matches!(kind, Kind::Usage) {
                row.assignments = service
                    .status(None)?
                    .into_iter()
                    .map(|view| Assignment {
                        name: view.name.unwrap_or_else(|| "?".into()),
                        endpoint: view.endpoint,
                        hardware: if view.accelerator == "NONE" {
                            "CPU".into()
                        } else {
                            view.accelerator.clone()
                        },
                        accelerator: view.accelerator,
                        shape: if view.machine_shape == "HIGH_RAM" {
                            "High-RAM".into()
                        } else {
                            "Standard".into()
                        },
                        machine_shape: view.machine_shape,
                        variant: view.variant,
                    })
                    .collect();
            }
            Ok(())
        };
        if let Err(error) = probe() {
            row.error = Some(error.to_string());
        }
        Ok(row)
    })
    .into_iter()
    .map(|result| {
        result.result.unwrap_or_else(|| {
            let mut row = empty(&registry, &result.account, cli);
            row.error = result.error;
            row
        })
    })
    .collect::<Vec<_>>();
    let signal = signals.as_ref().map_or(0, |guard| {
        guard.flag.load(std::sync::atomic::Ordering::SeqCst)
    });
    if signal != 0 {
        return Err(Error::CliExit(128 + signal as i32));
    }
    Ok(rows)
}

pub fn run(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    config: &ApiConfig,
    json: bool,
) -> Result<()> {
    let rows = collect(
        cli,
        accounts,
        env,
        storage,
        tokens,
        config,
        Query {
            kind: Kind::Status,
            all: true,
            json,
        },
    )?;
    if json {
        return print_json(&rows);
    }
    if rows.is_empty() {
        println!("[account] No accounts configured.");
    }
    for row in rows {
        let mark = if row.active { "*" } else { " " };
        let email = row.email.as_deref().unwrap_or("-");
        let drive = format!(
            "drive:{}({})",
            row.drive_account.as_ref().map_or("?", ProfileId::as_str),
            if row.drive_ready { "ok" } else { "missing" }
        );
        if let Some(error) = row.error {
            println!(
                "[{mark}{}] ({email}) | {drive} | ERROR: {error}",
                row.account
            );
        } else if let Some(usage) = row.usage {
            let sessions = if row.assignments.is_empty() {
                "none".into()
            } else {
                row.assignments
                    .iter()
                    .map(|a| format!("{}:{}", a.name, a.hardware))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!(
                "[{mark}{}] ({email}) | CCU: {:.2} ({:.2}/hr) | sessions: {sessions} | {drive}",
                row.account, usage.balance, usage.rate_hourly
            );
        }
    }
    Ok(())
}

/// JSON inventory shares the merged account row schema; partial account failures remain rows.
pub fn run_query(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    config: &ApiConfig,
    query: Query,
) -> Result<()> {
    let Query { kind, all, json } = query;
    let rows = collect(cli, accounts, env, storage, tokens, config, query)?;
    if json {
        return if all {
            print_json(&rows)
        } else {
            print_json(&rows[0])
        };
    }
    match kind {
        Kind::Sessions => {
            let mut printed = false;
            for row in rows {
                if let Some(error) = row.error {
                    eprintln!("[{}] ERROR: {error}", row.account);
                    continue;
                }
                for assignment in row.assignments {
                    printed = true;
                    println!(
                        "[{}/{}] {} | Hardware: {} | Shape: {} | Variant: {}",
                        row.account,
                        assignment.name,
                        assignment.endpoint,
                        assignment.hardware,
                        assignment.shape,
                        assignment.variant
                    );
                }
            }
            if !printed {
                println!("[colab] No active sessions found across any account.");
            }
        }
        Kind::Usage => {
            for row in rows {
                let mark = if row.active { "*" } else { " " };
                let email = row.email.as_deref().unwrap_or("-");
                if let Some(error) = row.error {
                    println!("[{mark}{}] ({email}) ERROR: {error}", row.account);
                } else if let Some(usage) = row.usage {
                    println!(
                        "[{mark}{}] ({email}) | Balance: {:.2} CCU | Rate: {:.2}/hr | Active: {}",
                        row.account, usage.balance, usage.rate_hourly, usage.assignments_count
                    );
                }
            }
        }
        Kind::Status => unreachable!("status has its own display"),
    }
    Ok(())
}
