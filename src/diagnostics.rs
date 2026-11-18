//! Read-only diagnostics: no bootstrap, native credential access, recovery, or HTTP.
use crate::{Cli, print_json};
use clap::{Args, Subcommand};
use colabcli_rust::{
    Error, Result,
    accounts::Accounts,
    auth::OAuthCredentials,
    context::{self, Environment},
    credentials::{CredentialBackend, TokenStorageMode, TokenStore},
    drive_credentials::DriveCredentials,
    drivefs::lease::Lease,
    models::{AuthProvider, ProfileId, Registry, Secret},
    sessions::PARALLEL_RELEASE_JOURNAL,
    storage::Storage,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Subcommand)]
pub enum ExtCommand {
    /// Inspect local files and pending recovery without keyring access or Google requests.
    #[command(alias = "status")]
    Doctor(DoctorArgs),
    /// Extension features are built in; accepts legacy injection flags without changing Python files.
    Install(InstallArgs),
    /// Display native extension version and capabilities without reading configuration.
    Version {
        #[arg(long)]
        json: bool,
    },
}
#[derive(Args, Clone)]
pub struct DoctorArgs {
    #[arg(long)]
    pub drive_account: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Return exit code 1 for local issues or authentication that cannot be inspected locally.
    #[arg(long)]
    pub strict: bool,
}
#[derive(Args, Clone)]
pub struct InstallArgs {
    /// Legacy Python injection target; accepted but not modified.
    #[arg(long)]
    pub site_dir: Option<PathBuf>,
    #[arg(long)]
    pub quiet: bool,
    #[arg(long)]
    pub json: bool,
}

