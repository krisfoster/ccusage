//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

pub(crate) mod auth;
pub(crate) mod bucket;
pub(crate) mod project;

use std::sync::Arc;

use ccusage_config::{
    ConfigContext, SyncWriteback, config::explicit_config_path, persist_sync, sync_writeback_path,
};

use crate::{
    Result,
    cli::{SyncArgs, SyncCommand, SyncSetupArgs},
    cli_error,
    gcs::{
        JsonApi, RetryPolicy,
        bucket::{BucketAdmin, BucketSpec},
        projects::ProjectCatalog,
    },
};

/// Multi-region US unless the user says otherwise: the data is tiny, and a
/// multi-region survives a single region going away without a migration.
const DEFAULT_LOCATION: &str = "US";
const STORAGE_ENDPOINT: &str = "https://storage.googleapis.com";

pub(crate) fn run(args: SyncArgs) -> Result<()> {
    match args.command {
        SyncCommand::Setup(setup) => setup_sync(&setup),
        other => Err(cli_error(format!(
            "`ccusage sync {}` is not available yet; it arrives in a later release.",
            other.name()
        ))),
    }
}

/// Authenticate, choose a project, create the bucket, remember all three.
///
/// Every step is idempotent, so a setup interrupted halfway — or re-run after a
/// permissions fix — resumes rather than duplicating anything.
fn setup_sync(setup: &SyncSetupArgs) -> Result<()> {
    let credentials = Arc::new(
        auth::resolve(setup.auth, setup.non_interactive, &mut auth::TerminalPrompt)
            .map_err(|error| cli_error(error.to_string()))?,
    );
    println!("Authenticated via {}.", credentials.source());

    let catalog = ProjectCatalog::new(Box::new(Arc::clone(&credentials)));
    let project = project::select(
        setup.project.as_deref(),
        &std::env::vars().collect(),
        setup.non_interactive,
        &mut || catalog.list(),
        &mut project::TerminalPicker,
    )
    .map_err(|error| cli_error(error.to_string()))?;
    println!("Using project {}.", project.id);

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let config = ConfigContext::from_args(&args);
    let configured_bucket = config
        .sync()
        .and_then(|sync| sync.bucket.as_deref())
        .map(str::to_string);
    let planned = bucket::plan(
        setup.bucket.as_deref(),
        configured_bucket.as_deref(),
        setup.recreate,
        &mut bucket::os_entropy,
    )
    .map_err(|error| cli_error(error.to_string()))?;

    let location = setup.location.as_deref().unwrap_or(DEFAULT_LOCATION);
    let admin = BucketAdmin::new(
        JsonApi::new(
            STORAGE_ENDPOINT,
            Box::new(Arc::clone(&credentials)),
            RetryPolicy::default(),
        ),
        &planned.name,
    );
    let info = bucket::ensure_private(&admin, &BucketSpec::new(&project.id, location))
        .map_err(|error| cli_error(error.to_string()))?;
    println!("Bucket gs://{} ready in {}.", info.name, info.location);

    let Some(path) = sync_writeback_path(explicit_config_path(&args).as_deref()) else {
        return Err(cli_error(
            "no writable ccusage.json location could be determined; pass --config with a path"
                .to_string(),
        ));
    };
    persist_sync(
        &path,
        &SyncWriteback {
            provider: Some("gcs".to_string()),
            project_id: Some(project.id.clone()),
            bucket: Some(info.name.clone()),
            location: Some(info.location.clone()),
            prefix: setup.prefix.clone(),
            ..SyncWriteback::default()
        },
    )
    .map_err(cli_error)?;
    println!("Saved sync settings to {}.", path.display());

    Ok(())
}
