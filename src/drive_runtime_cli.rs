use crate::{Cli, Command, authenticated_service, drive_cli::DriveCommand, print_json};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::TokenClient,
    colab_api::ApiConfig,
    context::{self, Environment},
    credentials::TokenStore,
    drive_credentials::{self as drive, DriveCredentialService, DriveCredentials},
    drivefs::{
        lifecycle, probe,
        remote::{AuthMode, RemoteRequest},
    },
    runs::SignalGuard,
    storage::Storage,
};
use std::time::Duration;

pub fn run(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
) -> Result<()> {
    let Command::Drive { command } = &cli.command else {
        unreachable!()
    };
    run_command(cli, accounts, env, storage, tokens, command)
}

/// Shared native implementation for drive mount and the drivemount wrapper.
pub(crate) fn run_command(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    command: &DriveCommand,
) -> Result<()> {
    let credentials = DriveCredentialService::new(storage.clone(), TokenClient::new()?);
    if let DriveCommand::Leases { drive_account } = command {
        let selected = context::resolve(accounts, env, cli.account.as_deref(), None, false)?;
        let account = context::resolve_drive(
            accounts,
            env,
            &selected.profile,
            drive_account.as_deref(),
            false,
        )?;
        return print_json(&credentials.leases(&account)?);
    }
    if let DriveCommand::LeaseCleanup { endpoint } = command {
        if endpoint.is_empty() || endpoint.len() > 1024 || endpoint.chars().any(char::is_control) {
            return Err(Error::KernelProtocol("DriveFS endpoint 无效"));
        }
        let selected = context::resolve(accounts, env, cli.account.as_deref(), None, false)?;
        let config = api_config(cli)?;
        let _guard = SignalGuard::install()?;
        authenticated_service(cli, storage, tokens, &selected, &config)?
            .cleanup_drive_leases(endpoint)?;
        return print_json(&serde_json::json!({"endpoint":endpoint,"lease_cleanup":"completed"}));
    }
    let (session, request) = match command {
        DriveCommand::Status { session, path, .. } => {
            (session.as_deref(), Some(RemoteRequest::new(path)?))
        }
        DriveCommand::ProbeCleanup { session, .. } => (session.as_deref(), None),
        DriveCommand::Mount {
            session,
            path,
            mode,
            credentials: source,
            drive_extra,
            readonly,
            force,
            purge_state,
            port,
            timeout,
            ..
        } => {
            let mut request = RemoteRequest::new(path)?;
            request.mode = if mode == "rt" && !source.cli_token {
                AuthMode::Rt
            } else {
                AuthMode::Shim
            };
            request.extra_flags = drive_extra.clone();
            request.readonly = *readonly;
            request.force = *force;
            request.purge_state = *purge_state;
            request.port = *port;
            request.timeout = *timeout;
            request.validate()?;
            (session.as_deref(), Some(request))
        }
        DriveCommand::Umount {
            session,
            path,
            purge_state,
            ..
        } => {
            let mut request = RemoteRequest::new(path)?;
            request.purge_state = *purge_state;
            (session.as_deref(), Some(request))
        }
        _ => unreachable!(),
    };
    let selected = context::resolve(accounts, env, cli.account.as_deref(), session, true)?;
    crate::announce_session_route(&selected, session);
    crate::check_runtime_endpoint(cli, storage, &selected.profile)?;
    let sessions = storage.sessions(&selected.profile)?;
    let name = match session {
        Some(name) if sessions.contains_key(name) => name.to_owned(),
        Some(name) => return Err(Error::SessionMissing(name.into())),
        None if sessions.len() == 1 => sessions.keys().next().unwrap().clone(),
        None if sessions.is_empty() => {
            return Err(Error::SessionMissing("当前账户没有本地会话".into()));
        }
        None => return Err(Error::Ambiguous("请用 -s 指定 DriveFS 核查的会话".into())),
    };
    // No token acquisition or kernel allocation for an empty cleanup.
    if request.is_none() && !sessions[&name].extra.contains_key("drive_probe") {
        return print_json(&serde_json::json!({"kernel_id":null}));
    }
    let status_drive = if let DriveCommand::Status { drive_account, .. } = command {
        Some(context::resolve_drive(
            accounts,
            env,
            &selected.profile,
            drive_account.as_deref(),
            false,
        )?)
    } else {
        None
    };
    let drive_account = if let DriveCommand::Mount {
        drive_account,
        dry_run,
        credentials: source,
        ..
    } = command
    {
        let account = if source.cli_token {
            if drive_account.is_some() {
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
                drive_account.as_deref(),
                false,
            )?
        };
        let mut supplied = if source.cli_token {
            let oauth = tokens.load(&selected.profile)?.ok_or(Error::NeedsAuth)?;
            Some(DriveCredentials {
                flavor: "imported".into(),
                user_id: source
                    .user_id
                    .clone()
                    .or_else(|| std::env::var("DFS_UID").ok()),
                email: source
                    .email
                    .clone()
                    .or_else(|| std::env::var("DFS_EMAIL").ok()),
                created_at: None,
                oauth,
            })
        } else if source.from_env || source.refresh_token.is_some() {
            Some(drive::imported(
                drive::ImportRequest {
                    refresh_token: colabcli_rust::models::Secret::new(
                        source
                            .refresh_token
                            .clone()
                            .ok_or(Error::OAuth("环境挂载需要 COLAB_DRIVE_REFRESH_TOKEN"))?,
                    ),
                    client_id: source
                        .client_id
                        .clone()
                        .ok_or(Error::OAuth("环境挂载需要 COLAB_DRIVE_CLIENT_ID"))?,
                    client_secret: source
                        .client_secret
                        .clone()
                        .map(colabcli_rust::models::Secret::new),
                    token_uri: source
                        .token_uri
                        .clone()
                        .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into()),
                    scopes: source
                        .scopes
                        .as_deref()
                        .unwrap_or("")
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                    user_id: source.user_id.clone(),
                    email: source.email.clone(),
                },
                chrono::Utc::now(),
            )?)
        } else {
            None
        };
        let request = request.as_ref().unwrap();
        if request.mode == AuthMode::Rt {
            if let Some(credentials) = &mut supplied {
                if credentials.oauth.client_id == drive::DRIVEFS_CLIENT_ID {
                    credentials.flavor = "dfs".into();
                }
            }
        }
        credentials.configure_mount(
            &account,
            supplied.as_ref(),
            source.user_id.as_deref(),
            source.email.as_deref(),
            request.mode == AuthMode::Rt,
            !dry_run,
        )?;
        if *dry_run {
            return print_json(
                &serde_json::json!({"account":selected.profile,"drive_account":account,"session":name,"path":request.mountpoint,
                "mode":request.mode,"readonly":request.readonly,"force":request.force,"purge_state":request.purge_state,"timeout":request.timeout,"port":request.port,"extra_flags":request.extra_flags,"transport":"native","dry_run":true}),
            );
        }
        Some(account)
    } else {
        None
    };
    let config = api_config(cli)?;
    let guard = SignalGuard::install()?;
    let service = authenticated_service(cli, storage, tokens, &selected, &config)?;
    match command {
        DriveCommand::Mount { .. } | DriveCommand::Umount { .. } => {
            let options = lifecycle::Options {
                http_timeout: config.timeout,
                cancellation: Some(guard.flag.clone()),
            };
            let result = if matches!(command, DriveCommand::Mount { .. }) {
                lifecycle::mount(
                    &service,
                    &credentials,
                    Some(&name),
                    drive_account.as_ref().unwrap(),
                    request.as_ref().unwrap(),
                    options,
                )?
            } else {
                lifecycle::unmount(
                    &service,
                    &credentials,
                    Some(&name),
                    request.as_ref().unwrap(),
                    options,
                )?
            };
            print_json(&result)?;
            if !result.ok {
                return Err(Error::CliExit(1));
            }
        }
        DriveCommand::Status { .. } => {
            let result = probe::query(
                &service,
                Some(&name),
                request.as_ref().unwrap(),
                config.timeout,
                Some(guard.flag.clone()),
            )?;
            let drive = status_drive
                .as_ref()
                .ok_or(Error::KernelProtocol("缺少 Drive 核查账户"))?;
            let local_path = storage.profile_path(drive, "dfs_credentials.json")?;
            let present = local_path.try_exists().map_err(|source| Error::Io {
                path: local_path,
                source,
            })?;
            let mut output = serde_json::to_value(&result)
                .map_err(|_| Error::KernelProtocol("Drive 核查结果编码失败"))?;
            let fields = output
                .as_object_mut()
                .ok_or(Error::KernelProtocol("Drive 核查结果不是对象"))?;
            fields.insert("account".into(), serde_json::json!(selected.profile));
            fields.insert("drive_account".into(), serde_json::json!(drive));
            fields.insert(
                "local_credentials_present".into(),
                serde_json::json!(present),
            );
            print_json(&output)?;
            if !result.ok {
                return Err(Error::CliExit(1));
            }
        }
        DriveCommand::ProbeCleanup { kernel_id, .. } => {
            let target =
                probe::cleanup(&service, Some(&name), kernel_id.as_deref(), config.timeout)?;
            print_json(&serde_json::json!({"kernel_id":target}))?;
        }
        _ => unreachable!(),
    }
    Ok(())
}
fn api_config(cli: &Cli) -> Result<ApiConfig> {
    let config = ApiConfig {
        base_url: cli.control_plane_url.clone(),
        timeout: Duration::try_from_secs_f64(cli.request_timeout)
            .map_err(|_| Error::KernelProtocol("请求期限必须为有限非负秒数"))?,
        retries: cli.assign_retries,
        backoff: Duration::try_from_secs_f64(cli.retry_backoff)
            .map_err(|_| Error::KernelProtocol("退避必须为有限非负秒数"))?,
    };
    config.validate()?;
    Ok(config)
}
