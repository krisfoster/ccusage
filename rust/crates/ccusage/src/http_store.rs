//! An `ObjectStore` that talks to a `ccusage sync serve` hub over HTTP.
//!
//! This is the sandbox side of local sync: the hub owns a directory on the host, and this client
//! reaches it through whatever network path the sandbox has. The wire protocol is deliberately a
//! thin mapping of the four store verbs, so every guarantee the sync engine relies on (atomic
//! preconditions, conflicts that are never retried) lives in one place, the hub's `FsStore`.
//!
//! ```text
//! GET    /v1/objects/<key>            200 body | 404 {"error":{"code":"object_not_found"}}
//! PUT    /v1/objects/<key>            200      | 412 on a failed precondition
//! DELETE /v1/objects/<key>            204      | 412 | 403 when the hub refuses deletes
//! GET    /v1/list?prefix=<prefix>     200 {"objects":[{key,generation,etag,size,updatedMs}]}
//! GET    /v1/health                   200 {"protocol":1,"prefix":"..."}
//! ```
//!
//! Preconditions travel as `If-None-Match: *` (create only) and `X-Ccusage-If-Generation-Match`.
//! Every response about an object carries `X-Ccusage-Generation`, `ETag` and
//! `X-Ccusage-Updated-Ms`. Requests carry `Authorization: Bearer <token>`.

use ccusage_objectstore::{Key, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, Result};
use serde_json::Value;

use crate::gcs::{BearerToken, JsonApi, Response, RetryPolicy, encode, header, status_error};

pub(crate) const PROTOCOL_VERSION: u64 = 1;
pub(crate) const OBJECTS_PATH: &str = "/v1/objects/";
pub(crate) const LIST_PATH: &str = "/v1/list";
pub(crate) const HEALTH_PATH: &str = "/v1/health";
pub(crate) const GENERATION_HEADER: &str = "x-ccusage-generation";
pub(crate) const UPDATED_HEADER: &str = "x-ccusage-updated-ms";
pub(crate) const IF_GENERATION_HEADER: &str = "x-ccusage-if-generation-match";
/// Distinguishes "the hub has no such object" from "this URL is not a hub": a 404 from anything
/// else must not be read as an empty store, because setup would then try to create one.
pub(crate) const OBJECT_NOT_FOUND: &str = "object_not_found";
/// The environment variable the bearer token is read from. The sandbox only ever holds a
/// placeholder; the sbx proxy swaps the real value into the header.
pub(crate) const TOKEN_ENV: &str = "CCUSAGE_SYNC_TOKEN";

pub(crate) struct HttpStore {
    api: JsonApi,
}

impl HttpStore {
    pub(crate) fn new(url: &str, token: &str) -> Self {
        Self::with_retry(url, token, RetryPolicy::default())
    }

    pub(crate) fn with_retry(url: &str, token: &str, retry: RetryPolicy) -> Self {
        Self {
            api: JsonApi::new(url, Box::new(BearerToken::new(token)), retry),
        }
    }

    /// Confirms the URL is a hub that speaks this protocol and accepts the token, and returns
    /// the key prefix the hub serves.
    pub(crate) fn ping(&self) -> Result<String> {
        let url = format!("{}{HEALTH_PATH}", self.api.endpoint());
        self.api.with_retry(|| {
            let response = self.api.send_without_body(self.api.agent.get(&url))?;
            if let Some(error) = self.error_for(&response, HEALTH_PATH) {
                return Err(error);
            }
            let document: Value =
                serde_json::from_slice(&response.body).map_err(|_| not_a_hub(&self.api))?;
            if document.get("protocol").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
                return Err(ObjectStoreError::Other {
                    detail: format!(
                        "the hub at {} speaks a different protocol version; upgrade ccusage on \
                         both sides",
                        self.api.endpoint()
                    ),
                });
            }
            document
                .get("prefix")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| not_a_hub(&self.api))
        })
    }

    fn object_url(&self, key: &str) -> String {
        format!("{}{OBJECTS_PATH}{key}", self.api.endpoint())
    }

    fn error_for(&self, response: &Response, key: &str) -> Option<ObjectStoreError> {
        status_error(response, key).map(|error| error.with_credential_source(TOKEN_ENV))
    }
}

