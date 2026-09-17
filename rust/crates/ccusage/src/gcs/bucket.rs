//! Bucket administration for `sync setup`: create the bucket, hold it to the
//! access shape the dashboard depends on, and sign blobs for share links.
//!
//! The data path never calls any of this. Setup runs it once, `sync doctor`
//! re-reads it to report drift, and everything here is idempotent so a repeated
//! run converges instead of failing.
//!
//! The access shape is the whole point of the module. The dashboard shell is
//! world-readable and every usage object is private. A prefix-scoped public
//! binding would express that in one bucket, but GCS refuses it — an `allUsers`
//! binding cannot carry an IAM condition ("Conditions are not allowed on public
//! resources"), so the boundary has to be the bucket itself: the data bucket
//! never becomes public, and the dashboard shell is deployed to a second,
//! separate bucket that holds nothing else. Get that wrong and the bucket
//! either serves nothing or serves the user's spend to the internet.

use ccusage_objectstore::{ObjectStoreError, Result};
use serde_json::{Value, json};

use super::{JsonApi, encode, status_error};

/// Where share-link signing happens when the caller has no private key: the
/// IAM Credentials API signs with the service account's Google-managed key.
const DEFAULT_IAM_CREDENTIALS_ENDPOINT: &str = "https://iamcredentials.googleapis.com";

/// Whether the bucket refuses to become public.
///
/// `Enforced` blocks the `allUsers` binding, so the public assets bucket is
/// created `Inherited`. The data bucket has no reason to ever be public and is
/// left `Enforced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicAccessPrevention {
    Enforced,
    Inherited,
}

impl PublicAccessPrevention {
    fn as_api(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::Inherited => "inherited",
        }
    }

    fn from_api(value: &str) -> Option<Self> {
        match value {
            "enforced" => Some(Self::Enforced),
            "inherited" => Some(Self::Inherited),
            _ => None,
        }
    }
}

/// What the bucket should look like. Only the fields setup actually decides;
/// everything else is left at the project default.
#[derive(Debug, Clone)]
pub(crate) struct BucketSpec {
    pub(crate) project: String,
    pub(crate) location: String,
    pub(crate) storage_class: String,
    pub(crate) public_access_prevention: PublicAccessPrevention,
    /// `0` turns soft delete off. The default retention makes deleted objects
    /// billable for a week, which is surprising on a bucket this small.
    pub(crate) soft_delete_retention_days: u32,
}

impl BucketSpec {
    pub(crate) fn new(project: &str, location: &str) -> Self {
        Self {
            project: project.to_string(),
            location: location.to_string(),
            storage_class: "STANDARD".to_string(),
            public_access_prevention: PublicAccessPrevention::Inherited,
            soft_delete_retention_days: 0,
        }
    }
}

/// The parts of the bucket resource that decide whether sync and the dashboard
/// will work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BucketInfo {
    pub(crate) name: String,
    pub(crate) location: String,
    pub(crate) uniform_bucket_level_access: bool,
    pub(crate) public_access_prevention: Option<PublicAccessPrevention>,
}

impl BucketInfo {
    fn from_value(value: &Value) -> Self {
        let iam_configuration = value.get("iamConfiguration");
        Self {
            name: string_at(value, "name").unwrap_or_default(),
            location: string_at(value, "location").unwrap_or_default(),
            uniform_bucket_level_access: iam_configuration
                .and_then(|configuration| configuration.get("uniformBucketLevelAccess"))
                .and_then(|access| access.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            public_access_prevention: iam_configuration
                .and_then(|configuration| string_at(configuration, "publicAccessPrevention"))
                .as_deref()
                .and_then(PublicAccessPrevention::from_api),
        }
    }
}

/// A browser origin allowed to read public assets cross-origin.
#[derive(Debug, Clone)]
pub(crate) struct CorsRule {
    pub(crate) origins: Vec<String>,
    pub(crate) max_age_seconds: u32,
}

pub(crate) struct BucketAdmin {
    api: JsonApi,
    bucket: String,
    iam_credentials_endpoint: String,
}

impl BucketAdmin {
    pub(crate) fn new(api: JsonApi, bucket: &str) -> Self {
        Self {
            api,
            bucket: bucket.to_string(),
            iam_credentials_endpoint: DEFAULT_IAM_CREDENTIALS_ENDPOINT.to_string(),
        }
    }

