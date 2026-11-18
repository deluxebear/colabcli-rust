//! Automation owns a separate kernel, never the persisted exec kernel.
use super::*;
use crate::models::SessionState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PENDING: &str = "automation_pending";
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Creating,
    Ready,
    Executing,
    Unknown,
    Finished,
    Deleting,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    operation: String,
    source_hash: String,
    kernel_id: Option<String>,
    session_id: String,
    phase: Phase,
    msg_id: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}
fn pending(state: &SessionState) -> Result<Option<Pending>> {
    state
        .extra
        .get(PENDING)
        .map(|value| {
            let pending: Pending = serde_json::from_value(value.clone())
                .map_err(|_| Error::KernelProtocol("自动化恢复记录损坏"))?;
            if !["auth", "drivemount", "install", "drivefs"].contains(&pending.operation.as_str())
                || pending.session_id.is_empty()
                || pending.source_hash.len() != 64
                || !pending.source_hash.bytes().all(|b| b.is_ascii_hexdigit())
                || (pending.phase == Phase::Ready && pending.msg_id.is_some())
                || (matches!(
                    pending.phase,
                    Phase::Executing | Phase::Unknown | Phase::Finished
                ) && pending.msg_id.as_deref().is_none_or(str::is_empty))
                || (pending.phase != Phase::Creating && pending.kernel_id.is_none())
                || pending.kernel_id.as_ref().is_some_and(|id| {
                    id.is_empty() || id == "." || id == ".." || id.chars().any(char::is_control)
                })
            {
                return Err(Error::KernelProtocol("自动化恢复记录无效"));
            }
            Ok(pending)
        })
        .transpose()
}
fn persist(
    state: &mut SessionState,
    pending: &Pending,
    checkpoint: &dyn Fn(SessionState) -> Result<()>,
) -> Result<()> {
    state.extra.insert(
        PENDING.into(),
        serde_json::to_value(pending)
            .map_err(|_| Error::KernelProtocol("自动化恢复记录编码失败"))?,
    );
    checkpoint(state.clone())
}
fn result_checkpoint(
    state: &mut SessionState,
    pending: &mut Pending,
    result: &ExecutionResult,
    user_code: bool,
    checkpoint: &dyn Fn(SessionState) -> Result<()>,
) -> Result<()> {
    if result.status == ExecutionStatus::Unknown {
        pending.phase = Phase::Unknown;
    } else {
        pending.phase = if !user_code && result.succeeded() {
            Phase::Ready
        } else {
            Phase::Finished
        };
        if pending.phase == Phase::Ready {
            pending.msg_id = None;
        }
        state.running = None;
    }
    persist(state, pending, checkpoint)
}
fn finish(
    state: &mut SessionState,
    pending: &Pending,
    result: &ExecutionResult,
    checkpoint: &dyn Fn(SessionState) -> Result<()>,
) -> Result<()> {
    if result.status != ExecutionStatus::Unknown {
        state.extra.remove(PENDING);
        state.extra.insert("automation_last".into(), json!({"operation":pending.operation,"kernel_id":pending.kernel_id,"session_id":pending.session_id,"status":result.status,"msg_id":result.msg_id}));
        checkpoint(state.clone())?;
    }
    Ok(())
}
pub(super) fn execute(
    service: &SessionService,
    name: Option<&str>,
    document: &mut Document,
    options: ExecuteOptions<'_>,
    mut output_hook: impl FnMut(&OutputChange) -> Result<()>,
    mut input: Option<&mut dyn crate::jupyter::InputHandler>,
) -> Result<Vec<ExecutionResult>> {
    let operation = document
        .source_label
        .strip_prefix("automation:")
        .ok_or(Error::KernelProtocol("无效的自动化来源"))?;
    let source = &document.cells[0].1;
    let mut digest = Sha256::new();
    digest.update(format!("{operation}\0{}\0{source}", document.setup).as_bytes());
    if let Some((remote, bytes)) = &document.install_upload {
        digest.update(b"\0upload\0");
        digest.update(remote.as_bytes());
        digest.update(b"\0");
        digest.update(bytes);
    }
    let fingerprint = format!("{:x}", digest.finalize());
    let history_source = document.history_code.as_deref().unwrap_or(source);
    service.with_execution_session(name, |original,checkpoint| {
        let mut state = original.clone();
        if options.kernel_id.is_some() { return Err(Error::KernelProtocol("自动化不得采用普通 exec 的 kernel 选择参数")); }
        if options.cancellation.as_ref().is_some_and(|flag|flag.load(std::sync::atomic::Ordering::SeqCst)!=0) {
            return Err(Error::Run("收到终止信号；未启动新的自动化请求"));
        }
        let mut journal = match pending(&state)? {
            Some(existing) if existing.phase == Phase::Ready && existing.operation == operation && existing.source_hash == fingerprint && state.running.is_none() && !state.extra.contains_key("execution_pending") => existing,
            Some(_) => return Err(Error::AutomationPending),
            None => {
                if let Some(running) = state.running.as_deref().or_else(||state.extra.get("execution_pending").and_then(Value::as_str)) { return Err(Error::ExecutionPending(running.into())); }
                if state.extra.get("kernel_start_pending") == Some(&json!(true)) { return Err(Error::KernelStartPending); }
                if state.extra.get("kernel_restart_pending") == Some(&json!(true)) { return Err(Error::KernelRestartPending); }
                Pending {operation:operation.into(),source_hash:fingerprint.clone(),kernel_id:None,session_id:codec::session_id(),phase:Phase::Creating,msg_id:None,extra:Default::default()}
            }
        };
        if operation=="drivefs" {
            if let Some(id)=document.history_code.as_deref().and_then(|code|code.strip_prefix("# drivefs operation: ")) {
                journal.extra.insert("drivefs_operation_id".into(),json!(id));
            }
        }
        if let Some(handler) = input.as_deref_mut() { handler.bind_endpoint(&state.endpoint)?; }
        if let Some((remote,bytes))=&document.install_upload {
            contents::ContentsClient::new(&state,options.http_timeout)?.upload_bytes(remote,bytes)?;
            service.record_event(&state.name,Some(&state.endpoint),"file_operation",json!({"op":"upload","path":remote,"via":"install"}),None)
                .map_err(|error|Error::HistoryAfterOperation {operation:"install requirements upload".into(),source:Box::new(error)})?;
        }
        if options.cancellation.as_ref().is_some_and(|flag|flag.load(std::sync::atomic::Ordering::SeqCst)!=0) {
            return Err(Error::Run("收到终止信号；未发送新的自动化执行请求"));
        }
        let rest = KernelRest::new(&state,options.http_timeout)?;
        if journal.kernel_id.is_none() {
            persist(&mut state,&journal,checkpoint)?;
            journal.kernel_id = Some(rest.create()?.id);
            journal.phase = Phase::Ready;
            persist(&mut state,&journal,checkpoint)?;
        } else { rest.get(journal.kernel_id.as_deref().unwrap())?; }
        let mut runtime = KernelRuntime::connect(&rest,journal.kernel_id.as_deref().unwrap(),&journal.session_id)?;
        runtime.set_cancellation(options.cancellation.clone());
        let setup = runtime.execute_with_checkpoint(&document.setup,options.timeout,|id| {
            journal.phase=Phase::Executing;journal.msg_id=Some(id.into());state.running=Some(id.into());persist(&mut state,&journal,checkpoint)
        },&mut output_hook)?;
        result_checkpoint(&mut state,&mut journal,&setup,false,checkpoint)?;
        if !setup.succeeded() { finish(&mut state,&journal,&setup,checkpoint)?;return Ok(vec![setup]); }
        service.record_event(&state.name,Some(&state.endpoint),"automation",json!({"op":operation,"code":history_source}),None)?;
        let mut result=runtime.execute_with_input(source,options.timeout,|id| {
            journal.phase=Phase::Executing;journal.msg_id=Some(id.into());state.running=Some(id.into());
            state.last_execution=Some((document.source_label.clone(),None,chrono::Utc::now().to_rfc3339()));
            persist(&mut state,&journal,checkpoint)
        },&mut output_hook,input.as_mut().map(|handler| &mut **handler as &mut dyn crate::jupyter::InputHandler))?;
        result_checkpoint(&mut state,&mut journal,&result,true,checkpoint)?;
        // DriveFS has a separate validated result boundary. Never persist its
        // raw streams, MIME extensions, or kernel error strings: they could
        // accidentally contain the private stdin configuration.
        let audit_outputs=if operation=="drivefs" {json!([])} else {json!(result.outputs)};
        let audit_error=if operation=="drivefs" {None} else {result.error.clone()};
        let audit_count=result.reply.as_ref().and_then(|reply|reply.get("execution_count")).filter(|value| operation!="drivefs" || value.is_u64()).cloned();
        for (kind,data,event_id) in [
            ("automation_result",json!({"op":operation,"outputs":audit_outputs,"status":result.status}),format!("automation:{}",result.msg_id)),
            ("execution",json!({"code":history_source,"outputs":audit_outputs,"status":result.status,"msg_id":result.msg_id,"error":audit_error,"source_file":document.source_label,"cell_index":null,"execution_count":audit_count,"via":"automation"}),result.msg_id.clone()),
        ] {
            if let Err(error)=service.record_event(&state.name,Some(&state.endpoint),kind,data,Some(&event_id)) {result.history_error=Some(error.to_string());}
        }
        finish(&mut state,&journal,&result,checkpoint)?;
        Ok(vec![result])
    })
}

