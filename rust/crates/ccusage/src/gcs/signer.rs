//! The service account and HMAC key that let a published dashboard read the
//! data, provisioned by setup rather than by hand.
//!
//! A deployed page is static and anonymous, so the only way it can read a
//! private object is a signed URL, and signing needs a key. Asking the user to
//! mint one themselves makes `--deploy` publish a page that is guaranteed to
//! show nothing until they run a second, undocumented `gcloud` incantation, so
//! setup does it: a dedicated service account with read access to the data
//! bucket and nothing else, and one HMAC key for it.
//!
//! Dedicated is the load-bearing word. The key signs links that read usage
//! data, so the blast radius of losing it is exactly that — not the user's
//! whole project, and revoking it is deleting one account.

use std::sync::Arc;

use ccusage_objectstore::{ObjectStoreError, Result};
use serde_json::{Value, json};

use super::{Authorizer, JsonApi, RetryPolicy, encode, status_error};

const DEFAULT_IAM_ENDPOINT: &str = "https://iam.googleapis.com";
const DEFAULT_STORAGE_ENDPOINT: &str = "https://storage.googleapis.com";

/// The account id setup provisions. Fixed, so re-running setup adopts the
/// account it made last time instead of littering the project with one account
/// per run.
pub(crate) const SIGNER_ACCOUNT_ID: &str = "ccusage-dashboard";
const SIGNER_DISPLAY_NAME: &str = "ccusage dashboard share links";

/// An HMAC key, as GCS hands it over: the secret is returned exactly once, at
/// creation, and cannot be read back afterwards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HmacSecret {
    pub(crate) access_id: String,
    pub(crate) secret: String,
}

pub(crate) struct SignerAdmin {
    iam: JsonApi,
    storage: JsonApi,
    project: String,
}

impl SignerAdmin {
    pub(crate) fn new(project: &str, authorizer: Arc<dyn Authorizer>) -> Self {
        Self::with_endpoints(
            DEFAULT_IAM_ENDPOINT,
            DEFAULT_STORAGE_ENDPOINT,
            project,
            authorizer,
            RetryPolicy::default(),
        )
    }

    pub(crate) fn with_endpoints(
        iam_endpoint: &str,
        storage_endpoint: &str,
        project: &str,
        authorizer: Arc<dyn Authorizer>,
        retry: RetryPolicy,
    ) -> Self {
        Self {
            iam: JsonApi::new(iam_endpoint, Box::new(Arc::clone(&authorizer)), retry),
            storage: JsonApi::new(storage_endpoint, Box::new(authorizer), retry),
            project: project.to_string(),
        }
    }

    /// The address of the signer account, whether or not it exists yet.
    pub(crate) fn service_account_email(&self) -> String {
        format!(
            "{SIGNER_ACCOUNT_ID}@{}.iam.gserviceaccount.com",
            self.project
        )
    }

    /// Creates the signer account if it is absent, and returns its email either
    /// way.
    ///
    /// A 409 means someone — usually an earlier run of setup — created it
    /// between the read and the write, which is the outcome this wanted.
    pub(crate) fn ensure_service_account(&self) -> Result<String> {
        let email = self.service_account_email();
        if self.get_service_account(&email)? {
            return Ok(email);
        }
        let url = format!(
            "{}/v1/projects/{}/serviceAccounts",
            self.iam.endpoint(),
            encode(&self.project),
        );
        let payload = serialize(&json!({
            "accountId": SIGNER_ACCOUNT_ID,
            "serviceAccount": { "displayName": SIGNER_DISPLAY_NAME },
        }))?;
        let created = self.iam.with_retry(|| {
            let response =
                self.iam
                    .send_with_body(self.iam.agent.post(&url), &payload, "application/json")?;
            if response.status == 409 {
                return Ok(false);
            }
            if let Some(error) = status_error(&response, &email) {
                return Err(enrich(error, "iam.googleapis.com"));
            }
            Ok(true)
        })?;
        if !created && !self.get_service_account(&email)? {
            return Err(ObjectStoreError::Other {
                detail: format!("{email} was reported as existing but cannot be read"),
            });
        }
        Ok(email)
    }

