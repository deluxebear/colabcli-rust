mod account_cli;
mod account_inventory;
mod account_mutation_cli;
mod account_selection_cli;
mod account_status;
mod completion_cli;
mod diagnostics;
mod drive_cli;
mod drive_runtime_cli;
mod drivemount_cli;
mod repl_editor;
mod ssh_cli;
mod terminal_input;
mod update_cli;
mod utility_cli;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::{AdcProvider, OAuthService, TokenClient},
    cli_compat,
    colab_api::{ApiConfig, ColabApi, RuntimeOptions},
    contents::{self, ContentsClient},
    context::{self, Environment},
    credentials::{SystemKeyring, TokenStorageMode, TokenStore},
    execution::{self, Document, ExecuteOptions},
    history::{History, HistoryQuery},
    jupyter::{ExecutionStatus, ExecutionTimeout, KernelRest},
    models::{AuthProvider, ProfileId},
    runs::{self, PreparedRun, RunOptions},
    sessions::SessionService,
    storage::Storage,
};
use serde_json::json;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
};
use url::Url;

#[derive(Parser)]
#[command(
    name = "colab",
    version,
    about = "Colab CLI",
    disable_help_subcommand = true
)]
struct Cli {
    #[arg(long, global = true, env = "COLAB_CLI_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    #[arg(short = 'a', long, global = true, overrides_with = "account")]
    account: Option<String>,
    #[arg(skip)]
    root_account: Option<String>,
    #[arg(long, global = true, value_enum, ignore_case = true)]
    auth: Option<AuthProvider>,
    #[arg(long = "colab-runtime-endpoint", global = true, hide = true, value_parser = clap::builder::NonEmptyStringValueParser::new())]
    runtime_endpoint: Option<String>,
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
    #[arg(long,global=true,env="COLAB_KEYRING_TIMEOUT",default_value="30",value_parser=clap::value_parser!(u64).range(1..=300))]
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
    #[arg(long, hide = true)]
    drive_userinfo_url: Option<Url>,
    #[command(flatten)]
    completion: completion_cli::Options,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Show help for a command.
    Help {
        command: Option<String>,
    },
    #[command(alias = "acct")]
    Account {
        #[command(subcommand)]
        command: Box<AccountCommand>,
    },
    /// Diagnose built-in extension capabilities and local state without authentication.
    Ext {
        #[command(subcommand)]
        command: diagnostics::ExtCommand,
    },
    /// Create a new session
    New {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        gpu: Option<String>,
        #[arg(long)]
        tpu: Option<String>,
        #[arg(long)]
        high_mem: bool,
        #[arg(long)]
        json: bool,
    },
    /// List active sessions on the server
    Sessions {
        #[arg(long, short = 'A', visible_alias = "all-accounts")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show session status
    Status {
        #[arg(long, conflicts_with_all = ["account", "session"])]
        all: bool,
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Stop a session
    Stop {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long = "colab-expected-endpoint", hide = true, requires = "session", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        expected_endpoint: Option<String>,
    },
    /// 运行时文件操作；不创建 VM。
    #[command(flatten)]
    Files(FileCommand),
    /// Execute code in a session
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
        /// 允许远端 input/getpass；需要 -f 和交互终端，timeout 为整次执行期限。
        #[arg(long, requires = "file")]
        allow_stdin: bool,
        /// 显式使用该 VM 上已有的 kernel，亦用于恢复创建结果未知的状态。
        #[arg(long)]
        kernel_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Start an interactive Python REPL, or execute piped input.
    Repl {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        output_image: Option<PathBuf>,
    },
    /// Connect to a Colab runtime via SSH.
    Ssh {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        proxy_mode: bool,
        #[arg(short = 'i', long)]
        identity: Option<PathBuf>,
        #[arg(long)]
        gpu: Option<String>,
        #[arg(long)]
        tpu: Option<String>,
        #[arg(long)]
        high_mem: bool,
        #[arg(long)]
        rm: bool,
        #[arg(long = "colab-internal-mode", default_value = "proxy", hide = true)]
        internal_mode: String,
        #[arg(long = "colab-expected-endpoint", hide = true, requires = "session", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        expected_endpoint: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Manage independent DriveFS credentials.
    #[command(alias = "dfs")]
    Drive {
        #[command(subcommand)]
        command: Box<drive_cli::DriveCommand>,
    },
    /// Authenticate with Google on the VM.
    Auth {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, default_value = "600", allow_hyphen_values = true)]
        timeout: f64,
        #[arg(long)]
        json: bool,
    },
    /// Mount with native DriveFS by default; --upstream selects official Colab propagation.
    Drivemount {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(default_value = "/content/drive")]
        path: String,
        #[arg(long, default_value = "30", allow_hyphen_values = true)]
        timeout: f64,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        options: Box<drivemount_cli::Options>,
    },
    /// Install python packages on the VM, preferring uv and falling back to pip.
    Install {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(short = 'r', long)]
        requirement: Option<PathBuf>,
        packages: Vec<String>,
        #[arg(long, default_value = "0", allow_hyphen_values = true)]
        timeout: f64,
        #[arg(long)]
        json: bool,
    },
    /// Delete a pending automation kernel without affecting the normal exec kernel.
    AutomationCleanup {
        #[arg(short = 's', long)]
        session: Option<String>,
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
    /// Run a local Python script on a fresh Colab VM, then tear it down.
    Run {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long)]
        gpu: Option<String>,
        #[arg(long)]
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
    /// Manage and view session history logs
    Log {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(short = 'n', long, allow_negative_numbers = true)]
        lines: Option<isize>,
        #[arg(short = 't', long = "type")]
        event_type: Option<String>,
        #[arg(short = 'o', long, requires = "session")]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Usage {
        #[arg(long, short = 'A', visible_alias = "all-accounts")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Version,
    /// Check or install a native release; no Python package manager is used.
    Update {
        #[command(flatten)]
        options: Box<update_cli::Options>,
    },
    /// Print a browser connection URL for an existing local session.
    Url {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(long, default_value = "https://colab.research.google.com")]
        host: String,
        #[arg(long = "open")]
        open_browser: bool,
    },
    /// Open the Colab subscription page.
    Pay {
        #[arg(long)]
        url_only: bool,
    },
    /// Print the bundled native CLI documentation.
    #[command(alias = "README")]
    Readme,
    /// Print bundled agent instructions, including account and Drive workflows.
    #[command(alias = "SKILL")]
    Skill,
    /// Refresh the selected credentials and inspect token identity without printing tokens.
    #[command(hide = true)]
    Whoami {
        #[arg(long)]
        json: bool,
        #[arg(
            long,
            hide = true,
            default_value = "https://oauth2.googleapis.com/tokeninfo"
        )]
        tokeninfo_url: Url,
    },
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
    /// Query usage and live sessions across all configured accounts.
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    Current {
        #[arg(long)]
        json: bool,
    },
    Add(Box<account_cli::AddArgs>),
    #[command(alias = "switch")]
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
    #[command(alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
    },
    Login(Box<account_cli::LoginArgs>),
    /// 完成上次中断的账户重命名或删除事务。
    Repair,
}
fn cli_command() -> clap::Command {
    Cli::command()
        .subcommand_required(false)
        .arg_required_else_help(true)
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().collect();
    let parser = cli_command();
    let root_account = account_selection_cli::root_selector(&parser, &arguments);
    let parsed = parser.try_get_matches_from(arguments);
    let parsed = parsed.and_then(|matches| {
        if matches
            .get_one::<completion_cli::Shell>("show_completion")
            .is_some()
            || matches
                .get_one::<completion_cli::Shell>("install_completion")
                .is_some()
        {
            match completion_cli::run(&matches) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        Cli::from_arg_matches(&matches)
    });
    let mut cli = match parsed {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::{ContextKind, ContextValue, ErrorKind};
            if error.kind() == ErrorKind::MissingRequiredArgument
                && matches!(error.get(ContextKind::InvalidArg), Some(ContextValue::Strings(missing))
                    if missing.as_slice() == ["<SCRIPT_AND_ARGS>..."])
            {
                let _ = cli_compat::argument_error(Some("run"), "Missing argument 'script'.");
                std::process::exit(2);
            }
            if error.kind() == ErrorKind::InvalidValue {
                let diagnostic = error.to_string();
                let first = diagnostic.lines().next().unwrap_or("");
                for flag in ["--gpu", "--tpu"] {
                    if first.contains(flag) && first.contains("required") {
                        let _ = cli_compat::argument_error(
                            None,
                            &format!("Option '{flag}' requires an argument."),
                        );
                        std::process::exit(2);
                    }
                }
            }
            error.exit();
        }
    };
    cli.root_account = root_account;
    if let Err(error) = run(&cli) {
        if let Some(code) = compatible_failure(&cli, &error) {
            std::process::exit(code);
        }
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
                "请先执行 account login；默认 OAuth 配置可自动获取，也可用 -c 指定。登录和后续命令需使用相同账户、配置目录及凭据存储方式。"
            );
        }
        std::process::exit(match error {
            Error::ScriptExit(code) => code,
            _ => 1,
        });
    }
    update_cli::daily_notice(&cli);
}
fn runtime_guard_session(cli: &Cli) -> Option<&str> {
    match &cli.command {
        Command::Exec { session, .. }
        | Command::Repl { session, .. }
        | Command::RestartKernel { session } => session.as_deref(),
        Command::Files(command) => match command {
            FileCommand::Ls { session, .. }
            | FileCommand::Upload { session, .. }
            | FileCommand::Download { session, .. }
            | FileCommand::Rm { session, .. }
            | FileCommand::Edit { session, .. } => session.as_deref(),
        },
        Command::Drive { command } => match command.as_ref() {
            drive_cli::DriveCommand::Mount { session, .. }
            | drive_cli::DriveCommand::Umount { session, .. }
            | drive_cli::DriveCommand::Status { session, .. } => session.as_deref(),
            _ => None,
        },
        _ => None,
    }
}

