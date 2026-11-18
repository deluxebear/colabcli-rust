//! Native Drive credential CLI. Mounting is implemented separately from consent.
use crate::{Cli, Command, print_json};
use clap::{Args, Subcommand};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::{OAuthClient, REMOTE_REDIRECT, TokenClient},
    context::{self, Environment},
    drive_credentials::{self as drive, ConsentRequest, DriveCredentialService},
    models::{Registry, Secret},
    runs::SignalGuard,
    storage::Storage,
};
use std::{
    io::{self, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use url::Url;

#[derive(Subcommand)]
pub enum DriveCommand {
    /// Diagnose native extension support and local state without authentication.
    #[command(hide = true)]
    Doctor(crate::diagnostics::DoctorArgs),
    /// Display the built-in extension version.
    #[command(hide = true)]
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Compatibility entry: extension features are built in; no Python injection is needed.
    #[command(hide = true)]
    Install(crate::diagnostics::InstallArgs),
    /// Authorize a separate Drive account; supports resumable URL/code exchange.
    Consent(Box<ConsentArgs>),
    /// Import and validate a refresh token and its matching OAuth client.
    Import(Box<ImportArgs>),
    /// Display credential metadata with secrets redacted.
    Show {
        #[arg(long)]
        drive_account: Option<String>,
    },
    /// Explicitly print a newly minted Drive access token as JSON.
    Token {
        #[arg(long)]
        drive_account: Option<String>,
        #[arg(long)]
        whoami: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove local Drive credentials and pending authorizations; does not revoke Google grants.
    Clear,
    /// Read actual mount and process state on an existing VM, without Drive credentials.
    Status {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, default_value = "/content/drive")]
        path: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        drive_account: Option<String>,
    },
    /// Clean only a pending read-only probe kernel; verify unknown IDs using kernels first.
    ProbeCleanup {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        kernel_id: Option<String>,
    },
    /// Mount DriveFS on an existing VM using a stored or explicitly supplied Drive account.
    Mount {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, default_value = "/content/drive")]
        path: String,
        #[arg(long)]
        drive_account: Option<String>,
        #[arg(long,default_value="shim",value_parser=["shim","rt"])]
        mode: String,
        #[command(flatten)]
        credentials: Box<MountCredentialArgs>,
        /// Additional non-secret DriveFS flag, passed as one argv element.
        #[arg(long, allow_hyphen_values = true)]
        drive_extra: Vec<String>,
        /// Compatibility option; execution uses the native Rust transport.
        #[arg(long)]
        via_cli: bool,
        #[arg(long)]
        readonly: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        purge_state: bool,
        #[arg(long, default_value_t = 8012)]
        port: u16,
        #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Safely unmount an owned DriveFS instance and recover rotated credentials.
    Umount {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, default_value = "/content/drive")]
        path: String,
        #[arg(long)]
        purge_state: bool,
        #[arg(long)]
        json: bool,
    },
    /// List secret-free local Drive credential leases without contacting a VM.
    Leases {
        #[arg(long)]
        drive_account: Option<String>,
    },
    /// Remove this compute account's leases only after confirming the VM is absent.
    LeaseCleanup {
        #[arg(long)]
        endpoint: String,
    },
}

#[derive(Args, Clone)]
pub struct MountCredentialArgs {
    /// Use COLAB_DRIVE_* credentials; a supplied refresh token also selects this source.
    #[arg(long, conflicts_with = "cli_token")]
    pub from_env: bool,
    /// Experimental: copy the compute OAuth refresh credentials into the Drive store; forces shim.
    #[arg(long)]
    pub cli_token: bool,
    #[arg(long, env = "COLAB_DRIVE_USER_ID")]
    pub user_id: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_EMAIL")]
    pub email: Option<String>,
    #[arg(
        long,
        env = "COLAB_DRIVE_REFRESH_TOKEN",
        hide_env_values = true,
        hide = true
    )]
    pub refresh_token: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_CLIENT_ID", hide = true)]
    pub client_id: Option<String>,
    #[arg(
        long,
        env = "COLAB_DRIVE_CLIENT_SECRET",
        hide_env_values = true,
        hide = true
    )]
    pub client_secret: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_TOKEN_URI", hide = true)]
    pub token_uri: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_SCOPES", hide = true)]
    pub scopes: Option<String>,
}

