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
/// The seeded property layer from `specs/sync-merge-test-plan.md` §4.5.
#[cfg(test)]
mod merge_property;
pub(crate) mod project;
pub(crate) mod remove;
pub(crate) mod rollups;
pub(crate) mod run;
pub(crate) mod serve;
pub(crate) mod share;
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
use ccusage_objectstore::{KeySpace, ObjectStore};
use ccusage_sync::{
    fingerprint::{FingerprintSources, observe_fingerprint},
    identity::{IdentityInputs, IdentityOrigin, ResolvedIdentity, os_entropy, resolve_identity},
};

use crate::{
    Result,
    cli::{SyncArgs, SyncAuthMode, SyncCommand, SyncProvider, SyncServeArgs, SyncSetupArgs},
    cli_error,
    fs_store::FsStore,
    gcs::{
        GcsStore, JsonApi, RetryPolicy,
        bucket::{BucketAdmin, BucketSpec, PublicAccessPrevention},
        projects::ProjectCatalog,
    },
    http_store::{HttpStore, TOKEN_ENV},
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
        SyncCommand::Remove(remove_args) => {
            remove::execute(&config, &remove_args, args.config.as_deref())
        }
        SyncCommand::Share(share_args) => share::execute(&config, &share_args),
        SyncCommand::Dashboard(dashboard_args) => dashboard::execute(&config, &dashboard_args),
        SyncCommand::Serve(serve_args) => serve_hub(&serve_args, &config),
    }
}

/// A configured store, opened, with the identity this machine syncs as.
pub(crate) struct Session {
    pub store: Box<dyn ObjectStore>,
    pub keys: KeySpace,
    pub user_id: String,
    pub machine_id: String,
}

