//! The share-link signing key: where it comes from, and where it is kept.
//!
//! The key is a credential, and `ccusage.json` refuses to hold one — that file
//! is committed, synced between machines and read by everything, and the config
//! loader rejects a secret-looking key outright. So it lives on its own, beside
//! the sync lock in this machine's state directory, owner-readable and never
//! uploaded anywhere.
//!
//! It is scoped to the bucket it was minted for. Pointing a machine at a
//! different bucket has to mint a new key rather than sign links with a
//! credential that has no access to the data they name, which would produce
//! links that fail at the far end with an opaque 403.

use std::{
    fs,
    path::{Path, PathBuf},
};

use ccusage_objectstore::HmacKey;
use serde_json::{Value, json};

use crate::gcs::{bucket::BucketAdmin, signer::SignerAdmin};

/// The signer this machine uses for `--share`, as stored on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSigner {
    pub(crate) bucket: String,
    pub(crate) service_account: String,
    pub(crate) access_id: String,
    pub(crate) secret: String,
}

impl StoredSigner {
    pub(crate) fn hmac_key(&self) -> HmacKey {
        HmacKey::new(&self.access_id, &self.secret)
    }
}

/// Beside the sync lock: per-user state, not configuration, and on a path that
/// does not depend on which directory the user happens to be in.
pub(crate) fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| ccusage_core::home::home_dir().map(|home| home.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("ccusage").join("sync-signer.json")
}

/// Reads the stored signer, if there is one for `bucket`.
///
/// A key for another bucket, or a file that has been damaged, reads as absent:
/// the caller's next move either way is to mint a new one, and refusing to run
/// because of an unusable cache would be worse than replacing it.
pub(crate) fn load(path: &Path, bucket: &str) -> Option<StoredSigner> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let signer = StoredSigner {
        bucket: string_at(&value, "bucket")?,
        service_account: string_at(&value, "serviceAccount")?,
        access_id: string_at(&value, "accessId")?,
        secret: string_at(&value, "secret")?,
    };
    (signer.bucket == bucket).then_some(signer)
}

pub(crate) fn save(path: &Path, signer: &StoredSigner) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let document = json!({
        "bucket": signer.bucket,
        "serviceAccount": signer.service_account,
        "accessId": signer.access_id,
        "secret": signer.secret,
    });
    let body = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("could not render the signer: {error}"))?;
    write_owner_only(path, &body)
}

/// Written owner-only from the start rather than chmod'ed afterwards: a
/// world-readable window, however short, is a window in which anything on the
/// machine can read a key that reads the user's usage data.
#[cfg(unix)]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.write_all(body)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<(), String> {
    fs::write(path, body).map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Forgets the local copy of the key. The key itself is deleted in the project;
/// this only removes the file naming it.
pub(crate) fn forget(path: &Path) -> bool {
    fs::remove_file(path).is_ok()
}

/// What provisioning did, so setup can say it without the provisioning code
/// printing anything itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Provisioned {
    /// A usable key was already on this machine.
    Existing,
    /// A service account and key were created.
    Minted,
}