    pub(crate) fn with_iam_credentials_endpoint(mut self, endpoint: &str) -> Self {
        self.iam_credentials_endpoint = endpoint.trim_end_matches('/').to_string();
        self
    }

    fn bucket_url(&self, suffix: &str) -> String {
        format!(
            "{}/storage/v1/b/{}{suffix}",
            self.api.endpoint(),
            encode(&self.bucket),
        )
    }

    /// Reads the bucket, or `None` if it does not exist.
    ///
    /// A 403 is deliberately *not* folded into `None`: "someone else owns this
    /// name" and "this name is free" lead to opposite next steps, and only the
    /// first is worth telling the user about.
    pub(crate) fn get(&self) -> Result<Option<BucketInfo>> {
        let url = self.bucket_url("");
        self.api.with_retry(|| {
            let response = self.api.send_without_body(self.api.agent.get(&url))?;
            if response.status == 404 {
                return Ok(None);
            }
            if let Some(error) = status_error(&response, &self.bucket) {
                return Err(error);
            }
            parse_bucket(&self.bucket, &response.body).map(Some)
        })
    }

    /// Creates the bucket if it is absent and returns it either way.
    ///
    /// Two machines running setup against the same name race here, so a 409 is
    /// resolved by re-reading rather than by failing: the loser of the race
    /// still wants the bucket that the winner just made.
    pub(crate) fn ensure(&self, spec: &BucketSpec) -> Result<BucketInfo> {
        if let Some(existing) = self.get()? {
            return Ok(existing);
        }
        match self.create(spec) {
            Ok(created) => Ok(created),
            Err(ObjectStoreError::Conflict { .. }) => {
                self.get()?.ok_or_else(|| ObjectStoreError::Other {
                    detail: format!(
                        "bucket {} was reported as already existing but cannot be read; \
                         the name is taken by another project",
                        self.bucket
                    ),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn create(&self, spec: &BucketSpec) -> Result<BucketInfo> {
        let url = format!(
            "{}/storage/v1/b?project={}",
            self.api.endpoint(),
            encode(&spec.project),
        );
        // Uniform bucket-level access is non-negotiable: object ACLs would let a
        // single mis-set ACL publish usage data, and IAM conditions — the whole
        // basis of the public-prefix binding — are ignored without it.
        let body = json!({
            "name": self.bucket,
            "location": spec.location,
            "storageClass": spec.storage_class,
            "iamConfiguration": {
                "uniformBucketLevelAccess": { "enabled": true },
                "publicAccessPrevention": spec.public_access_prevention.as_api(),
            },
            "softDeletePolicy": {
                "retentionDurationSeconds": (u64::from(spec.soft_delete_retention_days) * 86_400)
                    .to_string(),
            },
        });
        self.send_bucket_write(&url, Method::Post, &body)
    }

    /// Flips public access prevention without touching anything else.
    pub(crate) fn set_public_access_prevention(
        &self,
        prevention: PublicAccessPrevention,
    ) -> Result<BucketInfo> {
        let body = json!({
            "iamConfiguration": {
                "publicAccessPrevention": prevention.as_api(),
            },
        });
        self.send_bucket_write(&self.bucket_url(""), Method::Patch, &body)
    }

    /// Replaces the CORS configuration.
    ///
    /// The dashboard is served from the bucket itself, so this matters only for
    /// a custom domain or a local development origin; an empty rule list is the
    /// way to remove one that is no longer wanted.
    pub(crate) fn set_cors(&self, rules: &[CorsRule]) -> Result<BucketInfo> {
        let cors: Vec<Value> = rules
            .iter()
            .map(|rule| {
                json!({
                    "origin": rule.origins,
                    "method": ["GET", "HEAD"],
                    "responseHeader": ["Content-Type", "Range"],
                    "maxAgeSeconds": rule.max_age_seconds,
                })
            })
            .collect();
        self.send_bucket_write(
            &self.bucket_url(""),
            Method::Patch,
            &json!({ "cors": cors }),
        )
    }

    fn send_bucket_write(&self, url: &str, method: Method, body: &Value) -> Result<BucketInfo> {
        let payload = serde_json::to_vec(body).map_err(|error| ObjectStoreError::Other {
            detail: format!("unserializable bucket request: {error}"),
        })?;
        self.api.with_retry(|| {
            let request = match method {
                Method::Post => self.api.agent.post(url),
                Method::Patch => self.api.agent.patch(url),
                Method::Put => self.api.agent.put(url),
            };
            let response = self
                .api
                .send_with_body(request, &payload, "application/json")?;
            if let Some(error) = status_error(&response, &self.bucket) {
                return Err(error);
            }
            parse_bucket(&self.bucket, &response.body)
        })
    }

    pub(crate) fn get_iam_policy(&self) -> Result<Value> {
        // Version 3 is required to see condition expressions; asking for a lower
        // version silently drops them, and writing back what you read would then
        // widen a conditional binding into an unconditional one.
        let url = self.bucket_url("/iam?optionsRequestedPolicyVersion=3");
        self.api.with_retry(|| {
            let response = self.api.send_without_body(self.api.agent.get(&url))?;
            if let Some(error) = status_error(&response, &self.bucket) {
                return Err(error);
            }
            serde_json::from_slice(&response.body).map_err(|error| ObjectStoreError::Other {
                detail: format!("unreadable IAM policy for {}: {error}", self.bucket),
            })
        })
    }

    /// Grants `allUsers` object read access on this whole bucket.
    ///
    /// Unconditional, because GCS rejects an IAM condition on a binding whose
    /// member is public. Containment therefore comes from *which* bucket this
    /// is called on: only the dashboard assets bucket, never the data bucket.
    ///
    /// Read-modify-write on the policy `etag`, so a concurrent edit loses rather
    /// than being clobbered. Re-running is a no-op once the binding is present,
    /// which keeps deploys idempotent.
    pub(crate) fn grant_public_read(&self) -> Result<bool> {
        let mut policy = self.get_iam_policy()?;
        let bindings = policy
            .get_mut("bindings")
            .and_then(Value::as_array_mut)
            .map_or_else(Vec::new, std::mem::take);
        if bindings.iter().any(|binding| {
            string_at(binding, "role").as_deref() == Some("roles/storage.objectViewer")
                && binding
                    .get("members")
                    .and_then(Value::as_array)
                    .is_some_and(|members| members.iter().any(|member| member == "allUsers"))
                && binding.get("condition").is_none()
        }) {
            return Ok(false);
        }
        let mut updated = bindings;
        updated.push(json!({
            "role": "roles/storage.objectViewer",
            "members": ["allUsers"],
        }));
        let etag = string_at(&policy, "etag");
        let mut request = json!({ "version": 3, "bindings": updated });
        if let Some(etag) = etag {
            request["etag"] = Value::String(etag);
        }
        let url = self.bucket_url("/iam");
        let payload = serde_json::to_vec(&request).map_err(|error| ObjectStoreError::Other {
            detail: format!("unserializable IAM policy: {error}"),
        })?;
        self.api.with_retry(|| {
            let response =
                self.api
                    .send_with_body(self.api.agent.put(&url), &payload, "application/json")?;
            if let Some(error) = status_error(&response, &self.bucket) {
                return Err(error);
            }
            Ok(())
        })?;
        Ok(true)
    }

    /// Signs `blob` with a service account's Google-managed key, for a V4
    /// signed URL minted without ever holding a private key locally.
    ///
    /// The caller needs `roles/iam.serviceAccountTokenCreator` on the account.
    pub(crate) fn sign_blob(&self, service_account: &str, blob: &[u8]) -> Result<Vec<u8>> {
        let url = format!(
            "{}/v1/projects/-/serviceAccounts/{}:signBlob",
            self.iam_credentials_endpoint,
            encode(service_account),
        );
        let payload =
            serde_json::to_vec(&json!({ "payload": base64_encode(blob) })).map_err(|error| {
                ObjectStoreError::Other {
                    detail: format!("unserializable signBlob request: {error}"),
                }
            })?;
        self.api.with_retry(|| {
            let response =
                self.api
                    .send_with_body(self.api.agent.post(&url), &payload, "application/json")?;
            if let Some(error) = status_error(&response, service_account) {
                return Err(error);
            }
            let value: Value = serde_json::from_slice(&response.body).map_err(|error| {
                ObjectStoreError::Other {
                    detail: format!("unreadable signBlob response: {error}"),
                }
            })?;
            let signed =
                string_at(&value, "signedBlob").ok_or_else(|| ObjectStoreError::Other {
                    detail: "signBlob response had no signedBlob".to_string(),
                })?;
            base64_decode(&signed).ok_or_else(|| ObjectStoreError::Other {
                detail: "signBlob returned a signature that is not base64".to_string(),
            })
        })
    }
}

enum Method {
    Post,
    Patch,
    Put,
}

/// The condition that keeps `allUsers` inside the public prefix.
///
/// Taken from the key builder rather than from a caller-supplied string, so the
/// grant cannot drift away from the only prefix that produces public keys.
fn parse_bucket(bucket: &str, body: &[u8]) -> Result<BucketInfo> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ObjectStoreError::Other {
        detail: format!("unreadable bucket resource for {bucket}: {error}"),
    })?;
    Ok(BucketInfo::from_value(&value))
}

fn string_at(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ccusage_test_support::http_server::{ScriptedServer, json_response as json};

    use super::{
        super::{BearerToken, RetryPolicy},
        *,
    };

    fn no_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn admin_for(server: &ScriptedServer) -> BucketAdmin {
        BucketAdmin::new(
            JsonApi::new(
                server.endpoint(),
                Box::new(BearerToken::new("test-token")),
                no_retry(),
            ),
            "ccusage-abc123",
        )
        .with_iam_credentials_endpoint(server.endpoint())
    }

    const BUCKET_RESOURCE: &str = r#"{
        "name": "ccusage-abc123",
        "location": "EU",
        "iamConfiguration": {
            "uniformBucketLevelAccess": { "enabled": true },
            "publicAccessPrevention": "inherited"
        }
    }"#;

    #[test]
    fn an_absent_bucket_reads_as_none_rather_than_an_error() {
        let fake = ScriptedServer::serving(vec![json(404, r#"{"error":{"message":"Not Found"}}"#)]);

        assert!(admin_for(&fake).get().expect("get").is_none());
    }

    #[test]
    fn a_bucket_owned_by_someone_else_is_an_error_not_an_absence() {
        let fake = ScriptedServer::serving(vec![json(
            403,
            r#"{"error":{"message":"does not have storage.buckets.get access"}}"#,
        )]);

        let error = admin_for(&fake).get().expect_err("403 must surface");

        assert!(matches!(error, ObjectStoreError::Forbidden { .. }));
        assert!(!error.to_string().contains("test-token"));
    }

    #[test]
    fn creating_a_bucket_enables_uniform_access_and_disables_soft_delete() {
        let mut fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"Not Found"}}"#),
            json(200, BUCKET_RESOURCE),
        ]);

        let info = admin_for(&fake)
            .ensure(&BucketSpec::new("my-project", "EU"))
            .expect("ensure");

        assert_eq!(
            info,
            BucketInfo {
                name: "ccusage-abc123".to_string(),
                location: "EU".to_string(),
                uniform_bucket_level_access: true,
                public_access_prevention: Some(PublicAccessPrevention::Inherited),
            }
        );
        let create = fake.requests().remove(1);
        assert!(create.contains("project=my-project"), "{create}");
        assert!(
            create.contains(r#""uniformBucketLevelAccess":{"enabled":true}"#),
            "{create}"
        );
        assert!(
            create.contains(r#""publicAccessPrevention":"inherited""#),
            "{create}"
        );
        assert!(
            create.contains(r#""retentionDurationSeconds":"0""#),
            "{create}"
        );
    }

    #[test]
    fn an_existing_bucket_is_adopted_without_a_create_call() {
        let mut fake = ScriptedServer::serving(vec![json(200, BUCKET_RESOURCE)]);

        admin_for(&fake)
            .ensure(&BucketSpec::new("my-project", "EU"))
            .expect("ensure");

        assert_eq!(fake.requests().len(), 1, "setup must be idempotent");
    }

    #[test]
    fn losing_the_create_race_adopts_the_winners_bucket() {
        let mut fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"Not Found"}}"#),
            json(
                409,
                r#"{"error":{"message":"You already own this bucket"}}"#,
            ),
            json(200, BUCKET_RESOURCE),
        ]);

        let info = admin_for(&fake)
            .ensure(&BucketSpec::new("my-project", "EU"))
            .expect("a concurrent creator is not a failure");

        assert_eq!(info.name, "ccusage-abc123");
        assert_eq!(fake.requests().len(), 3);
    }

