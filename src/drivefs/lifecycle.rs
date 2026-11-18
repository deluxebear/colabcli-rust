//! Local intent, remote observation, and explicit cleanup form one lifecycle.
//! No mutation with an unknown result is blindly resubmitted.
use super::{
    lease::Lease,
    probe,
    remote::{self, Action, RemoteRequest},
    result::{self, RemoteResult, Stage},
};
use crate::{
    Error, Result,
    contents::ContentsClient,
    drive_credentials::{DriveCredentialService, DriveCredentials, remote_fingerprint},
    execution::{self, ExecuteOptions},
    jupyter::{ExecutionStatus, ExecutionTimeout},
    models::{ProfileId, Registry, SessionState},
    sessions::SessionService,
    storage::FileLock,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Options {
    pub http_timeout: Duration,
    pub cancellation: Option<Arc<AtomicUsize>>,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Dispatching,
    Observed,
}
#[derive(Clone, Serialize, Deserialize)]
struct Plan {
    version: u32,
    lease: Lease,
    action: Action,
    request: RemoteRequest,
    phase: Phase,
    receipt: Option<String>,
    #[serde(default)]
    purge_state: bool,
}
impl Plan {
    fn validate(&self, state: &SessionState, profile: &ProfileId) -> Result<()> {
        self.lease.validate()?;
        self.request.validate()?;
        if self.version != 1
            || self.lease.compute_account != *profile
            || self.lease.endpoint != state.endpoint
            || self.lease.session != state.name
            || self.request.mountpoint != self.lease.request.mountpoint
            || self.action == Action::Status
            || (self.action == Action::Mount
                && self.request.operation_id != self.lease.request.operation_id)
            || self
                .receipt
                .as_deref()
                .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::KernelProtocol(
                "DriveFS 本地恢复计划与账户或 VM 不匹配",
            ));
        }
        Ok(())
    }
}
fn plans(
    state: &SessionState,
    profile: &ProfileId,
) -> Result<(Option<Plan>, BTreeMap<String, Lease>)> {
    let plan: Option<Plan> = state
        .extra
        .get("drive_plan")
        .map(|v| {
            serde_json::from_value(v.clone())
                .map_err(|_| Error::KernelProtocol("DriveFS 本地计划损坏"))
        })
        .transpose()?;
    if let Some(plan) = &plan {
        plan.validate(state, profile)?;
    }
    let mounts: BTreeMap<String, Lease> = state
        .extra
        .get("drive_mounts")
        .map(|v| {
            serde_json::from_value(v.clone())
                .map_err(|_| Error::KernelProtocol("DriveFS 本地挂载记录损坏"))
        })
        .transpose()?
        .unwrap_or_default();
    for (path, lease) in &mounts {
        lease.validate()?;
        if path != &lease.request.mountpoint
            || lease.compute_account != *profile
            || lease.endpoint != state.endpoint
            || lease.session != state.name
        {
            return Err(Error::KernelProtocol("DriveFS 挂载记录归属不匹配"));
        }
    }
    Ok((plan, mounts))
}
fn ascii_json(value: &str) -> String {
    let encoded = serde_json::to_string(value).expect("string serialization");
    let mut ascii = String::new();
    for c in encoded.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            for word in c.encode_utf16(&mut [0; 2]) {
                ascii.push_str(&format!("\\u{word:04x}"));
            }
        }
    }
    ascii
}
pub fn identity(credentials: &DriveCredentials) -> Result<String> {
    credentials.validate_for_mount(false)?;
    let encoded = format!(
        "[{}, {}]",
        ascii_json(&credentials.oauth.client_id),
        ascii_json(credentials.user_id.as_deref().unwrap())
    );
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}
/// Serialize native and official mount strategies for one compute VM. Callers
/// must re-check endpoint and ownership after acquiring this guard.
pub fn operation_lock(
    storage: &crate::storage::Storage,
    profile: &ProfileId,
    endpoint: &str,
) -> Result<FileLock> {
    storage.lock(&storage.path(format!(
        "locks/drive/{}/{:x}.lock",
        profile,
        Sha256::digest(endpoint.as_bytes())
    ))?)
}
struct Context<'a> {
    service: &'a SessionService,
    credentials: &'a DriveCredentialService,
    name: String,
    endpoint: String,
    options: Options,
    _lock: FileLock,
}
impl<'a> Context<'a> {
    fn new(
        service: &'a SessionService,
        credentials: &'a DriveCredentialService,
        name: Option<&str>,
        options: Options,
    ) -> Result<Self> {
        let state = service.with_session(name, |state| Ok(state.clone()))?;
        let lock = operation_lock(service.storage(), service.profile(), &state.endpoint)?;
        let context = Self {
            service,
            credentials,
            name: state.name,
            endpoint: state.endpoint,
            options,
            _lock: lock,
        };
        context.snapshot()?;
        Ok(context)
    }
    fn snapshot(&self) -> Result<SessionState> {
        self.service.with_session(Some(&self.name), |state| {
            if state.endpoint != self.endpoint {
                return Err(Error::KernelProtocol("DriveFS 操作期间 VM 已被替换"));
            }
            Ok(state.clone())
        })
    }
    fn save(&self, plan: &Plan) -> Result<()> {
        self.service
            .with_execution_session(Some(&self.name), |original, checkpoint| {
                plan.validate(original, self.service.profile())?;
                let (old, _) = plans(original, self.service.profile())?;
                if old.as_ref().is_some_and(|old| {
                    old.lease.request.operation_id != plan.lease.request.operation_id
                }) {
                    return Err(Error::KernelProtocol("已有其他 DriveFS 待恢复操作"));
                }
                let mut state = original.clone();
                state.extra.insert(
                    "drive_plan".into(),
                    serde_json::to_value(plan)
                        .map_err(|_| Error::KernelProtocol("DriveFS 计划编码失败"))?,
                );
                checkpoint(state)
            })
    }
    fn commit(&self, lease: &Lease, mounted: bool) -> Result<()> {
        if !mounted {
            self.credentials.release_lease(lease)?;
        }
        self.service
            .with_execution_session(Some(&self.name), |original, checkpoint| {
                if original.endpoint != lease.endpoint {
                    return Err(Error::KernelProtocol("DriveFS 完成时 VM 已变化"));
                }
                let (plan, mut mounts) = plans(original, self.service.profile())?;
                if plan.as_ref().is_some_and(|plan| {
                    plan.lease.request.operation_id != lease.request.operation_id
                }) {
                    return Err(Error::KernelProtocol("不能清除其他 DriveFS 计划"));
                }
                if mounted {
                    mounts.insert(lease.request.mountpoint.clone(), lease.clone());
                } else {
                    mounts.remove(&lease.request.mountpoint);
                }
                let mut state = original.clone();
                state.extra.remove("drive_plan");
                if mounts.is_empty() {
                    state.extra.remove("drive_mounts");
                } else {
                    state.extra.insert(
                        "drive_mounts".into(),
                        serde_json::to_value(mounts)
                            .map_err(|_| Error::KernelProtocol("DriveFS 记录编码失败"))?,
                    );
                }
                checkpoint(state)
            })
    }
    fn observe(&self, path: &str) -> Result<RemoteResult> {
        probe::query(
            self.service,
            Some(&self.name),
            &RemoteRequest::new(path)?,
            self.options.http_timeout,
            self.options.cancellation.clone(),
        )
    }
    fn cancel_owned(&self, plan: &Plan) -> Result<()> {
        let state = self.snapshot()?;
        if let Some(pending) = state.extra.get("automation_pending") {
            if pending["operation"] != "drivefs"
                || pending["drivefs_operation_id"] != plan.request.operation_id
            {
                return Err(Error::AutomationPending);
            }
            execution::automation_cleanup(
                self.service,
                Some(&self.name),
                None,
                self.options.http_timeout,
            )?;
        }
        Ok(())
    }
    fn execute(
        &self,
        plan: &mut Plan,
        credentials: Option<&DriveCredentials>,
    ) -> Result<RemoteResult> {
        self.snapshot()?;
        let mut prepared =
            remote::prepare(plan.action, &plan.request, credentials, &self.endpoint)?;
        plan.phase = Phase::Dispatching;
        self.save(plan)?;
        let mut document = prepared.document()?;
        let results = execution::execute_document_with_input(
            self.service,
            Some(&self.name),
            &mut document,
            ExecuteOptions {
                timeout: ExecutionTimeout::Deadline(Some(Duration::from_secs(
                    plan.request.timeout.saturating_mul(2) + 90,
                ))),
                http_timeout: self.options.http_timeout,
                kernel_id: None,
                cancellation: self.options.cancellation.clone(),
            },
            |_| Ok(()),
            prepared
                .input
                .as_mut()
                .map(|input| input as &mut dyn crate::jupyter::InputHandler),
        )?;
        let result = results
            .first()
            .filter(|result| result.status != ExecutionStatus::Unknown)
            .ok_or(Error::KernelProtocol(
                "DriveFS 操作结果未知；请先 status 核查，再以相同参数重试或 umount",
            ))?;
        if !result.succeeded() {
            return Err(Error::KernelProtocol(
                "DriveFS 执行失败；未输出原始远端诊断，保留恢复计划",
            ));
        }
        let parsed = result::parse(&result.outputs, &plan.request)?;
        plan.phase = Phase::Observed;
        self.save(plan)?;
        self.service.record_event(
            &self.name,
            Some(&self.endpoint),
            "drive_operation",
            serde_json::json!({"action":plan.action,"request":plan.request,"result":parsed}),
            None,
        )?;
        Ok(parsed)
    }
    fn matches(&self, result: &RemoteResult, lease: &Lease) -> bool {
        result.ok
            && result.owner_operation_id.as_deref() == Some(&lease.request.operation_id)
            && result.identity.as_deref() == Some(&lease.identity)
            && result.mode == Some(lease.request.mode)
            && result.readonly == Some(lease.request.readonly)
    }
    fn ready(&self, result: &RemoteResult, lease: &Lease) -> bool {
        self.matches(result, lease)
            && result.stage == Stage::Mounted
            && result.mounted == Some(true)
            && result.drive_alive == Some(true)
            && (lease.request.mode == remote::AuthMode::Rt || result.helper_alive == Some(true))
    }
    fn finish_mount(&self, plan: &Plan, result: RemoteResult) -> Result<RemoteResult> {
        if result.ok {
            if !self.ready(&result, &plan.lease) {
                return Err(Error::KernelProtocol("挂载未确认正常运行；保留租约和计划"));
            }
            self.commit(&plan.lease, true)?;
        }
        Ok(result)
    }
    fn retained_lease(&self, path: &str) -> Result<Option<Lease>> {
        let registry: Registry = self
            .service
            .storage()
            .read(&self.service.storage().path("accounts.json")?)?
            .ok_or(Error::KernelProtocol("账户注册表缺失；拒绝恢复挂载"))?;
        let mut found = None;
        for account in registry.accounts.keys() {
            for lease in self.credentials.leases(account)? {
                if lease.compute_account == *self.service.profile()
                    && lease.endpoint == self.endpoint
                    && lease.session == self.name
                    && lease.request.mountpoint == path
                {
                    if found.is_some() {
                        return Err(Error::KernelProtocol(
                            "多个 DriveFS 租约对应同一挂载；拒绝恢复",
                        ));
                    }
                    found = Some(lease);
                }
            }
        }
        Ok(found)
    }
    fn recover_unmount(&self, plan: Plan, status: RemoteResult) -> Result<RemoteResult> {
        self.save(&plan)?;
        let (_, mounts) = plans(&self.snapshot()?, self.service.profile())?;
        if status.ok
            && status.managed == Some(false)
            && status.mounted == Some(false)
            && !mounts.contains_key(&plan.request.mountpoint)
        {
            self.cancel_owned(&plan)?;
            let after = self.observe(&plan.request.mountpoint)?;
            if after.ok && after.managed == Some(false) && after.mounted == Some(false) {
                self.commit(&plan.lease, false)?;
                return Ok(after);
            }
            return Err(Error::KernelProtocol(
                "取消后远端状态变化；保留 DriveFS 计划",
            ));
        }
        self.complete_unmount(plan, status)
    }
    fn complete_unmount(&self, mut plan: Plan, mut status: RemoteResult) -> Result<RemoteResult> {
        if !status.ok {
            return Ok(status);
        }
        if !self.matches(&status, &plan.lease) {
            return Err(Error::KernelProtocol(
                "远端挂载不属于本次本地租约；未执行卸载或删除",
            ));
        }
        self.cancel_owned(&plan)?;
        // Query after terminating an uncertain owned kernel, before any further
        // mutation. Detached processes and mount state remain authoritative.
        status = self.observe(&plan.request.mountpoint)?;
        if !self.matches(&status, &plan.lease) {
            return Err(Error::KernelProtocol("清理核查时远端挂载归属变化"));
        }
        if status.stage == Stage::Finished {
            self.commit(&plan.lease, false)?;
            return Ok(status);
        }
        if !matches!(status.stage, Stage::CredentialsPending | Stage::Finalizing) {
            plan.action = Action::Umount;
            plan.request = RemoteRequest::new(&plan.lease.request.mountpoint)?;
            plan.request.owner_operation_id = Some(plan.lease.request.operation_id.clone());
            plan.request.purge_state = plan.purge_state;
            plan.phase = Phase::Prepared;
            self.save(&plan)?;
            status = self.execute(&mut plan, None)?;
            if !status.ok {
                return Ok(status);
            }
            if !self.matches(&status, &plan.lease) {
                return Err(Error::KernelProtocol("卸载响应的所有权发生变化"));
            }
        }
        if status.stage == Stage::CredentialsPending || status.stage == Stage::Finalizing {
            if plan.receipt.is_none() {
                if status.stage == Stage::Finalizing {
                    return Err(Error::OAuth("本地缺少已采用凭据的确认；保留远端清理状态"));
                }
                let remote = format!(
                    "content/.colab-cli-drivefs/operations/{}/credentials.json",
                    plan.lease.request.operation_id
                );
                let raw = self
                    .service
                    .with_execution_session(Some(&self.name), |state, _| {
                        if state.endpoint != self.endpoint {
                            return Err(Error::KernelProtocol("凭据回收时 VM 已变化"));
                        }
                        ContentsClient::new(state, self.options.http_timeout)?
                            .download_bytes(&remote)
                            .map(Zeroizing::new)
                    })?;
                if raw.len() > 65536 {
                    return Err(Error::OAuth("DriveFS 远端凭据过大"));
                }
                plan.receipt = Some(
                    self.credentials
                        .adopt_remote(&plan.lease.drive_account, &plan.lease.fingerprint, &raw)?
                        .receipt,
                );
            }
            plan.action = Action::Finalize;
            plan.request = RemoteRequest::new(&plan.lease.request.mountpoint)?;
            plan.request.owner_operation_id = Some(plan.lease.request.operation_id.clone());
            plan.request.credentials_receipt = plan.receipt.clone();
            plan.request.purge_state = plan.purge_state;
            plan.phase = Phase::Prepared;
            self.save(&plan)?;
            status = self.execute(&mut plan, None)?;
            if !status.ok {
                return Ok(status);
            }
        }
        if status.stage != Stage::Finished
            || status.mounted != Some(false)
            || !self.matches(&status, &plan.lease)
        {
            return Err(Error::KernelProtocol(
                "DriveFS 清理未确认完成；保留租约与计划",
            ));
        }
        self.commit(&plan.lease, false)?;
        Ok(status)
    }
}

