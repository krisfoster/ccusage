//! `ccusage sync remove`: the undo for everything the other sync commands did.
//!
//! Four things were created by syncing and all four are removed here: the
//! objects under the key prefix, the two buckets (the private data bucket and
//! the public `-dashboard` one), the service account and HMAC key that signed
//! share links, and the `sync` block in `ccusage.json` that points at them.
//!
//! Two rules make this safe to run:
//!
//! - Objects *outside* the key prefix are never touched, and their presence
//!   keeps the bucket itself: a user who pointed ccusage at a bucket they were
//!   already using for something else gets their prefix emptied, not their
//!   bucket deleted.
//! - The warning is printed whether or not the user passed `--force`. `--force`
//!   suppresses the question, not the disclosure.
//!
//! Order matters for the same reason it does in `prune`: objects go first, so
//! a failure part-way through leaves a bucket that still exists and config that
//! still points at it, which the next `sync remove` can finish. Deleting the
//! config first would strand the objects with nothing that knows where they are.

use std::sync::Arc;

use ccusage_config::{ConfigContext, clear_sync, sync_writeback_path};
use ccusage_objectstore::{KeySpace, ObjectStore, ObjectStoreError, Precondition};

use super::{STORAGE_ENDPOINT, dashboard, lock, now_ms, share, status};
use crate::{
    cli::SyncRemoveArgs,
    cli_error,
    credentials::Credentials,
    gcs::{Authorizer, GcsStore, JsonApi, RetryPolicy, bucket::BucketAdmin, signer::SignerAdmin},
};

type Result<T> = std::result::Result<T, String>;

/// What a bucket holds, split by whether ccusage put it there.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Inventory {
    /// Object paths under ccusage's key prefix, in listing order.
    pub mine: Vec<String>,
    /// Objects the bucket holds outside that prefix. Only the count matters:
    /// one is enough to keep the bucket.
    pub foreign: usize,
}

impl Inventory {
    /// A bucket ccusage has to itself is a bucket ccusage may delete.
    pub fn bucket_is_exclusively_ours(&self) -> bool {
        self.foreign == 0
    }
}

/// Lists the whole bucket and sorts it into ours and theirs.
///
/// A bucket that does not exist inventories as empty: a removal finished
/// earlier, or a dashboard that was never deployed, is not a failure. Every
/// other listing error — expired credentials, no permission, no network —
/// surfaces, because an empty inventory would otherwise read as "nothing to
/// delete" and take the buckets with it.
pub(crate) fn take_inventory(store: &dyn ObjectStore, keys: &KeySpace) -> Result<Inventory> {
    let owned_prefix = format!("{}/", keys.prefix());
    let mut inventory = Inventory::default();
    let listing = match store.list("") {
        Ok(listing) => listing,
        Err(ObjectStoreError::NotFound { .. }) => return Ok(inventory),
        Err(error) => return Err(error.to_string()),
    };
    for object in listing {
        if object.key.starts_with(&owned_prefix) {
            inventory.mine.push(object.key);
        } else {
            inventory.foreign += 1;
        }
    }
    Ok(inventory)
}

/// Deletes every object the inventory claims, and reports how many went.
///
/// An object that is already gone counts as deleted: two `remove` runs racing,
/// or one resumed after a failure, should not turn a success into an error.
pub(crate) fn purge(
    store: &dyn ObjectStore,
    keys: &KeySpace,
    inventory: &Inventory,
) -> Result<usize> {
    let mut deleted = 0;
    for path in &inventory.mine {
        let key = keys.listed(path).map_err(|error| error.to_string())?;
        match store.delete(&key, &Precondition::None) {
            Ok(()) | Err(ObjectStoreError::NotFound { .. }) => deleted += 1,
            Err(error) => {
                return Err(format!(
                    "deleted {deleted} of {} object(s); {path} could not be deleted: {error}",
                    inventory.mine.len()
                ));
            }
        }
    }
    Ok(deleted)
}

