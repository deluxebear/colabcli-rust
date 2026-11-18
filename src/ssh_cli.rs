use crate::{Cli, Command, authenticated_service};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    colab_api::{ApiConfig, RuntimeOptions},
    context::{self, Environment},
    credentials::{TokenStorageMode, TokenStore},
    models::AuthProvider,
    runs::SignalGuard,
    ssh::{self, Mode},
    storage::Storage,
};
use std::{
    io::{self, IsTerminal},
    path::Path,
    sync::atomic::Ordering,
    time::Duration,
};

pub fn run(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
) -> Result<()> {
    let Command::Ssh {
        session,
        proxy_mode,
        identity,
        gpu,
        tpu,
        high_mem,
        rm,
        internal_mode,
        expected_endpoint,
        command,
    } = &cli.command
    else {
        unreachable!()
    };
    if *proxy_mode && !command.is_empty() {
        eprintln!("[colab] --proxy-mode takes no command.");
        return Err(Error::CliExit(2));
    }
    if !cfg!(unix) {
        return Err(Error::Ssh(
            "SSH CLI 当前只实现 Unix stdio 桥接；Windows 原生路径仍待完成",
        ));
    }
    let mode = if !command.is_empty() {
        Mode::Command
    } else if *proxy_mode {
        match internal_mode.as_str() {
            "interactive" => Mode::Interactive,
            "cmd" => Mode::Command,
            _ => Mode::Proxy,
        }
    } else {
        Mode::Interactive
    };
    let config = ApiConfig {
        base_url: cli.control_plane_url.clone(),
        timeout: Duration::try_from_secs_f64(cli.request_timeout)
            .map_err(|_| Error::Ssh("请求期限必须为有限非负秒数"))?,
        retries: cli.assign_retries,
        backoff: Duration::try_from_secs_f64(cli.retry_backoff)
            .map_err(|_| Error::Ssh("退避必须为有限非负秒数"))?,
    };
    config.validate()?;
    let selected = context::resolve(
        accounts,
        env,
        cli.account.as_deref(),
        session.as_deref(),
        true,
    )?;
    if io::stdout().is_terminal() {
        crate::announce_session_route(&selected, session.as_deref());
    }
    let sessions = storage.sessions(&selected.profile)?;
    let create = if *proxy_mode {
        expected_endpoint.is_none()
            && mode != Mode::Command
            && session
                .as_ref()
                .is_some_and(|name| !sessions.contains_key(name))
    } else {
        command.is_empty() && session.is_none() && sessions.is_empty()
    };
    // Hardware flags are ignored on reused VMs, including invalid model names.
    let runtime = if create {
        Some(
            RuntimeOptions::new(gpu.as_deref(), tpu.as_deref(), *high_mem).map_err(|error| {
                eprintln!("[colab] {error}");
                Error::CliExit(2)
            })?,
        )
    } else {
        None
    };
    let signals = SignalGuard::install()?;
    let service = authenticated_service(cli, storage, tokens, &selected, &config)?;
    let signal = signals.flag.load(Ordering::SeqCst);
    if signal != 0 {
        return Err(Error::CliExit(128 + signal as i32));
    }
    let mut owner = None;
    let mut allocated_endpoint = None;
    let name = if let Some(runtime) = runtime {
        let name = session
            .clone()
            .unwrap_or_else(|| colabcli_rust::colab_api::notebook_identity()[..6].to_owned());
        let announce = |message: &str| {
            if *proxy_mode {
                eprintln!("{message}");
            } else {
                println!("{message}");
            }
        };
        announce(&format!("[colab] Creating runtime '{name}'..."));
        if runtime.high_mem && runtime.shape().is_none() {
            eprintln!(
                "[colab] --high-mem ignored: this accelerator only offers one machine shape."
            );
        }
        announce(&format!("[colab] Creating session '{name}'..."));
        let allocation_owner = format!("ssh:{}", colabcli_rust::jupyter::codec::message_id());
        let view = match service.create_owned(Some(&name), runtime, Some(&allocation_owner)) {
            Ok(view) => view,
            Err(error) => {
                if *rm {
                    if let Err(cleanup) = service.cleanup_owned(&allocation_owner, &name, None) {
                        eprintln!(
                            "[colab] --rm: 清理待恢复: {cleanup}；请核查会话后使用 new/stop 恢复，不能重放 SSH 命令"
                        );
                    }
                }
                return Err(error);
            }
        };
        allocated_endpoint = Some(view.endpoint);
        owner = Some(allocation_owner);
        announce("[colab] Session READY.");
        name
    } else {
        if gpu.is_some() || tpu.is_some() || *high_mem {
            eprintln!("[colab] --gpu/--tpu/--high-mem ignored: only applies to a created runtime.");
        }
        if let Some(name) = session {
            name.clone()
        } else {
            let names: Vec<_> = if sessions.is_empty() {
                Vec::new()
            } else {
                service
                    .status(None)?
                    .into_iter()
                    .filter_map(|s| s.name)
                    .collect()
            };
            match names.as_slice() {
                [name] => {
                    if *proxy_mode || !command.is_empty() {
                        eprintln!("[colab] Using unique session '{name}'.");
                    } else {
                        println!("[colab] Using unique session '{name}'.");
                    }
                    name.clone()
                }
                [] => {
                    eprintln!(
                        "[colab] Error: No active sessions found. Create one with 'colab new'."
                    );
                    return Err(Error::CliExit(1));
                }
                _ => {
                    eprintln!(
                        "[colab] Error: Multiple active sessions found. Specify one with -s: {}",
                        names.join(", ")
                    );
                    return Err(Error::CliExit(1));
                }
            }
        }
    };
    let remove = if *proxy_mode {
        *rm
    } else if command.is_empty() {
        *rm && create
    } else {
        false
    };
    if *rm && !*proxy_mode && !create {
        eprintln!(
            "{}",
            if command.is_empty() {
                "[colab] --rm ignored: only a runtime auto-created by `colab ssh` is removed on exit."
            } else {
                "[colab] --rm ignored with -- CMD."
            }
        );
    }
    let state = match service.with_session(Some(&name), |state| Ok(state.clone())) {
        Ok(state) => state,
        Err(error) => {
            if remove {
                if let Some(owner) = &owner {
                    if let Err(cleanup) =
                        service.cleanup_owned(owner, &name, allocated_endpoint.as_deref())
                    {
                        eprintln!("[colab] --rm: 清理待恢复: {cleanup}");
                    }
                }
            }
            if matches!(error, Error::SessionMissing(_)) {
                eprintln!(
                    "[colab] session '{name}' not found. Run `colab sessions` to list active sessions."
                );
                return Err(Error::CliExit(2));
            }
            return Err(error);
        }
    };
    if expected_endpoint
        .as_deref()
        .is_some_and(|expected| expected != state.endpoint)
    {
        return Err(Error::Ssh("会话已被替换；没有连接替代 VM"));
    }
    let outcome = ssh::with_cleanup_progress(
        &service,
        &state,
        remove,
        || {
            if signals.flag.load(Ordering::SeqCst) != 0 {
                return Ok(128 + signals.flag.load(Ordering::SeqCst) as i32);
            }
            if *proxy_mode {
                let key = ssh::resolve_pubkey(
                    identity.as_deref(),
                    env.home.as_deref(),
                    config.timeout,
                    &signals.flag,
                )
                .map_err(|error| {
                    eprintln!("[colab] {error}");
                    Error::CliExit(2)
                })?;
                let socket = ssh::connect(&state, &key, mode, config.timeout)?;
                #[cfg(unix)]
                {
                    let mut stdio = ssh::StdioGuard::open()?;
                    ssh::bridge(socket, &mut stdio.input, &mut stdio.output, &signals.flag)
                }
                #[cfg(not(unix))]
                {
                    drop(socket);
                    Err(Error::Ssh("本平台的原生 SSH stdio 桥接尚未实现"))
                }
            } else {
                let absolute = |path: &Path| -> Result<String> {
                    let path = if path.is_absolute() {
                        path.to_owned()
                    } else {
                        std::env::current_dir()
                            .map_err(|_| Error::Ssh("无法确定当前目录"))?
                            .join(path)
                    };
                    path.to_str()
                        .map(str::to_owned)
                        .ok_or(Error::Ssh("ProxyCommand 路径必须为 UTF-8"))
                };
                let mut argv = vec![
                    absolute(
                        &std::env::current_exe()
                            .map_err(|_| Error::Ssh("无法定位 colab 可执行文件"))?,
                    )?,
                    "--config-dir".into(),
                    absolute(storage.root())?,
                    "--account".into(),
                    selected.profile.to_string(),
                    "--auth".into(),
                    match cli.auth.unwrap_or(selected.provider) {
                        AuthProvider::Oauth2 => "oauth2",
                        AuthProvider::Adc => "adc",
                    }
                    .into(),
                    "--token-storage".into(),
                    match cli.token_storage {
                        TokenStorageMode::Auto => "auto",
                        TokenStorageMode::File => "file",
                        TokenStorageMode::Keyring => "keyring",
                    }
                    .into(),
                    "--keyring-timeout".into(),
                    cli.keyring_timeout.to_string(),
                    "--control-plane-url".into(),
                    config.base_url.to_string(),
                    "--request-timeout".into(),
                    cli.request_timeout.to_string(),
                ];
                if let Some(path) = &cli.client_oauth_config {
                    argv.extend(["--client-oauth-config".into(), absolute(path)?]);
                }
                argv.extend([
                    "ssh".into(),
                    "--proxy-mode".into(),
                    "-s".into(),
                    name.clone(),
                ]);
                if let Some(identity) = identity {
                    argv.extend([
                        "--identity".into(),
                        absolute(&ssh::expand_identity(identity, env.home.as_deref())?)?,
                    ]);
                }
                argv.extend([
                    "--colab-internal-mode".into(),
                    mode.tag().into(),
                    "--colab-expected-endpoint".into(),
                    state.endpoint.clone(),
                ]);
                let proxy = ssh::proxy_command(&argv)?;
                let args = ssh::openssh_args(
                    &proxy,
                    identity.as_deref(),
                    env.home.as_deref(),
                    command,
                    io::stdin().is_terminal() && io::stdout().is_terminal(),
                )?;
                ssh::run_openssh(&args, &signals.flag)
            }
        },
        |progress| {
            let text = match progress {
                ssh::CleanupProgress::Releasing => format!("[colab] Stopping session '{name}'..."),
                ssh::CleanupProgress::Released => "[colab] Session terminated.".into(),
            };
            if *proxy_mode {
                eprintln!("{text}");
            } else {
                println!("{text}");
            }
        },
    );
    let cleanup_failed = outcome.cleanup.is_some();
    if let Some(error) = outcome.cleanup {
        eprintln!(
            "[colab] --rm: failed to stop '{name}': {error}；请用同一账户 stop -s '{name}' 恢复"
        );
    }
    let signal = signals.flag.load(Ordering::SeqCst);
    if signal != 0 {
        if *proxy_mode && *rm {
            return if cleanup_failed {
                Err(Error::CliExit(1))
            } else {
                Ok(())
            };
        }
        return Err(Error::CliExit(128 + signal as i32));
    }
    let code = outcome.action?;
    if code != 0 {
        return Err(Error::CliExit(code));
    }
    if cleanup_failed {
        return Err(Error::CliExit(1));
    }
    Ok(())
}
