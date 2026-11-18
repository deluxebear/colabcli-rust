use super::{CONTROLLER_SOURCE, METADATA_SHIM_SOURCE, metadata_config};
use crate::{
    Error, Result,
    drive_credentials::DriveCredentials,
    execution::Document,
    jupyter::{InputHandler, InputRequest, codec},
    models::Secret,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const WRAPPER: &str = include_str!("wrapper.py");

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Mount,
    Umount,
    Status,
    Finalize,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Shim,
    Rt,
}
/// Contains only non-secret intent, suitable for a future recovery journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteRequest {
    pub operation_id: String,
    pub mountpoint: String,
    pub mode: AuthMode,
    pub readonly: bool,
    pub force: bool,
    pub timeout: u64,
    #[serde(default = "default_port")]
    pub port: u16,
    pub purge_state: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_receipt: Option<String>,
}
impl RemoteRequest {
    pub fn new(mountpoint: &str) -> Result<Self> {
        let request = Self {
            operation_id: codec::session_id().replace('-', ""),
            mountpoint: mountpoint.into(),
            mode: AuthMode::Shim,
            readonly: false,
            force: false,
            timeout: 30,
            port: default_port(),
            purge_state: false,
            extra_flags: vec![],
            owner_operation_id: None,
            credentials_receipt: None,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        if self.operation_id.len() != 32
            || !self
                .operation_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::KernelProtocol("DriveFS operation ID 无效"));
        }
        let path = &self.mountpoint;
        if !path.starts_with('/')
            || path == "/"
            || path.len() > 4096
            || path.chars().any(|c| c.is_control() || ",:".contains(c))
            || path.split('/').any(|part| part == "." || part == "..")
            || path.contains("//")
            || path.ends_with('/')
            || path == "/content"
            || path == "/content/.colab-cli-drivefs"
            || path.starts_with("/content/.colab-cli-drivefs/")
        {
            return Err(Error::KernelProtocol("DriveFS 挂载点无效或与控制目录重叠"));
        }
        if !(1..=3600).contains(&self.timeout) {
            return Err(Error::KernelProtocol(
                "DriveFS timeout 必须在 1–3600 秒之间",
            ));
        }
        validate_extra_flags(&self.extra_flags)?;
        if self.owner_operation_id.as_ref().is_some_and(|id| {
            id.len() != 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) || self.credentials_receipt.as_ref().is_some_and(|id| {
            id.len() != 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(Error::KernelProtocol("DriveFS 凭据回收确认参数无效"));
        }
        Ok(())
    }
}

/// Keep authentication, mount ownership and private state under controller control.
/// Flags are argv data, never shell fragments. They must not carry secrets.
pub fn validate_extra_flags(flags: &[String]) -> Result<()> {
    const RESERVED: &[&str] = &[
        "features",
        "preferences",
        "user_id",
        "user_email",
        "client_id",
        "client_secret",
        "mountpoint",
        "mount_point_path",
        "single_process",
        "push_changes_and_quit",
        "timeout_sec",
        "home",
        "log_dir",
        "log_file",
        "log_path",
        "config",
        "help",
        "version",
    ];
    if flags.len() > 32
        || flags.iter().any(|flag| {
            let name = flag
                .strip_prefix("--")
                .unwrap_or("")
                .split('=')
                .next()
                .unwrap_or("");
            name.is_empty()
                || !name.as_bytes()[0].is_ascii_lowercase()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                || flag.len() > 4096
                || flag.chars().any(char::is_control)
                || RESERVED.contains(&name)
                || ["auth", "token", "secret", "password"]
                    .iter()
                    .any(|word| name.contains(word))
        })
    {
        return Err(Error::KernelProtocol(
            "DriveFS 额外参数无效或覆盖受管认证/路径；只接受非秘密 --name[=value]",
        ));
    }
    Ok(())
}

pub struct Prepared {
    source: String,
    operation_id: String,
    pub input: Option<CredentialInput>,
}
fn default_port() -> u16 {
    8012
}
impl Prepared {
    pub fn document(&self) -> Result<Document> {
        Document::drivefs(&self.source, &self.operation_id)
    }
    /// Executable source contains static components and non-secret intent only.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Sends credentials once, to the selected endpoint and exact password prompt.
pub struct CredentialInput {
    endpoint: String,
    bound: bool,
    prompt: String,
    payload: Option<Secret>,
}
impl InputHandler for CredentialInput {
    fn bind_endpoint(&mut self, endpoint: &str) -> Result<()> {
        if endpoint != self.endpoint {
            self.payload = None;
            return Err(Error::KernelProtocol("DriveFS 输入的会话 endpoint 已变化"));
        }
        self.bound = true;
        Ok(())
    }
    fn poll(&mut self, request: &InputRequest) -> Result<Option<String>> {
        if !self.bound || !request.password || request.prompt != self.prompt {
            self.payload = None;
            return Err(Error::KernelProtocol("拒绝非预期的 DriveFS 凭据输入请求"));
        }
        self.payload
            .take()
            .map(|payload| Some(payload.expose().to_owned()))
            .ok_or(Error::KernelProtocol("DriveFS 凭据输入不得重复发送"))
    }
}

pub fn prepare(
    action: Action,
    request: &RemoteRequest,
    credentials: Option<&DriveCredentials>,
    endpoint: &str,
) -> Result<Prepared> {
    request.validate()?;
    if action == Action::Finalize
        && (request.owner_operation_id.is_none() || request.credentials_receipt.is_none())
    {
        return Err(Error::KernelProtocol(
            "DriveFS 清理需要已持久保存的凭据回收确认",
        ));
    }
    if endpoint.is_empty() {
        return Err(Error::KernelProtocol("DriveFS 缺少绑定 endpoint"));
    }
    let prompt = format!("COLAB_DRIVE_CONFIG:{}", request.operation_id);
    let input = if action == Action::Mount {
        let credentials = credentials.ok_or(Error::NeedsAuth)?;
        credentials.validate_for_mount(request.mode == AuthMode::Rt)?;
        Some(CredentialInput {
            endpoint: endpoint.into(),
            bound: false,
            prompt: prompt.clone(),
            payload: Some(metadata_config(credentials)?),
        })
    } else {
        None
    };
    let payload = json!({"action":action,"request":request,"prompt":prompt,
        "shim_source":METADATA_SHIM_SOURCE,"controller_source":CONTROLLER_SOURCE});
    let literal = serde_json::to_string(&payload.to_string())
        .map_err(|_| Error::KernelProtocol("DriveFS 远端请求编码失败"))?;
    Ok(Prepared {
        source: WRAPPER.replacen("__COLAB_DRIVEFS_PAYLOAD__", &literal, 1),
        operation_id: request.operation_id.clone(),
        input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credentials() -> DriveCredentials {
        serde_json::from_value(json!({"flavor":"imported","user_id":"fixture-gaia",
            "client_id":"fixture-client","client_secret":"fixture-private-client-secret",
            "refresh_token":"fixture-private-refresh-token","token":"fixture-private-access-token",
            "token_uri":"https://oauth2.googleapis.com/token","scopes":[crate::drive_credentials::DRIVE_SCOPE]})).unwrap()
    }
    fn prompt(id: &str) -> InputRequest {
        InputRequest {
            id: "parent-message".into(),
            prompt: format!("COLAB_DRIVE_CONFIG:{id}"),
            password: true,
        }
    }
    #[test]
    fn secret_input_is_endpoint_bound_password_only_and_one_shot() {
        let request = RemoteRequest::new("/content/drive 空格'$(printf x)").unwrap();
        let mut prepared =
            prepare(Action::Mount, &request, Some(&credentials()), "vm-one").unwrap();
        for value in [
            "fixture-private-client-secret",
            "fixture-private-refresh-token",
            "fixture-private-access-token",
        ] {
            assert!(!prepared.source().contains(value));
        }
        assert!(prepared.document().is_ok());
        let input = prepared.input.as_mut().unwrap();
        input.bind_endpoint("vm-one").unwrap();
        let secret = input.poll(&prompt(&request.operation_id)).unwrap().unwrap();
        assert!(secret.contains("fixture-private-refresh-token"));
        assert!(!secret.contains("fixture-private-access-token"));
        assert!(input.poll(&prompt(&request.operation_id)).is_err());
    }
    #[test]
    fn invalid_prompts_or_endpoint_clear_credentials_without_a_reply() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        for invalid in ["endpoint", "prompt", "password", "unbound"] {
            let mut prepared =
                prepare(Action::Mount, &request, Some(&credentials()), "vm-one").unwrap();
            let input = prepared.input.as_mut().unwrap();
            let mut question = prompt(&request.operation_id);
            if invalid == "endpoint" {
                assert!(input.bind_endpoint("vm-two").is_err());
            } else if invalid != "unbound" {
                input.bind_endpoint("vm-one").unwrap();
            }
            if invalid == "prompt" {
                question.prompt.push('x');
            }
            if invalid == "password" {
                question.password = false;
            }
            assert!(input.poll(&question).is_err());
            assert!(input.payload.is_none());
        }
    }
    #[test]
    fn reads_and_unmounts_do_not_request_or_copy_credentials() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        for action in [Action::Status, Action::Umount] {
            assert!(
                prepare(action, &request, None, "vm-one")
                    .unwrap()
                    .input
                    .is_none()
            );
        }
        assert!(prepare(Action::Mount, &request, None, "vm-one").is_err());
        let mut rt = request;
        rt.mode = AuthMode::Rt;
        assert!(prepare(Action::Mount, &rt, Some(&credentials()), "vm-one").is_err());
        assert!(prepare(Action::Finalize, &rt, None, "vm-one").is_err());
        rt.owner_operation_id = Some("a".repeat(32));
        rt.credentials_receipt = Some("b".repeat(64));
        assert!(
            prepare(Action::Finalize, &rt, None, "vm-one")
                .unwrap()
                .input
                .is_none()
        );
    }
    #[test]
    fn extra_flags_validate_as_argv_without_overriding_auth_or_control_paths() {
        assert!(
            validate_extra_flags(&[
                "--curl_logging".into(),
                "--example_value=quote' 中文;$(ignored)".into()
            ])
            .is_ok()
        );
        for flag in [
            "--features=read_only_mode:false",
            "--preferences=mount_point_path:/other",
            "--single_process=false",
            "--push_changes_and_quit",
            "--timeout_sec=0",
            "--refresh_token=secret",
            "--metadata_server_auth_uri=other",
            "--user_id=other",
            "--log_dir=/other",
            "--help",
            "--",
            "path",
            "--flag\n--other",
            "--Flag",
        ] {
            assert!(validate_extra_flags(&[flag.into()]).is_err());
        }
        assert!(validate_extra_flags(&vec!["--flag".into(); 33]).is_err());
        assert!(validate_extra_flags(&[format!("--flag={}", "a".repeat(4096))]).is_err());
        let mut request = RemoteRequest::new("/content/drive").unwrap();
        request.extra_flags = vec!["--refresh_token=secret".into()];
        assert!(prepare(Action::Mount, &request, Some(&credentials()), "vm-one").is_err());
    }
    #[test]
    fn invalid_request_is_rejected_before_credential_transport() {
        for path in [
            "/",
            "/content",
            "relative",
            "/content/../drive",
            "/content/a,b",
            "/content/a:b",
            "/content/drive/",
            "/content//drive",
            "/content/.colab-cli-drivefs/private",
        ] {
            assert!(RemoteRequest::new(path).is_err(), "{path}");
        }
        let mut request = RemoteRequest::new("/content/drive").unwrap();
        request.operation_id = "invalid".into();
        assert!(prepare(Action::Status, &request, None, "vm-one").is_err());
        request.operation_id = "a".repeat(32);
        request.timeout = 0;
        assert!(prepare(Action::Status, &request, None, "vm-one").is_err());
    }
}