pub(crate) fn execute(
    config: &ConfigContext,
    args: &SyncRemoveArgs,
    config_path: Option<&std::path::Path>,
) -> crate::Result<()> {
    let status = status::Status::from_config(config.sync());
    let Some(bucket) = status.bucket.clone() else {
        return Err(cli_error(
            "sync is not configured, so there is nothing in the cloud to remove.".to_string(),
        ));
    };
    let keys = KeySpace::new(status.prefix.trim_end_matches('/'))
        .map_err(|error| cli_error(error.to_string()))?;
    let credentials = Arc::new(
        super::auth::resolve(
            super::config_auth_mode(config),
            true,
            &mut super::auth::TerminalPrompt,
        )
        .map_err(|error| cli_error(error.to_string()))?,
    );
    let store = GcsStore::new(&bucket, Box::new(Arc::clone(&credentials)));
    let assets_bucket = dashboard::assets_bucket(&bucket);
    let assets_store = GcsStore::new(&assets_bucket, Box::new(Arc::clone(&credentials)));

    let inventory = take_inventory(&store, &keys).map_err(cli_error)?;
    let delete_buckets = !args.keep_bucket && inventory.bucket_is_exclusively_ours();
    warn(
        &bucket,
        &assets_bucket,
        &keys,
        &inventory,
        delete_buckets,
        args,
    );

    if args.dry_run {
        println!("Dry run: nothing was deleted.");
        return Ok(());
    }
    if !args.force && !confirm(&bucket) {
        println!("Left everything where it is.");
        return Ok(());
    }

    let _lock = lock::acquire(&lock::default_path(), now_ms()).map_err(cli_error)?;
    let deleted = purge(&store, &keys, &inventory).map_err(cli_error)?;
    println!(
        "Deleted {deleted} object(s) from gs://{bucket}/{}.",
        keys.prefix()
    );
    // A deploy that never happened leaves no dashboard bucket, which
    // inventories as empty rather than as an error.
    let published = take_inventory(&assets_store, &keys).map_err(cli_error)?;
    if !published.mine.is_empty() {
        let deleted = purge(&assets_store, &keys, &published).map_err(cli_error)?;
        println!("Deleted {deleted} object(s) from gs://{assets_bucket}.");
    }

    if args.keep_bucket {
        println!("Kept gs://{bucket} and gs://{assets_bucket}, as asked.");
    } else {
        if delete_buckets {
            remove_bucket(&bucket, &credentials)?;
        } else {
            println!(
                "Kept gs://{bucket}: it holds {} object(s) ccusage did not write. Only the \
                 '{}' prefix was emptied.",
                inventory.foreign,
                keys.prefix()
            );
        }
        // Same rule for the published page's bucket: something else living in
        // it is reason enough to leave it standing.
        if published.bucket_is_exclusively_ours() {
            remove_bucket(&assets_bucket, &credentials)?;
        } else {
            println!(
                "Kept gs://{assets_bucket}: it holds {} object(s) ccusage did not write.",
                published.foreign
            );
        }
    }

    remove_signer(&bucket, status.project_id.as_deref(), &credentials);
    forget_local_settings(config_path);
    Ok(())
}

/// Removes the service account setup created for share links, its keys, and
/// this machine's copy of the secret.
///
/// Best effort, and last: the data is already gone, so a project that refuses
/// the deletion leaves behind an account whose read access now points at
/// nothing. Every key is deleted, not just the one this machine holds — a
/// second machine's key would otherwise keep the account undeletable, and the
/// message says where to finish by hand.
fn remove_signer(bucket: &str, project: Option<&str>, credentials: &Arc<Credentials>) {
    if share::forget(&share::default_path()) {
        println!("Removed this machine's dashboard signing key.");
    }
    let Some(project) = project else {
        return;
    };
    let admin = SignerAdmin::new(project, Arc::clone(credentials) as Arc<dyn Authorizer>);
    let email = admin.service_account_email();
    let outcome = admin.hmac_access_ids(&email).and_then(|access_ids| {
        for access_id in access_ids {
            admin.delete_hmac_key(&access_id)?;
        }
        admin.delete_service_account(&email)
    });
    match outcome {
        Ok(true) => println!("Deleted the dashboard signer {email}."),
        Ok(false) => {}
        Err(error) => println!(
            "The data in gs://{bucket} is gone, but the dashboard signer {email} could not be \
             deleted: {error}. Remove it with `gcloud iam service-accounts delete {email} \
             --project {project}`."
        ),
    }
}

