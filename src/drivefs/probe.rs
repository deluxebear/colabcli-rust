//! A read-only query owns a separate kernel and journal. It never clears a
//! user's execution or a pending mutation, and never runs credential input.
use super::{
    remote::{self, Action, RemoteRequest},
    result::{self, RemoteResult},
};
use crate::{
    Error, Result,
    jupyter::{ExecutionStatus, ExecutionTimeout, KernelRest, KernelRuntime, codec},
    models::SessionState,
    sessions::SessionService,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};

const KEY: &str = "drive_probe";
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Creating,
    Ready,
    Executing,
    Unknown,
    Deleting,
}
#[derive(Serialize, Deserialize)]
struct Pending {
    version: u32,
    endpoint: String,
    session_id: String,
    kernel_id: Option<String>,
    phase: Phase,
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 1024
        && id != "."
        && id != ".."
        && !id.chars().any(char::is_control)
}
fn foreign(state: &SessionState, id: &str) -> bool {
    state.kernel_id.as_deref() == Some(id)
        || ["automation_pending", "automation_last"].iter().any(|key| {
            state
                .extra
                .get(*key)
                .and_then(|v| v.get("kernel_id"))
                .and_then(|v| v.as_str())
                == Some(id)
        })
}
fn pending(state: &SessionState) -> Result<Option<Pending>> {
    state
        .extra
        .get(KEY)
        .map(|value| {
            let p: Pending = serde_json::from_value(value.clone())
                .map_err(|_| Error::KernelProtocol("DriveFS probe 恢复记录损坏"))?;
            if p.version != 1
                || p.endpoint != state.endpoint
                || p.session_id.is_empty()
                || p.kernel_id
                    .as_deref()
                    .is_some_and(|id| !safe_id(id) || foreign(state, id))
                || (!matches!(p.phase, Phase::Creating) && p.kernel_id.is_none())
            {
                return Err(Error::KernelProtocol("DriveFS probe 恢复记录无效"));
            }
            Ok(p)
        })
        .transpose()
}
fn save(
    state: &mut SessionState,
    p: &Pending,
    checkpoint: &dyn Fn(SessionState) -> Result<()>,
) -> Result<()> {
    state.extra.insert(
        KEY.into(),
        serde_json::to_value(p)
            .map_err(|_| Error::KernelProtocol("DriveFS probe 恢复记录编码失败"))?,
    );
    checkpoint(state.clone())
}
fn delete(
    state: &mut SessionState,
    p: &mut Pending,
    rest: &KernelRest,
    checkpoint: &dyn Fn(SessionState) -> Result<()>,
) -> Result<()> {
    let id = p.kernel_id.clone().ok_or(Error::KernelProtocol(
        "DriveFS probe kernel 创建结果未知；请用 drive probe-cleanup --kernel-id 核查并清理",
    ))?;
    p.phase = Phase::Deleting;
    save(state, p, checkpoint)?;
    rest.delete(&id)?;
    state.extra.remove(KEY);
    checkpoint(state.clone())
}

pub fn query(
    service: &SessionService,
    name: Option<&str>,
    request: &RemoteRequest,
    http_timeout: Duration,
    cancellation: Option<Arc<AtomicUsize>>,
) -> Result<RemoteResult> {
    request.validate()?;
    service.with_execution_session(name, |original, checkpoint| {
        let mut state = original.clone();
        let rest = KernelRest::new(&state, http_timeout)?;
        // Only the identified probe kernel is cleaned. An unknown POST result
        // is never interpreted as permission to allocate another kernel.
        if let Some(mut old) = pending(&state)? {
            delete(&mut state, &mut old, &rest, checkpoint)?;
        }
        if cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst) != 0)
        {
            return Err(Error::KernelProtocol("DriveFS 只读核查已取消"));
        }
        let prepared = remote::prepare(Action::Status, request, None, &state.endpoint)?;
        let mut p = Pending {
            version: 1,
            endpoint: state.endpoint.clone(),
            session_id: codec::session_id(),
            kernel_id: None,
            phase: Phase::Creating,
        };
        save(&mut state, &p, checkpoint)?;
        let kernel = rest.create()?.id;
        if !safe_id(&kernel) || foreign(&state, &kernel) {
            return Err(Error::KernelProtocol("DriveFS probe 返回了非独立 kernel"));
        }
        p.kernel_id = Some(kernel.clone());
        p.phase = Phase::Ready;
        save(&mut state, &p, checkpoint)?;
        let mut runtime = KernelRuntime::connect(&rest, &kernel, &p.session_id)?;
        runtime.set_cancellation(cancellation);
        let report = runtime.execute_with_checkpoint(
            prepared.source(),
            ExecutionTimeout::Deadline(Some(Duration::from_secs(30))),
            |_| {
                p.phase = Phase::Executing;
                save(&mut state, &p, checkpoint)
            },
            |_| Ok(()),
        )?;
        if report.status == ExecutionStatus::Unknown {
            p.phase = Phase::Unknown;
            save(&mut state, &p, checkpoint)?;
            return Err(Error::KernelProtocol(
                "DriveFS 只读核查结果未知；重试 status 将先清理本次 probe kernel",
            ));
        }
        let parsed = if report.succeeded() {
            result::parse(&report.outputs, request)
        } else {
            Err(Error::KernelProtocol(
                "DriveFS 只读核查未成功；未输出原始远端诊断",
            ))
        };
        delete(&mut state, &mut p, &rest, checkpoint)?;
        let result = parsed?;
        service.record_event(
            &state.name,
            Some(&state.endpoint),
            "drive_status",
            json!({"result":result}),
            None,
        )?;
        Ok(result)
    })
}

pub fn cleanup(
    service: &SessionService,
    name: Option<&str>,
    kernel_id: Option<&str>,
    http_timeout: Duration,
) -> Result<Option<String>> {
    service.with_execution_session(name, |original, checkpoint| {
        let mut state = original.clone();
        let Some(mut p) = pending(&state)? else {
            return Ok(None);
        };
        if let (Some(known), Some(requested)) = (p.kernel_id.as_deref(), kernel_id) {
            if known != requested {
                return Err(Error::KernelProtocol(
                    "不能替换已知 DriveFS probe kernel 清理目标",
                ));
            }
        }
        if p.kernel_id.is_none() {
            let requested = kernel_id.ok_or(Error::KernelProtocol(
                "DriveFS probe 创建结果未知；请指定已核查的 --kernel-id",
            ))?;
            if !safe_id(requested) || foreign(&state, requested) {
                return Err(Error::KernelProtocol(
                    "不能把普通或自动化 kernel 作为 probe 清理目标",
                ));
            }
            p.kernel_id = Some(requested.into());
        }
        let target = p.kernel_id.clone();
        let rest = KernelRest::new(&state, http_timeout)?;
        delete(&mut state, &mut p, &rest, checkpoint)?;
        Ok(target)
    })
}