/// Makes sure this machine can sign share links for `bucket`.
///
/// Idempotent in the way setup needs: an existing local key is kept, an
/// existing service account is adopted, and the read binding is re-asserted
/// (which is a no-op once it is there) so a bucket recreated without it heals
/// on the next setup.
pub(crate) fn ensure(
    admin: &SignerAdmin,
    bucket_admin: &BucketAdmin,
    bucket: &str,
    path: &Path,
) -> Result<(StoredSigner, Provisioned), String> {
    if let Some(existing) = load(path, bucket) {
        bucket_admin
            .grant_object_viewer(&format!("serviceAccount:{}", existing.service_account))
            .map_err(|error| error.to_string())?;
        return Ok((existing, Provisioned::Existing));
    }
    let email = admin
        .ensure_service_account()
        .map_err(|error| error.to_string())?;
    bucket_admin
        .grant_object_viewer(&format!("serviceAccount:{email}"))
        .map_err(|error| error.to_string())?;
    let key = admin
        .create_hmac_key(&email)
        .map_err(|error| error.to_string())?;
    let signer = StoredSigner {
        bucket: bucket.to_string(),
        service_account: email,
        access_id: key.access_id,
        secret: key.secret,
    };
    save(path, &signer)?;
    Ok((signer, Provisioned::Minted))
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::http_server::{ScriptedServer, json_response as json};

    use super::*;

    fn signer(bucket: &str) -> StoredSigner {
        StoredSigner {
            bucket: bucket.to_string(),
            service_account: "ccusage-dashboard@my-project.iam.gserviceaccount.com".to_string(),
            access_id: "GOOG1EXAMPLE".to_string(),
            secret: "c2VjcmV0".to_string(),
        }
    }

    #[test]
    fn a_saved_signer_reads_back() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("state").join("sync-signer.json");

        save(&path, &signer("ccusage-abc")).expect("saved");

        assert_eq!(load(&path, "ccusage-abc"), Some(signer("ccusage-abc")));
    }

    /// The key only grants read on the bucket it was minted against, so
    /// reusing it elsewhere would mint links that 403 for the recipient.
    #[test]
    fn a_signer_for_another_bucket_is_not_used() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");
        save(&path, &signer("ccusage-abc")).expect("saved");

        assert_eq!(load(&path, "ccusage-xyz"), None);
    }

    #[test]
    fn a_damaged_or_absent_file_reads_as_no_signer() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let missing = dir.path().join("sync-signer.json");
        assert_eq!(load(&missing, "ccusage-abc"), None);

        let truncated = dir.path().join("half.json");
        fs::write(&truncated, r#"{"bucket":"ccusage-abc","accessId":"#).expect("wrote");
        assert_eq!(load(&truncated, "ccusage-abc"), None);

        let incomplete = dir.path().join("incomplete.json");
        fs::write(&incomplete, r#"{"bucket":"ccusage-abc"}"#).expect("wrote");
        assert_eq!(load(&incomplete, "ccusage-abc"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_never_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");

        save(&path, &signer("ccusage-abc")).expect("saved");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
    }

    #[test]
    fn forgetting_removes_the_file_and_tolerates_its_absence() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");
        save(&path, &signer("ccusage-abc")).expect("saved");

        assert!(forget(&path));
        assert!(!forget(&path));
        assert_eq!(load(&path, "ccusage-abc"), None);
    }

    fn no_retry() -> crate::gcs::RetryPolicy {
        crate::gcs::RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::ZERO,
            max_delay: std::time::Duration::ZERO,
        }
    }

    fn admins(server: &ScriptedServer) -> (SignerAdmin, BucketAdmin) {
        let signer = SignerAdmin::with_endpoints(
            server.endpoint(),
            server.endpoint(),
            "my-project",
            std::sync::Arc::new(crate::gcs::BearerToken::new("test-token")),
            no_retry(),
        );
        let bucket = BucketAdmin::new(
            crate::gcs::JsonApi::new(
                server.endpoint(),
                Box::new(crate::gcs::BearerToken::new("test-token")),
                no_retry(),
            ),
            "ccusage-abc",
        );
        (signer, bucket)
    }

    const SIGNER_EMAIL: &str = "ccusage-dashboard@my-project.iam.gserviceaccount.com";

    /// The whole point of the redesign: one `sync setup` leaves the machine
    /// able to sign, with no key for the user to create or paste.
    #[test]
    fn a_first_setup_creates_the_account_grants_it_read_and_keeps_the_key() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");
        let mut fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"not found"}}"#),
            json(200, &format!(r#"{{"email":"{SIGNER_EMAIL}"}}"#)),
            json(200, r#"{"etag":"tag","bindings":[]}"#),
            json(200, r#"{"etag":"tag2"}"#),
            json(
                200,
                r#"{"secret":"c2VjcmV0","metadata":{"accessId":"GOOG1MINTED","state":"ACTIVE"}}"#,
            ),
        ]);
        let (signer_admin, bucket_admin) = admins(&fake);

        let (stored, provisioned) =
            ensure(&signer_admin, &bucket_admin, "ccusage-abc", &path).expect("provisioned");

        assert_eq!(provisioned, Provisioned::Minted);
        assert_eq!(stored.access_id, "GOOG1MINTED");
        assert_eq!(load(&path, "ccusage-abc"), Some(stored));
        let requests = fake.requests();
        assert!(
            requests[3].contains(&format!("serviceAccount:{SIGNER_EMAIL}")),
            "{}",
            requests[3]
        );
        assert!(
            requests[3].contains("roles/storage.objectViewer"),
            "read-only, never write: {}",
            requests[3]
        );
    }

    /// Setup is run again after the first one worked. Minting a second key
    /// every time would pile up credentials that no cleanup knows about.
    #[test]
    fn a_rerun_keeps_the_existing_key_and_mints_nothing() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");
        save(&path, &signer("ccusage-abc")).expect("saved");
        let mut fake = ScriptedServer::serving(vec![json(
            200,
            r#"{"etag":"tag","bindings":[{"role":"roles/storage.objectViewer",
                 "members":["serviceAccount:ccusage-dashboard@my-project.iam.gserviceaccount.com"]}]}"#,
        )]);
        let (signer_admin, bucket_admin) = admins(&fake);

        let (stored, provisioned) =
            ensure(&signer_admin, &bucket_admin, "ccusage-abc", &path).expect("provisioned");

        assert_eq!(provisioned, Provisioned::Existing);
        assert_eq!(stored, signer("ccusage-abc"));
        assert_eq!(
            fake.requests().len(),
            1,
            "only the read binding was re-checked"
        );
    }

    /// A project that refuses the service account must not leave a file
    /// claiming this machine can sign.
    #[test]
    fn a_refused_account_stores_nothing() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join("sync-signer.json");
        let fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"not found"}}"#),
            json(403, r#"{"error":{"message":"permission denied"}}"#),
        ]);
        let (signer_admin, bucket_admin) = admins(&fake);

        let error =
            ensure(&signer_admin, &bucket_admin, "ccusage-abc", &path).expect_err("refused");

        assert!(error.contains("permission denied"), "{error}");
        assert_eq!(load(&path, "ccusage-abc"), None);
    }

    /// Per-user state, not per-directory: a key found only when the user
    /// happens to be in one checkout is a key they will be told is missing.
    #[test]
    fn the_default_path_is_absolute_state_not_a_relative_file() {
        let path = default_path();

        assert!(path.ends_with("ccusage/sync-signer.json"), "{path:?}");
        assert!(path.is_absolute(), "{path:?}");
    }
}