pub fn mount(
    service: &SessionService,
    credentials: &DriveCredentialService,
    name: Option<&str>,
    drive_account: &ProfileId,
    request: &RemoteRequest,
    options: Options,
) -> Result<RemoteResult> {
    request.validate()?;
    let context = Context::new(service, credentials, name, options)?;
    let state = context.snapshot()?;
    let (existing, mut mounts) = plans(&state, service.profile())?;
    if let Some(mut plan) = existing {
        if plan.lease.request.mountpoint != request.mountpoint {
            return Err(Error::KernelProtocol("其他路径已有 DriveFS 待恢复操作"));
        }
        let observed = context.observe(&request.mountpoint)?;
        if request.force {
            plan.purge_state = request.purge_state;
            let result = context.recover_unmount(plan, observed)?;
            if !result.ok {
                return Ok(result);
            }
            mounts.remove(&request.mountpoint);
        } else {
            let mut same = request.clone();
            same.operation_id = plan.lease.request.operation_id.clone();
            if same != plan.lease.request || drive_account != &plan.lease.drive_account {
                return Err(Error::KernelProtocol(
                    "已有不同参数的 DriveFS 计划；请先 umount 或 --force 恢复清理",
                ));
            }
            if context.ready(&observed, &plan.lease) {
                context.cancel_owned(&plan)?;
                let verified = context.observe(&request.mountpoint)?;
                return context.finish_mount(&plan, verified);
            }
            let current = context.snapshot()?;
            let unexecuted = current
                .extra
                .get("automation_pending")
                .is_some_and(|pending| {
                    pending["phase"] == "ready"
                        && pending["operation"] == "drivefs"
                        && pending["drivefs_operation_id"] == plan.request.operation_id
                });
            if (plan.phase == Phase::Prepared || unexecuted)
                && plan.action == Action::Mount
                && observed.managed == Some(false)
                && observed.mounted == Some(false)
            {
                let current = credentials.acquire_lease(&plan.lease)?;
                let result = context.execute(&mut plan, Some(&current))?;
                return context.finish_mount(&plan, result);
            }
            return Err(Error::KernelProtocol(
                "DriveFS 上次操作结果待核查；没有重复挂载，请用 status 或 umount",
            ));
        }
    }
    let observed = context.observe(&request.mountpoint)?;
    if !observed.ok {
        return Ok(observed);
    }
    if let Some(lease) = mounts.get(&request.mountpoint) {
        if !context.matches(&observed, lease) {
            return Err(Error::KernelProtocol("本地记录与远端挂载不匹配；保留租约"));
        }
        if !request.force {
            if lease.drive_account == *drive_account
                && lease.request.mode == request.mode
                && lease.request.readonly == request.readonly
                && lease.request.extra_flags == request.extra_flags
                && context.ready(&observed, lease)
            {
                return Ok(observed);
            }
            return Err(Error::KernelProtocol(
                "现有挂载参数不同或未正常运行；请先 umount 或 --force",
            ));
        }
        let cleanup = Plan {
            version: 1,
            lease: lease.clone(),
            action: Action::Umount,
            request: RemoteRequest::new(&request.mountpoint)?,
            phase: Phase::Prepared,
            receipt: None,
            purge_state: request.purge_state,
        };
        let result = context.recover_unmount(cleanup, observed)?;
        if !result.ok {
            return Ok(result);
        }
    } else if observed.mounted != Some(false)
        || (observed.managed != Some(false) && observed.stage != Stage::Finished)
    {
        return Err(Error::KernelProtocol(
            "远端已有未被本地租约跟踪的挂载；拒绝接管",
        ));
    }
    let current = credentials.load(drive_account)?.ok_or(Error::NeedsAuth)?;
    current.validate_for_mount(request.mode == remote::AuthMode::Rt)?;
    let lease = Lease {
        version: 1,
        compute_account: service.profile().clone(),
        drive_account: drive_account.clone(),
        session: context.name.clone(),
        endpoint: context.endpoint.clone(),
        request: request.clone(),
        fingerprint: remote_fingerprint(&current),
        identity: identity(&current)?,
    };
    let mut plan = Plan {
        version: 1,
        lease,
        action: Action::Mount,
        request: request.clone(),
        phase: Phase::Prepared,
        receipt: None,
        purge_state: request.purge_state,
    };
    context.save(&plan)?;
    let current = credentials.acquire_lease(&plan.lease)?;
    let result = context.execute(&mut plan, Some(&current))?;
    context.finish_mount(&plan, result)
}