#[derive(Args)]
pub struct ConsentArgs {
    #[arg(long)]
    flavor: Option<String>,
    #[arg(long, default_value_t=true, action=clap::ArgAction::Set, num_args=0..=1, default_missing_value="true")]
    minimal: bool,
    #[arg(long, env = "COLAB_DRIVE_SCOPES")]
    scopes: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_CLIENT_ID")]
    client_id: Option<String>,
    #[arg(long, hide_env_values = true, env = "COLAB_DRIVE_CLIENT_SECRET")]
    client_secret: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_TOKEN_URI")]
    token_uri: Option<Url>,
    #[arg(
        long,
        hide = true,
        default_value = "https://accounts.google.com/o/oauth2/auth"
    )]
    auth_uri: Url,
    #[arg(long)]
    redirect_uri: Option<Url>,
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[arg(long)]
    url_only: bool,
    #[arg(long, conflicts_with_all=["callback_url","url_only"])]
    code: Option<String>,
    #[arg(long, conflicts_with = "url_only")]
    callback_url: Option<Url>,
    #[arg(long)]
    no_browser: bool,
    #[arg(long)]
    include_granted_scopes: bool,
    #[arg(long, env = "COLAB_DRIVE_USER_ID")]
    user_id: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_EMAIL")]
    email: Option<String>,
    #[arg(long)]
    login_hint: Option<String>,
    #[arg(long, default_value_t=300, value_parser=clap::value_parser!(u64).range(1..=3600))]
    consent_timeout: u64,
}
impl ConsentArgs {
    pub(crate) fn for_account(
        flavor: &str,
        hint: Option<String>,
        no_browser: bool,
    ) -> Result<Self> {
        let value = |name: &str| std::env::var(name).ok().filter(|s| !s.trim().is_empty());
        let token_uri = value("COLAB_DRIVE_TOKEN_URI")
            .map(|s| Url::parse(&s).map_err(|_| Error::OAuth("Drive token URI 无效")))
            .transpose()?;
        Ok(Self {
            flavor: Some(flavor.into()),
            minimal: true,
            scopes: value("COLAB_DRIVE_SCOPES"),
            client_id: value("COLAB_DRIVE_CLIENT_ID"),
            client_secret: value("COLAB_DRIVE_CLIENT_SECRET"),
            token_uri,
            auth_uri: Url::parse("https://accounts.google.com/o/oauth2/auth").unwrap(),
            redirect_uri: None,
            port: 0,
            url_only: false,
            code: None,
            callback_url: None,
            no_browser,
            include_granted_scopes: false,
            user_id: value("COLAB_DRIVE_USER_ID"),
            email: value("COLAB_DRIVE_EMAIL"),
            login_hint: hint,
            consent_timeout: 300,
        })
    }
}

#[derive(Args)]
pub struct ImportArgs {
    #[arg(long, env = "COLAB_DRIVE_REFRESH_TOKEN", hide_env_values = true)]
    refresh_token: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_CLIENT_ID")]
    client_id: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_CLIENT_SECRET", hide_env_values = true)]
    client_secret: Option<String>,
    #[arg(
        long,
        env = "COLAB_DRIVE_TOKEN_URI",
        default_value = "https://oauth2.googleapis.com/token"
    )]
    token_uri: Url,
    #[arg(long, env = "COLAB_DRIVE_SCOPES")]
    scopes: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_USER_ID")]
    user_id: Option<String>,
    #[arg(long, env = "COLAB_DRIVE_EMAIL")]
    email: Option<String>,
    #[arg(long, default_value = "imported")]
    flavor: String,
}

