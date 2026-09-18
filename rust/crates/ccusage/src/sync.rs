//! Dispatch for `ccusage sync`. The grammar is complete ahead of the behavior, so
//! every subcommand parses and the ones that are still being built say so instead of
//! looking like a typo.

pub(crate) mod auth;
pub(crate) mod bootstrap;
pub(crate) mod bucket;
pub(crate) mod dashboard;
pub(crate) mod doctor;
pub(crate) mod failures;
pub(crate) mod lock;
pub(crate) mod machine;
pub(crate) mod maintenance;
/// The merge matrices from `specs/sync-merge-test-plan.md`.
#[cfg(test)]
mod merge_matrix;
pub(crate) mod project;
pub(crate) mod rollups;
pub(crate) mod run;
pub(crate) mod sources;
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
use ccusage_sync::{
    fingerprint::{FingerprintSources, observe_fingerprint},
    identity::{IdentityInputs, os_entropy, resolve_identity},
};

use crate::{
    Result,
    cli::{SyncArgs, SyncAuthMode, SyncCommand, SyncSetupArgs},
    cli_error,
    gcs::{
        GcsStore, JsonApi, RetryPolicy,
        bucket::{BucketAdmin, BucketSpec, PublicAccessPrevention},
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
        SyncCommand::Run(run_args) => run::execute(&config, &run_args),
        SyncCommand::Setup(setup) => setup_sync(&setup, &config, args.config),
        SyncCommand::Status => show_status(&config, args.json),
        SyncCommand::Doctor => run_doctor(&config, args.json),
        SyncCommand::Repair(repair) => maintenance::execute_repair(&config, &repair),
        SyncCommand::Forget(forget) => maintenance::execute_forget(&config, &forget),
        SyncCommand::MergeMachine(merge) => maintenance::execute_merge(&config, &merge),
        SyncCommand::Dashboard(dashboard_args) => dashboard::execute(&config, &dashboard_args),
    }
}

/// A configured bucket, opened, with the identity this machine syncs as.
pub(crate) struct Session {
    pub store: GcsStore,
    pub keys: KeySpace,
    pub user_id: String,
    pub machine_id: String,
}

