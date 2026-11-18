//! Presentation for merged account mutations; durable operations remain in Accounts.
use crate::{AccountCommand, Cli};
use colabcli_rust::{
    Error, Result,
    accounts::{Accounts, lookup},
    context::Environment,
    models::{ProfileId, Registry},
};

fn failure(message: impl std::fmt::Display) -> Error {
    eprintln!("[account] {message}");
    Error::CliExit(1)
}
fn repr(text: &str) -> String {
    // Reuse the interchange formatter, including quoting for user-supplied selectors.
    colabcli_rust::cli_compat::python_string_repr(text)
}
fn select(registry: &Registry, selector: &str) -> Result<ProfileId> {
    select_with_prefix(registry, selector, "account")
}
pub(crate) fn select_with_prefix(
    registry: &Registry,
    selector: &str,
    prefix: &str,
) -> Result<ProfileId> {
    let failure = |message: String| {
        eprintln!("[{prefix}] {message}");
        Error::CliExit(1)
    };
    if selector.is_empty() {
        return Err(failure("Account identifier cannot be empty.".into()));
    }
    lookup(registry, selector).map_err(|error| match error {
        Error::AccountMissing(_) => failure(format!("Account {} not found (configured: {}). Run `colab account add {selector}` first.", repr(selector), registry.accounts.keys().map(ToString::to_string).collect::<Vec<_>>().join(", "))),
        Error::Ambiguous(_) => {
            let matches = registry.accounts.iter().filter(|(_, meta)| meta.email.as_ref().is_some_and(|email| email.trim().eq_ignore_ascii_case(selector.trim()))).map(|(id, _)| id.to_string()).collect::<Vec<_>>().join(", ");
            failure(format!("Email {} matches multiple accounts ({matches}); specify the profile name explicitly.", repr(selector)))
        }
        error => error,
    })
}

pub fn run(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    command: &AccountCommand,
) -> Result<()> {
    let registry = accounts.registry()?;
    match command {
        AccountCommand::Use { name } => {
            let id = select(&registry, name)?;
            accounts.use_account(id.as_str())?;
            let suffix = registry.accounts[&id]
                .email
                .as_deref()
                .filter(|email| !email.is_empty())
                .map(|email| format!(" ({email})"))
                .unwrap_or_default();
            println!("[account] Active account -> {id}{suffix}");
        }
        AccountCommand::SetDrive {
            storage: drive,
            default,
            clear,
        } => {
            if !clear && drive.is_none() {
                return Err(failure("Specify a storage account name or pass --clear."));
            }
            let drive = if *clear {
                None
            } else {
                drive
                    .as_deref()
                    .map(|name| select(&registry, name))
                    .transpose()?
            };
            let compute = if *default {
                None
            } else {
                // Resolve user selection before the durable change, with merged error presentation.
                let selector = cli
                    .account
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or(env
                        .account
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty()))
                    .unwrap_or(registry.active.as_str());
                Some(select(&registry, selector)?)
            };
            accounts.set_drive(compute.as_ref(), drive.as_ref())?;
            if let Some(compute) = compute {
                println!(
                    "[account] Drive binding for '{compute}' -> {}",
                    drive
                        .as_ref()
                        .map(ProfileId::as_str)
                        .unwrap_or("(own credentials)")
                );
            } else {
                println!(
                    "[account] Default shared Drive account -> {}",
                    drive.as_ref().map(ProfileId::as_str).unwrap_or("(cleared)")
                );
            }
        }
        AccountCommand::Rename { old, new } => {
            // The extension validates the new profile before looking up the old account.
            let new_id = ProfileId::new(new.clone()).map_err(|_| failure(format!("Invalid account name {}: must start with alphanumeric and contain only letters, digits, '.', '_', '@', '-' (max 64 chars, no '..').", repr(new))))?;
            let old_id = select(&registry, old)?;
            accounts
                .rename(&old_id, &new_id)
                .map_err(|error| match error {
                    Error::AccountExists(_) => {
                        failure(format!("Target account {} already exists.", repr(new)))
                    }
                    error => error,
                })?;
            let active = if registry.active == old_id {
                new_id
            } else {
                registry.active
            };
            println!("[account] Renamed '{old}' -> '{new}' (active: {active})");
        }
        AccountCommand::Remove { name, force } => {
            let id = select(&registry, name)?;
            accounts.remove(&id,*force).map_err(|error| match error {
                Error::AccountInUse if !force => {
                    let sessions = accounts.storage.sessions(&id).unwrap_or_default();
                    if sessions.is_empty() { return Error::AccountInUse; }
                    failure(format!("Account {} still has local session(s): {}. Stop them first (`colab -a {id} stop -s <session>`) or pass --force.", repr(id.as_str()), sessions.keys().cloned().collect::<Vec<_>>().join(", ")))
                }
                error => error,
            })?;
            let active = accounts.registry()?.active;
            println!("[account] Removed '{id}' (active: {active})");
        }
        _ => unreachable!("only account mutation commands reach this module"),
    }
    Ok(())
}