    fn get_service_account(&self, email: &str) -> Result<bool> {
        let url = self.service_account_url(email);
        self.iam.with_retry(|| {
            let response = self.iam.send_without_body(self.iam.agent.get(&url))?;
            if response.status == 404 {
                return Ok(false);
            }
            if let Some(error) = status_error(&response, email) {
                return Err(enrich(error, "iam.googleapis.com"));
            }
            Ok(true)
        })
    }

    /// Deletes the signer account, reporting whether there was one. Its HMAC
    /// keys must be gone first; GCS refuses otherwise.
    pub(crate) fn delete_service_account(&self, email: &str) -> Result<bool> {
        let url = self.service_account_url(email);
        self.iam.with_retry(|| {
            let response = self.iam.send_without_body(self.iam.agent.delete(&url))?;
            if response.status == 404 {
                return Ok(false);
            }
            if let Some(error) = status_error(&response, email) {
                return Err(enrich(error, "iam.googleapis.com"));
            }
            Ok(true)
        })
    }

    fn service_account_url(&self, email: &str) -> String {
        format!(
            "{}/v1/projects/{}/serviceAccounts/{}",
            self.iam.endpoint(),
            encode(&self.project),
            encode(email),
        )
    }

    /// Mints an HMAC key for the account. The secret in the response is the
    /// only copy there will ever be.
    pub(crate) fn create_hmac_key(&self, email: &str) -> Result<HmacSecret> {
        let url = format!(
            "{}/storage/v1/projects/{}/hmacKeys?serviceAccountEmail={}",
            self.storage.endpoint(),
            encode(&self.project),
            encode(email),
        );
        self.storage.with_retry(|| {
            let response = self.storage.send_with_body(
                self.storage.agent.post(&url),
                b"",
                "application/json",
            )?;
            if let Some(error) = status_error(&response, email) {
                return Err(error);
            }
            let value: Value = serde_json::from_slice(&response.body).map_err(|error| {
                ObjectStoreError::Other {
                    detail: format!("unreadable HMAC key response: {error}"),
                }
            })?;
            let access_id = value
                .get("metadata")
                .and_then(|metadata| metadata.get("accessId"))
                .and_then(Value::as_str)
                .ok_or_else(|| ObjectStoreError::Other {
                    detail: "HMAC key response carried no accessId".to_string(),
                })?;
            let secret = value.get("secret").and_then(Value::as_str).ok_or_else(|| {
                ObjectStoreError::Other {
                    detail: "HMAC key response carried no secret".to_string(),
                }
            })?;
            Ok(HmacSecret {
                access_id: access_id.to_string(),
                secret: secret.to_string(),
            })
        })
    }

