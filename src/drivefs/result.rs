//! Only this allowlisted protocol leaves the DriveFS execution boundary.
use super::remote::{AuthMode, RemoteRequest};
use crate::{Error, Result, jupyter::output::Output};
use serde::{Deserialize, Serialize};

pub const RESULT_MIME: &str = "application/vnd.colab-cli.drivefs-result+json";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Unmanaged,
    Prepared,
    ShimStarting,
    ShimReady,
    DriveStarting,
    Mounting,
    Mounted,
    Unmounting,
    CredentialsPending,
    Finalizing,
    Finished,
    Unknown,
    Error,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    Ok,
    InvalidGrant,
    RefreshFailed,
    #[serde(other)]
    Unrecognized,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteResult {
    pub version: u32,
    pub operation_id: String,
    pub mountpoint: String,
    pub ok: bool,
    pub stage: Stage,
    pub error: Option<String>,
    pub mounted: Option<bool>,
    pub managed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<AuthMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub helper_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_status: Option<RefreshStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub helper_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub helper_unknown: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_unknown: Option<bool>,
}
fn operation_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn public_error(code: &str) -> &'static str {
    match code {
        "invalid_operation" => "invalid_operation",
        "invalid_mountpoint" => "invalid_mountpoint",
        "mountpoint_overlaps_state" => "mountpoint_overlaps_state",
        "parent_missing" => "parent_missing",
        "unsafe_mountpoint" => "unsafe_mountpoint",
        "unsafe_state" => "unsafe_state",
        "invalid_state" => "invalid_state",
        "mount_table_unavailable" => "mount_table_unavailable",
        "invalid_process" => "invalid_process",
        "process_ownership_mismatch" => "process_ownership_mismatch",
        "process_identity_unavailable" => "process_identity_unavailable",
        "ambiguous_owned_process" => "ambiguous_owned_process",
        "process_not_started" => "process_not_started",
        "process_exited" => "process_exited",
        "safe_process_signal_unavailable" => "safe_process_signal_unavailable",
        "process_stop_timeout" => "process_stop_timeout",
        "mount_ownership_mismatch" => "mount_ownership_mismatch",
        "unmount_failed" => "unmount_failed",
        "drivefs_unavailable" => "drivefs_unavailable",
        "fuse_unavailable" => "fuse_unavailable",
        "metadata_probe_failed" => "metadata_probe_failed",
        "stacked_mounts" => "stacked_mounts",
        "state_on_fuse" => "state_on_fuse",
        "unsafe_lock" => "unsafe_lock",
        "mount_busy" => "mount_busy",
        "credentials_required" => "credentials_required",
        "invalid_options" => "invalid_options",
        "rt_client_mismatch" => "rt_client_mismatch",
        "operation_pending" => "operation_pending",
        "foreign_mount" => "foreign_mount",
        "nested_fuse_mount" => "nested_fuse_mount",
        "mountpoint_not_empty" => "mountpoint_not_empty",
        "operation_already_exists" => "operation_already_exists",
        "shim_source_required" => "shim_source_required",
        "helper_exited" => "helper_exited",
        "helper_timeout" => "helper_timeout",
        "invalid_helper_readiness" => "invalid_helper_readiness",
        "unexpected_filesystem" => "unexpected_filesystem",
        "drive_exited" => "drive_exited",
        "mount_timeout" => "mount_timeout",
        "process_result_unknown" => "process_result_unknown",
        "mount_reappeared" => "mount_reappeared",
        "mountpoint_replaced" => "mountpoint_replaced",
        "state_contains_mount" => "state_contains_mount",
        "invalid_action" => "invalid_action",
        "credentials_recovery_required" => "credentials_recovery_required",
        "credentials_missing" => "credentials_missing",
        "credentials_owner_mismatch" => "credentials_owner_mismatch",
        "invalid_credentials_receipt" => "invalid_credentials_receipt",
        "credentials_receipt_mismatch" => "credentials_receipt_mismatch",
        "credentials_not_frozen" => "credentials_not_frozen",
        _ => "remote_operation_failed",
    }
}