/// Resolves config and credentials into something that can talk to the bucket.
///
/// Every command past setup needs the same four values, and each one deciding
/// for itself how to fall back on a bad prefix is how two commands end up
/// reading different keys in the same bucket.
pub(crate) fn connect(config: &ConfigContext) -> Result<Session> {
    let status = status::Status::from_config(config.sync());
    let (Some(bucket), Some(machine_id), Some(user_id)) = (
        status.bucket.clone(),
        status.machine_id.clone(),
        status.user_id.clone(),
    ) else {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    };
    let keys = KeySpace::new(status.prefix.as_str().trim_end_matches('/'))
        .or_else(|_| KeySpace::new(DEFAULT_PREFIX))
        .map_err(|error| cli_error(error.to_string()))?;
    let credentials = Arc::new(
        auth::resolve(config_auth_mode(config), true, &mut auth::TerminalPrompt)
            .map_err(|error| cli_error(error.to_string()))?,
    );
    Ok(Session {
        store: GcsStore::new(&bucket, Box::new(credentials)),
        keys,
        user_id,
        machine_id,
    })
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
        setup.non_interactive,
        &mut bucket::os_entropy,
        &mut bucket::TerminalNamePrompt,
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
    // The data bucket never serves the dashboard — that lives in a separate
    // bucket — so it can refuse to become public at all.
    let mut spec = BucketSpec::new(&project.id, location);
    spec.public_access_prevention = PublicAccessPrevention::Enforced;
    let info =
        bucket::ensure_private(&admin, &spec).map_err(|error| cli_error(error.to_string()))?;
    println!("Bucket gs://{} ready in {}.", info.name, info.location);

    let prefix = setup
        .prefix
        .clone()
        .unwrap_or_else(|| DEFAULT_PREFIX.to_string());
    let keys = KeySpace::new(&prefix).map_err(|error| cli_error(error.to_string()))?;
    let store = GcsStore::new(&info.name, Box::new(Arc::clone(&credentials)));
    let sync_config = config.sync();
    let (salt, _) = bootstrap::ensure_salt(
        &store,
        &keys,
        sync_config.and_then(|sync| sync.salt.as_deref()),
    )
    .map_err(cli_error)?;

    // The bucket's user ID wins over a freshly minted one, so pointing a second
    // machine at an existing bucket is the whole of joining it.
    let manifest_user_id = bootstrap::manifest_user_id(&store, &keys).map_err(cli_error)?;
    let identity = resolve_identity(
        &IdentityInputs {
            configured_user_id: sync_config.and_then(|sync| sync.user_id.as_deref()),
            manifest_user_id: manifest_user_id.as_deref(),
            configured_machine_id: sync_config.and_then(|sync| sync.machine_id.as_deref()),
            ..IdentityInputs::default()
        },
        &mut os_entropy,
    )
    .map_err(|error| cli_error(error.to_string()))?;
    bootstrap::ensure_manifest(&store, &keys, identity.user.as_str()).map_err(cli_error)?;
    let machine_id = identity.machine.to_string();
    bootstrap::register_machine(&store, &keys, identity.user.as_str(), &machine_id)
        .map_err(cli_error)?;
    let fingerprint = observe_fingerprint(&FingerprintSources::platform_default());
    let label = sync_config.and_then(|sync| sync.machine_label.clone());
    machine::update_machine(
        &store,
        &keys,
        identity.user.as_str(),
        &machine_id,
        |record| {
            record.label.clone_from(&label);
            record.os = Some(std::env::consts::OS.to_string());
            record.fingerprint.clone_from(&fingerprint);
        },
    )
    .map_err(cli_error)?;
    for warning in &identity.warnings {
        println!("{warning}");
    }
    println!("Syncing as machine {machine_id}.");

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
            prefix: Some(prefix),
            machine_id: Some(machine_id),
            user_id: Some(identity.user.to_string()),
            salt: Some(salt.expose().to_string()),
        },
    )
    .map_err(cli_error)?;
    println!("Saved sync settings to {}.", path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::fs_fixture;

    use super::*;

    fn config_with(sync_block: &str) -> (ccusage_test_support::Fixture, ConfigContext) {
        let fixture = fs_fixture!({
            "ccusage.json": format!("{{\"sync\":{sync_block}}}"),
        });
        let args = vec![
            "sync".to_string(),
            "status".to_string(),
            "--config".to_string(),
            fixture.path("ccusage.json").to_string_lossy().into_owned(),
        ];
        let config = ConfigContext::from_args(&args);
        (fixture, config)
    }

    #[test]
    fn the_configured_auth_kind_decides_the_mode_and_absence_means_auto() {
        let (_fixture, adc) = config_with(r#"{"auth":{"kind":"adc"}}"#);
        let (_fixture, hmac) = config_with(r#"{"auth":{"kind":"hmac"}}"#);
        let (_fixture, auto) = config_with(r#"{"auth":{"kind":"auto"}}"#);
        let (_fixture, unset) = config_with("{}");

        assert_eq!(config_auth_mode(&adc), SyncAuthMode::Adc);
        assert_eq!(config_auth_mode(&hmac), SyncAuthMode::Hmac);
        assert_eq!(config_auth_mode(&auto), SyncAuthMode::Auto);
        assert_eq!(config_auth_mode(&unset), SyncAuthMode::Auto);
    }

    #[test]
    fn the_clock_reads_forward_of_the_epoch() {
        assert!(now_ms() > 1_700_000_000_000);
    }

    /// Status is offline by design, so both renderings have to work from the
    /// config file alone.
    #[test]
    fn status_renders_from_the_config_file_in_both_shapes() {
        let (_fixture, config) = config_with(r#"{"bucket":"ccusage-abc","prefix":"ccusage/v1"}"#);

        show_status(&config, false).expect("the text rendering");
        show_status(&config, true).expect("the JSON rendering");
    }

    /// Doctor talks to the bucket, so with no bucket configured it has to say
    /// that rather than fail later on a credential it never needed.
    #[test]
    fn doctor_without_a_bucket_points_at_setup() {
        let (_fixture, config) = config_with("{}");

        let error = run_doctor(&config, false).expect_err("refused");

        assert!(format!("{error}").contains("ccusage sync setup"), "{error}");
    }
}