    /// The access IDs of every key the account holds, so cleanup can remove
    /// keys this machine never saw the secret of.
    pub(crate) fn hmac_access_ids(&self, email: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/storage/v1/projects/{}/hmacKeys?serviceAccountEmail={}",
            self.storage.endpoint(),
            encode(&self.project),
            encode(email),
        );
        let body = self.storage.with_retry(|| {
            let response = self
                .storage
                .send_without_body(self.storage.agent.get(&url))?;
            if response.status == 404 {
                return Ok(Vec::new());
            }
            if let Some(error) = status_error(&response, email) {
                return Err(error);
            }
            Ok(response.body)
        })?;
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let value: Value =
            serde_json::from_slice(&body).map_err(|error| ObjectStoreError::Other {
                detail: format!("unreadable HMAC key list: {error}"),
            })?;
        Ok(value
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("accessId").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Deactivates and then deletes a key, reporting whether there was one.
    /// GCS refuses to delete an active key, and an already-inactive key makes
    /// the first step a no-op rather than an error.
    pub(crate) fn delete_hmac_key(&self, access_id: &str) -> Result<bool> {
        let url = format!(
            "{}/storage/v1/projects/{}/hmacKeys/{}",
            self.storage.endpoint(),
            encode(&self.project),
            encode(access_id),
        );
        let payload = serialize(&json!({
            "accessId": access_id,
            "state": "INACTIVE",
        }))?;
        let present = self.storage.with_retry(|| {
            let response = self.storage.send_with_body(
                self.storage.agent.put(&url),
                &payload,
                "application/json",
            )?;
            if response.status == 404 {
                return Ok(false);
            }
            if let Some(error) = status_error(&response, access_id) {
                return Err(error);
            }
            Ok(true)
        })?;
        if !present {
            return Ok(false);
        }
        self.storage.with_retry(|| {
            let response = self
                .storage
                .send_without_body(self.storage.agent.delete(&url))?;
            if response.status == 404 {
                return Ok(false);
            }
            if let Some(error) = status_error(&response, access_id) {
                return Err(error);
            }
            Ok(true)
        })
    }
}

fn serialize(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| ObjectStoreError::Other {
        detail: format!("unserializable request: {error}"),
    })
}

