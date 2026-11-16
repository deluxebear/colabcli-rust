//! One-shot script execution with durable ownership and explicit, non-replaying cleanup recovery.
use crate::{
    Error, Result, contents,
    error::io,
    execution::{self, Document, ExecuteOptions},
    jupyter::{
        ExecutionResult, ExecutionStatus, ExecutionTimeout, KernelRest, codec, output::OutputChange,
    },
    models::{ProfileId, Registry},
    sessions::SessionService,
    storage::Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const RESULT_MIME: &str = "application/vnd.colab-cli.run-result+json";
pub struct PreparedRun {
    pub id: String,
    pub name: String,
    document: Document,
}
impl PreparedRun {
    pub fn prepare(
        path: &Path,
        args: &[String],
        environment: &[String],
        name: Option<&str>,
    ) -> Result<Self> {
        let source = String::from_utf8(contents::read_local_file(path)?)
            .map_err(|_| Error::Run("脚本必须为 UTF-8"))?;
        let source = if source.starts_with("#!") {
            source
                .split_once('\n')
                .map(|(_, body)| body)
                .unwrap_or("")
                .to_owned()
        } else {
            source
        };
        let basename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(Error::Run("脚本文件名必须为 UTF-8"))?;
        let id = codec::message_id();
        let name = name
            .map(str::to_owned)
            .unwrap_or_else(|| format!("run-{}", &id[..8]));
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(Error::Run("会话名称必须为 1–128 字节且不含控制字符"));
        }
        let argv: Vec<_> = std::iter::once(basename.to_owned())
            .chain(args.iter().cloned())
            .collect();
        let encoded =
            serde_json::to_string(&json!({"source":source,"file":basename,"argv":argv,"nonce":id}))
                .map_err(|_| Error::Run("脚本参数编码失败"))?;
        let literal =
            serde_json::to_string(&encoded).map_err(|_| Error::Run("脚本参数编码失败"))?;
        let payload = format!(
            "def __colab_cli_run__():\n    import json, sys\n    data = json.loads({literal})\n    previous_argv = sys.argv\n    sys.argv = data['argv']\n    exit_code = 0\n    try:\n        exec(compile(data['source'], data['file'], 'exec'), {{'__name__': '__main__', '__file__': data['file']}})\n    except SystemExit as error:\n        value = error.code\n        if value is None:\n            exit_code = 0\n        elif isinstance(value, int):\n            exit_code = int(value) & 255\n        else:\n            exit_code = 1\n            print(value, file=sys.stderr)\n    except BaseException:\n        exit_code = 1\n        raise\n    finally:\n        sys.argv = previous_argv\n        __import__('IPython.display', fromlist=['display']).display({{'{RESULT_MIME}': {{'nonce': data['nonce'], 'exit_code': exit_code}}}}, raw=True)\n__colab_cli_run__()\ndel __colab_cli_run__\n"
        );
        let document = Document::script(
            payload,
            source,
            path.to_string_lossy().into_owned(),
            environment,
        )?;
        Ok(Self { id, name, document })
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RunIntent {
    pub id: String,
    pub account: ProfileId,
    pub session: String,
    pub endpoint: Option<String>,
    pub keep: bool,
    pub created_at: String,
    #[serde(default)]
    pub runtime: Option<crate::colab_api::RuntimeOptions>,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    NotAllocated,
    ConfirmedReleased,
    Kept,
    Pending,
}
#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub account: ProfileId,
    pub session: String,
    pub endpoint: Option<String>,
    pub exit_code: i32,
    pub execution: Vec<ExecutionResult>,
    pub execution_error: Option<String>,
    pub cleanup: Cleanup,
    pub cleanup_error: Option<String>,
    pub kernel_cleanup_error: Option<String>,
    pub history_error: Option<String>,
}
pub struct RunOptions {
    pub runtime: crate::colab_api::RuntimeOptions,
    pub keep: bool,
    pub timeout: ExecutionTimeout,
    pub http_timeout: Duration,
    pub cancellation: Arc<AtomicUsize>,
}
fn intent_path(storage: &Storage, profile: &ProfileId, id: &str) -> Result<PathBuf> {
    if id.len() != 36
        || !id.bytes().enumerate().all(|(index, b)| {
            if [8, 13, 18, 23].contains(&index) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(Error::Run("无效的 run ID"));
    }
    storage.profile_path(profile, &format!("runs/{id}.json"))
}
fn check_account(storage: &Storage, profile: &ProfileId) -> Result<()> {
    if storage.path("account-transaction.json")?.exists() {
        return Err(Error::MigrationPending);
    }
    let registry: Registry = storage
        .read(&storage.path("accounts.json")?)?
        .ok_or_else(|| Error::AccountMissing(profile.to_string()))?;
    registry.validate()?;
    if !registry.accounts.contains_key(profile) {
        return Err(Error::AccountMissing(profile.to_string()));
    }
    Ok(())
}
fn write_intent(storage: &Storage, intent: &RunIntent) -> Result<()> {
    let _guard = storage.profile_lock(&intent.account)?;
    check_account(storage, &intent.account)?;
    storage.write(&intent_path(storage, &intent.account, &intent.id)?, intent)
}
fn remove_intent(storage: &Storage, intent: &RunIntent) -> Result<()> {
    let _guard = storage.profile_lock(&intent.account)?;
    check_account(storage, &intent.account)?;
    storage.remove(&intent_path(storage, &intent.account, &intent.id)?)
}
/// The factory must bind to one immutable account and refresh its authentication for each lifecycle phase.
pub fn run(
    storage: &Storage,
    account: ProfileId,
    mut prepared: PreparedRun,
    options: RunOptions,
    mut service: impl FnMut() -> Result<SessionService>,
    mut output: impl FnMut(&OutputChange) -> Result<()>,
) -> Result<RunOutcome> {
    let lease = storage.path(format!("locks/run/{}/{}.lock", account, prepared.id))?;
    let _lease = storage.lock(&lease)?;
    let mut intent = RunIntent {
        id: prepared.id.clone(),
        account: account.clone(),
        session: prepared.name.clone(),
        endpoint: None,
        keep: options.keep,
        created_at: chrono::Utc::now().to_rfc3339(),
        runtime: Some(options.runtime.clone()),
    };
    write_intent(storage, &intent)?;
    let mut outcome = RunOutcome {
        run_id: intent.id.clone(),
        account,
        session: intent.session.clone(),
        endpoint: None,
        exit_code: 1,
        execution: Vec::new(),
        execution_error: None,
        cleanup: Cleanup::Pending,
        cleanup_error: None,
        kernel_cleanup_error: None,
        history_error: None,
    };
    let allocated = if options.cancellation.load(Ordering::SeqCst) == 0 {
        service()
            .and_then(|s| s.create_owned(Some(&prepared.name), options.runtime, Some(&prepared.id)))
    } else {
        Err(Error::Run("分配前收到终止信号"))
    };
    match allocated {
        Ok(view) => {
            intent.endpoint = Some(view.endpoint.clone());
            outcome.endpoint = Some(view.endpoint);
            let execution = write_intent(storage, &intent).and_then(|_| {
                execution::execute_document(
                    &service()?,
                    Some(&prepared.name),
                    &mut prepared.document,
                    ExecuteOptions {
                        timeout: options.timeout,
                        http_timeout: options.http_timeout,
                        kernel_id: None,
                        cancellation: Some(options.cancellation.clone()),
                    },
                    &mut output,
                )
            });
            match execution {
                Ok(mut results) => {
                    let code = script_exit_code(&results, &prepared.id);
                    for result in &mut results {
                        result.outputs.retain(|output| {
                            !output
                                .data
                                .get("data")
                                .is_some_and(|data| data.get(RESULT_MIME).is_some())
                        });
                    }
                    outcome.execution = results;
                    match code {
                        Ok(code) => outcome.exit_code = code,
                        Err(error) => {
                            outcome.execution_error = Some(error.to_string());
                            outcome.exit_code = signal_exit(&options.cancellation);
                        }
                    }
                }
                Err(error) => {
                    outcome.execution_error = Some(error.to_string());
                    outcome.exit_code = signal_exit(&options.cancellation);
                }
            }
        }
        Err(error) => {
            outcome.execution_error = Some(error.to_string());
            outcome.exit_code = signal_exit(&options.cancellation);
        }
    }
    if options.keep && intent.endpoint.is_some() {
        outcome.cleanup = Cleanup::Kept;
    } else {
        if let Some(endpoint) = &intent.endpoint {
            let teardown = service().and_then(|s| {
                s.with_session(Some(&intent.session), |state| {
                    if state.endpoint == *endpoint
                        && state.extra.get("run_owner").and_then(Value::as_str)
                            == Some(intent.id.as_str())
                    {
                        if let Some(id) = &state.kernel_id {
                            KernelRest::new(state, options.http_timeout)?.delete(id)?;
                        }
                    }
                    Ok(())
                })
            });
            if let Err(error) = teardown {
                outcome.kernel_cleanup_error = Some(error.to_string());
            }
        }
        match service()
            .and_then(|s| s.cleanup_owned(&intent.id, &intent.session, intent.endpoint.as_deref()))
        {
            Ok(endpoint) => {
                if let Some(endpoint) = endpoint {
                    outcome.endpoint = Some(endpoint);
                    outcome.cleanup = Cleanup::ConfirmedReleased;
                } else {
                    outcome.cleanup = Cleanup::NotAllocated;
                }
            }
            Err(error) => outcome.cleanup_error = Some(error.to_string()),
        }
    }
    if let Err(error)=crate::history::History::new(storage.clone(),intent.account.clone()).append(&intent.session,intent.endpoint.as_deref(),"run_completed",json!({"run_id":intent.id,"exit_code":outcome.exit_code,
        "cleanup":outcome.cleanup,"execution_error":outcome.execution_error,"cleanup_error":outcome.cleanup_error}),Some(&format!("run-completed:{}",intent.id))) {
        outcome.history_error=Some(error.to_string());
    }
    if outcome.cleanup != Cleanup::Pending {
        if let Err(error) = remove_intent(storage, &intent) {
            outcome.cleanup_error = Some(error.to_string());
        }
    }
    Ok(outcome)
}
fn signal_exit(signal: &AtomicUsize) -> i32 {
    let signal = signal.load(Ordering::SeqCst);
    if signal == 0 {
        1
    } else {
        128 + (signal as i32)
    }
}
pub fn script_exit_code(results: &[ExecutionResult], nonce: &str) -> Result<i32> {
    if results.iter().any(|r| r.status == ExecutionStatus::Unknown) {
        return Err(Error::Run("脚本结果未知；没有自动重放"));
    }
    let mut code = None;
    for result in results {
        for output in &result.outputs {
            if let Some(value) = output
                .data
                .get("data")
                .and_then(|data| data.get(RESULT_MIME))
                .filter(|value| value["nonce"].as_str() == Some(nonce))
            {
                let parsed = value["exit_code"]
                    .as_i64()
                    .filter(|code| (0..=255).contains(code))
                    .ok_or(Error::Run("脚本退出结果无效"))? as i32;
                if code.replace(parsed).is_some() {
                    return Err(Error::Run("收到重复脚本退出结果"));
                }
            }
        }
    }
    if results.iter().any(|r| r.status == ExecutionStatus::Failed) {
        return Ok(1);
    }
    code.ok_or(Error::Run("未收到脚本退出结果；不能确认脚本成功"))
}
pub fn cleanup(
    storage: &Storage,
    account: &ProfileId,
    id: &str,
    service: impl FnOnce() -> Result<SessionService>,
) -> Result<Cleanup> {
    let _lease = storage.lock(&storage.path(format!("locks/run/{account}/{id}.lock"))?)?;
    let path = intent_path(storage, account, id)?;
    let intent: RunIntent = {
        let _guard = storage.profile_lock(account)?;
        check_account(storage, account)?;
        storage.read(&path)?.ok_or(Error::Run("找不到待清理 run"))?
    };
    if intent.account != *account || intent.id != id {
        return Err(Error::InvalidData(path));
    }
    let endpoint =
        service()?.cleanup_owned(&intent.id, &intent.session, intent.endpoint.as_deref())?;
    remove_intent(storage, &intent)?;
    Ok(if endpoint.is_some() {
        Cleanup::ConfirmedReleased
    } else {
        Cleanup::NotAllocated
    })
}
pub fn pending(storage: &Storage, account: &ProfileId) -> Result<Vec<RunIntent>> {
    let _guard = storage.profile_lock(account)?;
    check_account(storage, account)?;
    let directory = storage.profile_path(account, "runs")?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut intents = Vec::new();
    for entry in std::fs::read_dir(&directory).map_err(|e| io(&directory, e))? {
        let entry = entry.map_err(|e| io(&directory, e))?;
        let filename = entry.file_name();
        let Some(name) = filename
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let path = intent_path(storage, account, name)?;
        let intent: RunIntent = storage
            .read(&path)?
            .ok_or(Error::Run("读取 run 计划失败"))?;
        if intent.account != *account || intent.id != name {
            return Err(Error::InvalidData(path));
        }
        intents.push(intent);
    }
    intents.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(intents)
}

pub struct SignalGuard {
    pub flag: Arc<AtomicUsize>,
    handlers: Vec<signal_hook::SigId>,
}
impl SignalGuard {
    pub fn install() -> Result<Self> {
        let mut guard = Self {
            flag: Arc::new(AtomicUsize::new(0)),
            handlers: Vec::new(),
        };
        let mut signals = vec![signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM];
        #[cfg(unix)]
        signals.push(signal_hook::consts::SIGHUP);
        for signal in signals {
            guard.handlers.push(
                signal_hook::flag::register_usize(signal, guard.flag.clone(), signal as usize)
                    .map_err(|_| Error::Run("无法注册信号处理器"))?,
            );
        }
        Ok(guard)
    }
}
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}