fn not_a_hub(api: &JsonApi) -> ObjectStoreError {
    ObjectStoreError::Other {
        detail: format!(
            "{} does not look like a ccusage sync hub. Check the URL, and that 'ccusage sync \
             serve' is running on the host",
            api.endpoint()
        ),
    }
}

fn meta_from_headers(key: &str, headers: &[(String, String)], size: usize) -> ObjectMeta {
    ObjectMeta {
        key: key.to_string(),
        generation: header(headers, GENERATION_HEADER).map(str::to_string),
        etag: header(headers, "etag").map(str::to_string),
        size: size as u64,
        updated_ms: header(headers, UPDATED_HEADER).and_then(|value| value.parse().ok()),
    }
}

fn is_object_not_found(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .map(|code| code == OBJECT_NOT_FOUND)
        })
        .unwrap_or(false)
}

impl ObjectStore for HttpStore {
    fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, ObjectMeta)>> {
        let url = self.object_url(key.path());
        self.api.with_retry(|| {
            let response = self.api.send_without_body(self.api.agent.get(&url))?;
            if response.status == 404 {
                return if is_object_not_found(&response.body) {
                    Ok(None)
                } else {
                    Err(not_a_hub(&self.api))
                };
            }
            if let Some(error) = self.error_for(&response, key.path()) {
                return Err(error);
            }
            let meta = meta_from_headers(key.path(), &response.headers, response.body.len());
            Ok(Some((response.body, meta)))
        })
    }

    fn put(
        &self,
        key: &Key,
        body: &[u8],
        content_type: &str,
        precondition: &Precondition,
    ) -> Result<ObjectMeta> {
        let url = self.object_url(key.path());
        self.api.with_retry(|| {
            let mut request = self.api.agent.put(&url);
            match precondition {
                Precondition::None => {}
                Precondition::IfAbsent => request = request.header("if-none-match", "*"),
                Precondition::IfGenerationMatch(generation) => {
                    request = request.header(IF_GENERATION_HEADER, generation);
                }
            }
            let response = self.api.send_with_body(request, body, content_type)?;
            if let Some(error) = self.error_for(&response, key.path()) {
                return Err(error);
            }
            Ok(meta_from_headers(key.path(), &response.headers, body.len()))
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let url = format!(
            "{}{LIST_PATH}?prefix={}",
            self.api.endpoint(),
            encode(prefix)
        );
        self.api.with_retry(|| {
            let response = self.api.send_without_body(self.api.agent.get(&url))?;
            if let Some(error) = self.error_for(&response, prefix) {
                return Err(error);
            }
            let document: Value =
                serde_json::from_slice(&response.body).map_err(|_| ObjectStoreError::Other {
                    detail: format!("unreadable list response for '{prefix}'"),
                })?;
            let objects = document
                .get("objects")
                .and_then(Value::as_array)
                .ok_or_else(|| not_a_hub(&self.api))?;
            Ok(objects
                .iter()
                .filter_map(|object| {
                    Some(ObjectMeta {
                        key: object.get("key")?.as_str()?.to_string(),
                        generation: object
                            .get("generation")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        etag: object
                            .get("etag")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        size: object.get("size").and_then(Value::as_u64).unwrap_or(0),
                        updated_ms: object.get("updatedMs").and_then(Value::as_i64),
                    })
                })
                .collect())
        })
    }

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()> {
        let url = self.object_url(key.path());
        self.api.with_retry(|| {
            let mut request = self.api.agent.delete(&url);
            if let Precondition::IfGenerationMatch(generation) = precondition {
                request = request.header(IF_GENERATION_HEADER, generation);
            }
            let response = self.api.send_without_body(request)?;
            if response.status == 404 && is_object_not_found(&response.body) {
                return Ok(());
            }
            match self.error_for(&response, key.path()) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}
