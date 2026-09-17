//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

pub(crate) mod auth;
pub(crate) mod bucket;
pub(crate) mod doctor;
pub(crate) mod project;
pub(crate) mod status;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ccusage_config::{
    ConfigContext, SyncWriteback, config_schema::ConfigSyncAuthKind, persist_sync,
    sync_writeback_path,
};
use ccusage_objectstore::KeySpace;

use crate::{
    Result,
    cli::{SyncArgs, SyncAuthMode, SyncCommand, SyncSetupArgs},
    cli_error,
    gcs::{
        GcsStore, JsonApi, RetryPolicy,
        bucket::{BucketAdmin, BucketSpec},
        projects::ProjectCatalog,
    },
};

/// Multi-region US unless the user says otherwise: the data is tiny, and a
/// multi-region survives a single region going away without a migration.
const DEFAULT_LOCATION: &str = "US";
/// The key layout every object lives under, versioned so a future layout can be
/// written alongside this one instead of migrating in place.
pub(crate) const DEFAULT_PREFIX: &str = "ccusage/v1";
const STORAGE_ENDPOINT: &str = "https://storage.googleapis.com";

pub(crate) fn run(args: SyncArgs) -> Result<()> {
    let config = ConfigContext::from_args(&std::env::args().skip(1).collect::<Vec<_>>());
    match args.command {
        SyncCommand::Setup(setup) => setup_sync(&setup, &config, args.config),
        SyncCommand::Status => show_status(&config, args.json),
        SyncCommand::Doctor => run_doctor(&config, args.json),
        other => Err(cli_error(format!(
            "`ccusage sync {}` is not available yet; it arrives in a later release.",
            other.name()
        ))),
    }
}

fn show_status(config: &ConfigContext, json: bool) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status.to_json()).map_err(|error| cli_error(format!(
                "could not render sync status as JSON: {error}"
            )))?
        );
    } else {
        println!("{}", status.to_text());
    }
    Ok(())
}

/// The checks that need the bucket. Unlike setup, doctor never prompts: it is
/// run to find out what is wrong, including "there is no usable credential".
fn run_doctor(config: &ConfigContext, json: bool) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    let Some(bucket) = status.bucket.clone() else {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    };
    let credentials = Arc::new(
        auth::resolve(config_auth_mode(config), true, &mut auth::TerminalPrompt)
            .map_err(|error| cli_error(error.to_string()))?,
    );
    let keys = KeySpace::new(&status.prefix).map_err(|error| cli_error(error.to_string()))?;

    let admin = BucketAdmin::new(
        JsonApi::new(
            STORAGE_ENDPOINT,
            Box::new(Arc::clone(&credentials)),
            RetryPolicy::default(),
        ),
        &bucket,
    );
    let mut checks = Vec::new();
    match (admin.get(), admin.get_iam_policy()) {
        (Ok(Some(info)), Ok(policy)) => {
            checks.push(doctor::privacy_check(
                info.uniform_bucket_level_access,
                &policy,
            ));
        }
        (Ok(None), _) => {
            return Err(cli_error(format!(
                "bucket '{bucket}' does not exist. Re-run 'ccusage sync setup'."
            )));
        }
        (Err(error), _) | (_, Err(error)) => {
            return Err(cli_error(error.to_string()));
        }
    }

    let store = GcsStore::new(&bucket, Box::new(Arc::clone(&credentials)));
    let machine_id = status.machine_id.as_deref().unwrap_or("setup-pending");
    checks.extend(doctor::run_checks(&store, &keys, machine_id, now_ms()));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&doctor::to_json(&checks)).map_err(|error| cli_error(
                format!("could not render the doctor report as JSON: {error}")
            ))?
        );
    } else {
        println!("{}", doctor::to_text(&checks));
    }
    if checks
        .iter()
        .any(|check| check.outcome == doctor::Outcome::Fail)
    {
        return Err(cli_error(
            "sync doctor found problems that would corrupt or expose your usage data".to_string(),
        ));
    }
    Ok(())
}

fn config_auth_mode(config: &ConfigContext) -> SyncAuthMode {
    match config
        .sync()
        .and_then(|sync| sync.auth.as_ref())
        .and_then(|auth| auth.kind.as_ref())
    {
        Some(ConfigSyncAuthKind::Adc) => SyncAuthMode::Adc,
        Some(ConfigSyncAuthKind::Hmac) => SyncAuthMode::Hmac,
        Some(ConfigSyncAuthKind::Auto) | None => SyncAuthMode::Auto,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Authenticate, choose a project, create the bucket, remember all three.
///
/// Every step is idempotent, so a setup interrupted halfway — or re-run after a
/// permissions fix — resumes rather than duplicating anything.
fn setup_sync(
    setup: &SyncSetupArgs,
    config: &ConfigContext,
    config_path: Option<PathBuf>,
) -> Result<()> {
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

    let Some(path) = sync_writeback_path(config_path.as_deref()) else {
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
            prefix: Some(
                setup
                    .prefix
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
            ),
            ..SyncWriteback::default()
        },
    )
    .map_err(cli_error)?;
    println!("Saved sync settings to {}.", path.display());

    Ok(())
}