/// Deletes one bucket, tolerating one that was never created.
///
/// GCS refuses a bucket that still holds objects, so the caller has to have
/// purged it first; the dashboard bucket holds only the published page, which
/// lives under the same key prefix as everything else.
fn remove_bucket(name: &str, credentials: &Arc<Credentials>) -> crate::Result<()> {
    let admin = BucketAdmin::new(
        JsonApi::new(
            STORAGE_ENDPOINT,
            Box::new(Arc::clone(credentials)),
            RetryPolicy::default(),
        ),
        name,
    );
    match admin.delete() {
        Ok(true) => println!("Deleted bucket gs://{name}."),
        Ok(false) => {}
        Err(error) => {
            return Err(cli_error(format!(
                "the objects are gone but bucket gs://{name} could not be deleted: {error}"
            )));
        }
    }
    Ok(())
}

/// Clearing config is best-effort on purpose: the cloud side is already done,
/// and failing here would report a removal that did happen as an error.
fn forget_local_settings(config_path: Option<&std::path::Path>) {
    let Some(path) = sync_writeback_path(config_path) else {
        return;
    };
    match clear_sync(&path) {
        Ok(true) => println!("Removed the 'sync' settings from {}.", path.display()),
        Ok(false) => {}
        Err(error) => println!(
            "The cloud data is gone, but the 'sync' settings in {} could not be removed: {error}. \
             Delete that block by hand before syncing again.",
            path.display()
        ),
    }
}

/// Printed on every path, including `--force`: the point of the flag is to
/// skip the question, not to hide what is about to happen.
fn warn(
    bucket: &str,
    assets_bucket: &str,
    keys: &KeySpace,
    inventory: &Inventory,
    delete_buckets: bool,
    args: &SyncRemoveArgs,
) {
    let verb = if args.dry_run {
        "Would delete"
    } else {
        "About to delete"
    };
    println!(
        "{verb} {} object(s) under gs://{bucket}/{}.",
        inventory.mine.len(),
        keys.prefix()
    );
    if delete_buckets {
        println!("{verb} the buckets themselves: gs://{bucket} and gs://{assets_bucket}.");
    }
    println!("{verb} the service account that signed dashboard share links, and its keys.");
    println!(
        "This is every machine's uploaded usage, not just this one's, and it cannot be undone. \
         Your local logs are untouched."
    );
}

