use chrono::Utc;
use clap::{Parser, Subcommand};
use colabcli_rust::{
    Error, Result,
    accounts::{Accounts, lookup},
    auth::{AdcProvider, OAuthClient, OAuthService, REMOTE_REDIRECT, TokenClient},
    colab_api::{ApiConfig, ColabApi, RuntimeOptions},
    contents::{self, ContentsClient},
    context::{self, Environment},
    credentials::{SystemKeyring, TokenStorageMode, TokenStore},
    execution::{self, Document, ExecuteOptions},
    history::{History, HistoryQuery},
    jupyter::{ExecutionStatus, ExecutionTimeout, KernelRest},
    models::{Account, AuthProvider, ProfileId, Secret},
    runs::{self, PreparedRun, RunOptions},
    sessions::SessionService,
    storage::Storage,
};
use serde_json::json;
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
};
use url::Url;

#[derive(Parser)]
#[command(name = "colab", version, about = "多账户 Colab 客户端")]
struct Cli {
    #[arg(long, global = true, env = "COLAB_CLI_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    #[arg(short = 'a', long, global = true)]
    account: Option<String>,
    #[arg(long, global = true, value_enum, ignore_case = true)]
    auth: Option<AuthProvider>,
    #[arg(short = 'c', long = "client-oauth-config", global = true)]
    client_oauth_config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_enum,
        env = "COLAB_TOKEN_STORAGE",
        default_value = "auto"
    )]
    token_storage: TokenStorageMode,
    /// 系统凭据库的调用等待上限；不取消已发给操作系统的请求。
    #[arg(long,global=true,env="COLAB_KEYRING_TIMEOUT",default_value="2",value_parser=clap::value_parser!(u64).range(1..=300))]
    keyring_timeout: u64,
    #[arg(
        long,
        env = "COLAB_CLI_CONTROL_PLANE_URL",
        default_value = "https://colab.research.google.com",
        hide = true
    )]
    control_plane_url: Url,
    #[arg(
        long,
        env = "COLAB_CLI_REQUEST_TIMEOUT",
        default_value = "20",
        hide = true
    )]
    request_timeout: f64,
    #[arg(
        long,
        env = "COLAB_CLI_ASSIGN_RETRIES",
        default_value = "3",
        hide = true
    )]
    assign_retries: u32,
    #[arg(
        long,
        env = "COLAB_CLI_RETRY_BACKOFF",
        default_value = "10",
        hide = true
    )]
    retry_backoff: f64,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(alias = "acct")]
    Account {
        #[command(subcommand)]
        command: Box<AccountCommand>,
    },
    /// 分配 VM；相同参数可恢复中断的分配。
    New {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, conflicts_with = "tpu", num_args = 0..=1, default_missing_value = "T4")]
        gpu: Option<String>,
        #[arg(long, conflicts_with = "gpu", num_args = 0..=1, default_missing_value = "v5e1")]
        tpu: Option<String>,
        #[arg(long)]
        high_mem: bool,
        #[arg(long)]
        json: bool,
    },
    /// 查询后端分配，包含没有本地名称的会话。
    Sessions {
        #[arg(long, conflicts_with = "account")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// 同步并显示本地命名会话的状态。
    Status {
        #[arg(long, conflicts_with_all = ["account", "session"])]
        all: bool,
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 释放所选会话的 VM；失败时保留清理日志。
    Stop {
        #[arg(short = 's', long)]
        session: String,
    },
    /// 运行时文件操作；不创建 VM。
    #[command(flatten)]
    Files(FileCommand),
    /// 执行 stdin、Python 文件或 notebook；结果未知时不自动重放。
    Exec {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        #[arg(long, default_value = "30", allow_hyphen_values = true)]
        timeout: f64,
        #[arg(long = "env")]
        environment: Vec<String>,
        #[arg(long)]
        output_image: Option<PathBuf>,
        /// 显式使用该 VM 上已有的 kernel，亦用于恢复创建结果未知的状态。
        #[arg(long)]
        kernel_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 查询该会话的 kernel ID，供显式恢复使用。
    Kernels {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    RestartKernel {
        #[arg(short = 's', long)]
        session: Option<String>,
    },
    /// 新建 VM 执行脚本，结束后释放；脚本路径后的参数原样传递。
    Run {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, conflicts_with = "tpu")]
        gpu: Option<String>,
        #[arg(long, conflicts_with = "gpu")]
        tpu: Option<String>,
        #[arg(long)]
        high_mem: bool,
        #[arg(long)]
        keep: bool,
        #[arg(long, default_value = "30", allow_hyphen_values = true)]
        timeout: f64,
        #[arg(long = "env")]
        environment: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(required=true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true, value_name = "SCRIPT_AND_ARGS")]
        script: Vec<String>,
    },
    /// 仅清理已有 run 计划；不会重放脚本或申请 VM。
    RunCleanup {
        id: String,
    },
    /// 列出尚未完成清理的 run 计划（离线）。
    Runs,
    /// 查询历史；会话停止后仍可离线导出。
    Log {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(short = 'n', long)]
        lines: Option<usize>,
        #[arg(short = 't', long = "type")]
        event_type: Option<String>,
        #[arg(short = 'o', long, requires = "session")]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Usage {
        #[arg(long, conflicts_with = "account")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Version,
}
#[derive(Subcommand)]
enum FileCommand {
    Ls {
        #[arg(default_value = "content")]
        path: String,
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Upload {
        local: PathBuf,
        remote: String,
        #[arg(short = 's', long)]
        session: Option<String>,
    },
    Download {
        remote: String,
        local: PathBuf,
        #[arg(short = 's', long)]
        session: Option<String>,
    },
    Rm {
        path: String,
        #[arg(short = 's', long)]
        session: Option<String>,
    },
    Edit {
        path: String,
        #[arg(short = 's', long)]
        session: Option<String>,
    },
}
impl FileCommand {
    fn session(&self) -> Option<&str> {
        match self {
            Self::Ls { session, .. }
            | Self::Upload { session, .. }
            | Self::Download { session, .. }
            | Self::Rm { session, .. }
            | Self::Edit { session, .. } => session.as_deref(),
        }
    }
    fn path(&self) -> &str {
        match self {
            Self::Ls { path, .. } | Self::Rm { path, .. } | Self::Edit { path, .. } => path,
            Self::Upload { remote, .. } | Self::Download { remote, .. } => remote,
        }
    }
}
#[derive(Subcommand)]
enum AccountCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Current {
        #[arg(long)]
        json: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        adc_file: Option<PathBuf>,
    },
    Use {
        name: String,
    },
    SetDrive {
        storage: Option<String>,
        #[arg(long)]
        default: bool,
        #[arg(long)]
        clear: bool,
    },
    Rename {
        old: String,
        new: String,
    },
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
    },
    Login {
        name: Option<String>,
        #[arg(long,conflicts_with_all=["code","callback_url"])]
        url_only: bool,
        #[arg(long, conflicts_with = "callback_url")]
        code: Option<String>,
        #[arg(long)]
        callback_url: Option<Url>,
        #[arg(long,default_value=REMOTE_REDIRECT)]
        redirect_uri: Url,
    },
    /// 完成上次中断的账户重命名或删除事务。
    Repair,
}
fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        if matches!(
            error,
            Error::CredentialUnavailable
                | Error::CredentialTimeout(_)
                | Error::CredentialBackend(_)
        ) {
            eprintln!(
                "如选择文件凭据，可先用 --token-storage file account login 登录，后续命令同样指定 --token-storage file；该选项不会导出或解锁已有系统凭据。"
            );
        } else if matches!(error, Error::NeedsAuth) {
            eprintln!(
                "请先执行 account login；OAuth client 配置用 -c 指定，或放在 ~/.colab-cli-oauth-config.json。登录和后续命令需使用相同账户、配置目录及凭据存储方式。"
            );
        }
        std::process::exit(match error {
            Error::ScriptExit(code) => code,
            _ => 1,
        });
    }
}
fn run(cli: Cli) -> Result<()> {
    if matches!(cli.command, Command::Version) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let env = Environment::capture();
    let root = cli
        .config_dir
        .clone()
        .or_else(|| env.home.as_ref().map(|p| p.join(".config/colab-cli")))
        .ok_or(Error::OAuth("请指定 --config-dir"))?;
    let storage = Storage::new(root)?;
    let tokens = TokenStore::new(
        storage.clone(),
        Arc::new(SystemKeyring::with_timeout(
            std::time::Duration::from_secs(cli.keyring_timeout),
        )?),
        cli.token_storage,
    );
    let accounts = Accounts::new(storage.clone(), tokens.clone());
    if !matches!(cli.command, Command::Account { .. }) {
        accounts.bootstrap()?;
        return run_control(&cli, &accounts, &env, &storage, tokens);
    }
    let Command::Account { command } = &cli.command else {
        unreachable!()
    };
    let command = command.as_ref();
    if matches!(command, AccountCommand::Repair) {
        accounts.recover()?;
        return accounts.bootstrap();
    }
    accounts.bootstrap()?;
    match command {
        AccountCommand::List { json: as_json } => {
            // Offline inventory deliberately never opens the OS keyring or requests OAuth.
            let r = accounts.registry()?;
            if *as_json {
                print_json(&r)?;
            } else {
                for (id, a) in &r.accounts {
                    println!(
                        "{} {} {}",
                        if id == &r.active { "*" } else { " " },
                        id,
                        a.email.as_deref().unwrap_or("")
                    );
                }
            }
        }
        AccountCommand::Current { json: as_json } => {
            let selected = context::resolve(&accounts, &env, cli.account.as_deref(), None, false)?;
            if *as_json {
                print_json(
                    &json!({"account":selected.profile,"source":format!("{:?}",selected.source).to_lowercase()}),
                )?;
            } else {
                println!("{}", selected.profile);
            }
        }
        AccountCommand::Add {
            name,
            email,
            adc_file,
        } => {
            let adc_file = adc_file
                .as_ref()
                .map(|p| {
                    std::fs::canonicalize(p).map_err(|e| Error::Io {
                        path: p.clone(),
                        source: e,
                    })
                })
                .transpose()?;
            let provider = cli.auth.unwrap_or(if adc_file.is_some() {
                AuthProvider::Adc
            } else {
                AuthProvider::Oauth2
            });
            accounts.add(
                ProfileId::new(name.clone())?,
                Account {
                    email: email.clone(),
                    auth_provider: provider,
                    adc_credentials_file: adc_file.map(|p| p.to_string_lossy().into_owned()),
                    ..Account::default()
                },
            )?;
            println!("已添加账户 {name}；尚未进行在线认证");
        }
        AccountCommand::Use { name } => accounts.use_account(name)?,
        AccountCommand::SetDrive {
            storage: drive,
            default,
            clear,
        } => {
            if *clear == drive.is_some() {
                return Err(Error::OAuth("指定存储账户或 --clear"));
            }
            let r = accounts.registry()?;
            let drive = drive.as_deref().map(|s| lookup(&r, s)).transpose()?;
            let compute = if *default {
                None
            } else {
                Some(
                    context::resolve(&accounts, &env, cli.account.as_deref(), None, false)?.profile,
                )
            };
            accounts.set_drive(compute.as_ref(), drive.as_ref())?;
        }
        AccountCommand::Rename { old, new } => {
            let r = accounts.registry()?;
            accounts.rename(&lookup(&r, old)?, &ProfileId::new(new.clone())?)?;
        }
        AccountCommand::Remove { name, force } => {
            accounts.remove(&lookup(&accounts.registry()?, name)?, *force)?
        }
        AccountCommand::Login {
            name,
            url_only,
            code,
            callback_url,
            redirect_uri,
        } => {
            let selected = context::resolve(
                &accounts,
                &env,
                name.as_deref().or(cli.account.as_deref()),
                None,
                false,
            )?;
            let provider = cli.auth.unwrap_or(selected.provider);
            let client = TokenClient::new()?;
            if provider == AuthProvider::Adc {
                if *url_only || code.is_some() || callback_url.is_some() {
                    return Err(Error::OAuth("ADC 登录不接受 OAuth 授权码参数"));
                }
                let path = selected.adc_file.ok_or(Error::NeedsAuth)?;
                let _token = AdcProvider::new(path, client).access_token(Utc::now())?;
                println!(
                    "账户 {} 的 ADC token 已验证；尚未验证 Colab 权限",
                    selected.profile
                );
            } else {
                let service = OAuthService::new(storage.clone(), tokens, client);
                if code.is_some() || callback_url.is_some() {
                    service.finish(
                        &selected.profile,
                        code.clone().map(Secret::new),
                        callback_url.clone(),
                        Utc::now(),
                    )?;
                    println!("账户 {} 已保存授权", selected.profile);
                } else {
                    if !*url_only && !io::stdin().is_terminal() {
                        return Err(Error::OAuth("非交互环境请使用 --url-only，然后 --code"));
                    }
                    let config = cli
                        .client_oauth_config
                        .clone()
                        .or_else(|| {
                            env.home
                                .as_ref()
                                .map(|p| p.join(".colab-cli-oauth-config.json"))
                        })
                        .ok_or(Error::OAuth("请指定 --client-oauth-config"))?;
                    let r = accounts.registry()?;
                    let url = service.begin(
                        &selected.profile,
                        OAuthClient::from_file(&config)?,
                        redirect_uri.clone(),
                        r.accounts[&selected.profile].email.as_deref(),
                        Utc::now(),
                    )?;
                    println!("{url}");
                    if !*url_only {
                        eprint!("完成授权后粘贴 code: ");
                        io::stderr().flush().map_err(|e| Error::Io {
                            path: "stderr".into(),
                            source: e,
                        })?;
                        let mut input = String::new();
                        io::stdin().read_line(&mut input).map_err(|e| Error::Io {
                            path: "stdin".into(),
                            source: e,
                        })?;
                        service.finish(
                            &selected.profile,
                            Some(Secret::new(input.trim())),
                            None,
                            Utc::now(),
                        )?;
                        println!("账户 {} 已保存授权", selected.profile);
                    }
                }
            }
        }
        AccountCommand::Repair => unreachable!(),
    }
    Ok(())
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let value = serde_json::to_string_pretty(value).map_err(|_| Error::OAuth("JSON 输出失败"))?;
    println!("{value}");
    Ok(())
}