struct NeverCredentials;
impl CredentialBackend for NeverCredentials {
    fn get(&self, _: &str) -> Result<Option<Secret>> {
        Err(Error::CredentialUnavailable)
    }
    fn set(&self, _: &str, _: &Secret) -> Result<()> {
        Err(Error::CredentialUnavailable)
    }
    fn delete(&self, _: &str) -> Result<()> {
        Err(Error::CredentialUnavailable)
    }
}
pub(crate) fn local_accounts(storage: &Storage, mode: TokenStorageMode) -> Accounts {
    Accounts::new(
        storage.clone(),
        TokenStore::new(storage.clone(), Arc::new(NeverCredentials), mode),
    )
}
fn capabilities() -> Value {
    json!({"implementation":"rust","version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "builtin":true,"python_required":false,"python_injection":"not_applicable",
        "native_keyring_compiled":cfg!(feature="native-keyring"),
        "features":["accounts","oauth2","authorized_user_adc","service_account_adc","impersonated_service_account_adc","external_account_adc","external_account_authorized_user_adc","metadata_adc","external_account_executable_adc","external_account_aws_adc","control_plane","contents","jupyter_websocket","repl","ssh","drivefs"],
        "remote_drivefs":"not_checked","online_compatibility":"not_checked"})
}
fn present(storage: &Storage, relative: impl AsRef<Path>) -> &'static str {
    match storage
        .path(relative)
        .and_then(|p| std::fs::symlink_metadata(&p).map_err(|source| Error::Io { path: p, source }))
    {
        Ok(meta) if meta.is_file() => "present",
        Ok(_) => "invalid",
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(Error::UnsafePath(_)) => "unsafe",
        Err(_) => "unreadable",
    }
}
fn oauth(storage: &Storage, profile: &ProfileId, mode: TokenStorageMode) -> Value {
    if mode == TokenStorageMode::Keyring {
        return json!({"provider":"oauth2","storage":"keyring","state":"not_inspected"});
    }
    let result = storage
        .profile_path(profile, "oauth_token.json")
        .and_then(|path| storage.read::<OAuthCredentials>(&path));
    let state = match result {
        Ok(Some(credentials))
            if credentials.client_id.trim().is_empty()
                || !url::Url::parse(&credentials.token_uri)
                    .is_ok_and(|url| colabcli_rust::auth::validate_endpoint(&url).is_ok()) =>
        {
            "invalid"
        }
        Ok(Some(credentials)) if credentials.valid_at(chrono::Utc::now()) => "access_token_cached",
        Ok(Some(credentials))
            if credentials
                .refresh_token
                .as_ref()
                .is_some_and(|t| !t.is_empty()) =>
        {
            "refresh_required"
        }
        Ok(Some(_)) => "needs_authorization",
        Ok(None) if mode == TokenStorageMode::Auto => "keyring_not_inspected",
        Ok(None) => "missing",
        Err(_) => "invalid_or_unreadable",
    };
    json!({"provider":"oauth2","storage":"file","state":state,"online_validity":"not_checked"})
}
#[derive(serde::Deserialize)]
struct AdcSnapshot {
    #[serde(rename = "type")]
    kind: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<Secret>,
    client_email: Option<String>,
    private_key: Option<Secret>,
    client_secret: Option<Secret>,
    token: Option<Secret>,
    expiry: Option<String>,
    service_account_impersonation_url: Option<String>,
    source_credentials: Option<AdcSourceSnapshot>,
    audience: Option<String>,
    subject_token_type: Option<String>,
    token_url: Option<String>,
    credential_source: Option<ExternalSourceSnapshot>,
}
#[derive(serde::Deserialize)]
struct ExternalSourceSnapshot {
    file: Option<String>,
    url: Option<String>,
    environment_id: Option<String>,
    regional_cred_verification_url: Option<String>,
    executable: Option<ExecutableSnapshot>,
    certificate: Option<serde::de::IgnoredAny>,
}
#[derive(serde::Deserialize)]
struct ExecutableSnapshot {
    command: Option<Secret>,
}
#[derive(serde::Deserialize)]
struct AdcSourceSnapshot {
    #[serde(rename = "type")]
    kind: Option<String>,
}
fn adc(path: Option<&Path>) -> Value {
    // Only inspect the credential type, never evaluate external-account executables,
    // metadata endpoints, impersonation URLs, or refresh any credential.
    let state = match path {
        None => "missing",
        Some(path) => match std::fs::File::open(path) {
            Ok(file) => {
                use std::io::Read;
                let mut raw = zeroize::Zeroizing::new(Vec::new());
                match file.take(65537).read_to_end(&mut raw) {
                    Ok(_) if raw.len() <= 65536 => {
                        match serde_json::from_slice::<AdcSnapshot>(&raw) {
                            Ok(value)
                                if value.kind.as_deref() == Some("authorized_user")
                                    && value
                                        .client_id
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && value
                                        .refresh_token
                                        .as_ref()
                                        .is_some_and(|s| !s.is_empty()) =>
                            {
                                "authorized_user_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref() == Some("service_account")
                                    && value
                                        .client_email
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && value
                                        .private_key
                                        .as_ref()
                                        .is_some_and(|s| !s.is_empty()) =>
                            {
                                "service_account_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref()
                                    == Some("impersonated_service_account")
                                    && value
                                        .service_account_impersonation_url
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && value.source_credentials.as_ref().is_some_and(
                                        |source| {
                                            matches!(
                                                source.kind.as_deref(),
                                                Some(
                                                    "authorized_user"
                                                        | "service_account"
                                                        | "external_account_authorized_user"
                                                )
                                            )
                                        },
                                    ) =>
                            {
                                "impersonated_service_account_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref() == Some("external_account")
                                    && value.audience.as_deref().is_some_and(|s| !s.is_empty())
                                    && value
                                        .token_url
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && matches!(
                                        value.subject_token_type.as_deref(),
                                        Some(
                                            "urn:ietf:params:oauth:token-type:jwt"
                                                | "urn:ietf:params:oauth:token-type:idToken"
                                                | "urn:ietf:params:oauth:token-type:id_token"
                                                | "urn:ietf:params:oauth:token-type:saml2"
                                        )
                                    )
                                    && value.credential_source.as_ref().is_some_and(|s| {
                                        s.environment_id.is_none()
                                            && s.executable.is_none()
                                            && s.certificate.is_none()
                                            && (s.file.as_deref().is_some_and(|v| !v.is_empty())
                                                || s.url.as_deref().is_some_and(|v| !v.is_empty()))
                                    }) =>
                            {
                                "external_account_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref()
                                    == Some("external_account_authorized_user")
                                    && ((value.token.as_ref().is_some_and(|t| !t.is_empty())
                                        && value
                                            .expiry
                                            .as_deref()
                                            .is_some_and(|s| !s.is_empty()))
                                        || (value
                                            .refresh_token
                                            .as_ref()
                                            .is_some_and(|t| !t.is_empty())
                                            && value
                                                .client_id
                                                .as_deref()
                                                .is_some_and(|s| !s.is_empty())
                                            && value
                                                .client_secret
                                                .as_ref()
                                                .is_some_and(|s| !s.is_empty())
                                            && value
                                                .token_url
                                                .as_deref()
                                                .is_some_and(|s| !s.is_empty()))) =>
                            {
                                "external_account_authorized_user_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref() == Some("external_account")
                                    && value.audience.as_deref().is_some_and(|s| !s.is_empty())
                                    && value
                                        .token_url
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && matches!(
                                        value.subject_token_type.as_deref(),
                                        Some(
                                            "urn:ietf:params:oauth:token-type:jwt"
                                                | "urn:ietf:params:oauth:token-type:id_token"
                                                | "urn:ietf:params:oauth:token-type:idToken"
                                                | "urn:ietf:params:oauth:token-type:saml2"
                                        )
                                    )
                                    && value.credential_source.as_ref().is_some_and(|s| {
                                        s.environment_id.is_none()
                                            && s.certificate.is_none()
                                            && s.executable.as_ref().is_some_and(|e| {
                                                e.command.as_ref().is_some_and(|c| !c.is_empty())
                                            })
                                    }) =>
                            {
                                "external_account_executable_configured"
                            }
                            Ok(value)
                                if value.kind.as_deref() == Some("external_account")
                                    && value.audience.as_deref().is_some_and(|s| !s.is_empty())
                                    && value
                                        .token_url
                                        .as_deref()
                                        .is_some_and(|s| !s.is_empty())
                                    && value.subject_token_type.as_deref()
                                        == Some("urn:ietf:params:aws:token-type:aws4_request")
                                    && value.credential_source.as_ref().is_some_and(|s| {
                                        s.environment_id.as_deref() == Some("aws1")
                                            && s.regional_cred_verification_url
                                                .as_deref()
                                                .is_some_and(|u| !u.is_empty())
                                            && s.executable.is_none()
                                            && s.certificate.is_none()
                                    }) =>
                            {
                                "external_account_aws_configured"
                            }
                            Ok(value) if value.kind.is_some() => "unsupported_or_incomplete",
                            _ => "invalid",
                        }
                    }
                    _ => "invalid_or_unreadable",
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
            Err(_) => "unreadable",
        },
    };
    json!({"provider":"adc","state":state,"online_validity":"not_checked"})
}
fn directory_count(storage: &Storage, relative: impl AsRef<Path>, leases: bool) -> Result<usize> {
    let path = storage.path(relative)?;
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let mut count = 0;
    for (index, entry) in entries.enumerate() {
        if index >= 4096 {
            return Err(Error::KernelProtocol("恢复记录过多；未完成扫描"));
        }
        let entry = entry.map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::KernelProtocol("本地恢复记录名称无效"));
        };
        if !name.ends_with(".json") || (leases && !name.starts_with("drivefs-")) {
            continue;
        }
        let checked = storage.path(
            entry
                .path()
                .strip_prefix(storage.root())
                .map_err(|_| Error::UnsafePath(entry.path()))?,
        )?;
        if leases {
            let lease: Lease = storage
                .read(&checked)?
                .ok_or(Error::KernelProtocol("本地租约消失"))?;
            lease.validate()?;
            if checked
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                != Some(lease.drive_account.as_str())
                || name != format!("drivefs-{}.json", lease.request.operation_id)
            {
                return Err(Error::KernelProtocol("本地租约名称不匹配"));
            }
        } else {
            let intent: colabcli_rust::runs::RunIntent = storage
                .read(&checked)?
                .ok_or(Error::KernelProtocol("本地 run 计划消失"))?;
            if name != format!("{}.json", intent.id)
                || checked
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    != Some(intent.account.as_str())
            {
                return Err(Error::KernelProtocol("run 计划归属不匹配"));
            }
        }
        count += 1;
    }
    Ok(count)
}
fn inspect(cli: &Cli, args: &DoctorArgs) -> Result<Value> {
    let env = Environment::capture();
    let root = cli
        .config_dir
        .clone()
        .or_else(|| env.home.as_ref().map(|p| p.join(".config/colab-cli")))
        .ok_or(Error::OAuth("请指定 --config-dir"))?;
    let mut report = json!({"version":1,"client":capabilities(),"snapshot":"read_only_non_atomic","registry":"missing","issues":[],"ssh_executable_available":ssh_available()});
    let mut issues = Vec::<&str>::new();
    let storage = match Storage::open_existing(&root) {
        Ok(storage) => storage,
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            report["issues"] = json!(["configuration_missing"]);
            return Ok(report);
        }
        Err(_) => {
            report["registry"] = json!("unreadable_or_unsafe");
            report["issues"] = json!(["configuration_unreadable"]);
            return Ok(report);
        }
    };
    let transaction = present(&storage, "account-transaction.json");
    report["account_transaction"] = json!(transaction);
    if transaction != "missing" {
        issues.push("account_repair_required");
    }
    let registry = match storage
        .path("accounts.json")
        .and_then(|path| storage.read::<Registry>(&path))
    {
        Ok(Some(registry)) if registry.validate().is_ok() => registry,
        Ok(None) => {
            issues.push("registry_missing_or_legacy_unmigrated");
            report["issues"] = json!(issues);
            return Ok(report);
        }
        _ => {
            report["registry"] = json!("invalid_or_unreadable");
            issues.push("registry_invalid");
            report["issues"] = json!(issues);
            return Ok(report);
        }
    };
    report["registry"] = json!("ready");
    report["account_count"] = json!(registry.accounts.len());
    if transaction != "missing" {
        report["selection"] = json!("blocked_pending_account_repair");
        report["issues"] = json!(issues);
        return Ok(report);
    }
    // NeverCredentials makes accidental token reads impossible, even in future changes.
    let accounts = local_accounts(&storage, cli.token_storage);
    let selected = context::resolve(&accounts, &env, cli.account.as_deref(), None, false)?;
    let drive = context::resolve_drive(
        &accounts,
        &env,
        &selected.profile,
        args.drive_account.as_deref(),
        false,
    )?;
    let source = match selected.source {
        context::AccountSource::Flag => "flag",
        context::AccountSource::Environment => "environment",
        context::AccountSource::Session => "session",
        context::AccountSource::Active => "active",
    };
    report["account"] = json!({"id":selected.profile,"source":source,"drive_account":drive});
    let auth = if cli.auth.unwrap_or(selected.provider) == AuthProvider::Adc {
        adc(selected.adc_file.as_deref())
    } else {
        oauth(&storage, &selected.profile, cli.token_storage)
    };
    if !matches!(
        auth["state"].as_str(),
        Some(
            "access_token_cached"
                | "refresh_required"
                | "authorized_user_configured"
                | "service_account_configured"
                | "impersonated_service_account_configured"
                | "external_account_configured"
                | "external_account_authorized_user_configured"
                | "external_account_executable_configured"
                | "external_account_aws_configured"
        )
    ) {
        issues.push("compute_auth_unverified_or_missing");
    }
    let mut auth = auth;
    if cli.auth.unwrap_or(selected.provider) == AuthProvider::Adc {
        auth["metadata_fallback"] = json!(if selected.adc_file_required {
            "blocked_explicit_file"
        } else if selected.metadata.disabled {
            "disabled"
        } else {
            "not_probed"
        });
    }
    if auth["state"] == "external_account_executable_configured" {
        auth["executable_policy"] = json!(if selected.allow_executables {
            "enabled"
        } else {
            "disabled"
        });
        if !selected.allow_executables {
            issues.push("executable_adc_disabled");
        }
    }
    report["authentication"] = auth;
    let drive_credentials = match storage
        .profile_path(&drive, "dfs_credentials.json")
        .and_then(|path| storage.read::<DriveCredentials>(&path))
    {
        Ok(Some(credentials)) if credentials.validate_for_mount(false).is_ok() => {
            json!({"state":"configured","full_drive_scope":true,"online_validity":"not_checked"})
        }
        Ok(Some(_)) => {
            issues.push("drive_credentials_incomplete");
            json!({"state":"incomplete"})
        }
        Ok(None) => {
            issues.push("drive_credentials_missing");
            json!({"state":"missing"})
        }
        Err(_) => {
            issues.push("drive_credentials_invalid");
            json!({"state":"invalid_or_unreadable"})
        }
    };
    report["drive_credentials"] = drive_credentials;
    let pending_files = [
        "control_operation.json",
        PARALLEL_RELEASE_JOURNAL,
        "oauth_pending.json",
        "oauth_refresh_pending.json",
        "dfs_pending.json",
        "dfs_refresh_pending.json",
    ];
    let mut pending = serde_json::Map::new();
    for file in pending_files {
        let state = present(
            &storage,
            Path::new("accounts")
                .join(selected.profile.as_str())
                .join(file),
        );
        if state != "missing" {
            issues.push("local_recovery_pending");
        }
        pending.insert(file.into(), json!(state));
    }
    // The Drive profile can differ from compute; inspect both refresh journals.
    for file in ["dfs_pending.json", "dfs_refresh_pending.json"] {
        let state = present(
            &storage,
            Path::new("accounts").join(drive.as_str()).join(file),
        );
        if state != "missing" {
            issues.push("drive_recovery_pending");
        }
        pending.insert(format!("drive/{file}"), json!(state));
    }
    report["pending_files"] = json!(pending);
    report["sessions"]=match storage.sessions(&selected.profile) {
        Ok(sessions)=>json!(sessions.values().map(|state| {
            let pending=["execution_pending","automation_pending","drive_probe","drive_plan","kernel_start_pending","kernel_restart_pending"].iter().any(|key|state.extra.contains_key(*key));
            if pending || state.running.is_some() {issues.push("session_operation_pending");}
            let mounts=match state.extra.get("drive_mounts") {
                None=>Some(0),
                Some(value)=>value.as_object().and_then(|mounts|mounts.iter().all(|(path,value)| {
                    serde_json::from_value::<Lease>(value.clone()).is_ok_and(|lease|lease.validate().is_ok() && lease.compute_account==selected.profile
                        && lease.endpoint==state.endpoint && lease.session==state.name && lease.request.mountpoint==*path)
                }).then_some(mounts.len())),
            };
            if mounts.is_none(){issues.push("drive_mount_records_invalid");}
            json!({"name":state.name,"normal_kernel_known":state.kernel_id.is_some(),"operation_pending":pending||state.running.is_some(),"drive_mount_count":mounts})
        }).collect::<Vec<_>>()),
        Err(_)=>{issues.push("sessions_invalid");json!({"state":"invalid_or_unreadable"})},
    };
    for (key, relative, leases) in [
        (
            "drive_leases",
            Path::new("accounts").join(drive.as_str()).join("managed"),
            true,
        ),
        (
            "run_cleanup_plans",
            Path::new("accounts")
                .join(selected.profile.as_str())
                .join("runs"),
            false,
        ),
    ] {
        report[key] = match directory_count(&storage, relative, leases) {
            Ok(count) => {
                if count > 0 && !leases {
                    issues.push("run_cleanup_pending");
                }
                json!({"count":count})
            }
            Err(_) => {
                issues.push("recovery_records_invalid");
                json!({"state":"invalid_or_unreadable"})
            }
        };
    }
    issues.sort_unstable();
    issues.dedup();
    report["issues"] = json!(issues);
    Ok(report)
}
fn ssh_available() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            let candidate = directory.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(candidate)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
    })
}
pub fn run(cli: &Cli, command: &ExtCommand) -> Result<()> {
    match command {
        ExtCommand::Version { .. } => print_json(&capabilities()),
        ExtCommand::Install(args) => {
            if args.json {
                print_json(
                    &json!({"builtin":true,"changed":false,"python_injection":"not_applicable"}),
                )?;
            } else if !args.quiet {
                println!("扩展功能已内建，无需安装 Python 注入文件。");
            }
            Ok(())
        }
        ExtCommand::Doctor(args) => {
            let report = inspect(cli, args)?;
            let issues = report["issues"].as_array().unwrap();
            let failed = !issues.is_empty();
            if args.json {
                print_json(&report)?;
            } else {
                println!("colab Rust {} · 扩展功能已内建", env!("CARGO_PKG_VERSION"));
                println!("配置注册表：{}", report["registry"].as_str().unwrap());
                if let Some(account) = report.get("account") {
                    println!(
                        "计算账户：{}；Drive 账户：{}",
                        account["id"].as_str().unwrap(),
                        account["drive_account"].as_str().unwrap()
                    );
                }
                println!(
                    "本地诊断：{}",
                    if failed {
                        "需要检查"
                    } else {
                        "未发现本地问题"
                    }
                );
                for issue in issues {
                    println!("  {}", issue.as_str().unwrap());
                }
                println!("未查询钥匙串、Google 或远端挂载；配置文件存在不代表线上授权有效。");
            }
            if args.strict && failed {
                Err(Error::CliExit(1))
            } else {
                Ok(())
            }
        }
    }
}
