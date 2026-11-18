//! Account-bound credentials retained for the lifetime of a control client.
use super::{AdcProvider, OAuthCredentials, OAuthService, RequestCredentials};
use crate::{
    Error, Result,
    models::{ProfileId, Secret},
    storage::FileLock,
};
use chrono::Utc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub struct ControlCredentials {
    source: Source,
    cancellation: Option<Arc<AtomicUsize>>,
}
enum Source {
    OAuth {
        service: OAuthService,
        profile: ProfileId,
        cache: Mutex<Option<OAuthCredentials>>,
    },
    Adc {
        provider: AdcProvider,
        refresh: Mutex<()>,
    },
}
impl ControlCredentials {
    pub fn oauth(service: OAuthService, profile: ProfileId) -> Self {
        Self {
            source: Source::OAuth {
                service,
                profile,
                cache: Mutex::new(None),
            },
            cancellation: None,
        }
    }
    pub fn adc(provider: AdcProvider) -> Self {
        Self {
            source: Source::Adc {
                provider,
                refresh: Mutex::new(()),
            },
            cancellation: None,
        }
    }
    pub fn with_cancellation(mut self, cancellation: Option<Arc<AtomicUsize>>) -> Self {
        self.cancellation = cancellation;
        self
    }
    fn cancelled(&self) -> Result<()> {
        let signal = self
            .cancellation
            .as_ref()
            .map_or(0, |flag| flag.load(Ordering::SeqCst));
        if signal != 0 {
            return Err(Error::CliExit(128 + signal as i32));
        }
        Ok(())
    }
    pub(crate) fn credentials(
        &self,
        rejected: Option<&Secret>,
        guard: Option<&FileLock>,
    ) -> Result<RequestCredentials> {
        self.cancelled()?;
        let credentials = match &self.source {
            Source::OAuth {
                service,
                profile,
                cache,
            } => {
                // Always lifecycle -> memory -> refresh lock. In particular an
                // unguarded propagation worker must never invert session locks.
                let owned;
                let guard = match guard {
                    Some(guard) if guard.protects_profile(&service.storage, profile)? => {
                        service.validate_account(profile)?;
                        guard
                    }
                    Some(_) => return Err(Error::OAuth("凭据刷新账户锁不匹配")),
                    None => {
                        owned = service.guard_account(profile)?;
                        &owned
                    }
                };
                let mut cache = cache.lock().map_err(|_| Error::OAuth("控制面凭据锁失效"))?;
                self.cancelled()?;
                let now = Utc::now();
                let valid = cache
                    .as_ref()
                    .is_some_and(|credentials| credentials.valid_at(now));
                let force = valid
                    && rejected.is_some_and(|rejected| {
                        cache.as_ref().is_some_and(|credentials| {
                            credentials.token.expose() == rejected.expose()
                        })
                    });
                if !valid || force {
                    let credentials =
                        service.oauth_credentials(profile, now, force, Some(guard))?;
                    *cache = Some(credentials);
                }
                RequestCredentials {
                    token: cache.as_ref().unwrap().token.clone(),
                    quota_project_id: None,
                }
            }
            Source::Adc { provider, refresh } => {
                let _refresh = refresh
                    .lock()
                    .map_err(|_| Error::OAuth("控制面凭据锁失效"))?;
                self.cancelled()?;
                let now = Utc::now();
                let credentials = provider.request_credentials(now)?;
                if rejected.is_some_and(|rejected| rejected.expose() == credentials.token.expose())
                {
                    provider.refresh_request_credentials(now)?
                } else {
                    credentials
                }
            }
        };
        self.cancelled()?;
        Ok(credentials)
    }
}