/// Resolves config and credentials into something that can talk to the store.
///
/// Every command past setup needs the same four values, and each one deciding
/// for itself how to fall back on a bad prefix is how two commands end up
/// reading different keys in the same store.
pub(crate) fn connect(config: &ConfigContext) -> Result<Session> {
    let status = status::Status::from_config(config.sync());
    let (true, Some(machine_id), Some(user_id)) = (
        status.is_configured(),
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
    Ok(Session {
        store: open_store(config, &status)?,
        keys,
        user_id,
        machine_id,
    })
}

/// Opens whichever store the settings describe. Only the bucket needs credentials from the
/// environment; a directory is reached as the current user, and a hub through the bearer token.
pub(crate) fn open_store(
    config: &ConfigContext,
    status: &status::Status,
) -> Result<Box<dyn ObjectStore>> {
    match status.kind() {
        status::Kind::Gcs => {
            let bucket = status.bucket.clone().ok_or_else(not_configured)?;
            let credentials = Arc::new(
                auth::resolve(config_auth_mode(config), true, &mut auth::TerminalPrompt)
                    .map_err(|error| cli_error(error.to_string()))?,
            );
            Ok(Box::new(GcsStore::new(&bucket, Box::new(credentials))))
        }
        status::Kind::Fs => {
            let dir = status.dir.as_deref().ok_or_else(not_configured)?;
            Ok(Box::new(FsStore::new(expand_home(dir))))
        }
        status::Kind::Http => {
            let url = status.url.as_deref().ok_or_else(not_configured)?;
            Ok(Box::new(HttpStore::new(url, &hub_token()?)))
        }
    }
}

fn not_configured() -> crate::CliError {
    cli_error("sync is not configured. Run 'ccusage sync setup' first.".to_string())
}

/// The hub's bearer token. It is read from the environment rather than the config file, which
/// refuses secrets: inside a sandbox the variable holds a placeholder that the sandbox proxy
/// replaces on the wire.
fn hub_token() -> Result<String> {
    match std::env::var(TOKEN_ENV) {
        Ok(token) if !token.trim().is_empty() => Ok(token.trim().to_string()),
        _ => Err(cli_error(format!(
            "this machine syncs through a hub, which needs a bearer token. Set {TOKEN_ENV} to the \
             contents of the hub's token file (see 'ccusage sync serve --help')."
        ))),
    }
}

/// Config files cannot rely on a shell to expand `~`.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    let home = || ccusage_core::home::home_dir();
    match path {
        "~" => home().unwrap_or_else(|| PathBuf::from(path)),
        _ => match path.strip_prefix("~/") {
            Some(rest) => home().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
            None => PathBuf::from(path),
        },
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

/// The checks that need the store. Unlike setup, doctor never prompts: it is
/// run to find out what is wrong, including "there is no usable credential".
fn run_doctor(config: &ConfigContext, json: bool) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    if !status.is_configured() {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    }
    let keys = KeySpace::new(&status.prefix).map_err(|error| cli_error(error.to_string()))?;

    let mut checks = Vec::new();
    let store: Box<dyn ObjectStore> = match status.kind() {
        status::Kind::Gcs => {
            let bucket = status.bucket.clone().ok_or_else(not_configured)?;
            let credentials = Arc::new(
                auth::resolve(config_auth_mode(config), true, &mut auth::TerminalPrompt)
                    .map_err(|error| cli_error(error.to_string()))?,
            );
            let admin = BucketAdmin::new(
                JsonApi::new(
                    STORAGE_ENDPOINT,
                    Box::new(Arc::clone(&credentials)),
                    RetryPolicy::default(),
                ),
                &bucket,
            );
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
            Box::new(GcsStore::new(&bucket, Box::new(Arc::clone(&credentials))))
        }
        status::Kind::Fs => open_store(config, &status)?,
        status::Kind::Http => {
            let url = status.url.as_deref().ok_or_else(not_configured)?;
            let hub = HttpStore::new(url, &hub_token()?);
            let reachable = match hub.ping() {
                Ok(served) if served == status.prefix => doctor::Check::new(
                    "hub",
                    doctor::Outcome::Pass,
                    format!("the hub at {url} is reachable and serves '{served}'"),
                ),
                Ok(served) => doctor::Check::new(
                    "hub",
                    doctor::Outcome::Fail,
                    format!(
                        "the hub serves '{served}' but this machine's prefix is '{}'. Re-run \
                         'ccusage sync setup --prefix {served}'",
                        status.prefix
                    ),
                ),
                Err(error) => doctor::Check::new("hub", doctor::Outcome::Fail, error.to_string()),
            };
            let unusable = reachable.outcome == doctor::Outcome::Fail;
            checks.push(reachable);
            if unusable {
                return finish_doctor(&checks, json);
            }
            Box::new(hub)
        }
    };

    let machine_id = status.machine_id.as_deref().unwrap_or("setup-pending");
    checks.extend(doctor::run_checks(&*store, &keys, machine_id, now_ms()));
    finish_doctor(&checks, json)
}

fn finish_doctor(checks: &[doctor::Check], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&doctor::to_json(checks)).map_err(|error| cli_error(
                format!("could not render the doctor report as JSON: {error}")
            ))?
        );
    } else {
        println!("{}", doctor::to_text(checks));
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

/// Where sync stores usage and what to remember about it, so the tail of setup is the same for
/// every provider.
struct Target {
    provider: &'static str,
    project_id: Option<String>,
    bucket: Option<String>,
    location: Option<String>,
    dir: Option<String>,
    url: Option<String>,
}

fn setup_sync(
    setup: &SyncSetupArgs,
    config: &ConfigContext,
    config_path: Option<PathBuf>,
) -> Result<()> {
    match setup.provider {
        SyncProvider::Gcs => setup_gcs(setup, config, config_path),
        SyncProvider::Fs | SyncProvider::Http => setup_local(setup, config, config_path),
    }
}

/// Authenticate, choose a project, create the bucket, remember all three.
///
/// Every step is idempotent, so a setup interrupted halfway — or re-run after a
/// permissions fix — resumes rather than duplicating anything.
fn setup_gcs(
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
    let store = GcsStore::new(&info.name, Box::new(Arc::clone(&credentials)));
    finish_setup(
        &store,
        prefix,
        &Target {
            provider: "gcs",
            project_id: Some(project.id.clone()),
            bucket: Some(info.name.clone()),
            location: Some(info.location.clone()),
            dir: None,
            url: None,
        },
        setup,
        config,
        config_path,
    )?;

    println!(
        "Publishing a dashboard other people can open needs a signing key, which \
         `ccusage sync share` creates when you want one."
    );
    Ok(())
}

/// Point this machine at a directory, or at a hub that owns one. Neither needs credentials from
/// the environment beyond the hub's bearer token, so nothing here can prompt.
fn setup_local(
    setup: &SyncSetupArgs,
    config: &ConfigContext,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let prefix = setup.prefix.clone();
    match setup.provider {
        SyncProvider::Fs => {
            let dir = setup.dir.clone().ok_or_else(|| {
                cli_error(
                    "the fs provider needs a directory. Pass --dir <path>, or set sync.dir."
                        .to_string(),
                )
            })?;
            let path = expand_home(&dir);
            std::fs::create_dir_all(&path).map_err(|error| {
                cli_error(format!("could not create {}: {error}", path.display()))
            })?;
            println!("Using directory {}.", path.display());
            finish_setup(
                &FsStore::new(path),
                prefix.unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
                &Target {
                    provider: "fs",
                    project_id: None,
                    bucket: None,
                    location: None,
                    dir: Some(dir),
                    url: None,
                },
                setup,
                config,
                config_path,
            )
        }
        _ => {
            let url = setup.url.clone().ok_or_else(|| {
                cli_error(
                    "the http provider needs a hub address. Pass --url <address>, or set sync.url."
                        .to_string(),
                )
            })?;
            let hub = HttpStore::new(&url, &hub_token()?);
            let served = hub.ping().map_err(|error| cli_error(error.to_string()))?;
            println!("Reached the hub at {url}.");
            // A prefix the user did not choose is the hub's to decide. One they did choose has to
            // match, or every key this machine writes would land outside what the hub serves.
            let prefix = match prefix {
                Some(chosen) if chosen != served => {
                    return Err(cli_error(format!(
                        "the hub serves '{served}' but setup was asked for '{chosen}'. Drop \
                         --prefix, or start the hub with --prefix {chosen}."
                    )));
                }
                Some(chosen) => chosen,
                None => served,
            };
            finish_setup(
                &hub,
                prefix,
                &Target {
                    provider: "http",
                    project_id: None,
                    bucket: None,
                    location: None,
                    dir: None,
                    url: Some(url),
                },
                setup,
                config,
                config_path,
            )
        }
    }
}

/// Everything after the store exists: the salt and user ID the store hands to every machine that
/// joins it, this machine's identity, and the settings written back to the config file.
fn finish_setup(
    store: &dyn ObjectStore,
    prefix: String,
    target: &Target,
    setup: &SyncSetupArgs,
    config: &ConfigContext,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let keys = KeySpace::new(&prefix).map_err(|error| cli_error(error.to_string()))?;
    let sync_config = config.sync();
    let (salt, _) = bootstrap::ensure_salt(
        store,
        &keys,
        sync_config.and_then(|sync| sync.salt.as_deref()),
    )
    .map_err(cli_error)?;

    // The store's user ID wins over a freshly minted one, so pointing a second
    // machine at an existing store is the whole of joining it.
    let manifest_user_id = bootstrap::manifest_user_id(store, &keys).map_err(cli_error)?;
    let configured_machine_id = setup
        .machine_id
        .as_deref()
        .or_else(|| sync_config.and_then(|sync| sync.machine_id.as_deref()));
    let identity = resolve_identity(
        &IdentityInputs {
            configured_user_id: sync_config.and_then(|sync| sync.user_id.as_deref()),
            manifest_user_id: manifest_user_id.as_deref(),
            configured_machine_id,
            ..IdentityInputs::default()
        },
        &mut os_entropy,
    )
    .map_err(|error| cli_error(error.to_string()))?;
    bootstrap::ensure_manifest(store, &keys, identity.user.as_str()).map_err(cli_error)?;
    let identity = adopt_winning_user(store, &keys, identity)?;
    let machine_id = identity.machine.to_string();
    bootstrap::register_machine(store, &keys, identity.user.as_str(), &machine_id)
        .map_err(cli_error)?;
    let fingerprint = observe_fingerprint(&FingerprintSources::platform_default());
    let label = setup
        .machine_label
        .clone()
        .or_else(|| sync_config.and_then(|sync| sync.machine_label.clone()));
    machine::update_machine(
        store,
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
            provider: Some(target.provider.to_string()),
            project_id: target.project_id.clone(),
            bucket: target.bucket.clone(),
            dir: target.dir.clone(),
            url: target.url.clone(),
            location: target.location.clone(),
            prefix: Some(prefix),
            machine_id: Some(machine_id),
            machine_label: label,
            user_id: Some(identity.user.to_string()),
            salt: Some(salt.expose().to_string()),
        },
    )
    .map_err(cli_error)?;
    println!("Saved sync settings to {}.", path.display());
    Ok(())
}

