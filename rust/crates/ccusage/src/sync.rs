//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

pub(crate) mod auth;

use crate::{
    Result,
    cli::{SyncArgs, SyncCommand, SyncSetupArgs},
    cli_error,
};

pub(crate) fn run(args: SyncArgs) -> Result<()> {
    match args.command {
        SyncCommand::Setup(setup) => setup_auth(&setup),
        other => Err(cli_error(format!(
            "`ccusage sync {}` is not available yet; it arrives in a later release.",
            other.name()
        ))),
    }
}

/// Setup's first step: prove there is a credential to create a bucket with. Bucket
/// resolution itself lands next, so the command stops once the credential is known.
fn setup_auth(setup: &SyncSetupArgs) -> Result<()> {
    let credentials = auth::resolve(setup.auth, setup.non_interactive, &mut auth::TerminalPrompt)
        .map_err(|error| cli_error(error.to_string()))?;
    println!("Authenticated via {}.", credentials.source());
    Err(cli_error(
        "`ccusage sync setup` cannot create a bucket yet; it arrives in a later release."
            .to_string(),
    ))
}
