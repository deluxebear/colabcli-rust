use clap::{ArgGroup, Args};
use colabcli_rust::{
    Error, Result,
    update::{self, UpdateClient},
};
use std::{path::PathBuf, time::Duration};
use url::Url;
#[derive(Args)]
#[command(group(ArgGroup::new("update_action").args(["install","rollback","status"]).multiple(false)))]
pub struct Options {
    #[arg(long,conflicts_with_all=["rollback","status"])]
    manifest: Option<Url>,
    #[arg(long)]
    install: bool,
    #[arg(long)]
    rollback: bool,
    #[arg(long)]
    status: bool,
    /// Existing copy of this CLI; defaults to the running executable.
    #[arg(long, requires = "update_action")]
    install_path: Option<PathBuf>,
    #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    #[arg(long)]
    json: bool,
}
pub fn run(options: &Options) -> Result<()> {
    update::current_target()?;
    let target = || -> Result<PathBuf> {
        options.install_path.clone().map(Ok).unwrap_or_else(|| {
            std::env::current_exe().map_err(|_| Error::Update("无法确定当前可执行文件位置"))
        })
    };
    if options.status {
        return crate::print_json(&update::status(&target()?)?);
    }
    if options.rollback {
        let result = update::rollback(&target()?)?;
        if options.json {
            return crate::print_json(&result);
        }
        println!("已回滚至 {}", result.version);
        if result.cleanup_pending {
            eprintln!("旧暂存文件尚未清理；请用 update --status 核查。");
        }
        return Ok(());
    }
    let uri = options
        .manifest
        .clone()
        .map(Ok)
        .or_else(|| configured_source().transpose())
        .transpose()?
        .ok_or(Error::Update(
            "未配置原生发行源；用 --manifest URL 或 COLAB_CLI_UPDATE_MANIFEST 指定发行清单",
        ))?;
    let client = UpdateClient::new(Duration::from_secs(options.timeout))?;
    let plan = client.check(&uri, env!("CARGO_PKG_VERSION"))?;
    if options.install {
        let result = client.install(&plan, &target()?)?;
        if options.json {
            return crate::print_json(&result);
        }
        if result.status == "installed" {
            println!(
                "已安装 {}；保留一个旧版本，可用 update --rollback 恢复。",
                result.version
            );
        } else {
            println!("当前版本 {} 已是最新。", result.version);
        }
        if result.cleanup_pending {
            eprintln!("旧暂存文件尚未清理；请用 update --status 核查。");
        }
    } else if options.json {
        crate::print_json(&plan)?;
    } else if plan.update_available {
        println!(
            "发现新版本 {}（当前 {}）；用 update --install 安装。",
            plan.latest_version, plan.current_version
        );
    } else {
        println!(
            "当前版本 {} 已是最新（发行源 {}）。",
            plan.current_version, plan.latest_version
        );
    }
    Ok(())
}

fn configured_source() -> Result<Option<Url>> {
    let raw = std::env::var("COLAB_CLI_UPDATE_MANIFEST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            option_env!("COLAB_CLI_BUILTIN_UPDATE_MANIFEST")
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        });
    raw.map(|raw| Url::parse(&raw).map_err(|_| Error::Update("发行源 URL 无效")))
        .transpose()
}

pub fn daily_notice(cli: &crate::Cli) {
    use crate::{AccountCommand, Command};
    use std::io::IsTerminal;
    // Suppress all redirected invocations, including JSON and SSH transport.
    if !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
        || std::env::args_os().any(|arg| arg == "--json" || arg == "--url-only")
    {
        return;
    }
    let eligible = match &cli.command {
        Command::Account { command } => matches!(
            command.as_ref(),
            AccountCommand::List { .. }
                | AccountCommand::Current { .. }
                | AccountCommand::Status { .. }
                | AccountCommand::Use { .. }
                | AccountCommand::SetDrive { .. }
                | AccountCommand::Rename { .. }
                | AccountCommand::Remove { .. }
        ),
        Command::Sessions { .. }
        | Command::Usage { .. }
        | Command::Status { .. }
        | Command::New { .. }
        | Command::Stop { .. } => true,
        _ => false,
    };
    if !eligible {
        return;
    }
    let Some(source) = configured_source().ok().flatten() else {
        return;
    };
    let root = cli.config_dir.clone().or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/colab-cli"))
    });
    let Some(storage) = root.and_then(|root| colabcli_rust::storage::Storage::new(root).ok())
    else {
        return;
    };
    if let Some(latest) = update::daily::notice(
        &storage,
        &source,
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now(),
    ) {
        eprintln!(
            "发现 Colab CLI 新版本 {latest}；用 colab update --install 更新。可在 settings.json 设置 enable_update_check=false 关闭检查。"
        );
    }
}