fn run_control(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: TokenStore,
) -> Result<()> {
    if matches!(cli.command, Command::Log { .. } | Command::Runs) {
        return run_offline(cli, accounts, env, storage);
    }
    let prepared_run = if let Command::Run {
        script,
        environment,
        session,
        timeout,
        ..
    } = &cli.command
    {
        if !timeout.is_finite() {
            return Err(Error::Run("timeout 必须为有限秒数"));
        }
        Some(PreparedRun::prepare(
            std::path::Path::new(&script[0]),
            &script[1..],
            environment,
            session.as_deref(),
        )?)
    } else {
        None
    };
    // Validate hardware and HTTP configuration before requesting credentials.
    let mut document = if let Command::Exec {
        file,
        environment,
        timeout,
        output_image,
        ..
    } = &cli.command
    {
        if !timeout.is_finite() {
            return Err(Error::KernelProtocol("执行 timeout 必须为有限秒数"));
        }
        if let Some(path) = output_image {
            contents::validate_download_target(path)?;
        }
        let document = if let Some(path) = file {
            Document::from_file(path, environment)?
        } else {
            if io::stdin().is_terminal() {
                return Err(Error::KernelProtocol("请通过 -f 或 stdin 提供代码"));
            }
            Document::from_stdin(io::stdin().lock(), environment)?
        };
        if !document.has_code() {
            document.save()?;
            return Ok(());
        }
        Some(document)
    } else {
        None
    };
    let local_upload = if let Command::Files(command) = &cli.command {
        contents::validate_remote_path(command.path(), !matches!(command, FileCommand::Ls { .. }))?;
        match command {
            FileCommand::Upload { local, .. } => Some(contents::read_local_file(local)?),
            FileCommand::Download { local, .. } => {
                contents::validate_download_target(local)?;
                None
            }
            _ => None,
        }
    } else {
        None
    };
    let options = if let Command::New {
        gpu, tpu, high_mem, ..
    }
    | Command::Run {
        gpu, tpu, high_mem, ..
    } = &cli.command
    {
        Some(RuntimeOptions::new(
            gpu.as_deref(),
            tpu.as_deref(),
            *high_mem,
        )?)
    } else {
        None
    };
    let duration = |seconds: f64| {
        std::time::Duration::try_from_secs_f64(seconds)
            .map_err(|_| Error::ControlProtocol("超时/退避必须为有限非负秒数"))
    };
    let config = ApiConfig {
        base_url: cli.control_plane_url.clone(),
        timeout: duration(cli.request_timeout)?,
        retries: cli.assign_retries,
        backoff: duration(cli.retry_backoff)?,
    };
    config.validate()?;
    let session = match &cli.command {
        Command::Status { session, .. } => session.as_deref(),
        Command::Stop { session } => Some(session.as_str()),
        Command::Files(command) => command.session(),
        Command::Exec { session, .. }
        | Command::Kernels { session, .. }
        | Command::RestartKernel { session } => session.as_deref(),
        _ => None,
    };
    if matches!(
        cli.command,
        Command::Status { all: true, .. }
            | Command::Sessions { all: true, .. }
            | Command::Usage { all: true, .. }
    ) {
        return run_inventory(cli, accounts, env, storage, &tokens, &config);
    }
    let selected = context::resolve(accounts, env, cli.account.as_deref(), session, true)?;
    let service = || authenticated_service(cli, storage, &tokens, &selected, &config);
    match &cli.command {
        Command::Run {
            timeout,
            keep,
            json,
            ..
        } => {
            let signals = runs::SignalGuard::install()?;
            let timeout = if *timeout <= 0.0 {
                None
            } else {
                Some(duration(*timeout)?)
            };
            let prepared = prepared_run.unwrap();
            if !*json {
                eprintln!(
                    "正在创建 run {}，会话 {}，账户 {}",
                    prepared.id, prepared.name, selected.profile
                );
            }
            let outcome = runs::run(
                storage,
                selected.profile.clone(),
                prepared,
                RunOptions {
                    runtime: options.unwrap(),
                    keep: *keep,
                    timeout: ExecutionTimeout::Idle(timeout),
                    http_timeout: config.timeout,
                    cancellation: signals.flag.clone(),
                },
                service,
                |change| {
                    if *json {
                        return Ok(());
                    }
                    if matches!(change, colabcli_rust::jupyter::output::OutputChange::Append(output) | colabcli_rust::jupyter::output::OutputChange::Update {output,..} if output.data.get("data").is_some_and(|data|data.get(runs::RESULT_MIME).is_some()))
                    {
                        return Ok(());
                    }
                    execution::render_output(change)
                },
            )?;
            if *json {
                print_json(&outcome)?;
            } else {
                if let Some(error) = &outcome.execution_error {
                    eprintln!("执行失败: {error}");
                }
                if let Some(error) = &outcome.history_error {
                    eprintln!("run 历史记录失败: {error}");
                }
                if let Some(error) = &outcome.kernel_cleanup_error {
                    eprintln!("kernel 关闭请求失败: {error}；VM 释放状态以清理结果为准");
                }
                if let Some(error) = &outcome.cleanup_error {
                    eprintln!(
                        "清理待恢复，run {}: {error}；使用同一账户 run-cleanup {}",
                        outcome.run_id, outcome.run_id
                    );
                }
                for result in &outcome.execution {
                    if let Some(error) = &result.history_error {
                        eprintln!("历史记录失败: {error}");
                    }
                }
                eprintln!(
                    "run {}：退出码 {}，清理状态 {:?}",
                    outcome.run_id, outcome.exit_code, outcome.cleanup
                );
            }
            if outcome.exit_code != 0 {
                return Err(Error::ScriptExit(outcome.exit_code));
            }
            if outcome.cleanup == runs::Cleanup::Pending || outcome.cleanup_error.is_some() {
                return Err(Error::Run("脚本退出码为 0，但 VM 清理尚未完成"));
            }
        }
        Command::RunCleanup { id } => {
            let cleanup = runs::cleanup(storage, &selected.profile, id, service)?;
            println!("run {id} 清理状态 {cleanup:?}");
        }
        Command::New { session, json, .. } => {
            let view = service()?.create(session.as_deref(), options.unwrap())?;
            if *json {
                print_json(&view)?;
            } else {
                println!(
                    "已分配 {} {} ({})",
                    view.name.as_deref().unwrap_or(""),
                    view.endpoint,
                    view.account
                );
            }
        }
        Command::Sessions { json, .. } => {
            let views = service()?.status(None)?;
            if *json {
                print_json(&views)?;
            } else if views.is_empty() {
                println!("[colab] No active sessions found on server.");
            } else {
                for view in views {
                    println!("{}", session_line(&view, false));
                }
            }
        }
        Command::Status { session, json, .. } => {
            let views = match service()?.status(session.as_deref()) {
                Err(Error::SessionMissing(_)) if !*json && session.is_some() => {
                    println!(
                        "[colab] Session '{}' not found.",
                        session.as_deref().unwrap()
                    );
                    return Ok(());
                }
                result => result?,
            };
            let views: Vec<_> = views
                .into_iter()
                .filter(|view| view.name.is_some())
                .collect();
            if *json {
                print_json(&views)?;
            } else if views.is_empty() {
                println!("[colab] No active sessions.");
            } else {
                for view in views {
                    println!("{}", session_line(&view, true));
                    if let Some((file, cell, time)) = view.last_execution {
                        println!(
                            "  Last Execution: {file}{} at {time}",
                            cell.filter(|cell| !cell.is_empty())
                                .map(|cell| format!(" | Cell: {cell}"))
                                .unwrap_or_default()
                        );
                    }
                }
            }
        }
        Command::Stop { session } => {
            service()?.stop(session)?;
            println!("已释放 {session}");
        }
        Command::Exec {
            session,
            timeout,
            output_image,
            kernel_id,
            json,
            ..
        } => {
            let timeout = if *timeout <= 0.0 {
                None
            } else {
                Some(duration(*timeout)?)
            };
            let results = execution::execute_document(
                &service()?,
                session.as_deref(),
                document.as_mut().unwrap(),
                ExecuteOptions {
                    timeout: ExecutionTimeout::Idle(timeout),
                    http_timeout: config.timeout,
                    kernel_id: kernel_id.as_deref(),
                    cancellation: None,
                },
                |change| {
                    if *json {
                        Ok(())
                    } else {
                        execution::render_output(change)
                    }
                },
            )?;
            if let Some(path) = output_image {
                execution::save_last_image(&results, path)?;
            }
            if *json {
                print_json(&results)?;
            }
            for result in &results {
                if let Some(error) = &result.history_error {
                    eprintln!("历史记录失败: {error}");
                }
                match result.status {
                    ExecutionStatus::Unknown => {
                        return Err(Error::ExecutionUnknown(result.msg_id.clone()));
                    }
                    ExecutionStatus::Failed => return Err(Error::ExecutionFailed),
                    ExecutionStatus::Succeeded => {}
                }
            }
        }
        Command::Kernels { session, json } => {
            let models = service()?.with_session(session.as_deref(), |state| {
                KernelRest::new(state, config.timeout)?.list()
            })?;
            if *json {
                let rows: Vec<_> = models
                    .iter()
                    .map(|m| json!({"id":m.id,"execution_state":m.execution_state}))
                    .collect();
                print_json(&rows)?;
            } else {
                for model in models {
                    println!(
                        "{} {}",
                        model.id,
                        model.execution_state.as_deref().unwrap_or("unknown")
                    );
                }
            }
        }
        Command::RestartKernel { session } => {
            execution::restart_kernel(&service()?, session.as_deref(), config.timeout)?;
            println!("已请求重启 kernel；旧执行状态已清除");
        }
        Command::Files(command) => {
            let timeout = config.timeout;
            match command {
                FileCommand::Ls {
                    path,
                    session,
                    json,
                } => {
                    let entries = service()?.with_file_operation(
                        session.as_deref(),
                        "ls",
                        path,
                        |state| ContentsClient::new(state, timeout)?.list(path),
                    )?;
                    if *json {
                        let metadata: Vec<_> = entries.iter().map(|entry| json!({"name":entry.name,"path":entry.path,"type":entry.kind})).collect();
                        print_json(&metadata)?;
                    } else {
                        for entry in entries {
                            println!(
                                "{}{}",
                                entry.name,
                                if entry.kind == "directory" { "/" } else { "" }
                            );
                        }
                    }
                }
                FileCommand::Upload {
                    remote, session, ..
                } => {
                    service()?.with_file_operation(
                        session.as_deref(),
                        "upload",
                        remote,
                        |state| {
                            ContentsClient::new(state, timeout)?
                                .upload_bytes(remote, local_upload.as_ref().unwrap())
                                .map(|_| ())
                        },
                    )?;
                    println!("已上传 {remote}");
                }
                FileCommand::Download {
                    remote,
                    local,
                    session,
                } => {
                    service()?.with_file_operation(
                        session.as_deref(),
                        "download",
                        remote,
                        |state| ContentsClient::new(state, timeout)?.download(remote, local),
                    )?;
                    println!("已下载 {}", local.display());
                }
                FileCommand::Rm { path, session } => {
                    service()?.with_file_operation(session.as_deref(), "rm", path, |state| {
                        ContentsClient::new(state, timeout)?.remove(path)
                    })?;
                    println!("已删除 {path}");
                }
                FileCommand::Edit { path, session } => {
                    let (name, endpoint, before) =
                        service()?.with_session(session.as_deref(), |state| {
                            let before =
                                match ContentsClient::new(state, timeout)?.download_bytes(path) {
                                    Ok(bytes) => bytes,
                                    Err(Error::RemoteMissing) => Vec::new(),
                                    Err(error) => return Err(error),
                                };
                            Ok((state.name.clone(), state.endpoint.clone(), before))
                        })?;
                    let changed = contents::edit_with_access(
                        path,
                        || Ok(before),
                        |bytes| {
                            service()?.with_file_operation(Some(&name), "edit", path, |state| {
                                if state.endpoint != endpoint {
                                    return Err(Error::ContentsProtocol(
                                        "编辑期间会话已被替换；没有上传更改",
                                    ));
                                }
                                ContentsClient::new(state, timeout)?
                                    .upload_bytes(path, bytes)
                                    .map(|_| ())
                            })
                        },
                        run_editor,
                    )?;
                    println!(
                        "{}",
                        if changed {
                            "已上传编辑结果"
                        } else {
                            "内容没有变化"
                        }
                    );
                }
            }
        }
        Command::Usage { json, .. } => {
            let usage = service()?.usage()?;
            if *json {
                print_json(&usage)?;
            } else {
                println!(
                    "CCU 余额: {}；每小时消耗: {}；会话数: {}",
                    usage.current_balance, usage.consumption_rate_hourly, usage.assignments_count
                );
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn run_editor(path: &std::path::Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    if editor.is_none() && !io::stdin().is_terminal() {
        return Err(Error::ContentsProtocol("非交互编辑需指定 VISUAL 或 EDITOR"));
    }
    // The user's editor value is an intentional local shell command; the filename is a positional argument.
    // Remote paths and file contents are never interpolated into this command.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec {} \"$1\"", editor.as_deref().unwrap_or("vi")))
        .arg("colab-editor")
        .arg(path)
        .status()
        .map_err(|_| Error::EditorFailed)?;
    if !status.success() {
        return Err(Error::EditorFailed);
    }
    Ok(())
}

fn authenticated_service(
    cli: &Cli,
    storage: &Storage,
    tokens: &TokenStore,
    selected: &context::AccountContext,
    config: &ApiConfig,
) -> Result<SessionService> {
    let client = TokenClient::new()?;
    let bearer = match cli.auth.unwrap_or(selected.provider) {
        AuthProvider::Oauth2 => OAuthService::new(storage.clone(), tokens.clone(), client)
            .access_token(&selected.profile, Utc::now())?,
        AuthProvider::Adc => {
            AdcProvider::new(selected.adc_file.clone().ok_or(Error::NeedsAuth)?, client)
                .access_token(Utc::now())?
        }
    };
    Ok(SessionService::new(
        storage.clone(),
        selected.profile.clone(),
        ColabApi::new(config.clone(), bearer)?,
    ))
}

fn run_inventory(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    config: &ApiConfig,
) -> Result<()> {
    let profiles: Vec<_> = accounts.registry()?.accounts.into_keys().collect();
    let query = |profile: &ProfileId| -> Result<serde_json::Value> {
        let selected = context::resolve(accounts, env, Some(profile.as_str()), None, false)?;
        let service = authenticated_service(cli, storage, tokens, &selected, config)?;
        match &cli.command {
            Command::Status { .. } => serde_json::to_value(
                service
                    .status(None)?
                    .into_iter()
                    .filter(|view| view.name.is_some())
                    .collect::<Vec<_>>(),
            ),
            Command::Sessions { .. } => serde_json::to_value(service.status(None)?),
            Command::Usage { .. } => serde_json::to_value(service.usage()?),
            _ => unreachable!(),
        }
        .map_err(|_| Error::ControlProtocol("汇总序列化失败"))
    };
    let rows = colabcli_rust::inventory::collect(&profiles, query);
    let as_json = match cli.command {
        Command::Status { json, .. }
        | Command::Sessions { json, .. }
        | Command::Usage { json, .. } => json,
        _ => unreachable!(),
    };
    if as_json {
        return print_json(&rows);
    }
    for row in rows {
        if let Some(error) = row.error {
            println!("{}: {error}", row.account);
        } else if let Some(result) = row.result {
            println!("{}: {result}", row.account);
        }
    }
    Ok(())
}

fn run_offline(cli: &Cli, accounts: &Accounts, env: &Environment, storage: &Storage) -> Result<()> {
    if matches!(cli.command, Command::Runs) {
        let selected = context::resolve(accounts, env, cli.account.as_deref(), None, false)?;
        return print_json(&runs::pending(storage, &selected.profile)?);
    }
    let Command::Log {
        session,
        lines,
        event_type,
        output,
        json,
    } = &cli.command
    else {
        unreachable!()
    };
    let selected = if cli.account.is_none() && env.account.is_none() {
        if let Some(name) = session {
            let registry = accounts.registry()?;
            let active = History::new(storage.clone(), registry.active.clone());
            let profile = if active.has_session(name)? {
                registry.active
            } else {
                let mut owners = Vec::new();
                for profile in registry.accounts.keys() {
                    if History::new(storage.clone(), profile.clone()).has_session(name)? {
                        owners.push(profile.clone());
                    }
                }
                match owners.len() {
                    0 => registry.active,
                    1 => owners.remove(0),
                    _ => return Err(Error::Ambiguous(name.clone())),
                }
            };
            context::resolve(accounts, env, Some(profile.as_str()), None, false)?
        } else {
            context::resolve(accounts, env, None, None, false)?
        }
    } else {
        context::resolve(accounts, env, cli.account.as_deref(), None, false)?
    };
    let history = History::new(storage.clone(), selected.profile);
    if let Some(name) = session {
        let events = history.read(
            name,
            HistoryQuery {
                event_type: event_type.as_deref(),
                lines: *lines,
            },
        )?;
        if let Some(path) = output {
            colabcli_rust::history::export(&events, name, path)?;
            println!("已导出 {}", path.display());
        } else if *json {
            print_json(&events)?;
        } else {
            for event in events {
                println!(
                    "{}",
                    serde_json::to_string(&event).map_err(|_| Error::History("历史输出失败"))?
                );
            }
        }
    } else {
        let names = history.sessions()?;
        if *json {
            print_json(&names)?;
        } else {
            for name in names {
                println!("{name}");
            }
        }
    }
    Ok(())
}

fn session_line(view: &colabcli_rust::sessions::SessionView, with_status: bool) -> String {
    let hardware = if view.accelerator == "NONE" {
        "CPU"
    } else {
        view.accelerator.as_str()
    };
    let shape = match view.machine_shape.as_str() {
        "STANDARD" => "Standard",
        "HIGH_RAM" => "High-RAM",
        unknown => unknown,
    };
    let mut line = format!(
        "[{}] {} | Hardware: {} | Shape: {} | Variant: {}",
        view.name.as_deref().unwrap_or("?"),
        view.endpoint,
        hardware,
        shape,
        view.variant
    );
    if with_status {
        line.push_str(&format!(
            " | Status: {}",
            view.running
                .as_ref()
                .map(|running| format!("BUSY ({running})"))
                .unwrap_or_else(|| "IDLE".into())
        ));
    }
    line
}