/// Explicitly terminate only the pending automation kernel. Never allocate or replay code.
pub fn cleanup(
    service: &SessionService,
    name: Option<&str>,
    kernel_id: Option<&str>,
    http_timeout: Duration,
) -> Result<Option<String>> {
    service.with_session_checkpoints(name, |original, checkpoint| {
        let mut state = original.clone();
        let Some(mut journal) = pending(&state)? else {
            return Ok(None);
        };
        if let (Some(known), Some(requested)) = (&journal.kernel_id, kernel_id) {
            if known != requested {
                return Err(Error::KernelProtocol(
                    "不能用其他 kernel 替换已知自动化清理目标",
                ));
            }
        }
        let target = journal
            .kernel_id
            .as_deref()
            .or(kernel_id)
            .ok_or(Error::AutomationKernelUnknown)?
            .to_owned();
        if state.kernel_id.as_deref() == Some(&target) {
            return Err(Error::KernelProtocol(
                "不能把普通 exec kernel 作为自动化清理目标",
            ));
        }
        let rest = KernelRest::new(&state, http_timeout)?;
        if journal.kernel_id.is_none() {
            // Explicitly supplied ID is checked only on this VM. 404 also confirms it is gone.
            match rest.get(&target) {
                Ok(_) | Err(Error::RuntimeStatus(404)) => {}
                Err(error) => return Err(error),
            }
            journal.kernel_id = Some(target.clone());
        }
        journal.phase = Phase::Deleting;
        state
            .extra
            .insert("kernel_generation".into(), json!(codec::message_id()));
        persist(&mut state, &journal, checkpoint)?;
        rest.delete(&target)?;
        service.record_event_locked(
            &state.name,
            Some(&state.endpoint),
            "automation_cleanup",
            json!({"operation":journal.operation,"kernel_id":target}),
            Some(&format!("automation-cleanup:{}", journal.session_id)),
        )?;
        if state.running == journal.msg_id {
            state.running = None;
        }
        state.extra.remove(PENDING);
        state.extra.insert(
            "automation_last".into(),
            json!({"operation":journal.operation,"kernel_id":target,"status":"deleted"}),
        );
        checkpoint(state)?;
        Ok(Some(target))
    })
}
