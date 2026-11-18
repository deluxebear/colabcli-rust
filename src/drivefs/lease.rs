//! Durable, secret-free ownership shared by the compute and Drive accounts.
use super::remote::RemoteRequest;
use crate::{Error, Result, models::ProfileId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub version: u32,
    pub compute_account: ProfileId,
    pub drive_account: ProfileId,
    pub session: String,
    pub endpoint: String,
    pub request: RemoteRequest,
    pub fingerprint: String,
    pub identity: String,
}
impl Lease {
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;
        if self.version != 1
            || self.session.is_empty()
            || self.endpoint.is_empty()
            || [&self.fingerprint, &self.identity]
                .iter()
                .any(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::KernelProtocol("DriveFS 凭据租约无效"));
        }
        Ok(())
    }
    pub(crate) fn file(&self) -> String {
        format!("managed/drivefs-{}.json", self.request.operation_id)
    }
}
