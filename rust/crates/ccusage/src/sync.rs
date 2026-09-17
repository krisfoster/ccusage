//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

use crate::{Result, cli::SyncArgs, cli_error};

pub(crate) fn run(args: SyncArgs) -> Result<()> {
    Err(cli_error(format!(
        "`ccusage sync {}` is not available yet; it arrives in a later release.",
        args.command.name()
    )))
}
