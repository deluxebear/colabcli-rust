use crate::{Cli, Command, authenticated_service, diagnostics, print_json};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::{AdcProvider, OAuthService, TokenClient},
    colab_api::ApiConfig,
    context::{self, Environment},
    credentials::{SystemKeyring, TokenStore},
    models::AuthProvider,
    storage::Storage,
    utility,
};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command as Process, Stdio},
    sync::Arc,
    time::Duration,
};
const PAY_URL: &str = "https://colab.research.google.com/signup";
fn browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(target_os = "windows")]
    let command = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let command = "xdg-open";
    let status = Process::new(command)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| Error::OAuth("无法启动系统浏览器"))?;
    if !status.success() {
        return Err(Error::OAuth("系统浏览器打开失败"));
    }
    Ok(())
}
fn root(cli: &Cli, env: &Environment) -> Result<PathBuf> {
    cli.config_dir
        .clone()
        .or_else(|| env.home.as_ref().map(|p| p.join(".config/colab-cli")))
        .ok_or(Error::OAuth("请指定 --config-dir"))
}
pub fn run_early(cli: &Cli) -> Option<Result<()>> {
    Some(match &cli.command {
        Command::Readme => {
            println!("{}", include_str!("../README.md"));
            Ok(())
        }
        Command::Skill => {
            println!("{}", include_str!("../resources/SKILL.md"));
            Ok(())
        }
        Command::Pay { url_only } => {
            if *url_only {
                println!("{PAY_URL}");
                Ok(())
            } else {
                println!("[colab] Opening {PAY_URL}...");
                browser(PAY_URL)
            }
        }
        Command::Url {
            session,
            host,
            open_browser,
        } => url(cli, session.as_deref(), host, *open_browser),
        _ => return None,
    })
}
fn url(cli: &Cli, session: Option<&str>, host: &str, open: bool) -> Result<()> {
    // Validate the host before looking up credentials or local sessions.
    utility::connection_url(host, "validate", None)?;
    let env = Environment::capture();
    let storage = Storage::open_existing(root(cli, &env)?)?;
    let accounts = diagnostics::local_accounts(&storage, cli.token_storage);
    let selected = context::resolve(&accounts, &env, cli.account.as_deref(), session, true)?;
    crate::announce_session_route(&selected, session);
    let sessions = storage.sessions(&selected.profile)?;
    let endpoint = match session {
        Some(name) => {
            let Some(state) = sessions.get(name) else {
                eprintln!("[colab] Session '{}' not found.", name.escape_debug());
                return Err(Error::CliExit(1));
            };
            state.endpoint.clone()
        }
        None => {
            let active = if sessions.is_empty() {
                Vec::new()
            } else {
                let config = ApiConfig {
                    base_url: cli.control_plane_url.clone(),
                    timeout: Duration::try_from_secs_f64(cli.request_timeout)
                        .map_err(|_| Error::ControlProtocol("超时必须为有限非负秒数"))?,
                    ..ApiConfig::default()
                };
                let tokens = TokenStore::new(
                    storage.clone(),
                    Arc::new(SystemKeyring::with_timeout(Duration::from_secs(
                        cli.keyring_timeout,
                    ))?),
                    cli.token_storage,
                );
                authenticated_service(cli, &storage, &tokens, &selected, &config)?
                    .status(None)?
                    .into_iter()
                    .filter_map(|view| view.name.map(|name| (name, view.endpoint)))
                    .collect::<Vec<_>>()
            };
            match active.as_slice() {
                [(_, endpoint)] => endpoint.clone(),
                [] => {
                    eprintln!(
                        "[colab] Error: No active sessions found. Create one with 'colab new'."
                    );
                    return Err(Error::CliExit(1));
                }
                _ => {
                    let names = active
                        .iter()
                        .map(|(name, _)| name.escape_debug().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    eprintln!(
                        "[colab] Error: Multiple active sessions found. Specify one with -s: {names}"
                    );
                    return Err(Error::CliExit(1));
                }
            }
        }
    };
    let registry = accounts.registry()?;
    let email = registry
        .accounts
        .get(&selected.profile)
        .and_then(|account| account.email.as_deref());
    let url = utility::connection_url(host, &endpoint, email)?;
    println!("{url}");
    if open {
        std::io::stdout()
            .flush()
            .map_err(|_| Error::OAuth("URL 输出失败"))?;
        browser(&url)?;
    }
    Ok(())
}
pub fn whoami(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
) -> Result<()> {
    let Command::Whoami {
        json,
        tokeninfo_url,
    } = &cli.command
    else {
        unreachable!()
    };
    // Reject unsafe tokeninfo endpoints before changing any refresh state.
    colabcli_rust::auth::validate_endpoint(tokeninfo_url)?;
    if tokeninfo_url.query().is_some() {
        return Err(Error::OAuth("tokeninfo URL 不得包含 query"));
    }
    let selected = context::resolve(accounts, env, cli.account.as_deref(), None, false)?;
    let provider = cli.auth.unwrap_or(selected.provider);
    let bearer = match provider {
        AuthProvider::Oauth2 => {
            OAuthService::new(storage.clone(), tokens.clone(), TokenClient::new()?)
                .refresh_access_token(&selected.profile, chrono::Utc::now())?
        }
        AuthProvider::Adc => AdcProvider::from_context(&selected, TokenClient::new()?)
            .refresh_access_token(chrono::Utc::now())
            .map_err(|error| match error {
                Error::UnsupportedAdc(_) => Error::OAuth("whoami 不支持此 ADC 类型"),
                other => other,
            })?,
    };
    let identity = utility::token_identity(tokeninfo_url, &bearer, Duration::from_secs(10))?;
    // Backfill only an OAuth profile whose current stored bearer is exactly the
    // queried token. Registry -> profile lock order matches rename/remove.
    if provider == AuthProvider::Oauth2 && selected.provider == AuthProvider::Oauth2 {
        if let Some(email) = identity
            .email
            .as_ref()
            .filter(|s| !s.chars().any(char::is_whitespace))
        {
            let recorded = accounts.update(|registry| {
                let meta = registry
                    .accounts
                    .get_mut(&selected.profile)
                    .ok_or_else(|| Error::AccountMissing(selected.profile.to_string()))?;
                if meta.auth_provider == AuthProvider::Oauth2
                    && meta.email.as_deref().is_none_or(str::is_empty)
                {
                    let _guard = storage.profile_lock(&selected.profile)?;
                    if tokens
                        .load(&selected.profile)?
                        .is_some_and(|current| current.token == bearer)
                    {
                        meta.email = Some(email.clone());
                        meta.updated_at = chrono::Utc::now().to_rfc3339();
                    }
                }
                Ok(())
            });
            if recorded.is_err() {
                eprintln!("身份查询成功，但未更新本地账户邮箱。");
            }
        }
    }
    if *json {
        return print_json(
            &serde_json::json!({"account":selected.profile,"provider":provider,"identity":identity}),
        );
    }
    let source = match selected.source {
        context::AccountSource::Flag => "flag",
        context::AccountSource::Environment => "environment",
        context::AccountSource::Active => "active",
        context::AccountSource::Session => "session",
    };
    println!(
        "Account:       {} (source: {source}, storage: {})",
        selected.profile,
        match cli.token_storage {
            colabcli_rust::credentials::TokenStorageMode::Auto => "auto",
            colabcli_rust::credentials::TokenStorageMode::File => "file",
            colabcli_rust::credentials::TokenStorageMode::Keyring => "keyring",
        }
    );
    println!(
        "Auth provider: {}",
        if provider == AuthProvider::Adc {
            "adc"
        } else {
            "oauth2"
        }
    );
    println!(
        "Email:         {}",
        identity
            .email
            .as_deref()
            .unwrap_or("<unavailable: token has no userinfo.email scope>")
    );
    println!(
        "Audience:      {}",
        identity.audience.as_deref().unwrap_or("<none>")
    );
    println!(
        "Expires in:    {}",
        identity
            .expires_in
            .map(|n| format!("{}m", n / 60))
            .unwrap_or_else(|| "<unknown>".into())
    );
    if identity.scopes.is_empty() {
        println!("Scopes:        <none>");
    } else {
        println!("Scopes:");
        for scope in &identity.scopes {
            println!("  - {scope}");
        }
    }
    Ok(())
}
