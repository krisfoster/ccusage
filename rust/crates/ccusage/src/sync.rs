//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

pub(crate) mod auth;
pub(crate) mod project;

use crate::{
    Result,
    cli::{SyncArgs, SyncCommand, SyncSetupArgs},
    cli_error,
    gcs::projects::ProjectCatalog,
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

/// Setup's first two steps: find a credential, then the project a bucket would be
/// billed to. Creating the bucket lands next, so the command stops once both are
/// known rather than pretending to have finished.
fn setup_auth(setup: &SyncSetupArgs) -> Result<()> {
    let credentials = auth::resolve(setup.auth, setup.non_interactive, &mut auth::TerminalPrompt)
        .map_err(|error| cli_error(error.to_string()))?;
    println!("Authenticated via {}.", credentials.source());

    let catalog = ProjectCatalog::new(Box::new(credentials));
    let selected = project::select(
        setup.project.as_deref(),
        &std::env::vars().collect(),
        setup.non_interactive,
        &mut || catalog.list(),
        &mut project::TerminalPicker,
    )
    .map_err(|error| cli_error(error.to_string()))?;
    println!("Using project {}.", selected.id);

    Err(cli_error(
        "`ccusage sync setup` cannot create a bucket yet; it arrives in a later release."
            .to_string(),
    ))
}