fn check_runtime_endpoint(cli: &Cli, storage: &Storage, profile: &ProfileId) -> Result<()> {
    if let Some(expected) = &cli.runtime_endpoint {
        let _guard = storage.profile_lock(profile)?;
        if storage.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let name = runtime_guard_session(cli)
            .ok_or(Error::KernelProtocol("缺少 endpoint 条件的显式会话"))?;
        let sessions = storage.sessions(profile)?;
        let state = sessions
            .get(name)
            .ok_or_else(|| Error::SessionMissing(name.into()))?;
        if &state.endpoint != expected {
            return Err(Error::KernelProtocol(
                "会话 endpoint 与预期不匹配；未连接运行时",
            ));
        }
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    if cli.runtime_endpoint.is_some() {
        let session = runtime_guard_session(cli);
        if session.is_none_or(str::is_empty) {
            eprintln!("--colab-runtime-endpoint 仅用于显式 -s 的执行、文件或 Drive 运行时命令");
            return Err(Error::CliExit(2));
        }
    }
    if let Command::Help { command } = &cli.command {
        let mut root = cli_command();
        if let Some(name) = command {
            if let Some(subcommand) = root.find_subcommand_mut(name) {
                subcommand.print_help().map_err(|e| Error::Io {
                    path: "stdout".into(),
                    source: e,
                })?;
            } else {
                eprintln!("No such command '{name}'.");
                return Err(Error::CliExit(2));
            }
        } else {
            root.print_help().map_err(|e| Error::Io {
                path: "stdout".into(),
                source: e,
            })?;
        }
        println!();
        return Ok(());
    }
    if matches!(cli.command, Command::Version) {
        println!("Version: {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Command::Update { options } = &cli.command {
        return update_cli::run(options);
    }
    if let Some(result) = utility_cli::run_early(cli) {
        return result;
    }
    if let Command::Ext { command } = &cli.command {
        return diagnostics::run(cli, command);
    }
    if let Command::Drive { command } = &cli.command {
        match command.as_ref() {
            drive_cli::DriveCommand::Doctor(args) => {
                return diagnostics::run(cli, &diagnostics::ExtCommand::Doctor(args.clone()));
            }
            drive_cli::DriveCommand::Version { .. } => {
                return diagnostics::run(cli, &diagnostics::ExtCommand::Version { json: true });
            }
            drive_cli::DriveCommand::Install(args) => {
                return diagnostics::run(cli, &diagnostics::ExtCommand::Install(args.clone()));
            }
            _ => {}
        }
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
        return run_control(cli, &accounts, &env, &storage, tokens);
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
    account_selection_cli::preflight(cli, &accounts, &env, command)?;
    if matches!(command, AccountCommand::Status { .. }) {
        return run_control(cli, &accounts, &env, &storage, tokens);
    }
    match command {
        AccountCommand::List { json: as_json } => {
            account_inventory::list(cli, &accounts, &env, *as_json)?
        }
        AccountCommand::Current { json: as_json } => {
            account_inventory::current(cli, &accounts, &env, *as_json)?
        }
        AccountCommand::Add(args) => {
            account_cli::add(cli, &accounts, &env, &storage, &tokens, args)?
        }
        AccountCommand::Use { .. }
        | AccountCommand::SetDrive { .. }
        | AccountCommand::Rename { .. }
        | AccountCommand::Remove { .. } => {
            account_mutation_cli::run(cli, &accounts, &env, command)?
        }
        AccountCommand::Login(args) => {
            account_cli::login(cli, &accounts, &env, &storage, &tokens, args)?
        }
        AccountCommand::Repair | AccountCommand::Status { .. } => unreachable!(),
    }
    Ok(())
}
fn announce_session_route(selected: &context::AccountContext, session: Option<&str>) {
    if let (context::AccountSource::Session, Some(name)) = (selected.source, session) {
        print_session_route(name, &selected.profile);
    }
}

fn print_session_route(name: &str, profile: &ProfileId) {
    eprintln!(
        "[colab] Routing session '{}' -> account '{profile}'.",
        name.escape_debug()
    );
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
    if let Some(result) = drivemount_cli::dispatch(cli, accounts, env, storage, &tokens) {
        return result;
    }
    if matches!(cli.command, Command::Whoami { .. }) {
        return utility_cli::whoami(cli, accounts, env, storage, &tokens);
    }
    if matches!(cli.command, Command::Drive { .. }) {
        return drive_cli::run(cli, accounts, env, storage, &tokens);
    }
    if matches!(cli.command, Command::Ssh { .. }) {
        return ssh_cli::run(cli, accounts, env, storage, &tokens);
    }
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
        cli_compat::validate_environment(environment)?;
        if !std::path::Path::new(&script[0]).is_file() {
            eprintln!("[colab] Script not found: {}", script[0]);
            return Err(Error::CliExit(2));
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
    let mut terminal_input = if matches!(
        &cli.command,
        Command::Exec {
            allow_stdin: true,
            ..
        }
    ) {
        if !io::stdin().is_terminal() {
            return Err(Error::KernelProtocol(
                "此交互操作需要终端，不能从管道读取远端输入",
            ));
        }
        Some(terminal_input::TerminalInput::open()?)
    } else if matches!(
        &cli.command,
        Command::Auth { .. } | Command::Drivemount { .. }
    ) && io::stdin().is_terminal()
    {
        Some(terminal_input::TerminalInput::open()?)
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
        cli_compat::validate_environment(environment)?;
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
    if let Command::Repl { output_image, .. } = &cli.command {
        if let Some(path) = output_image {
            contents::validate_download_target(path)?;
        }
        if !io::stdin().is_terminal() {
            document = Some(Document::for_repl_stdin(io::stdin().lock())?);
        }
    }
    if let Command::Auth { timeout, .. } | Command::Drivemount { timeout, .. } = &cli.command {
        if !timeout.is_finite() {
            return Err(Error::KernelProtocol("执行 timeout 必须为有限秒数"));
        }
        let code = match &cli.command {
            Command::Drivemount { path, .. } => colabcli_rust::automation::mount_code(path)?,
            _ => colabcli_rust::automation::auth_code().to_owned(),
        };
        document = Some(Document::automation(
            &code,
            if matches!(cli.command, Command::Auth { .. }) {
                "auth"
            } else {
                "drivemount"
            },
        )?);
    }
    if let Command::Install {
        packages,
        requirement,
        timeout,
        ..
    } = &cli.command
    {
        if !timeout.is_finite() {
            return Err(Error::KernelProtocol("执行 timeout 必须为有限秒数"));
        }
        if packages.is_empty() && requirement.is_none() {
            println!("[colab] No packages or requirements specified.");
            return Err(Error::CliExit(1));
        }
        if let Some(path) = requirement {
            if !path.is_file() {
                println!(
                    "[colab] Requirements file '{}' not found locally.",
                    path.display()
                );
                return Err(Error::CliExit(1));
            }
        }
        document = Some(colabcli_rust::installation::prepare(
            packages,
            requirement.as_deref(),
        )?);
    }
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
        Some(RuntimeOptions::new(gpu.as_deref(), tpu.as_deref(), *high_mem).map_err(|error| {
            if let Some((flag, value)) = tpu.as_ref().map(|value| ("--tpu", value)).or_else(|| gpu.as_ref().map(|value| ("--gpu", value))) {
                let supported = if flag == "--tpu" { "V5E1, V6E1" } else { "T4, L4, G4, H100, A100" };
                let command = if matches!(cli.command, Command::Run { .. }) { "run" } else { "new" };
                cli_compat::argument_error(Some(command), &format!("Invalid value for {flag}: unsupported value '{value}'. Supported: {supported}."))
            } else { error }
        })?)
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
    if let Command::Account { command } = &cli.command {
        if let AccountCommand::Status { json } = command.as_ref() {
            return account_status::run(cli, accounts, env, storage, &tokens, &config, *json);
        }
    }
    if let Command::Sessions { all, json } | Command::Usage { all, json } = &cli.command {
        account_selection_cli::validate(accounts, env, cli.account.as_deref())?;
        if *all || *json {
            let kind = if matches!(cli.command, Command::Sessions { .. }) {
                account_status::Kind::Sessions
            } else {
                account_status::Kind::Usage
            };
            return account_status::run_query(
                cli,
                accounts,
                env,
                storage,
                &tokens,
                &config,
                account_status::Query {
                    kind,
                    all: *all,
                    json: *json,
                },
            );
        }
    }
    let session = match &cli.command {
        Command::Status { session, .. } => session.as_deref(),
        Command::Stop { session, .. } => session.as_deref(),
        Command::Files(command) => command.session(),
        Command::Exec { session, .. }
        | Command::Repl { session, .. }
        | Command::Install { session, .. }
        | Command::AutomationCleanup { session, .. }
        | Command::Auth { session, .. }
        | Command::Drivemount { session, .. }
        | Command::Kernels { session, .. }
        | Command::RestartKernel { session } => session.as_deref(),
        _ => None,
    };
    if matches!(cli.command, Command::Status { all: true, .. }) {
        return run_inventory(cli, accounts, env, storage, &tokens, &config);
    }
    let selected = context::resolve(accounts, env, cli.account.as_deref(), session, true)?;
    announce_session_route(&selected, session);
    let service = || authenticated_service(cli, storage, &tokens, &selected, &config);
    match &cli.command {
        Command::Ssh { .. }
        | Command::Drive { .. }
        | Command::Ext { .. }
        | Command::Url { .. }
        | Command::Pay { .. }
        | Command::Readme
        | Command::Skill
        | Command::Whoami { .. } => unreachable!(),
        Command::Run {
            timeout,
            keep,
            json,
            script,
            ..
        } => {
            let signals = runs::SignalGuard::install()?;
            let timeout = if *timeout <= 0.0 {
                None
            } else {
                Some(duration(*timeout)?)
            };
            let prepared = prepared_run.unwrap();
            let run_name = prepared.name.clone();
            let runtime = options.unwrap();
            // Share account-bound credentials across allocation, execution and cleanup.
            // Requests still refresh an expired bearer through the original provider.
            let mut run_service = None;
            let service = || {
                if let Some(cached) = &run_service {
                    return Ok(SessionService::clone(cached));
                }
                let created = service()?;
                run_service = Some(created.clone());
                Ok(created)
            };
            if !*json {
                if runtime.high_mem && runtime.shape().is_none() {
                    eprintln!(
                        "[colab] --high-mem ignored: this accelerator only offers one machine shape."
                    );
                }
                eprintln!("[colab] Creating session '{run_name}'...");
            }
            let outcome = runs::run_with_progress(
                storage,
                selected.profile.clone(),
                prepared,
                RunOptions {
                    runtime,
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
                |progress| {
                    if *json {
                        return;
                    }
                    match progress {
                        runs::RunProgress::Allocated => eprintln!(
                            "[colab] Session READY ({run_name}). Executing {}...",
                            script[0]
                        ),
                        runs::RunProgress::Releasing => {
                            eprintln!("[colab] Stopping session '{run_name}'...")
                        }
                        runs::RunProgress::Released => eprintln!("[colab] Session terminated."),
                    }
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
            let options = options.unwrap();
            let name = match session {
                Some(name) => name.clone(),
                None => storage
                    .read::<serde_json::Value>(
                        &storage.profile_path(&selected.profile, "control_operation.json")?,
                    )?
                    .and_then(|operation| operation["name"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| {
                        colabcli_rust::colab_api::notebook_identity()[..6].to_owned()
                    }),
            };
            if !*json {
                if options.high_mem && options.shape().is_none() {
                    eprintln!(
                        "[colab] --high-mem ignored: this accelerator only offers one machine shape."
                    );
                }
                println!("[colab] Creating session '{name}'...");
            }
            let accelerator = options.accelerator.clone();
            let view = match service()?.create(Some(&name), options) {
                Err(Error::ControlStatus(412)) if !*json => {
                    eprintln!(
                        "[colab] Allocation refused (precondition failed). This can mean too many active sessions, or a temporary usage or capacity limit for the requested runtime. Run `colab stop` to free up a session, wait and retry, or try a different accelerator."
                    );
                    return Err(Error::CliExit(1));
                }
                Err(Error::ControlStatus(400)) if !*json && accelerator != "NONE" => {
                    eprintln!(
                        "[colab] Backend rejected accelerator '{accelerator}'. You may not have quota or entitlement for this accelerator on your account. Try a different one (e.g. --gpu T4) or omit --gpu/--tpu for a CPU runtime."
                    );
                    return Err(Error::CliExit(1));
                }
                result => result?,
            };
            if *json {
                print_json(&view)?;
            } else {
                println!("[colab] Session READY.");
            }
        }
        Command::Sessions { .. } => {
            let views = status_with_notice(&service()?, storage, &selected.profile, None, true)?;
            if views.is_empty() {
                println!("[colab] No active sessions found on server.");
            } else {
                for view in views {
                    println!("{}", session_line(&view, false));
                }
            }
        }
        Command::Status { session, json, .. } => {
            let views = match status_with_notice(
                &service()?,
                storage,
                &selected.profile,
                session.as_deref(),
                !*json,
            ) {
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
        Command::Stop {
            session,
            expected_endpoint,
        } => {
            if let Some(name) = session {
                let pending = storage.profile_path(&selected.profile, "control_operation.json")?;
                if !storage.sessions(&selected.profile)?.contains_key(name) && !pending.exists() {
                    if expected_endpoint.is_some() {
                        return Err(Error::SessionMissing(name.clone()));
                    }
                    println!("[colab] Session '{name}' not found.");
                    return Ok(());
                }
                println!("[colab] Stopping session '{name}'...");
                service()?.stop_if_endpoint(name, expected_endpoint.as_deref())?;
            } else {
                service()?.stop_resolved(|names, pruned| {
                    if pruned > 0 {
                        println!("[colab] Pruned {pruned} stale local session(s).");
                    }
                    let name = choose_cli_session(names, false)?;
                    println!("[colab] Stopping session '{name}'...");
                    Ok(name)
                })?;
            }
            println!("[colab] Session terminated.");
        }
        Command::Install {
            session,
            timeout,
            json,
            ..
        } => {
            let service = service()?;
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                *json,
                || Ok(service.clone()),
            )?;
            if !*json {
                println!("[colab] Installing packages on {name} (preferring uv)...");
            }
            let signals = runs::SignalGuard::install()?;
            let results = execution::execute_document(
                &service,
                Some(&name),
                document.as_mut().unwrap(),
                ExecuteOptions {
                    timeout: ExecutionTimeout::Idle(if *timeout <= 0.0 {
                        None
                    } else {
                        Some(duration(*timeout)?)
                    }),
                    http_timeout: config.timeout,
                    kernel_id: None,
                    cancellation: Some(signals.flag.clone()),
                },
                |change| {
                    if *json {
                        Ok(())
                    } else {
                        execution::render_output(change)
                    }
                },
            );
            let signal = signals.flag.load(std::sync::atomic::Ordering::SeqCst);
            if signal != 0 {
                return Err(Error::CliExit(128 + signal as i32));
            }
            render_automation_results(results?, *json)?;
        }
        Command::AutomationCleanup {
            session,
            kernel_id,
            json,
        } => {
            let service = service()?;
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                *json,
                || Ok(service.clone()),
            )?;
            let deleted = execution::automation_cleanup(
                &service,
                Some(&name),
                kernel_id.as_deref(),
                config.timeout,
            )?;
            if *json {
                print_json(&json!({"kernel_id":deleted,"deleted":deleted.is_some()}))?;
            } else if let Some(id) = deleted {
                println!("[colab] Deleted automation kernel '{id}'.");
            } else {
                println!("[colab] No pending automation kernel.");
            }
        }
        Command::Auth {
            session,
            timeout,
            json,
        }
        | Command::Drivemount {
            session,
            timeout,
            json,
            ..
        } => {
            let service = service()?;
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                *json,
                || Ok(service.clone()),
            )?;
            let _mount_guard = if matches!(cli.command, Command::Drivemount { .. }) {
                Some(drivemount_cli::upstream_guard(
                    cli, accounts, env, storage, &service, &selected, &name,
                )?)
            } else {
                None
            };
            let signals = runs::SignalGuard::install()?;
            let document = document.as_mut().unwrap();
            let mut unavailable = terminal_input::UnavailableInput;
            let terminal: &mut dyn colabcli_rust::jupyter::InputHandler =
                match terminal_input.as_mut() {
                    Some(terminal) => terminal,
                    None => &mut unavailable,
                };
            if !*json {
                match &cli.command {
                    Command::Auth { .. } => {
                        println!("[colab] Starting Google Auth flow on {name}...")
                    }
                    Command::Drivemount { path, .. } => {
                        println!("[colab] Mounting Google Drive to '{path}' on {name}...")
                    }
                    _ => unreachable!(),
                }
            }
            let mut input = colabcli_rust::automation::AutomationInput::new(&service, terminal)
                .with_history(name.clone());
            let results = execution::execute_document_with_input(
                &service,
                Some(&name),
                document,
                ExecuteOptions {
                    timeout: ExecutionTimeout::Deadline(if *timeout <= 0.0 {
                        None
                    } else {
                        Some(duration(*timeout)?)
                    }),
                    http_timeout: config.timeout,
                    kernel_id: None,
                    cancellation: Some(signals.flag.clone()),
                },
                |change| {
                    if *json {
                        Ok(())
                    } else {
                        execution::render_output(change)
                    }
                },
                Some(&mut input),
            );
            drop(input);
            drop(terminal_input.take());
            let signal = signals.flag.load(std::sync::atomic::Ordering::SeqCst);
            if signal != 0 {
                return Err(Error::CliExit(128 + signal as i32));
            }
            let results = results?;
            render_automation_results(results, *json)?;
        }
        Command::Repl {
            session,
            output_image,
        } => {
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                false,
                service,
            )?;
            let service = service()?;
            if io::stdin().is_terminal() {
                repl_editor::run(&service, &name, config.timeout, output_image.as_deref())?;
            } else {
                let guard = runs::SignalGuard::install()?;
                let document = document.as_mut().unwrap();
                document.use_official_output();
                let results = execution::execute_document(
                    &service,
                    Some(&name),
                    document,
                    ExecuteOptions {
                        timeout: ExecutionTimeout::Idle(None),
                        http_timeout: config.timeout,
                        kernel_id: None,
                        cancellation: Some(guard.flag.clone()),
                    },
                    |change| {
                        execution::render_output_with_image_target(change, output_image.as_deref())
                    },
                )?;
                let signal = guard.flag.load(std::sync::atomic::Ordering::SeqCst);
                if signal != 0 {
                    return Err(Error::CliExit(128 + signal as i32));
                }
                if let Some(path) = output_image {
                    repl_editor::save_image(&results, path)?;
                }
                for result in results {
                    if let Some(error) = result.history_error {
                        eprintln!("历史记录失败: {error}");
                    }
                    if result.status == ExecutionStatus::Unknown {
                        return Err(Error::ExecutionUnknown(result.msg_id));
                    }
                }
            }
        }
        Command::Exec {
            session,
            timeout,
            output_image,
            kernel_id,
            allow_stdin,
            json,
            ..
        } => {
            let timeout = if *timeout <= 0.0 {
                None
            } else {
                Some(duration(*timeout)?)
            };
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                *json,
                service,
            )?;
            let document = document.as_mut().unwrap();
            if !*json {
                document.use_official_output();
                if let Command::Exec {
                    file: Some(file), ..
                } = &cli.command
                {
                    if document.output_path.is_some() {
                        println!("[colab] Parsing notebook '{}'...", file.display());
                    }
                }
            }
            let signal_guard = if *allow_stdin {
                Some(runs::SignalGuard::install()?)
            } else {
                None
            };
            let execution_result = execution::execute_document_with_input(
                &service()?,
                Some(&name),
                document,
                ExecuteOptions {
                    timeout: if *allow_stdin {
                        ExecutionTimeout::Deadline(timeout)
                    } else {
                        ExecutionTimeout::Idle(timeout)
                    },
                    http_timeout: config.timeout,
                    kernel_id: kernel_id.as_deref(),
                    cancellation: signal_guard.as_ref().map(|guard| guard.flag.clone()),
                },
                |change| {
                    if *json {
                        Ok(())
                    } else {
                        execution::render_output_with_image_target(change, output_image.as_deref())
                    }
                },
                terminal_input
                    .as_mut()
                    .map(|handler| handler as &mut dyn colabcli_rust::jupyter::InputHandler),
            );
            // Restore terminal echo before handling errors, signals, or rendering results.
            drop(terminal_input.take());
            if let Some(guard) = &signal_guard {
                let signal = guard.flag.load(std::sync::atomic::Ordering::SeqCst);
                if signal != 0 {
                    return Err(Error::CliExit(128 + signal as i32));
                }
            }

            if !*json {
                if let Some(path) = &document.output_path {
                    println!(
                        "[colab] Saving notebook with outputs to '{}'...",
                        path.display()
                    );
                }
            }
            let results = execution_result?;
            if let Some(path) = output_image {
                execution::save_last_image(&results, path)?;
                if !*json
                    && results.iter().flat_map(|r| &r.outputs).any(|output| {
                        output.data["data"].get("image/png").is_some()
                            || output.data["data"].get("image/jpeg").is_some()
                    })
                {
                    println!("\n[Image saved to: {}]", path.display());
                }
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
                        if let Some(limit) = timeout.filter(|_| !*json && result.timed_out) {
                            let kind = if *allow_stdin {
                                "总期限"
                            } else {
                                "空闲期限"
                            };
                            eprintln!(
                                "等待本次执行消息超时（{kind} {} 秒）。可用 --timeout 调整。",
                                limit.as_secs_f64()
                            );
                        }
                        return Err(Error::ExecutionUnknown(result.msg_id.clone()));
                    }
                    ExecutionStatus::Failed if *json => return Err(Error::ExecutionFailed),
                    ExecutionStatus::Failed => {}
                    ExecutionStatus::Succeeded => {}
                }
            }
        }
        Command::Kernels { session, json } => {
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                *json,
                service,
            )?;
            let models = service()?.with_session(Some(&name), |state| {
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
            let name = resolve_cli_session(
                storage,
                &selected.profile,
                session.as_deref(),
                false,
                service,
            )?;
            execution::restart_kernel_compatible(&service()?, Some(&name), config.timeout)?;
        }
        Command::Files(command) => {
            let timeout = config.timeout;
            let json = matches!(command, FileCommand::Ls { json: true, .. });
            let name =
                resolve_cli_session(storage, &selected.profile, command.session(), json, service)?;
            match command {
                FileCommand::Ls { path, json, .. } => {
                    let entries =
                        service()?.with_file_operation(Some(&name), "ls", path, |state| {
                            ContentsClient::new(state, timeout)?.list(path)
                        })?;
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
                FileCommand::Upload { remote, .. } => {
                    service()?.with_file_operation(Some(&name), "upload", remote, |state| {
                        ContentsClient::new(state, timeout)?
                            .upload_bytes(remote, local_upload.as_ref().unwrap())
                            .map(|_| ())
                    })?;
                    if let FileCommand::Upload { local, .. } = command {
                        println!("[colab] Uploaded '{}' to '{remote}'", local.display());
                    }
                }
                FileCommand::Download { remote, local, .. } => {
                    service()?.with_file_operation(Some(&name), "download", remote, |state| {
                        ContentsClient::new(state, timeout)?.download(remote, local)
                    })?;
                    println!("[colab] Downloaded '{remote}' to '{}'", local.display());
                }
                FileCommand::Rm { path, .. } => {
                    service()?.with_file_operation(Some(&name), "rm", path, |state| {
                        ContentsClient::new(state, timeout)?.remove(path)
                    })?;
                    println!("[colab] Deleted {path}");
                }
                FileCommand::Edit { path, .. } => {
                    let (name, endpoint, before) =
                        service()?.with_session(Some(&name), |state| {
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
                    if changed {
                        println!("[colab] Edited and uploaded '{path}'");
                    } else {
                        println!("[colab] No changes made to '{path}'");
                    }
                }
            }
        }
        Command::Usage { .. } => {
            let usage = service()?.usage()?;
            println!(
                "Current balance: {:.2} compute units\nUsage rate: {:.2}/hr\nActive assignments: {}",
                usage.current_balance, usage.consumption_rate_hourly, usage.assignments_count
            );
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
    check_runtime_endpoint(cli, storage, &selected.profile)?;
    let cancelled = || -> Result<()> {
        let signal = selected
            .cancellation
            .as_ref()
            .map_or(0, |flag| flag.load(std::sync::atomic::Ordering::SeqCst));
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        Ok(())
    };
    cancelled()?;
    let client = TokenClient::new()?;
    let source = match cli.auth.unwrap_or(selected.provider) {
        AuthProvider::Oauth2 => colabcli_rust::auth::ControlCredentials::oauth(
            OAuthService::new(storage.clone(), tokens.clone(), client),
            selected.profile.clone(),
        ),
        AuthProvider::Adc => colabcli_rust::auth::ControlCredentials::adc(
            AdcProvider::from_context(selected, client),
        ),
    }
    .with_cancellation(selected.cancellation.clone());
    Ok(SessionService::new(
        storage.clone(),
        selected.profile.clone(),
        if runtime_guard_session(cli).is_some()
            || matches!(cli.command, Command::Stop { session: None, .. })
        {
            ColabApi::with_deferred_credentials_source(config.clone(), source)?
        } else {
            ColabApi::with_credentials_source(config.clone(), source)?
        },
    )
    .with_expected_endpoint(cli.runtime_endpoint.clone()))
}

fn run_inventory(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    config: &ApiConfig,
) -> Result<()> {
    let signals = env
        .allow_executables
        .then(runs::SignalGuard::install)
        .transpose()?;
    let profiles: Vec<_> = accounts.registry()?.accounts.into_keys().collect();
    let query = |profile: &ProfileId| -> Result<serde_json::Value> {
        let mut selected = context::resolve(accounts, env, Some(profile.as_str()), None, false)?;
        selected.cancellation = signals.as_ref().map(|guard| guard.flag.clone());
        let service = authenticated_service(cli, storage, tokens, &selected, config)?;
        serde_json::to_value(
            service
                .status(None)?
                .into_iter()
                .filter(|view| view.name.is_some())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| Error::ControlProtocol("汇总序列化失败"))
    };
    let rows = colabcli_rust::inventory::collect(&profiles, query);
    let signal = signals.as_ref().map_or(0, |guard| {
        guard.flag.load(std::sync::atomic::Ordering::SeqCst)
    });
    if signal != 0 {
        return Err(Error::CliExit(128 + signal as i32));
    }
    let Command::Status { json: as_json, .. } = cli.command else {
        unreachable!()
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
    let (profile, routed) =
        context::resolve_history(accounts, env, cli.account.as_deref(), session.as_deref())?;
    let history = History::new(storage.clone(), profile.clone());
    if let Some(name) = session {
        if routed {
            print_session_route(name, &profile);
        }
        let events = history.read_skipping(
            name,
            HistoryQuery {
                event_type: event_type.as_deref(),
                lines: lines.filter(|n| *n > 0).map(isize::unsigned_abs),
            },
            lines.filter(|n| *n < 0).map_or(0, isize::unsigned_abs),
        )?;
        if !*json
            && events.is_empty()
            && history
                .read(
                    name,
                    HistoryQuery {
                        event_type: None,
                        lines: Some(1),
                    },
                )?
                .is_empty()
        {
            println!("[colab] No history found for session '{name}'.");
            return Ok(());
        }
        if let Some(path) = output {
            cli_compat::export_history(&events, name, path)?;
            println!("[colab] Exported history to '{}'.", path.display());
        } else if *json {
            print_json(&events)?;
        } else {
            for event in events {
                println!("{}", cli_compat::history_line(&event));
            }
        }
    } else {
        let names = history.sessions()?;
        if *json {
            print_json(&names)?;
        } else if names.is_empty() {
            println!("[colab] No session history found.");
        } else {
            println!("[colab] Sessions with history logs:");
            for name in names {
                println!("  {name}");
            }
        }
    }
    Ok(())
}

fn render_automation_results(
    results: Vec<colabcli_rust::jupyter::ExecutionResult>,
    json: bool,
) -> Result<()> {
    if json {
        print_json(&results)?;
    }
    for result in results {
        if let Some(error) = &result.history_error {
            eprintln!("自动化历史记录失败: {error}");
        }
        match result.status {
            ExecutionStatus::Unknown => {
                if !json {
                    if let Some(error) = result.error {
                        eprintln!("{error}");
                    }
                }
                return Err(Error::ExecutionUnknown(result.msg_id));
            }
            ExecutionStatus::Failed => return Err(Error::ExecutionFailed),
            ExecutionStatus::Succeeded => {}
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
        if let Some(pending) = &view.automation_pending {
            line.push_str(&format!(
                " | Automation: {}",
                pending["phase"].as_str().unwrap_or("invalid")
            ));
        }
        if let Some(pending) = &view.drive_probe {
            line.push_str(&format!(
                " | Drive probe: {}",
                pending["phase"].as_str().unwrap_or("invalid")
            ));
        }
    }
    line
}

fn compatible_failure(cli: &Cli, error: &Error) -> Option<i32> {
    if let Error::CliExit(code) | Error::ScriptExit(code) = error {
        return Some(*code);
    }
    if matches!(
        cli.command,
        Command::Exec { json: false, .. } | Command::Repl { .. } | Command::RestartKernel { .. }
    ) {
        if let Error::SessionMissing(name) = error {
            println!("[colab] Session '{name}' not found.");
            return Some(1);
        }
    }
    if let Command::Files(command) = &cli.command {
        if matches!(command, FileCommand::Ls { json: true, .. }) {
            return None;
        }
        if let FileCommand::Upload { local, .. } = command {
            if matches!(error, Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
            {
                println!("[colab] Local file '{}' not found.", local.display());
                return Some(1);
            }
        }
        if let Error::SessionMissing(name) = error {
            println!("[colab] Session '{name}' not found.");
            return Some(1);
        }
        if matches!(error, Error::RemoteMissing) {
            let prefix = match command {
                FileCommand::Download { .. } => "Download failed",
                FileCommand::Upload { .. } => "Upload failed",
                _ => "Error",
            };
            println!(
                "[colab] {prefix}: File or directory not found: {}",
                command.path()
            );
            return Some(1);
        }
        if matches!(command, FileCommand::Download { .. })
            && matches!(error, Error::RemoteDirectory)
        {
            println!(
                "[colab] Download failed: Cannot download a directory: {}",
                command.path()
            );
            return Some(1);
        }
    }
    None
}

fn status_with_notice(
    service: &SessionService,
    storage: &Storage,
    profile: &ProfileId,
    name: Option<&str>,
    show_notice: bool,
) -> Result<Vec<colabcli_rust::sessions::SessionView>> {
    let before = storage.sessions(profile)?;
    let result = service.status(name);
    if show_notice && (result.is_ok() || matches!(&result, Err(Error::SessionMissing(_)))) {
        let after = storage.sessions(profile)?;
        let pruned = before
            .keys()
            .filter(|name| !after.contains_key(*name))
            .count();
        if pruned > 0 {
            println!("[colab] Pruned {pruned} stale local session(s).");
        }
    }
    result
}

fn resolve_cli_session(
    storage: &Storage,
    profile: &ProfileId,
    name: Option<&str>,
    json: bool,
    service: impl FnOnce() -> Result<SessionService>,
) -> Result<String> {
    if let Some(name) = name {
        return Ok(name.into());
    }
    let mut names = Vec::new();
    if !storage.sessions(profile)?.is_empty() {
        names = status_with_notice(&service()?, storage, profile, None, !json)?
            .into_iter()
            .filter_map(|view| view.name)
            .collect();
    }
    choose_cli_session(names, json)
}

fn choose_cli_session(names: Vec<String>, json: bool) -> Result<String> {
    match names.as_slice() {
        [name] => {
            if !json {
                println!("[colab] Using unique session '{name}'.");
            }
            Ok(name.clone())
        }
        [] => {
            if !json {
                println!("[colab] Error: No active sessions found. Create one with 'colab new'.");
            }
            Err(Error::CliExit(1))
        }
        _ => {
            if !json {
                println!(
                    "[colab] Error: Multiple active sessions found. Specify one with -s: {}",
                    names.join(", ")
                );
            }
            Err(Error::CliExit(1))
        }
    }
}
