//! Keep the root selector before clap propagates global values from child commands.
use crate::{AccountCommand, Cli, account_mutation_cli};
use clap::Command;
use colabcli_rust::{Result, accounts::Accounts, context::Environment};
use std::ffi::OsString;

/// Inspect only root options, using clap's declarations to skip option values.
/// No environment/configuration is read here, and `--` ends inspection.
pub fn root_selector(command: &Command, arguments: &[OsString]) -> Option<String> {
    let mut selector = None;
    let mut index = 1;
    while let Some(token) = arguments.get(index).and_then(|s| s.to_str()) {
        if token == "--" || !token.starts_with('-') || token == "-" {
            break;
        }
        let (argument, attached) = if let Some(long) = token.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            (
                command
                    .get_arguments()
                    .find(|arg| arg.get_long() == Some(name)),
                value,
            )
        } else {
            let short = token.chars().nth(1)?;
            let rest = &token[1 + short.len_utf8()..];
            (
                command
                    .get_arguments()
                    .find(|arg| arg.get_short() == Some(short)),
                (!rest.is_empty()).then_some(rest),
            )
        };
        if let Some(argument) = argument.filter(|arg| arg.get_action().takes_values()) {
            let value = attached.or_else(|| arguments.get(index + 1).and_then(|s| s.to_str()));
            if argument.get_id() == "account" {
                selector = value.map(str::to_owned);
            }
            if attached.is_none() {
                index += 1;
            }
        }
        index += 1;
    }
    selector
}

pub fn preflight(
    cli: &Cli,
    accounts: &Accounts,
    env: &Environment,
    command: &AccountCommand,
) -> Result<()> {
    // Authentication orchestration and repair retain their own rules.
    if !matches!(
        command,
        AccountCommand::Status { .. }
            | AccountCommand::List { .. }
            | AccountCommand::Current { .. }
            | AccountCommand::Use { .. }
            | AccountCommand::SetDrive { .. }
            | AccountCommand::Rename { .. }
            | AccountCommand::Remove { .. }
    ) {
        return Ok(());
    }
    let explicit = if matches!(command, AccountCommand::SetDrive { .. }) {
        cli.root_account.as_deref()
    } else {
        cli.account.as_deref()
    };
    validate(accounts, env, explicit)
}

pub fn validate(accounts: &Accounts, env: &Environment, explicit: Option<&str>) -> Result<()> {
    if let Some(selector) = explicit.filter(|s| !s.is_empty()).or_else(|| {
        env.account
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }) {
        account_mutation_cli::select_with_prefix(&accounts.registry()?, selector, "colab")?;
    }
    Ok(())
}