fn confirm(bucket: &str) -> bool {
    print!("Type the bucket name ({bucket}) to confirm: ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    if std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer).is_err() {
        return false;
    }
    answer.trim() == bucket
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::objectstore::{Fault, MemoryStore, Op, When};

    use super::*;

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("prefix")
    }

    /// Seeds an object at an exact path, inside this key space or outside it:
    /// `listed` is the only way to name an arbitrary path, so a foreign object
    /// is named through a key space rooted at its own prefix.
    fn put(store: &MemoryStore, path: &str) {
        let root = path.split('/').next().unwrap_or(path);
        let space = KeySpace::new(root).expect("prefix");
        let key = space.listed(path).expect("path");
        store
            .put(&key, b"{}", "application/json", &Precondition::None)
            .expect("put");
    }

    #[test]
    fn an_empty_bucket_has_nothing_to_remove() {
        let inventory = take_inventory(&MemoryStore::new(), &keys()).expect("inventory");

        assert_eq!(inventory, Inventory::default());
        assert!(inventory.bucket_is_exclusively_ours());
    }

    /// Answers every request the way GCS answers one for a bucket that is not
    /// there, which `MemoryStore` has no fault for.
    struct AbsentBucket;

    impl AbsentBucket {
        fn gone<T>() -> std::result::Result<T, ObjectStoreError> {
            Err(ObjectStoreError::NotFound {
                key: "gs://ccusage-gone".to_string(),
            })
        }
    }

    impl ObjectStore for AbsentBucket {
        fn get(
            &self,
            _key: &ccusage_objectstore::Key,
        ) -> std::result::Result<Option<(Vec<u8>, ccusage_objectstore::ObjectMeta)>, ObjectStoreError>
        {
            Self::gone()
        }

        fn put(
            &self,
            _key: &ccusage_objectstore::Key,
            _body: &[u8],
            _content_type: &str,
            _precondition: &Precondition,
        ) -> std::result::Result<ccusage_objectstore::ObjectMeta, ObjectStoreError> {
            Self::gone()
        }

        fn list(
            &self,
            _prefix: &str,
        ) -> std::result::Result<Vec<ccusage_objectstore::ObjectMeta>, ObjectStoreError> {
            Self::gone()
        }

        fn delete(
            &self,
            _key: &ccusage_objectstore::Key,
            _precondition: &Precondition,
        ) -> std::result::Result<(), ObjectStoreError> {
            Self::gone()
        }
    }

    /// A dashboard that was never deployed, or a removal being re-run, must
    /// not read as a failure.
    #[test]
    fn a_bucket_that_is_not_there_inventories_as_empty() {
        assert_eq!(
            take_inventory(&AbsentBucket, &keys()).expect("inventory"),
            Inventory::default()
        );
    }

    /// The opposite case: an expired credential must not be mistaken for an
    /// empty bucket, which would delete the bucket as though nothing was lost.
    #[test]
    fn a_listing_that_fails_is_not_an_empty_bucket() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/manifest.json");
        store.fail_on(When::any("").op(Op::List).always(), Fault::Unauthenticated);

        take_inventory(&store, &keys()).expect_err("a refused listing must surface");
    }

    #[test]
    fn everything_under_the_prefix_is_ours_including_the_public_page() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/manifest.json");
        put(
            &store,
            "ccusage/v1/users/u1/machines/m1/shards/claude/2026/09/17.json",
        );
        put(&store, "ccusage/v1/dashboard/index.html");

        let inventory = take_inventory(&store, &keys()).expect("inventory");

        assert_eq!(inventory.mine.len(), 3);
        assert_eq!(inventory.foreign, 0);
    }

    #[test]
    fn objects_someone_else_put_in_the_bucket_are_neither_ours_nor_deleted() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/manifest.json");
        put(&store, "backups/photos.tar");
        put(&store, "elsewhere/notes.json");

        let inventory = take_inventory(&store, &keys()).expect("inventory");

        assert_eq!(inventory.mine, vec!["ccusage/v1/manifest.json".to_string()]);
        assert_eq!(inventory.foreign, 2);
        assert!(
            !inventory.bucket_is_exclusively_ours(),
            "a bucket holding someone else's objects must survive"
        );

        purge(&store, &keys(), &inventory).expect("purge");

        assert_eq!(
            store.keys(),
            vec![
                "backups/photos.tar".to_string(),
                "elsewhere/notes.json".to_string()
            ]
        );
    }

    #[test]
    fn purging_removes_every_object_it_promised_to() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/manifest.json");
        put(&store, "ccusage/v1/rollup/daily.json");
        let inventory = take_inventory(&store, &keys()).expect("inventory");

        let deleted = purge(&store, &keys(), &inventory).expect("purge");

        assert_eq!(deleted, 2);
        assert_eq!(
            take_inventory(&store, &keys()).expect("inventory"),
            Inventory::default()
        );
    }

    /// A second remove, or one resumed after a failure, must still succeed.
    #[test]
    fn purging_what_is_already_gone_is_not_an_error() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/manifest.json");
        let inventory = take_inventory(&store, &keys()).expect("inventory");
        purge(&store, &keys(), &inventory).expect("first purge");

        assert_eq!(purge(&store, &keys(), &inventory).expect("second purge"), 1);
    }

    /// The count in the error message is what tells the user how far it got,
    /// so a failure must not read as a no-op.
    #[test]
    fn a_failing_delete_says_how_much_was_already_removed() {
        let store = MemoryStore::new();
        put(&store, "ccusage/v1/a.json");
        put(&store, "ccusage/v1/b.json");
        let inventory = take_inventory(&store, &keys()).expect("inventory");
        store.fail_on(
            When::delete("b.json").always(),
            Fault::Server { status: 500 },
        );

        let error = purge(&store, &keys(), &inventory).expect_err("delete must fail");

        assert!(error.contains("deleted 1 of 2"), "{error}");
        assert!(error.contains("ccusage/v1/b.json"), "{error}");
    }
}