/// Reject mismatched, duplicate, incomplete, and malformed results without
/// formatting raw outputs, errors or unknown fields into diagnostics.
pub fn parse(outputs: &[Output], request: &RemoteRequest) -> Result<RemoteResult> {
    request.validate()?;
    let mut values = outputs.iter().filter_map(|output| {
        output
            .data
            .get("data")
            .and_then(|data| data.get(RESULT_MIME))
    });
    let value = values
        .next()
        .ok_or(Error::KernelProtocol("DriveFS 缺少结构化结果"))?;
    if values.next().is_some() {
        return Err(Error::KernelProtocol("DriveFS 返回重复结果"));
    }
    let mut result: RemoteResult = serde_json::from_value(value.clone())
        .map_err(|_| Error::KernelProtocol("DriveFS 结果协议无效"))?;
    if result.version != 1
        || result.operation_id != request.operation_id
        || result.mountpoint != request.mountpoint
        || result.ok == result.error.is_some()
        || (result.ok && result.stage == Stage::Error)
        || (!result.ok && result.stage != Stage::Error)
        || result
            .owner_operation_id
            .as_deref()
            .is_some_and(|id| !operation_id(id))
        || result
            .identity
            .as_deref()
            .is_some_and(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
        || (result.managed == Some(true) && result.owner_operation_id.is_none())
        || [result.helper_pid, result.drive_pid].contains(&Some(0))
        || (result.helper_alive == Some(true) && result.helper_pid.is_none())
        || (result.drive_alive == Some(true) && result.drive_pid.is_none())
    {
        return Err(Error::KernelProtocol(
            "DriveFS 结果与请求不匹配或字段不一致",
        ));
    }
    result.error = result.error.as_deref().map(public_error).map(str::to_owned);
    if result.refresh_status == Some(RefreshStatus::Unrecognized)
        || result.managed != Some(true)
        || result.mode != Some(AuthMode::Shim)
        || result.helper_alive != Some(true)
    {
        result.refresh_status = None;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn output(value: Value) -> Output {
        Output {
            data: json!({"output_type":"display_data","data":{RESULT_MIME:value}})
                .as_object()
                .unwrap()
                .clone(),
            buffers: vec![],
        }
    }
    fn result(request: &RemoteRequest) -> Value {
        json!({"version":1,"operation_id":request.operation_id,"mountpoint":request.mountpoint,
        "ok":true,"stage":"unmanaged","error":null,"mounted":false,"managed":false})
    }
    #[test]
    fn refresh_diagnostic_is_projected_only_for_an_active_shim() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        let mut value = result(&request);
        value["managed"] = json!(true);
        value["owner_operation_id"] = json!("a".repeat(32));
        value["stage"] = json!("mounted");
        value["mounted"] = json!(true);
        value["mode"] = json!("shim");
        value["helper_alive"] = json!(true);
        value["helper_pid"] = json!(42);
        value["refresh_status"] = json!("invalid_grant");
        assert_eq!(
            parse(&[output(value.clone())], &request)
                .unwrap()
                .refresh_status,
            Some(RefreshStatus::InvalidGrant)
        );
        value["refresh_status"] = json!("fixture-private-upstream-description");
        let report = parse(&[output(value.clone())], &request).unwrap();
        assert!(
            serde_json::to_value(report)
                .unwrap()
                .get("refresh_status")
                .is_none()
        );
        value["refresh_status"] = json!("ok");
        value["helper_alive"] = json!(false);
        assert!(
            parse(&[output(value)], &request)
                .unwrap()
                .refresh_status
                .is_none()
        );
    }
    #[test]
    fn structured_result_discards_unknown_fields_and_redacts_unknown_error_codes() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        let mut value = result(&request);
        value["private_unknown_field"] = json!("fixture-secret");
        let parsed = parse(&[output(value.clone())], &request).unwrap();
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("fixture-secret")
        );
        value["ok"] = json!(false);
        value["stage"] = json!("error");
        value["error"] = json!("fixture-secret");
        assert_eq!(
            parse(&[output(value.clone())], &request)
                .unwrap()
                .error
                .as_deref(),
            Some("remote_operation_failed")
        );
        value["error"] = json!("mount_ownership_mismatch");
        assert_eq!(
            parse(&[output(value)], &request).unwrap().error.as_deref(),
            Some("mount_ownership_mismatch")
        );
    }
    #[test]
    fn structured_result_requires_exact_operation_path_and_one_result() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        let value = result(&request);
        assert!(parse(&[], &request).is_err());
        assert!(parse(&[output(value.clone()), output(value.clone())], &request).is_err());
        for (key, bad) in [
            ("operation_id", json!("b".repeat(32))),
            ("mountpoint", json!("/content/other")),
            ("version", json!(2)),
            ("version", json!(true)),
            ("stage", json!("fixture-secret")),
        ] {
            let mut changed = value.clone();
            changed[key] = bad;
            let error = parse(&[output(changed)], &request).unwrap_err().to_string();
            assert!(!error.contains("fixture-secret"));
        }
    }
    #[test]
    fn inconsistent_success_identity_and_process_fields_are_rejected() {
        let request = RemoteRequest::new("/content/drive").unwrap();
        for (key, bad) in [
            ("error", json!("unmount_failed")),
            ("ok", json!(false)),
            ("stage", json!("error")),
            ("managed", json!(true)),
            ("drive_alive", json!(true)),
            ("helper_pid", json!(0)),
            ("owner_operation_id", json!("../other")),
        ] {
            let mut value = result(&request);
            value[key] = bad;
            assert!(parse(&[output(value)], &request).is_err(), "{key}");
        }
    }
}
