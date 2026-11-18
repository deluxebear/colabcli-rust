//! Choose one mount strategy before side effects; never fall back after dispatch.
use crate::{
    Cli, Command,
    drive_cli::{DriveCommand, MountCredentialArgs},
    drive_runtime_cli,
};
use clap::Args;
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    context::{self, Environment},
    credentials::TokenStore,
    storage::Storage,
};
use std::io::{self, IsTerminal};
#[derive(Args)]
pub struct Options {
    #[arg(long)]
    drive_account: Option<String>,
    #[arg(long,default_value="shim",value_parser=["shim","rt"])]
    mode: String,
    #[command(flatten)]
    credentials: Box<MountCredentialArgs>,
    #[arg(long)]
    readonly: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    purge_state: bool,
    #[arg(long, default_value_t = 8012)]
    port: u16,
    #[arg(long, allow_hyphen_values = true)]
    drive_extra: Vec<String>,
    #[arg(long)]
    via_cli: bool,
    #[arg(long)]
    dry_run: bool,
    /// Never fall back to the interactive official flow when Drive credentials are missing.
    #[arg(long)]
    no_fallback: bool,
    /// Use official Colab credentials propagation, without separate Drive credentials.
    #[arg(long)]
    upstream: bool,
}
// The VM uses POSIX paths regardless of the local client's platform.
fn overlaps(a: &str, b: &str) -> bool {
    fn normalize(path: &str) -> String {
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        format!("/{}", parts.join("/"))
    }
    let a = normalize(a);
    let b = normalize(b);
    a == b
        || a == "/"
        || b == "/"
        || a.starts_with(&format!("{b}/"))
        || b.starts_with(&format!("{a}/"))
}
fn upstream_default() -> bool {
    let setting = std::env::var("COLAB_DRIVE_DEFAULT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("COLAB_DFS_DEFAULT").ok());
    setting.is_some_and(|s| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}
/// None hands control to the existing official implementation; Some is terminal.
pub fn dispatch(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
) -> Option<Result<()>> {
    let Command::Drivemount {
        session,
        path,
        timeout,
        json,
        options,
    } = &cli.command
    else {
        return None;
    };
    let choose = || -> Result<bool> {
        let selected = context::resolve(
            accounts,
            env,
            cli.account.as_deref(),
            session.as_deref(),
            true,
        )?;
        let drive = if options.credentials.cli_token {
            if options.drive_account.is_some() {
                return Err(Error::OAuth(
                    "--cli-token 使用计算账户，不接受 --drive-account",
                ));
            }
            selected.profile.clone()
        } else {
            context::resolve_drive(
                accounts,
                env,
                &selected.profile,
                options.drive_account.as_deref(),
                false,
            )?
        };
        let dfs_only = options.drive_account.is_some()
            || drive != selected.profile
            || options.readonly
            || options.force
            || options.purge_state
            || options.credentials.from_env
            || options.credentials.cli_token
            || options.mode != "shim"
            || options.dry_run
            || options.port != 8012
            || !options.drive_extra.is_empty()
            || options.via_cli
            || options.credentials.user_id.is_some()
            || options.credentials.email.is_some();
        let states = storage.sessions(&selected.profile)?;
        let candidates: Vec<_> = states
            .values()
            .filter(|state| session.as_deref().is_none_or(|name| state.name == name))
            .collect();
        let owned_path = candidates.iter().any(|state| {
            let pending = state.extra.get("drive_plan").is_some_and(|value| {
                value["request"]["mountpoint"]
                    .as_str()
                    .is_none_or(|p| overlaps(p, path))
            });
            let mounted = state.extra.get("drive_mounts").is_some_and(|value| {
                value
                    .as_object()
                    .is_none_or(|map| map.keys().any(|p| overlaps(p, path)))
            });
            pending || mounted
        });
        if options.upstream || upstream_default() {
            if dfs_only {
                return Err(Error::OAuth(
                    "官方挂载策略不能与独立 Drive 账户或 DriveFS 专用参数组合",
                ));
            }
            let managed = storage.profile_path(&drive, "managed")?;
            let lease_owned = if managed.exists() {
                colabcli_rust::drive_credentials::DriveCredentialService::new(
                    storage.clone(),
                    colabcli_rust::auth::TokenClient::new()?,
                )
                .leases(&drive)?
                .iter()
                .any(|lease| {
                    lease.compute_account == selected.profile
                        && overlaps(&lease.request.mountpoint, path)
                        && (candidates.is_empty()
                            || candidates
                                .iter()
                                .any(|state| state.endpoint == lease.endpoint))
                })
            } else {
                false
            };
            if owned_path || lease_owned {
                return Err(Error::OAuth(
                    "此路径仍有 DriveFS 挂载或恢复责任；请先 drive umount，不能切换官方挂载策略",
                ));
            }
            return Ok(false);
        }
        let supplied = options.credentials.cli_token
            || options.credentials.from_env
            || options.credentials.refresh_token.is_some();
        let has_credentials = storage
            .profile_path(&drive, "dfs_credentials.json")?
            .exists()
            || storage
                .profile_path(&drive, "dfs_refresh_pending.json")?
                .exists();
        // A missing credential file must never route an owned pending mount into
        // another authentication strategy. Keep its recovery responsibility.
        let managed = storage.profile_path(&drive, "managed")?;
        let has_managed = if managed.exists() {
            std::fs::read_dir(&managed)
                .map_err(|source| Error::Io {
                    path: managed.clone(),
                    source,
                })?
                .next()
                .is_some()
        } else {
            false
        };
        let owned_pending = has_managed
            || states
                .values()
                .filter(|s| session.as_deref().is_none_or(|name| s.name == name))
                .any(|s| {
                    s.extra.contains_key("drive_plan") || s.extra.contains_key("drive_mounts")
                });
        if !has_credentials && !supplied {
            if owned_pending {
                return Err(Error::OAuth(
                    "已有 DriveFS 恢复责任但缺少凭据；请先 ext doctor/status 核查，不会回退官方挂载",
                ));
            }
            if options.no_fallback || dfs_only || !io::stdin().is_terminal() {
                return Err(Error::OAuth(
                    "缺少 DriveFS 凭据；请先 drive consent/import，或显式 --upstream 使用计算账户官方授权",
                ));
            }
            eprintln!("没有已保存的 DriveFS 凭据；将使用当前计算账户的官方交互挂载流程。");
            return Ok(false);
        }
        Ok(true)
    };
    match choose() {
        Err(error) => Some(Err(error)),
        Ok(false) => None,
        Ok(true) => {
            if !timeout.is_finite() || timeout.fract() != 0.0 || !(1.0..=3600.0).contains(timeout) {
                return Some(Err(Error::KernelProtocol(
                    "DriveFS timeout 必须为 1–3600 的整数秒",
                )));
            }
            let command = DriveCommand::Mount {
                session: session.clone(),
                path: path.clone(),
                drive_account: options.drive_account.clone(),
                mode: options.mode.clone(),
                credentials: options.credentials.clone(),
                drive_extra: options.drive_extra.clone(),
                via_cli: options.via_cli,
                readonly: options.readonly,
                force: options.force,
                purge_state: options.purge_state,
                port: options.port,
                timeout: *timeout as u64,
                dry_run: options.dry_run,
                json: *json,
            };
            Some(drive_runtime_cli::run_command(
                cli, accounts, env, storage, tokens, &command,
            ))
        }
    }
}

/// Hold the same operation lease as native DriveFS until the official execution
/// finishes. Initial routing alone is not an ownership check after concurrent work.
pub fn upstream_guard(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    service: &colabcli_rust::sessions::SessionService,
    selected: &context::AccountContext,
    name: &str,
) -> Result<colabcli_rust::storage::FileLock> {
    let Command::Drivemount { path, .. } = &cli.command else {
        unreachable!()
    };
    let before = service.with_session(Some(name), |state| Ok(state.clone()))?;
    let guard = colabcli_rust::drivefs::lifecycle::operation_lock(
        storage,
        &selected.profile,
        &before.endpoint,
    )?;
    let state = service.with_session(Some(name), |state| Ok(state.clone()))?;
    if state.endpoint != before.endpoint {
        return Err(Error::KernelProtocol(
            "等待挂载租约期间 VM 已变化；请重新核查",
        ));
    }
    let drive = context::resolve_drive(accounts, env, &selected.profile, None, false)?;
    if drive != selected.profile {
        return Err(Error::OAuth(
            "官方挂载前 Drive 绑定已变化；请重新选择挂载策略",
        ));
    }
    let pending = state.extra.get("drive_plan").is_some_and(|value| {
        value["request"]["mountpoint"]
            .as_str()
            .is_none_or(|p| overlaps(p, path))
    });
    let mounted = state.extra.get("drive_mounts").is_some_and(|value| {
        value
            .as_object()
            .is_none_or(|map| map.keys().any(|p| overlaps(p, path)))
    });
    let credentials = colabcli_rust::drive_credentials::DriveCredentialService::new(
        storage.clone(),
        colabcli_rust::auth::TokenClient::new()?,
    );
    let leased = credentials.leases(&drive)?.iter().any(|lease| {
        lease.compute_account == selected.profile
            && lease.endpoint == state.endpoint
            && overlaps(&lease.request.mountpoint, path)
    });
    if pending || mounted || leased {
        return Err(Error::OAuth(
            "此路径仍有 DriveFS 挂载或恢复责任；请先 drive umount，不能切换官方挂载策略",
        ));
    }
    Ok(guard)
}