/// A project that has never used the IAM API answers 403 with prose about the
/// API, not about permissions, and the fix is a different one.
fn enrich(error: ObjectStoreError, api: &str) -> ObjectStoreError {
    match &error {
        ObjectStoreError::Forbidden { detail, key } if detail.contains("has not been used") => {
            ObjectStoreError::Forbidden {
                detail: format!("{detail}. Enable it with `gcloud services enable {api}`"),
                key: key.clone(),
            }
        }
        _ => error,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ccusage_test_support::http_server::{ScriptedServer, json_response as json};

    use super::{super::BearerToken, *};

    fn admin_for(server: &ScriptedServer) -> SignerAdmin {
        SignerAdmin::with_endpoints(
            server.endpoint(),
            server.endpoint(),
            "my-project",
            Arc::new(BearerToken::new("test-token")),
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
    }

    #[test]
    fn an_existing_signer_account_is_adopted_rather_than_recreated() {
        let mut fake = ScriptedServer::serving(vec![json(
            200,
            r#"{"email":"ccusage-dashboard@my-project.iam.gserviceaccount.com"}"#,
        )]);

        let email = admin_for(&fake).ensure_service_account().expect("adopted");

        assert_eq!(
            email,
            "ccusage-dashboard@my-project.iam.gserviceaccount.com"
        );
        assert_eq!(fake.requests().len(), 1, "no creation was attempted");
    }

    #[test]
    fn an_absent_signer_account_is_created() {
        let mut fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"not found"}}"#),
            json(
                200,
                r#"{"email":"ccusage-dashboard@my-project.iam.gserviceaccount.com"}"#,
            ),
        ]);

        admin_for(&fake).ensure_service_account().expect("created");

        let requests = fake.requests();
        assert!(requests[1].starts_with("POST"), "{}", requests[1]);
        assert!(requests[1].contains("ccusage-dashboard"), "{}", requests[1]);
    }

    /// Two setups racing each other both want the account to exist, and it
    /// does. Only a 409 whose account then cannot be read is a real failure.
    #[test]
    fn losing_the_creation_race_still_yields_the_account() {
        let fake = ScriptedServer::serving(vec![
            json(404, r#"{"error":{"message":"not found"}}"#),
            json(409, r#"{"error":{"message":"already exists"}}"#),
            json(
                200,
                r#"{"email":"ccusage-dashboard@my-project.iam.gserviceaccount.com"}"#,
            ),
        ]);

        admin_for(&fake).ensure_service_account().expect("adopted");
    }

    #[test]
    fn a_project_without_the_iam_api_is_told_to_enable_it() {
        let fake = ScriptedServer::serving(vec![json(
            403,
            r#"{"error":{"message":"Identity and Access Management (IAM) API has not been used in project my-project"}}"#,
        )]);

        let error = admin_for(&fake).ensure_service_account().expect_err("403");

        assert!(
            format!("{error}").contains("gcloud services enable iam.googleapis.com"),
            "{error}"
        );
    }

    #[test]
    fn a_minted_key_carries_the_access_id_and_the_one_time_secret() {
        let mut fake = ScriptedServer::serving(vec![json(
            200,
            r#"{"secret":"c2VjcmV0","metadata":{"accessId":"GOOG1EXAMPLE","state":"ACTIVE"}}"#,
        )]);

        let key = admin_for(&fake)
            .create_hmac_key("signer@my-project.iam.gserviceaccount.com")
            .expect("minted");

        assert_eq!(
            key,
            HmacSecret {
                access_id: "GOOG1EXAMPLE".to_string(),
                secret: "c2VjcmV0".to_string(),
            }
        );
        assert!(
            fake.requests()[0].contains("serviceAccountEmail=signer%40my-project"),
            "{}",
            fake.requests()[0]
        );
    }

    /// A response without a secret is not a key, and treating it as one would
    /// store an unusable credential and mint links that never verify.
    #[test]
    fn a_key_response_missing_its_secret_is_an_error() {
        let fake = ScriptedServer::serving(vec![json(
            200,
            r#"{"metadata":{"accessId":"GOOG1EXAMPLE"}}"#,
        )]);

        let error = admin_for(&fake)
            .create_hmac_key("signer@my-project.iam.gserviceaccount.com")
            .expect_err("no secret");

        assert!(format!("{error}").contains("no secret"), "{error}");
    }

    #[test]
    fn deleting_a_key_deactivates_it_first() {
        let mut fake = ScriptedServer::serving(vec![
            json(200, r#"{"accessId":"GOOG1EXAMPLE","state":"INACTIVE"}"#),
            json(204, ""),
        ]);

        assert!(
            admin_for(&fake)
                .delete_hmac_key("GOOG1EXAMPLE")
                .expect("deleted")
        );

        let requests = fake.requests();
        assert!(requests[0].starts_with("PUT"), "{}", requests[0]);
        assert!(requests[1].starts_with("DELETE"), "{}", requests[1]);
    }

    #[test]
    fn a_key_that_is_already_gone_is_not_an_error() {
        let fake =
            ScriptedServer::serving(vec![json(404, r#"{"error":{"message":"no such key"}}"#)]);

        assert!(
            !admin_for(&fake)
                .delete_hmac_key("GOOG1EXAMPLE")
                .expect("absent")
        );
    }

    #[test]
    fn listing_keys_reads_the_access_ids_and_tolerates_none() {
        let empty = ScriptedServer::serving(vec![json(200, r#"{}"#)]);
        assert_eq!(
            admin_for(&empty)
                .hmac_access_ids("signer@my-project.iam.gserviceaccount.com")
                .expect("empty"),
            Vec::<String>::new()
        );

        let listed = ScriptedServer::serving(vec![json(
            200,
            r#"{"items":[{"accessId":"GOOG1ONE"},{"accessId":"GOOG1TWO"}]}"#,
        )]);
        assert_eq!(
            admin_for(&listed)
                .hmac_access_ids("signer@my-project.iam.gserviceaccount.com")
                .expect("listed"),
            vec!["GOOG1ONE".to_string(), "GOOG1TWO".to_string()]
        );
    }

    #[test]
    fn deleting_an_absent_service_account_is_not_an_error() {
        let fake = ScriptedServer::serving(vec![json(404, r#"{"error":{"message":"gone"}}"#)]);

        assert!(
            !admin_for(&fake)
                .delete_service_account("signer@my-project.iam.gserviceaccount.com")
                .expect("absent")
        );
    }
}