    #[test]
    fn public_access_prevention_can_be_flipped_back_to_enforced() {
        let mut fake = ScriptedServer::serving(vec![json(200, BUCKET_RESOURCE)]);

        admin_for(&fake)
            .set_public_access_prevention(PublicAccessPrevention::Enforced)
            .expect("patch");

        let request = fake.requests().remove(0);
        assert!(request.starts_with("PATCH "), "{request}");
        assert!(
            request.contains(r#""publicAccessPrevention":"enforced""#),
            "{request}"
        );
    }

    #[test]
    fn cors_is_limited_to_read_methods_for_the_given_origins() {
        let mut fake = ScriptedServer::serving(vec![json(200, BUCKET_RESOURCE)]);

        admin_for(&fake)
            .set_cors(&[CorsRule {
                origins: vec!["https://usage.example".to_string()],
                max_age_seconds: 3600,
            }])
            .expect("patch");

        let request = fake.requests().remove(0);
        assert!(
            request.contains(r#""origin":["https://usage.example"]"#),
            "{request}"
        );
        assert!(request.contains(r#""method":["GET","HEAD"]"#), "{request}");
    }

    /// GCS rejects a condition on a public member, so the binding is
    /// unconditional and the containment is the bucket this runs against.
    #[test]
    fn the_public_binding_carries_no_condition() {
        let mut fake = ScriptedServer::serving(vec![
            json(200, r#"{"version":1,"etag":"BwXhfw==","bindings":[]}"#),
            json(200, "{}"),
        ]);

        let granted = admin_for(&fake).grant_public_read().expect("grant");

        assert!(granted);
        let write = fake.requests().remove(1);
        assert!(write.starts_with("PUT "), "{write}");
        assert!(write.contains(r#""etag":"BwXhfw==""#), "{write}");
        assert!(write.contains(r#""members":["allUsers"]"#), "{write}");
        assert!(!write.contains("condition"), "{write}");
        assert!(write.contains("\"version\":3"), "{write}");
    }

    #[test]
    fn granting_the_public_binding_twice_writes_once() {
        let existing = r#"{
            "version": 3,
            "etag": "BwXhfw==",
            "bindings": [
                { "role": "roles/storage.objectViewer", "members": ["allUsers"] }
            ]
        }"#;
        let mut fake = ScriptedServer::serving(vec![json(200, existing)]);

        let granted = admin_for(&fake).grant_public_read().expect("grant");

        assert!(!granted);
        assert_eq!(fake.requests().len(), 1);
    }

    #[test]
    fn an_existing_unrelated_binding_is_preserved() {
        let existing = r#"{
            "version": 1,
            "etag": "BwXhfw==",
            "bindings": [
                { "role": "roles/storage.objectAdmin", "members": ["user:me@example.com"] }
            ]
        }"#;
        let mut fake = ScriptedServer::serving(vec![json(200, existing), json(200, "{}")]);

        admin_for(&fake).grant_public_read().expect("grant");

        let write = fake.requests().remove(1);
        assert!(write.contains("roles/storage.objectAdmin"), "{write}");
        assert!(write.contains("user:me@example.com"), "{write}");
    }

    #[test]
    fn iam_policies_are_read_at_version_three_so_conditions_are_visible() {
        let mut fake = ScriptedServer::serving(vec![json(200, r#"{"bindings":[]}"#)]);

        admin_for(&fake).get_iam_policy().expect("get");

        assert!(
            fake.requests()
                .remove(0)
                .contains("optionsRequestedPolicyVersion=3")
        );
    }

    #[test]
    fn sign_blob_round_trips_base64_through_the_iam_credentials_api() {
        let mut fake = ScriptedServer::serving(vec![json(200, r#"{"signedBlob":"c2lnbmF0dXJl"}"#)]);

        let signature = admin_for(&fake)
            .sign_blob("signer@my-project.iam.gserviceaccount.com", b"to-sign")
            .expect("sign");

        assert_eq!(signature, b"signature");
        let request = fake.requests().remove(0);
        assert!(
            request.contains("signer%40my-project.iam.gserviceaccount.com:signBlob"),
            "{request}"
        );
        assert!(request.contains(r#""payload":"dG8tc2lnbg==""#), "{request}");
    }
}
