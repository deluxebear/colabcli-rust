//! Account-scoped resource operations with durable allocation and cleanup intent.
use crate::{
    Error, Result,
    colab_api::{Assignment, ColabApi, RuntimeOptions, notebook_identity},
    models::{ProfileId, Registry, SessionState, Sessions},
    storage::{FileLock, Storage},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Map;

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub account: ProfileId,
    pub name: Option<String>,
    pub endpoint: String,
    pub accelerator: String,
    pub variant: String,
    pub machine_shape: String,
    pub shape_source: &'static str,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub kernel_id: Option<String>,
    pub session_id: Option<String>,
    pub running: Option<String>,
    pub kernel_start_pending: bool,
    pub kernel_restart_pending: bool,
    pub last_execution: Option<(String, Option<String>, String)>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Operation {
    Allocate {
        name: String,
        nbh: String,
        options: RuntimeOptions,
        session: Option<Box<SessionState>>,
        #[serde(default)]
        owner_id: Option<String>,
    },
    Release {
        name: String,
        endpoint: String,
        #[serde(default)]
        event_id: Option<String>,
    },
}

pub struct SessionService {
    storage: Storage,
    profile: ProfileId,
    api: ColabApi,
}
impl SessionService {
    pub fn new(storage: Storage, profile: ProfileId, api: ColabApi) -> Self {
        Self {
            storage,
            profile,
            api,
        }
    }
    fn guard(&self) -> Result<FileLock> {
        let guard = self.storage.profile_lock(&self.profile)?;
        if self.storage.path("account-transaction.json")?.exists() {
            return Err(Error::MigrationPending);
        }
        let registry: Registry = self
            .storage
            .read(&self.storage.path("accounts.json")?)?
            .ok_or_else(|| Error::AccountMissing(self.profile.to_string()))?;
        registry.validate()?;
        if !registry.accounts.contains_key(&self.profile) {
            return Err(Error::AccountMissing(self.profile.to_string()));
        }
        Ok(guard)
    }
    fn journal(&self) -> Result<std::path::PathBuf> {
        self.storage
            .profile_path(&self.profile, "control_operation.json")
    }
    // Caller holds the lifecycle lock. Also coordinate with ordinary session writers.
    fn update(&self, change: impl FnOnce(&mut Sessions) -> Result<()>) -> Result<()> {
        let _lock = self.storage.lock(
            &self
                .storage
                .profile_path(&self.profile, "sessions.json.rust.lock")?,
        )?;
        let mut sessions = self.storage.sessions(&self.profile)?;
        change(&mut sessions)?;
        self.storage.write(
            &self.storage.profile_path(&self.profile, "sessions.json")?,
            &sessions,
        )
    }
    pub fn create(&self, name: Option<&str>, options: RuntimeOptions) -> Result<SessionView> {
        self.create_owned(name, options, None)
    }
    pub fn create_owned(
        &self,
        name: Option<&str>,
        options: RuntimeOptions,
        owner_id: Option<&str>,
    ) -> Result<SessionView> {
        if name.is_some_and(|n| {
            n.trim().is_empty() || n.len() > 128 || n.chars().any(char::is_control)
        }) {
            return Err(Error::ControlProtocol(
                "会话名称必须为 1–128 字节且不含控制字符",
            ));
        }
        let _guard = self.guard()?;
        let path = self.journal()?;
        let pending: Option<Operation> = self.storage.read(&path)?;
        let (name, nbh, session, owner_id) = match pending {
            Some(Operation::Allocate {
                name: old,
                nbh,
                options: old_options,
                session,
                owner_id: old_owner,
            }) if (name.is_none_or(|n| n == old)
                || (session.is_none() && old_owner.is_none() && owner_id.is_none()))
                && options == old_options
                && owner_id.is_none_or(|id| old_owner.as_deref() == Some(id)) =>
            {
                let new_name = name.unwrap_or(&old).to_owned();
                if new_name != old {
                    if self
                        .storage
                        .sessions(&self.profile)?
                        .contains_key(&new_name)
                    {
                        return Err(Error::SessionExists(new_name));
                    }
                    // Keep the same remote identity, even after an uncertain POST. A new local
                    // label must not cause a second allocation or discard recovery information.
                    self.storage.write(
                        &path,
                        &Operation::Allocate {
                            name: new_name.clone(),
                            nbh: nbh.clone(),
                            options: old_options,
                            session: None,
                            owner_id: None,
                        },
                    )?;
                }
                (new_name, nbh, session, old_owner)
            }
            Some(_) => return Err(Error::ControlPending),
            None => {
                let nbh = notebook_identity();
                let name = name
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("session-{}", &nbh[..8]));
                if self.storage.sessions(&self.profile)?.contains_key(&name) {
                    return Err(Error::SessionExists(name));
                }
                self.storage.write(
                    &path,
                    &Operation::Allocate {
                        name: name.clone(),
                        nbh: nbh.clone(),
                        options: options.clone(),
                        session: None,
                        owner_id: owner_id.map(str::to_owned),
                    },
                )?;
                (name, nbh, None, owner_id.map(str::to_owned))
            }
        };
        let event_id = format!("allocate:{nbh}");
        let session = match session {
            Some(session) => *session,
            None => {
                let assignment = self.api.assign(&nbh, &options)?;
                let proxy = &assignment.runtime_proxy_info;
                let mut session = SessionState {
                    name: name.clone(),
                    token: proxy.token.clone(),
                    url: proxy.url.clone(),
                    endpoint: assignment.endpoint.clone(),
                    token_expires_at: Some(expiry(
                        assignment.runtime_proxy_info.token_expires_in_seconds,
                    )?),
                    variant: assignment
                        .variant
                        .map(variant)
                        .unwrap_or_else(|| options.variant().into()),
                    accelerator: assignment
                        .accelerator
                        .clone()
                        .unwrap_or_else(|| options.accelerator.clone()),
                    machine_shape: if options.shape().is_some() {
                        "HIGH_RAM"
                    } else {
                        "STANDARD"
                    }
                    .into(),
                    kernel_id: None,
                    session_id: None,
                    last_execution: None,
                    running: None,
                    extra: Map::new(),
                };
                if let Some(owner) = &owner_id {
                    session
                        .extra
                        .insert("run_owner".into(), serde_json::json!(owner));
                }
                // If this write fails, the original notebook identity still permits reconciliation.
                self.storage.write(
                    &path,
                    &Operation::Allocate {
                        name: name.clone(),
                        nbh,
                        options,
                        session: Some(Box::new(session.clone())),
                        owner_id,
                    },
                )?;
                session
            }
        };
        self.update(|sessions| {
            if sessions
                .get(&name)
                .is_some_and(|old| old.endpoint != session.endpoint)
            {
                return Err(Error::SessionExists(name.clone()));
            }
            sessions.insert(name.clone(), session.clone());
            Ok(())
        })?;
        crate::history::History::new(self.storage.clone(), self.profile.clone()).append_locked(
            &name, Some(&session.endpoint), "session_created",
            serde_json::json!({"accelerator":session.accelerator,"variant":session.variant,"machine_shape":session.machine_shape}), Some(&event_id),
        )?;
        self.storage.remove(&path)?;
        Ok(self.view(Some(name), &session, "request"))
    }
    pub fn status(&self, name: Option<&str>) -> Result<Vec<SessionView>> {
        let _guard = self.guard()?;
        let assignments = self.sync_locked()?;
        let sessions = self.storage.sessions(&self.profile)?;
        if let Some(name) = name {
            let session = sessions
                .get(name)
                .ok_or_else(|| Error::SessionMissing(name.into()))?;
            return Ok(vec![self.view(Some(name.into()), session, "backend")]);
        }
        let mut views: Vec<_> = sessions
            .iter()
            .map(|(name, s)| self.view(Some(name.clone()), s, "backend"))
            .collect();
        for assignment in &assignments {
            if !sessions.values().any(|s| s.endpoint == assignment.endpoint) {
                views.push(self.orphan(assignment)?);
            }
        }
        Ok(views)
    }
    fn sync_locked(&self) -> Result<Vec<Assignment>> {
        // Never prune on a transport, HTTP or schema failure.
        let assignments = self.api.assignments()?;
        self.update(|sessions| {
            for session in sessions
                .values()
                .filter(|s| !assignments.iter().any(|a| a.endpoint == s.endpoint))
            {
                crate::history::History::new(self.storage.clone(), self.profile.clone())
                    .append_locked(
                        &session.name,
                        Some(&session.endpoint),
                        "session_terminated",
                        serde_json::json!({"reason":"backend_missing"}),
                        Some(&format!("prune:{}", session.endpoint)),
                    )?;
            }
            sessions.retain(|_, s| assignments.iter().any(|a| a.endpoint == s.endpoint));
            for session in sessions.values_mut() {
                let a = assignments
                    .iter()
                    .find(|a| a.endpoint == session.endpoint)
                    .unwrap();
                session.token = a.runtime_proxy_info.token.clone();
                session.url = a.runtime_proxy_info.url.clone();
                session.token_expires_at =
                    Some(expiry(a.runtime_proxy_info.token_expires_in_seconds)?);
                session.accelerator = a.accelerator.clone().unwrap();
                session.variant = variant(a.variant.unwrap());
                session.machine_shape = shape(a.machine_shape.unwrap());
            }
            Ok(())
        })?;
        Ok(assignments)
    }
    /// Run against an existing VM while keeping its account lifecycle stable.
    /// No VM is created; credentials missing or within five minutes of expiry are refreshed.
    pub fn with_session<T>(
        &self,
        name: Option<&str>,
        action: impl FnOnce(&SessionState) -> Result<T>,
    ) -> Result<T> {
        self.with_session_checkpoints(name, |state, _| action(state))
    }
    /// A checkpoint runs under the lifecycle lock and preserves resource ownership.
    pub fn with_session_checkpoints<T>(
        &self,
        name: Option<&str>,
        action: impl FnOnce(&SessionState, &dyn Fn(SessionState) -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        let _guard = self.guard()?;
        if self.journal()?.exists() {
            return Err(Error::ControlPending);
        }
        let sessions = self.storage.sessions(&self.profile)?;
        let name = match name {
            Some(name) => name.to_owned(),
            None if sessions.len() == 1 => sessions.keys().next().unwrap().clone(),
            None if sessions.is_empty() => {
                return Err(Error::SessionMissing("当前账户没有本地会话".into()));
            }
            None => return Err(Error::Ambiguous("请用 -s 指定会话".into())),
        };
        let session = sessions
            .get(&name)
            .ok_or_else(|| Error::SessionMissing(name.clone()))?;
        if session.token.is_empty()
            || session
                .token_expires_at
                .is_none_or(|expiry| expiry <= Utc::now() + chrono::Duration::minutes(5))
        {
            self.sync_locked()?;
        }
        let sessions = self.storage.sessions(&self.profile)?;
        let state = sessions
            .get(&name)
            .ok_or_else(|| Error::SessionMissing(name.clone()))?;
        let checkpoint = |updated: SessionState| {
            if updated.name != name || updated.endpoint != state.endpoint {
                return Err(Error::KernelProtocol("checkpoint 不能切换会话或 endpoint"));
            }
            self.update(|sessions| {
                sessions.insert(name.clone(), updated);
                Ok(())
            })
        };
        action(state, &checkpoint)
    }
    /// Serialize execution per endpoint without holding the account lock while waiting on user code.
    /// stop/status/restart may proceed; stale checkpoints may never recreate or replace their state.
    pub fn with_execution_session<T>(
        &self,
        name: Option<&str>,
        action: impl FnOnce(&SessionState, &dyn Fn(SessionState) -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        use sha2::{Digest, Sha256};
        let snapshot = self.with_session(name, |state| Ok(state.clone()))?;
        let lease_path = self.storage.path(format!(
            "locks/runtime/{}/{:x}.lock",
            self.profile,
            Sha256::digest(snapshot.endpoint.as_bytes())
        ))?;
        let _execution = self.storage.lock(&lease_path)?;
        let state = self.with_session(Some(&snapshot.name), |state| Ok(state.clone()))?;
        if state.endpoint != snapshot.endpoint {
            return Err(Error::KernelProtocol("等待执行租约期间 VM 已被替换"));
        }
        let generation = state.extra.get("kernel_generation").cloned();
        let checkpoint = |updated: SessionState| {
            if updated.name != state.name || updated.endpoint != state.endpoint {
                return Err(Error::KernelProtocol("checkpoint 不能切换会话或 endpoint"));
            }
            let _guard = self.guard()?;
            if self.journal()?.exists() {
                return Err(Error::ControlPending);
            }
            self.update(|sessions| {
                let current = sessions
                    .get_mut(&state.name)
                    .ok_or_else(|| Error::SessionMissing(state.name.clone()))?;
                if current.endpoint != state.endpoint
                    || current.extra.get("kernel_generation") != generation.as_ref()
                {
                    return Err(Error::KernelProtocol(
                        "执行期间 VM 或 kernel 已变化；没有写回旧状态",
                    ));
                }
                // Preserve a proxy refresh or hardware sync performed concurrently by status.
                current.kernel_id = updated.kernel_id;
                current.session_id = updated.session_id;
                current.running = updated.running;
                current.last_execution = updated.last_execution;
                for key in ["kernel_start_pending", "kernel_restart_pending"] {
                    if let Some(value) = updated.extra.get(key) {
                        current.extra.insert(key.into(), value.clone());
                    } else {
                        current.extra.remove(key);
                    }
                }
                Ok(())
            })
        };
        action(&state, &checkpoint)
    }
    pub fn stop(&self, name: &str) -> Result<()> {
        self.stop_if_endpoint(name, None)
    }
    /// Convert only this run's durable allocation into cleanup; never allocate while cleaning up.
    pub fn cleanup_owned(
        &self,
        owner: &str,
        name: &str,
        known_endpoint: Option<&str>,
    ) -> Result<Option<String>> {
        let endpoint = {
            let _guard = self.guard()?;
            let path = self.journal()?;
            let pending: Option<Operation> = self.storage.read(&path)?;
            let endpoint = match pending {
                Some(Operation::Allocate {
                    name: old,
                    owner_id,
                    session,
                    ..
                }) if old == name && owner_id.as_deref() == Some(owner) => {
                    session.map(|s| s.endpoint).ok_or(Error::Run(
                        "分配结果未知；先用同一账户和参数 new 调和，再 run-cleanup",
                    ))?
                }
                Some(Operation::Release {
                    name: old,
                    endpoint,
                    event_id,
                }) if old == name
                    && (known_endpoint == Some(endpoint.as_str())
                        || event_id.as_deref()
                            == Some(format!("run-release:{owner}").as_str())) =>
                {
                    endpoint
                }
                Some(_) => return Err(Error::ControlPending),
                None => match known_endpoint {
                    Some(endpoint) => endpoint.into(),
                    None => match self.storage.sessions(&self.profile)?.get(name).filter(|s| {
                        s.extra.get("run_owner").and_then(serde_json::Value::as_str) == Some(owner)
                    }) {
                        Some(session) => session.endpoint.clone(),
                        None => return Ok(None),
                    },
                },
            };
            self.storage.write(
                &path,
                &Operation::Release {
                    name: name.into(),
                    endpoint: endpoint.clone(),
                    event_id: Some(format!("run-release:{owner}")),
                },
            )?;
            endpoint
        };
        self.stop_if_endpoint(name, Some(&endpoint))?;
        Ok(Some(endpoint))
    }
    pub fn stop_if_endpoint(&self, name: &str, expected: Option<&str>) -> Result<()> {
        let _guard = self.guard()?;
        let path = self.journal()?;
        let pending: Option<Operation> = self.storage.read(&path)?;
        let (endpoint, event_id) = match pending {
            Some(Operation::Release {
                name: old,
                endpoint,
                event_id,
            }) if old == name => (
                endpoint,
                event_id.unwrap_or_else(crate::jupyter::codec::message_id),
            ),
            Some(_) => return Err(Error::ControlPending),
            None => {
                let endpoint = self
                    .storage
                    .sessions(&self.profile)?
                    .get(name)
                    .ok_or_else(|| Error::SessionMissing(name.into()))?
                    .endpoint
                    .clone();
                (endpoint, crate::jupyter::codec::message_id())
            }
        };
        if expected.is_some_and(|expected| endpoint != expected) {
            return Err(Error::KernelProtocol(
                "会话已被替换；拒绝释放不属于本次 run 的 endpoint",
            ));
        }
        self.storage.write(
            &path,
            &Operation::Release {
                name: name.into(),
                endpoint: endpoint.clone(),
                event_id: Some(event_id.clone()),
            },
        )?;
        // After a lost POST response an explicit retry confirms absence before resending.
        if self
            .api
            .assignments()?
            .iter()
            .any(|a| a.endpoint == endpoint)
        {
            self.api.unassign(&endpoint)?;
        }
        self.update(|sessions| {
            sessions.retain(|_, s| s.endpoint != endpoint);
            Ok(())
        })?;
        crate::history::History::new(self.storage.clone(), self.profile.clone()).append_locked(
            name,
            Some(&endpoint),
            "session_terminated",
            serde_json::json!({"reason":"released"}),
            Some(&event_id),
        )?;
        self.storage.remove(&path)
    }
    pub fn record_event(
        &self,
        name: &str,
        endpoint: Option<&str>,
        kind: &str,
        data: serde_json::Value,
        event_id: Option<&str>,
    ) -> Result<()> {
        crate::history::History::new(self.storage.clone(), self.profile.clone())
            .append(name, endpoint, kind, data, event_id)
    }
    pub fn with_file_operation<T>(
        &self,
        name: Option<&str>,
        op: &str,
        path: &str,
        action: impl FnOnce(&SessionState) -> Result<T>,
    ) -> Result<T> {
        self.with_session(name, |state| {
            let result = action(state)?;
            crate::history::History::new(self.storage.clone(), self.profile.clone())
                .append_locked(
                    &state.name,
                    Some(&state.endpoint),
                    "file_operation",
                    serde_json::json!({"op":op,"path":path}),
                    None,
                )
                .map_err(|source| Error::HistoryAfterOperation {
                    operation: op.into(),
                    source: Box::new(source),
                })?;
            Ok(result)
        })
    }
    pub fn usage(&self) -> Result<crate::colab_api::Usage> {
        let _guard = self.guard()?;
        self.api.usage()
    }
    fn view(
        &self,
        name: Option<String>,
        s: &SessionState,
        shape_source: &'static str,
    ) -> SessionView {
        SessionView {
            account: self.profile.clone(),
            name,
            endpoint: s.endpoint.clone(),
            accelerator: s.accelerator.clone(),
            variant: s.variant.clone(),
            machine_shape: s.machine_shape.clone(),
            shape_source,
            token_expires_at: s.token_expires_at,
            kernel_id: s.kernel_id.clone(),
            session_id: s.session_id.clone(),
            running: s.running.clone(),
            kernel_start_pending: s.extra.get("kernel_start_pending")
                == Some(&serde_json::json!(true)),
            kernel_restart_pending: s.extra.get("kernel_restart_pending")
                == Some(&serde_json::json!(true)),
            last_execution: s.last_execution.clone(),
        }
    }
    fn orphan(&self, a: &Assignment) -> Result<SessionView> {
        Ok(SessionView {
            account: self.profile.clone(),
            name: None,
            endpoint: a.endpoint.clone(),
            accelerator: a.accelerator.clone().unwrap(),
            variant: variant(a.variant.unwrap()),
            machine_shape: shape(a.machine_shape.unwrap()),
            shape_source: "backend",
            token_expires_at: Some(expiry(a.runtime_proxy_info.token_expires_in_seconds)?),
            kernel_id: None,
            session_id: None,
            running: None,
            kernel_start_pending: false,
            kernel_restart_pending: false,
            last_execution: None,
        })
    }
}
fn expiry(seconds: i64) -> Result<DateTime<Utc>> {
    Utc::now()
        .checked_add_signed(
            chrono::Duration::try_seconds(seconds)
                .ok_or(Error::ControlProtocol("token 有效期溢出"))?,
        )
        .ok_or(Error::ControlProtocol("token 有效期溢出"))
}
fn variant(value: i64) -> String {
    match value {
        0 => "DEFAULT".into(),
        1 => "GPU".into(),
        2 => "TPU".into(),
        _ => format!("Unknown({value})"),
    }
}
fn shape(value: i64) -> String {
    match value {
        0 => "STANDARD".into(),
        1 => "HIGH_RAM".into(),
        _ => format!("Unknown({value})"),
    }
}

/// Pending release intent remains routable after a successful status query prunes local state.
pub(crate) fn owns_name(storage: &Storage, profile: &ProfileId, name: &str) -> Result<bool> {
    if storage.sessions(profile)?.contains_key(name) {
        return Ok(true);
    }
    let pending: Option<Operation> =
        storage.read(&storage.profile_path(profile, "control_operation.json")?)?;
    Ok(match pending {
        Some(
            Operation::Allocate { name: pending, .. } | Operation::Release { name: pending, .. },
        ) => pending == name,
        None => false,
    })
}