pub fn unmount(
    service: &SessionService,
    credentials: &DriveCredentialService,
    name: Option<&str>,
    request: &RemoteRequest,
    options: Options,
) -> Result<RemoteResult> {
    request.validate()?;
    let context = Context::new(service, credentials, name, options)?;
    let state = context.snapshot()?;
    let (existing, mut mounts) = plans(&state, service.profile())?;
    if existing.is_none() && !mounts.contains_key(&request.mountpoint) {
        if let Some(lease) = context.retained_lease(&request.mountpoint)? {
            mounts.insert(request.mountpoint.clone(), lease);
        }
    }
    let status = context.observe(&request.mountpoint)?;
    if !status.ok {
        return Ok(status);
    }
    let mut plan = match existing {
        Some(plan) if plan.request.mountpoint == request.mountpoint => plan,
        Some(_) => return Err(Error::KernelProtocol("其他路径已有 DriveFS 待恢复操作")),
        None => match mounts.get(&request.mountpoint) {
            Some(lease) => Plan {
                version: 1,
                lease: lease.clone(),
                action: Action::Umount,
                request: request.clone(),
                phase: Phase::Prepared,
                receipt: None,
                purge_state: request.purge_state,
            },
            None if (status.managed == Some(false) || status.stage == Stage::Finished)
                && status.mounted == Some(false) =>
            {
                return Ok(status);
            }
            None => {
                return Err(Error::KernelProtocol(
                    "缺少本地 DriveFS 租约；拒绝卸载外来挂载",
                ));
            }
        },
    };
    plan.purge_state = request.purge_state;
    // The immutable lease still refers to the original mount request.
    context.recover_unmount(plan, status)
}