/// Machines that set up at the same moment all find no manifest, so each mints its own user ID,
/// and only one of them can create the manifest. The rest must take the user ID it recorded: a
/// machine that kept its own would file every shard under a user nobody else reads, and its usage
/// would never reach the aggregate. A user ID the person pinned in their config is theirs to
/// keep, and one already adopted from the manifest needs no second look.
fn adopt_winning_user(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    identity: ResolvedIdentity,
) -> Result<ResolvedIdentity> {
    if identity.user_origin != IdentityOrigin::Minted {
        return Ok(identity);
    }
    let Some(winner) = bootstrap::manifest_user_id(store, keys).map_err(cli_error)? else {
        return Ok(identity);
    };
    if winner == identity.user.as_str() {
        return Ok(identity);
    }
    let adopted = resolve_identity(
        &IdentityInputs {
            manifest_user_id: Some(&winner),
            configured_machine_id: Some(identity.machine.as_str()),
            ..IdentityInputs::default()
        },
        &mut os_entropy,
    )
    .map_err(|error| cli_error(error.to_string()))?;
    Ok(ResolvedIdentity {
        warnings: identity.warnings,
        machine_origin: identity.machine_origin,
        ..adopted
    })
}

/// Serve a directory to sandboxes. The directory comes from `--dir`, or from the `sync.dir` the
/// host's own config already names, so one setting serves both the hub and the host's dashboard.
fn serve_hub(args: &SyncServeArgs, config: &ConfigContext) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    let dir = args.dir.clone().or(status.dir.clone()).ok_or_else(|| {
        cli_error("there is no directory to serve. Pass --dir <path>, or set sync.dir.".to_string())
    })?;
    let listen = args.listen.as_deref().unwrap_or(DEFAULT_HUB_ADDRESS);
    let listen = listen.parse().map_err(|_| {
        cli_error(format!(
            "'{listen}' is not an address and port, such as {DEFAULT_HUB_ADDRESS}."
        ))
    })?;
    let token_file = match &args.token_file {
        Some(path) => expand_home(path),
        None => default_token_file().ok_or_else(|| {
            cli_error(
                "there is no home directory to keep the token in. Pass --token-file <path>."
                    .to_string(),
            )
        })?,
    };
    serve::execute(&serve::ServeOptions {
        dir: expand_home(&dir),
        listen,
        token_file,
        prefix: args.prefix.clone().unwrap_or(status.prefix),
        allow_delete: args.allow_delete,
    })
}