fn finish_notice(
    account: &colabcli_rust::models::ProfileId,
    credentials: &drive::DriveCredentials,
) -> Result<()> {
    println!("已保存 Drive 账户 {account} 的授权；尚未验证真实挂载");
    if !credentials
        .oauth
        .scopes
        .iter()
        .any(|s| s == drive::DRIVE_SCOPE)
    {
        eprintln!("Drive 授权未包含完整 drive scope，不能用于 DriveFS 挂载");
    }
    Ok(())
}
pub fn run(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &colabcli_rust::credentials::TokenStore,
) -> Result<()> {
    let Command::Drive { command } = &cli.command else {
        unreachable!()
    };
    if matches!(
        command.as_ref(),
        DriveCommand::Status { .. }
            | DriveCommand::ProbeCleanup { .. }
            | DriveCommand::Mount { .. }
            | DriveCommand::Umount { .. }
            | DriveCommand::Leases { .. }
            | DriveCommand::LeaseCleanup { .. }
    ) {
        return crate::drive_runtime_cli::run(cli, accounts, env, storage, tokens);
    }
    if let DriveCommand::Consent(args) = command.as_ref() {
        prepare_consent_profiles(accounts, env, cli.account.as_deref(), args)?;
    }
    let compute = context::resolve(accounts, env, cli.account.as_deref(), None, false)?.profile;
    let (override_account, for_consent) = match command.as_ref() {
        DriveCommand::Show { drive_account } | DriveCommand::Token { drive_account, .. } => {
            (drive_account.as_deref(), false)
        }
        _ => (None, true),
    };
    let account = context::resolve_drive(accounts, env, &compute, override_account, for_consent)?;
    let mut service = DriveCredentialService::new(storage.clone(), TokenClient::new()?);
    if let Some(endpoint) = &cli.drive_userinfo_url {
        service = service.with_userinfo_endpoint(endpoint.clone())?;
    }
    match command.as_ref() {
        DriveCommand::Show { .. } => {
            let credentials = service.load(&account)?;
            let path = storage.profile_path(&account, "dfs_credentials.json")?;
            if let Some(credentials) = credentials {
                let mut summary = credentials.summary();
                let fields = summary.as_object_mut().expect("Drive summary is an object");
                fields.retain(|key, value| {
                    !matches!(key.as_str(), "access_token" | "expiry" | "scope_source")
                        || !value.is_null()
                });
                fields.insert("account".into(), serde_json::json!(compute));
                fields.insert("drive_account".into(), serde_json::json!(account));
                fields.insert("store".into(), serde_json::json!(path));
                print_json(&summary)?;
            } else {
                println!("[{account}] {}: empty (run `consent`)", path.display());
            }
        }
        DriveCommand::Clear => {
            let existed = service.clear(&account)?;
            let path = storage.profile_path(&account, "dfs_credentials.json")?;
            if existed {
                println!(
                    "removed [{account}] {} (remember to revoke the grant on https://myaccount.google.com/permissions)",
                    path.display()
                );
            } else {
                println!("[{account}] {}: nothing to remove", path.display());
            }
        }
        DriveCommand::Token { whoami, .. } => {
            let credentials = service.token(&account, *whoami, chrono::Utc::now())?;
            let seconds = credentials
                .oauth
                .expiry
                .map_or(3599, |t| (t - chrono::Utc::now()).num_seconds())
                .max(0);
            // This command explicitly requests the access token; other commands never print it.
            print_json(
                &serde_json::json!({"account":account,"access_token":credentials.oauth.token.expose(),"expires_in":seconds,"token_type":"Bearer","scope":credentials.oauth.scopes.join(" "),"user_id":credentials.user_id,"email":credentials.email}),
            )?;
        }
        DriveCommand::Import(args) => {
            let ImportArgs {
                refresh_token,
                client_id,
                client_secret,
                token_uri,
                scopes,
                user_id,
                email,
                flavor,
            } = args.as_ref();
            let mut credentials = drive::imported(
                drive::ImportRequest {
                    refresh_token: Secret::new(
                        refresh_token
                            .as_deref()
                            .ok_or(Error::OAuth("导入需要 refresh_token"))?,
                    ),
                    client_id: client_id
                        .clone()
                        .ok_or(Error::OAuth("导入需要 client_id"))?,
                    client_secret: client_secret.clone().map(Secret::new),
                    token_uri: token_uri.to_string(),
                    scopes: parse_scopes(scopes.as_deref(), false),
                    user_id: user_id.clone(),
                    email: email.clone(),
                },
                chrono::Utc::now(),
            )?;
            credentials.flavor = flavor.clone();
            credentials.validate()?;
            let credentials = service.import(&account, credentials, chrono::Utc::now())?;
            finish_notice(&account, &credentials)?;
        }
        DriveCommand::Consent(args) => consent(cli, env, storage, &account, args)?,

        DriveCommand::Status { .. }
        | DriveCommand::ProbeCleanup { .. }
        | DriveCommand::Mount { .. }
        | DriveCommand::Umount { .. }
        | DriveCommand::Leases { .. }
        | DriveCommand::LeaseCleanup { .. }
        | DriveCommand::Doctor(_)
        | DriveCommand::Version { .. }
        | DriveCommand::Install(_) => unreachable!(),
    }
    Ok(())
}

