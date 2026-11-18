//! Remote-only DriveFS components. This module does not start or mount anything.
pub mod lease;
pub mod lifecycle;
pub mod probe;
pub mod remote;
pub mod result;
use crate::{Result, drive_credentials::DriveCredentials, models::Secret};
use serde::Serialize;

/// Runs with the VM's Python standard library; no local Python dependency.
pub const METADATA_SHIM_SOURCE: &str = include_str!("drivefs/metadata_shim.py");
/// Owns one mount's remote state and processes. Invoked by the remote wrapper.
pub const CONTROLLER_SOURCE: &str = include_str!("drivefs/controller.py");

/// Sensitive transport payload with redacted `Debug`. Pass through a
/// password input_reply, never interpolate into Python source or history.
pub fn metadata_config(credentials: &DriveCredentials) -> Result<Secret> {
    credentials.validate_for_mount(false)?;
    #[derive(Serialize)]
    struct Config<'a> {
        version: u8,
        client_id: &'a str,
        client_secret: Option<&'a str>,
        refresh_token: &'a str,
        token_uri: &'a str,
        user_id: &'a str,
        email: &'a str,
        scopes: &'a [String],
        scope_confirmed: bool,
    }
    let config = Config {
        version: 1,
        client_id: &credentials.oauth.client_id,
        client_secret: credentials.oauth.client_secret.as_ref().map(|s| s.expose()),
        refresh_token: credentials.oauth.refresh_token.as_ref().unwrap().expose(),
        token_uri: &credentials.oauth.token_uri,
        user_id: credentials.user_id.as_deref().unwrap(),
        email: credentials.email.as_deref().unwrap_or(""),
        scopes: &credentials.oauth.scopes,
        scope_confirmed: credentials
            .oauth
            .extra
            .get("drive_scope_source")
            .and_then(|v| v.as_str())
            == Some("token_response"),
    };
    // All fields are strings/numbers; serialization cannot fail. Keep the
    // diagnostic static in case the representation changes in the future.
    let encoded = serde_json::to_string(&config)
        .map(Secret::new)
        .map_err(|_| crate::Error::OAuth("无法序列化 DriveFS 配置"))?;
    if encoded.expose().len() > 65536 {
        return Err(crate::Error::OAuth("DriveFS 配置超过大小限制"));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive_credentials::DRIVE_SCOPE;
    use serde_json::json;

    fn credentials() -> DriveCredentials {
        serde_json::from_value(json!({
            "flavor":"imported", "user_id":"fixture-gaia", "email":"fixture@example.invalid",
            "client_id":"client", "client_secret":"client-secret", "refresh_token":"refresh-secret",
            "token":"access-secret", "token_uri":"https://oauth2.googleapis.com/token",
            "scopes":[DRIVE_SCOPE], "unknown_private_field":"unknown-secret"
        }))
        .unwrap()
    }

    #[test]
    fn remote_config_transfers_only_required_drive_credentials() {
        let encoded = metadata_config(&credentials()).unwrap();
        let value: serde_json::Value = serde_json::from_str(encoded.expose()).unwrap();
        assert_eq!(value["refresh_token"], "refresh-secret");
        assert_eq!(value["client_secret"], "client-secret");
        assert_eq!(value["version"], 1);
        assert_eq!(value.as_object().unwrap().len(), 9);
        assert!(!encoded.expose().contains("access-secret"));
        assert!(!encoded.expose().contains("unknown-secret"));
        assert_eq!(format!("{encoded:?}"), "[REDACTED]");
        assert!(METADATA_SHIM_SOURCE.contains("class Tokens:"));
    }

    #[test]
    fn remote_config_requires_identity_and_full_scope() {
        let mut credentials = credentials();
        credentials.user_id = None;
        assert!(metadata_config(&credentials).is_err());
        credentials.user_id = Some("fixture-gaia".into());
        credentials.oauth.scopes = vec!["https://www.googleapis.com/auth/drive.file".into()];
        assert!(metadata_config(&credentials).is_err());
    }

    #[test]
    fn remote_config_rejects_oversized_credentials_without_echoing_values() {
        let mut credentials = credentials();
        credentials.oauth.client_id = "private-value".repeat(7000);
        let error = metadata_config(&credentials).unwrap_err().to_string();
        assert!(!error.contains("private-value"));
        assert!(error.contains("大小限制"));
    }
}