/// Loopback only by default. The sandbox proxy reaches a service bound here through
/// `host.docker.internal`, so nothing on the network has to be able to.
const DEFAULT_HUB_ADDRESS: &str = "127.0.0.1:8777";

fn default_token_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| ccusage_core::home::home_dir().map(|home| home.join(".config")))?;
    Some(base.join("ccusage").join("hub-token"))
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

    fn directory_setup(dir: &std::path::Path, machine: &str) -> SyncSetupArgs {
        SyncSetupArgs {
            provider: SyncProvider::Fs,
            dir: Some(dir.to_string_lossy().into_owned()),
            machine_id: Some(machine.to_string()),
            machine_label: Some(format!("{machine} label")),
            non_interactive: true,
            ..SyncSetupArgs::default()
        }
    }

    fn read_config(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("config file"))
            .expect("config json")
    }

    #[test]
    fn setting_up_a_directory_registers_the_machine_and_saves_what_a_later_run_needs() {
        let store_dir = assert_fs::TempDir::new().expect("temp dir");
        let (fixture, config) = config_with("{}");
        let path = fixture.path("ccusage.json");

        setup_sync(
            &directory_setup(store_dir.path(), "sandbox-1"),
            &config,
            Some(path.clone()),
        )
        .expect("setup");

        let saved = read_config(&path)["sync"].clone();
        assert_eq!(saved["provider"], "fs");
        assert_eq!(saved["machineId"], "sandbox-1");
        assert_eq!(saved["machineLabel"], "sandbox-1 label");
        assert!(saved["userId"].as_str().is_some_and(|id| !id.is_empty()));
        assert!(saved["salt"].as_str().is_some_and(|salt| !salt.is_empty()));

        // A later command reads that file and reaches the same store as the same machine.
        let reread = ConfigContext::from_args(&[
            "sync".to_string(),
            "run".to_string(),
            "--config".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
        let session = connect(&reread).expect("connect");
        assert_eq!(session.machine_id, "sandbox-1");
        assert_eq!(
            bootstrap::manifest_machines(&session.store, &session.keys).expect("roster"),
            vec!["sandbox-1".to_string()]
        );
    }

    #[test]
    fn a_second_machine_joins_the_same_directory_with_the_first_machines_user_and_salt() {
        let store_dir = assert_fs::TempDir::new().expect("temp dir");
        let (first_fixture, first_config) = config_with("{}");
        let (second_fixture, second_config) = config_with("{}");
        let first_path = first_fixture.path("ccusage.json");
        let second_path = second_fixture.path("ccusage.json");

        setup_sync(
            &directory_setup(store_dir.path(), "sandbox-1"),
            &first_config,
            Some(first_path.clone()),
        )
        .expect("first setup");
        setup_sync(
            &directory_setup(store_dir.path(), "sandbox-2"),
            &second_config,
            Some(second_path.clone()),
        )
        .expect("second setup");

        let first = read_config(&first_path)["sync"].clone();
        let second = read_config(&second_path)["sync"].clone();
        assert_eq!(
            first["userId"], second["userId"],
            "one user across machines"
        );
        assert_eq!(
            first["salt"], second["salt"],
            "one salt, or dedup cannot work"
        );
        assert_ne!(first["machineId"], second["machineId"]);

        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        let store = FsStore::new(store_dir.path());
        assert_eq!(
            bootstrap::manifest_machines(&store, &keys).expect("roster"),
            vec!["sandbox-1".to_string(), "sandbox-2".to_string()]
        );
    }

    /// Sandboxes created together all set up before any of them has written the manifest, so
    /// each mints its own user ID. Only one manifest can be created; the others have to adopt
    /// its user ID rather than keep theirs, or every machine files its usage under a user nobody
    /// else reads. Each worker retries a setup that lost its attempts on the shared roster, as
    /// the sandbox's sync loop does.
    #[test]
    fn machines_set_up_at_the_same_moment_all_join_one_user() {
        const MACHINES: usize = 8;
        let store_dir = assert_fs::TempDir::new().expect("temp dir");
        let start = std::sync::Arc::new(std::sync::Barrier::new(MACHINES));

        let handles: Vec<_> = (0..MACHINES)
            .map(|index| {
                let dir = store_dir.path().to_path_buf();
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    let (fixture, config) = config_with("{}");
                    let path = fixture.path("ccusage.json");
                    let args = directory_setup(&dir, &format!("sandbox-{index}"));
                    start.wait();
                    let mut outcome = Err(cli_error("never ran".to_string()));
                    for _ in 0..10 {
                        outcome = setup_sync(&args, &config, Some(path.clone()));
                        if outcome.is_ok() {
                            break;
                        }
                    }
                    outcome.expect("setup");
                    read_config(&path)["sync"]["userId"]
                        .as_str()
                        .expect("user id")
                        .to_string()
                })
            })
            .collect();
        let users: std::collections::BTreeSet<String> = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker finished"))
            .collect();

        assert_eq!(users.len(), 1, "machines split across users: {users:?}");
        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        let store = FsStore::new(store_dir.path());
        assert_eq!(
            bootstrap::manifest_machines(&store, &keys)
                .expect("roster")
                .len(),
            MACHINES
        );
        assert_eq!(
            bootstrap::manifest_user_id(&store, &keys)
                .expect("manifest")
                .as_ref(),
            users.iter().next()
        );
    }

    fn identity_with(configured_user: Option<&str>, machine: &str) -> ResolvedIdentity {
        resolve_identity(
            &IdentityInputs {
                configured_user_id: configured_user,
                configured_machine_id: Some(machine),
                ..IdentityInputs::default()
            },
            &mut || [7u8; 16],
        )
        .expect("identity")
    }

    #[test]
    fn a_minted_user_id_gives_way_to_the_one_the_manifest_recorded() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let store = FsStore::new(dir.path());
        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        bootstrap::ensure_manifest(&store, &keys, "winner").expect("manifest");
        let minted = identity_with(None, "sandbox-1");
        assert_ne!(minted.user.as_str(), "winner");

        let adopted = adopt_winning_user(&store, &keys, minted).expect("adopted");

        assert_eq!(adopted.user.as_str(), "winner");
        assert_eq!(adopted.user_origin, IdentityOrigin::Manifest);
        assert_eq!(adopted.machine.as_str(), "sandbox-1");
    }

    #[test]
    fn a_user_id_the_person_pinned_is_kept_even_if_the_manifest_differs() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let store = FsStore::new(dir.path());
        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        bootstrap::ensure_manifest(&store, &keys, "winner").expect("manifest");

        let kept = adopt_winning_user(&store, &keys, identity_with(Some("mine"), "sandbox-1"))
            .expect("kept");

        assert_eq!(kept.user.as_str(), "mine");
        assert_eq!(kept.user_origin, IdentityOrigin::Configured);
    }

    #[test]
    fn without_a_manifest_the_minted_identity_stands() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        let minted = identity_with(None, "sandbox-1");
        let user = minted.user.as_str().to_string();

        let unchanged =
            adopt_winning_user(&FsStore::new(dir.path()), &keys, minted).expect("unchanged");

        assert_eq!(unchanged.user.as_str(), user);
        assert_eq!(unchanged.user_origin, IdentityOrigin::Minted);
    }

    #[test]
    fn setting_up_the_same_machine_twice_is_idempotent() {
        let store_dir = assert_fs::TempDir::new().expect("temp dir");
        let (fixture, config) = config_with("{}");
        let path = fixture.path("ccusage.json");
        let args = directory_setup(store_dir.path(), "sandbox-1");

        setup_sync(&args, &config, Some(path.clone())).expect("first setup");
        let first = read_config(&path)["sync"].clone();
        let reread = ConfigContext::from_args(&[
            "sync".to_string(),
            "setup".to_string(),
            "--config".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
        setup_sync(&args, &reread, Some(path.clone())).expect("second setup");

        assert_eq!(read_config(&path)["sync"], first);
        let keys = KeySpace::new(DEFAULT_PREFIX).expect("keys");
        assert_eq!(
            bootstrap::manifest_machines(&FsStore::new(store_dir.path()), &keys).expect("roster"),
            vec!["sandbox-1".to_string()]
        );
    }

    #[test]
    fn a_directory_setup_without_a_directory_says_what_to_pass() {
        let (fixture, config) = config_with("{}");
        let args = SyncSetupArgs {
            provider: SyncProvider::Fs,
            ..SyncSetupArgs::default()
        };

        let error = setup_sync(&args, &config, Some(fixture.path("ccusage.json")))
            .expect_err("no directory");

        assert!(format!("{error}").contains("--dir"), "{error}");
    }

    #[test]
    fn serving_without_a_directory_says_what_to_pass() {
        let (_fixture, config) = config_with("{}");

        let error = serve_hub(&SyncServeArgs::default(), &config).expect_err("no directory");

        assert!(format!("{error}").contains("--dir"), "{error}");
    }

    #[test]
    fn a_tilde_expands_to_the_home_directory_and_other_paths_are_left_alone() {
        let home = ccusage_core::home::home_dir().expect("a home directory");

        assert_eq!(expand_home("~/usage"), home.join("usage"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/abs/usage"), PathBuf::from("/abs/usage"));
        assert_eq!(
            expand_home("relative/~/usage"),
            PathBuf::from("relative/~/usage")
        );
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