/// Beginning Drive consent may register named profiles, but finishing a pending
/// exchange and all other Drive operations must resolve existing profiles only.
/// Plan both selectors in one registry transaction so a bad storage selector
/// cannot leave a partially registered compute profile.
fn prepare_consent_profiles(
    accounts: &Accounts,
    env: &Environment,
    explicit: Option<&str>,
    args: &ConsentArgs,
) -> Result<()> {
    use colabcli_rust::{
        accounts::lookup,
        models::{Account, ProfileId},
    };
    if args.code.is_some() || args.callback_url.is_some() {
        return Ok(());
    }
    let compute = explicit
        .filter(|s| !s.is_empty())
        .or(env.account.as_deref());
    let selectors: Vec<_> = compute
        .into_iter()
        .chain(env.drive_account.as_deref())
        .collect();
    let registry = accounts.registry()?;
    let mut missing = false;
    for selector in &selectors {
        match lookup(&registry, selector) {
            Ok(_) => {}
            Err(Error::AccountMissing(_)) => {
                ProfileId::new(*selector)?;
                missing = true;
            }
            Err(error) => return Err(error),
        }
    }
    if !missing {
        return Ok(());
    }
    accounts.update(|registry| {
        for selector in selectors {
            match lookup(registry, selector) {
                Ok(_) => {}
                Err(Error::AccountMissing(_)) => {
                    registry
                        .accounts
                        .insert(ProfileId::new(selector)?, Account::default());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })
}

pub(crate) fn consent(
    cli: &Cli,
    env: &Environment,
    storage: &Storage,
    account: &colabcli_rust::models::ProfileId,
    args: &ConsentArgs,
) -> Result<()> {
    let mut service = DriveCredentialService::new(storage.clone(), TokenClient::new()?);
    if let Some(endpoint) = &cli.drive_userinfo_url {
        service = service.with_userinfo_endpoint(endpoint.clone())?;
    }
    let ConsentArgs {
        flavor,
        minimal,
        scopes,
        client_id,
        client_secret,
        token_uri,
        auth_uri,
        redirect_uri,
        port,
        url_only,
        code,
        callback_url,
        no_browser,
        include_granted_scopes,
        user_id,
        email,
        login_hint,
        consent_timeout,
    } = args;
    if code.is_some() || callback_url.is_some() {
        return finish_notice(
            account,
            &service.finish(
                account,
                code.clone().map(Secret::new),
                callback_url.clone(),
                chrono::Utc::now(),
            )?,
        );
    }
    let login_hint = match login_hint
        .as_ref()
        .filter(|s| !s.is_empty())
        .or_else(|| email.as_ref().filter(|s| !s.is_empty()))
    {
        Some(hint) => Some(hint.clone()),
        None => {
            let registry: Registry = storage
                .read(&storage.path("accounts.json")?)?
                .ok_or_else(|| Error::AccountMissing(account.to_string()))?;
            registry.validate()?;
            registry
                .accounts
                .get(account)
                .and_then(|metadata| metadata.email.clone())
                .filter(|email| !email.is_empty())
        }
    };
    let flavor = flavor.as_deref().unwrap_or(if client_id.is_some() {
        "custom"
    } else if *url_only {
        "sdk"
    } else {
        "dfs"
    });
    let client = if let Some(client_id) = client_id {
        OAuthClient {
            client_id: client_id.clone(),
            client_secret: client_secret.clone().map(Secret::new),
            auth_uri: auth_uri.clone(),
            token_uri: token_uri
                .clone()
                .unwrap_or_else(|| Url::parse("https://oauth2.googleapis.com/token").unwrap()),
        }
    } else {
        let path = cli
            .client_oauth_config
            .clone()
            .or_else(|| {
                env.home
                    .as_ref()
                    .map(|h| h.join(".colab-cli-oauth-config.json"))
            })
            .ok_or(Error::OAuth(
                "请提供 --client-oauth-config；DriveFS client 凭据不内嵌于程序",
            ))?;
        let mut client = OAuthClient::from_file(&path)?;
        if let Some(uri) = token_uri {
            client.token_uri = uri.clone();
        }
        if let Some(secret) = client_secret {
            client.client_secret = Some(Secret::new(secret));
        }
        client
    };
    // Client ID is public compatibility data. The associated client secret
    // must be supplied externally; do not copy upstream embedded secrets.
    if flavor == "dfs" && client.client_id != drive::DRIVEFS_CLIENT_ID {
        return Err(Error::OAuth(
            "dfs flavor 需要匹配的 DriveFS OAuth client；自建 client 请使用 custom",
        ));
    }
    let mut redirect = redirect_uri.clone().unwrap_or_else(|| {
        if flavor == "dfs" {
            Url::parse(&format!("http://localhost:{port}/")).unwrap()
        } else {
            Url::parse(REMOTE_REDIRECT).unwrap()
        }
    });
    let loopback = redirect.scheme() == "http"
        && matches!(
            redirect.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
    if !*url_only && !loopback && !io::stdin().is_terminal() {
        return Err(Error::OAuth(
            "非交互授权请先 --url-only，再 --code 或 --callback-url",
        ));
    }
    let listener = if !*url_only && loopback {
        let bind_host = if redirect.host_str() == Some("[::1]") {
            "::1"
        } else {
            "127.0.0.1"
        };
        let requested = redirect.port().unwrap_or(*port);
        let listener = TcpListener::bind((bind_host, requested))
            .or_else(|_| TcpListener::bind((bind_host, 0)))
            .map_err(|_| Error::OAuth("无法监听 Drive 授权 loopback"))?;
        redirect
            .set_port(Some(
                listener
                    .local_addr()
                    .map_err(|_| Error::OAuth("无法读取 loopback 端口"))?
                    .port(),
            ))
            .map_err(|_| Error::OAuth("无效的 callback 端口"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| Error::OAuthTransport)?;
        Some(listener)
    } else {
        None
    };
    if *url_only && loopback && redirect.port() == Some(0) {
        return Err(Error::OAuth(
            "--url-only 的 loopback redirect 必须指定非零端口",
        ));
    }
    let guard = SignalGuard::install()?;
    let url = service.begin(
        account,
        ConsentRequest {
            flavor: flavor.into(),
            client,
            redirect: redirect.clone(),
            scopes: parse_scopes(scopes.as_deref(), *minimal),
            user_id: user_id.clone(),
            email: email.clone(),
            login_hint,
            include_granted_scopes: *include_granted_scopes,
        },
        chrono::Utc::now(),
    )?;
    println!("{url}");
    io::stdout().flush().map_err(|_| Error::OAuthTransport)?;
    if *url_only {
        eprintln!("打开链接同意后，使用同一账户 drive consent --code 或 --callback-url 完成授权");
        return Ok(());
    }
    if !*no_browser {
        open_browser(&url);
    }
    let credentials = if let Some(listener) = listener {
        wait_callback(
            &listener,
            &redirect,
            &service,
            account,
            Duration::from_secs(*consent_timeout),
            &guard,
        )?
    } else {
        let code = read_authorization_code(Duration::from_secs(*consent_timeout), &guard)?;
        service.finish(account, Some(code), None, chrono::Utc::now())?
    };
    finish_notice(account, &credentials)?;
    Ok(())
}

fn read_authorization_code(timeout: Duration, guard: &SignalGuard) -> Result<Secret> {
    use colabcli_rust::jupyter::{InputHandler, InputRequest};
    let mut terminal = crate::terminal_input::TerminalInput::open()?;
    let request = InputRequest {
        id: "drive-consent-code".into(),
        prompt: "粘贴授权码（不回显）: ".into(),
        password: true,
    };
    let deadline = Instant::now() + timeout;
    loop {
        let signal = guard.flag.load(Ordering::SeqCst);
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        if Instant::now() >= deadline {
            return Err(Error::OAuth("等待 Drive 授权码超时；pending 已保留"));
        }
        if let Some(mut code) = terminal.poll(&request)? {
            let secret = Secret::new(code.trim());
            use zeroize::Zeroize;
            code.zeroize();
            return Ok(secret);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn parse_scopes(scopes: Option<&str>, minimal: bool) -> Vec<String> {
    scopes
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| {
            if minimal {
                drive::MINIMAL_SCOPES.to_vec()
            } else {
                drive::FULL_SCOPES.to_vec()
            }
            .into_iter()
            .map(str::to_owned)
            .collect()
        })
}
fn open_browser(url: &Url) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(target_os = "windows")]
    let command = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let command = "xdg-open";
    let _ = std::process::Command::new(command)
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
fn callback_target(stream: &mut TcpStream, deadline: Instant) -> Result<String> {
    let mut bytes = Vec::new();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| Error::OAuthTransport)?;
    loop {
        if Instant::now() >= deadline {
            return Err(Error::OAuth("Drive callback 读取超时"));
        }
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Err(Error::OAuth("Drive callback 连接提前关闭")),
            Ok(_) => bytes.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return Err(Error::OAuthTransport),
        }
        if bytes.len() > 8192 {
            return Err(Error::OAuth("Drive callback 请求过大"));
        }
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::OAuth("Drive callback 请求无效"))?;
    let mut parts = text.lines().next().unwrap_or_default().split_whitespace();
    if parts.next() != Some("GET") {
        return Err(Error::OAuth("Drive callback 需要 GET"));
    }
    let target = parts
        .next()
        .filter(|s| s.starts_with('/') && !s.starts_with("//"))
        .ok_or(Error::OAuth("Drive callback 路径无效"))?;
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) || parts.next().is_some() {
        return Err(Error::OAuth("Drive callback 请求行无效"));
    }
    Ok(target.into())
}
fn wait_callback(
    listener: &TcpListener,
    redirect: &Url,
    service: &DriveCredentialService,
    account: &colabcli_rust::models::ProfileId,
    timeout: Duration,
    guard: &SignalGuard,
) -> Result<drive::DriveCredentials> {
    let deadline = Instant::now() + timeout;
    loop {
        let signal = guard.flag.load(Ordering::SeqCst);
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        if Instant::now() >= deadline {
            return Err(Error::OAuth("Drive loopback 授权超时；pending 已保留"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let target = callback_target(
                    &mut stream,
                    deadline.min(Instant::now() + Duration::from_secs(2)),
                );
                let result = target.and_then(|target| {
                    let mut callback = redirect.clone();
                    let target = Url::parse(&format!("http://loopback{target}"))
                        .map_err(|_| Error::OAuth("无效 callback URL"))?;
                    if target.fragment().is_some() {
                        return Err(Error::OAuth("Drive callback 路径无效"));
                    }
                    callback.set_path(target.path());
                    callback.set_query(target.query());
                    service.finish(account, None, Some(callback), chrono::Utc::now())
                });
                let (status, body) = if result.is_ok() {
                    ("200 OK", "Authorized. You can close this tab.")
                } else {
                    (
                        "400 Bad Request",
                        "Authorization not completed. Check the terminal.",
                    )
                };
                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                match result {
                    Ok(credentials) => return Ok(credentials),
                    Err(error) => {
                        // Stray traffic cannot consume the flow; valid denial or
                        // exchange failure should return immediately to the user.
                        if matches!(
                            error,
                            Error::OAuth(
                                "OAuth state 不匹配"
                                    | "授权回调地址不匹配"
                                    | "回调缺少唯一 code"
                                    | "Drive callback 需要 GET"
                                    | "Drive callback 路径无效"
                                    | "Drive callback 请求行无效"
                                    | "Drive callback 请求过大"
                                    | "Drive callback 请求无效"
                                    | "Drive callback 读取超时"
                                    | "Drive callback 连接提前关闭"
                            )
                        ) {
                            eprintln!("Drive callback 未完成: {error}");
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(_) => return Err(Error::OAuthTransport),
        }
    }
}
