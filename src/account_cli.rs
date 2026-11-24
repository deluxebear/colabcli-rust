//! Account registration and explicit compute/Drive authorization orchestration.
use crate::{Cli, drive_cli};
use chrono::Utc;
use clap::Args;
use colabcli_rust::{
    Error, Result,
    accounts::{Accounts, lookup},
    auth::{AdcProvider, OAuthClient, OAuthService, REMOTE_REDIRECT, TokenClient},
    context::{self, Environment},
    credentials::TokenStore,
    models::{Account, AuthProvider, ProfileId, Secret},
    storage::Storage,
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use url::Url;
use zeroize::Zeroize;

#[derive(Args)]
pub struct DriveOptions {
    #[arg(long, conflicts_with = "no_drive")]
    with_drive: bool,
    #[arg(long)]
    no_drive: bool,
    #[arg(long, conflicts_with = "no_dfs")]
    with_dfs: bool,
    #[arg(long)]
    no_dfs: bool,
    #[arg(long, default_value = "dfs", value_parser = ["dfs", "sdk", "custom"])]
    flavor: String,
    /// Disable browser launch for Drive consent; compute consent uses a pasted code.
    #[arg(long)]
    no_browser: bool,
}
impl DriveOptions {
    fn enabled(&self, default: bool) -> bool {
        if self.with_dfs {
            true
        } else if self.no_dfs {
            false
        } else if self.with_drive {
            true
        } else if self.no_drive {
            false
        } else {
            default
        }
    }
}
#[derive(Args)]
pub struct AddArgs {
    name: String,
    #[arg(long)]
    email: Option<String>,
    #[arg(long)]
    adc_file: Option<PathBuf>,
    #[arg(long)]
    drive_account: Option<String>,
    #[arg(long, conflicts_with = "no_use")]
    r#use: bool,
    #[arg(long)]
    no_use: bool,
    #[arg(long)]
    url_only: bool,
    /// Register metadata only; do not authenticate or change the active account.
    #[arg(long, conflicts_with_all = ["url_only", "use", "with_drive", "with_dfs"])]
    configure_only: bool,
    #[command(flatten)]
    drive: DriveOptions,
}
#[derive(Args)]
pub struct LoginArgs {
    name: Option<String>,
    #[arg(long)]
    email: Option<String>,
    #[arg(long, conflicts_with_all = ["code", "callback_url"])]
    url_only: bool,
    #[arg(long, conflicts_with = "callback_url")]
    code: Option<String>,
    #[arg(long)]
    callback_url: Option<Url>,
    #[arg(long, default_value = REMOTE_REDIRECT)]
    redirect_uri: Url,
    #[command(flatten)]
    drive: DriveOptions,
}

pub fn add(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    args: &AddArgs,
) -> Result<()> {
    let profile = ProfileId::new(args.name.clone())?;
    let bound_drive = args
        .drive_account
        .as_deref()
        .map(|name| lookup(&accounts.registry()?, name))
        .transpose()?;
    let adc_file = args
        .adc_file
        .as_ref()
        .map(|p| {
            let path = std::fs::canonicalize(p).map_err(|source| Error::Io {
                path: p.clone(),
                source,
            })?;
            if !path.is_file() {
                return Err(Error::OAuth("ADC 路径必须为文件"));
            }
            Ok(path)
        })
        .transpose()?;
    let provider = if adc_file.is_some() {
        AuthProvider::Adc
    } else {
        cli.auth.unwrap_or(AuthProvider::Oauth2)
    };
    accounts.add(
        profile.clone(),
        Account {
            email: args.email.clone(),
            auth_provider: provider,
            adc_credentials_file: adc_file.map(|p| p.to_string_lossy().into_owned()),
            drive_account: bound_drive.clone(),
            ..Account::default()
        },
    )?;
    if args.configure_only {
        println!("已添加账户 {profile}；尚未进行在线认证");
        return Ok(());
    }
    let pending = authorize(
        cli,
        accounts,
        env,
        storage,
        tokens,
        &profile,
        provider,
        args.url_only,
        None,
        None,
        &Url::parse(REMOTE_REDIRECT).unwrap(),
        args.drive.no_browser,
    )?;
    if !pending
        && provider == AuthProvider::Oauth2
        && bound_drive.is_none()
        && args.drive.enabled(true)
    {
        if let Err(error) = consent(cli, accounts, env, storage, &profile, &args.drive) {
            if matches!(error, Error::CliExit(_)) {
                return Err(error);
            }
            eprintln!(
                "账户 {profile} 已登录，但 Drive 授权失败：{error}；可用 -a {profile} drive consent 重试"
            );
        }
    }
    if !args.no_use {
        accounts.use_account(profile.as_str())?;
        eprintln!("Active account -> {profile}");
    }
    Ok(())
}

pub fn login(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    args: &LoginArgs,
) -> Result<()> {
    let selector = args
        .name
        .as_deref()
        .or(cli.account.as_deref())
        .or(env.account.as_deref());
    if let Some(selector) = selector {
        match lookup(&accounts.registry()?, selector) {
            Ok(_) => {}
            Err(Error::AccountMissing(_)) if args.code.is_none() && args.callback_url.is_none() => {
                accounts.add(
                    ProfileId::new(selector)?,
                    Account {
                        auth_provider: cli.auth.unwrap_or_default(),
                        ..Account::default()
                    },
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    let selected = context::resolve(accounts, env, selector, None, false)?;
    if let Some(email) = &args.email {
        accounts.update(|registry| {
            let account = registry
                .accounts
                .get_mut(&selected.profile)
                .ok_or_else(|| Error::AccountMissing(selected.profile.to_string()))?;
            account.email = Some(email.clone());
            Ok(())
        })?;
    }
    let pending = authorize(
        cli,
        accounts,
        env,
        storage,
        tokens,
        &selected.profile,
        cli.auth.unwrap_or(selected.provider),
        args.url_only,
        args.code.clone().map(Secret::new),
        args.callback_url.clone(),
        &args.redirect_uri,
        args.drive.no_browser,
    )?;
    if !pending && args.drive.enabled(false) {
        consent(cli, accounts, env, storage, &selected.profile, &args.drive)?;
    }
    Ok(())
}
fn consent(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    profile: &ProfileId,
    options: &DriveOptions,
) -> Result<()> {
    let hint = accounts
        .registry()?
        .accounts
        .get(profile)
        .ok_or_else(|| Error::AccountMissing(profile.to_string()))?
        .email
        .clone();
    let args = drive_cli::ConsentArgs::for_account(&options.flavor, hint, options.no_browser)?;
    drive_cli::consent(cli, env, storage, profile, &args)
}
// true means a durable OAuth pending exists; Drive consent must wait for explicit completion.
#[allow(clippy::too_many_arguments)]
fn authorize(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    storage: &Storage,
    tokens: &TokenStore,
    profile: &ProfileId,
    provider: AuthProvider,
    url_only: bool,
    code: Option<Secret>,
    callback: Option<Url>,
    redirect: &Url,
    no_browser: bool,
) -> Result<bool> {
    let selected = context::resolve(accounts, env, Some(profile.as_str()), None, false)?;
    let client = TokenClient::new()?;
    if provider == AuthProvider::Adc {
        if url_only || code.is_some() || callback.is_some() {
            return Err(Error::OAuth("ADC 登录不接受 OAuth 授权码参数"));
        }
        let _token = AdcProvider::from_context(&selected, client).access_token(Utc::now())?;
        println!("账户 {profile} 的 ADC token 已验证；尚未验证 Colab 权限");
        return Ok(false);
    }
    let service = OAuthService::new(storage.clone(), tokens.clone(), client);
    if code.is_some() || callback.is_some() {
        service.finish(profile, code, callback, Utc::now())?;
    } else {
        if !url_only && !no_browser && !io::stdin().is_terminal() {
            return Err(Error::OAuth(
                "非交互环境请使用 --url-only，然后 --code；显式 --no-browser 可从 stdin 读取授权码",
            ));
        }
        let client_config =
            OAuthClient::resolve(cli.client_oauth_config.as_deref(), env.home.as_deref())?;
        let hint = accounts
            .registry()?
            .accounts
            .get(profile)
            .ok_or_else(|| Error::AccountMissing(profile.to_string()))?
            .email
            .clone();
        let url = service.begin(
            profile,
            client_config,
            redirect.clone(),
            hint.as_deref(),
            Utc::now(),
        )?;
        if url_only {
            println!("{url}");
            return Ok(true);
        }
        eprintln!("{url}");
        eprint!("完成授权后粘贴 code: ");
        io::stderr()
            .flush()
            .map_err(|_| Error::OAuth("无法写入授权提示"))?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|_| Error::OAuth("无法读取授权码"))?;
        let code = Secret::new(input.trim());
        input.zeroize();
        if code.is_empty() {
            return Err(Error::OAuth("授权码不能为空；pending 已保留"));
        }
        service.finish(profile, Some(code), None, Utc::now())?;
    }
    println!("账户 {profile} 已保存授权");
    Ok(false)
}
