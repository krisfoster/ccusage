//! The GCS implementation of [`ObjectStore`].
//!
//! Lives in the binary for the same reason `http.rs` does: `ureq` and its TLS
//! stack must not become dependencies of the crates every adapter builds
//! against. `ccusage-objectstore` owns the keys, the preconditions and the
//! error taxonomy; this module owns the wire format and the retry policy.
//!
//! Requests go to the JSON API with a bearer token. The HMAC signer in
//! `ccusage-objectstore` is not used here — it signs the XML API, which is
//! what share links and any future S3-compatible provider need.
//!
//! Nothing outside the tests calls this yet; `sync setup` in Phase 2 is the
//! first caller, so the module allows dead code until then.
#![allow(dead_code)]

use std::{
    io::Read as _,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ccusage_objectstore::{Key, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, Result};

const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const MAX_OBJECT_BYTES: u64 = 32 * 1024 * 1024;
const LIST_PAGE_SIZE: u32 = 1000;

/// Supplies the `Authorization` header for each request.
///
/// Credential resolution (ADC, `gcloud`, service accounts) is P1-05; this trait
/// is the seam it plugs into, and the reason `GcsStore` can be tested against a
/// local listener with a static token.
pub(crate) trait Authorizer: Send + Sync {
    /// Returns the header value, or `None` for an anonymous request.
    fn authorization(&self) -> Result<Option<String>>;
}

/// A fixed OAuth2 bearer token.
pub(crate) struct BearerToken {
    value: String,
}

impl BearerToken {
    pub(crate) fn new(token: &str) -> Self {
        Self {
            value: format!("Bearer {token}"),
        }
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BearerToken")
            .finish_non_exhaustive()
    }
}

impl Authorizer for BearerToken {
    fn authorization(&self) -> Result<Option<String>> {
        Ok(Some(self.value.clone()))
    }
}

/// Unauthenticated access, for public dashboard assets.
pub(crate) struct Anonymous;

impl Authorizer for Anonymous {
    fn authorization(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// How often a transport failure is retried, and how long the waits are.
///
/// `base_delay` is zero in tests; a zero delay also disables the jitter, which
/// keeps retry tests deterministic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub(crate) max_attempts: u32,
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    fn backoff(&self, attempt: u32, retry_after_ms: Option<u64>) -> Duration {
        if let Some(retry_after_ms) = retry_after_ms {
            return Duration::from_millis(retry_after_ms).min(self.max_delay);
        }
        if self.base_delay.is_zero() {
            return Duration::ZERO;
        }
        let exponential = self.base_delay.saturating_mul(1u32 << attempt.min(16));
        let capped = exponential.min(self.max_delay);
        // Full jitter. Two machines that collide on a CAS retry the same object,
        // so retrying in lockstep would just reproduce the collision.
        let jitter = jitter_fraction();
        capped.mul_f64(0.5 + jitter / 2.0)
    }
}

/// A cheap source of jitter in `[0, 1)`; this does not need to be uniform or
/// unpredictable, only different between two processes that started together.
fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::from(elapsed.subsec_nanos()));
    f64::from(u32::try_from(nanos % 1_000).unwrap_or(0)) / 1_000.0
}

pub(crate) struct GcsStore {
    agent: ureq::Agent,
    endpoint: String,
    bucket: String,
    authorizer: Box<dyn Authorizer>,
    retry: RetryPolicy,
}

impl GcsStore {
    /// The production constructor. `sync setup` (P2) is its first caller.
    pub(crate) fn new(bucket: &str, authorizer: Box<dyn Authorizer>) -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT, bucket, authorizer, RetryPolicy::default())
    }

    pub(crate) fn with_endpoint(
        endpoint: &str,
        bucket: &str,
        authorizer: Box<dyn Authorizer>,
        retry: RetryPolicy,
    ) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))
            // Statuses are mapped onto the error taxonomy here, and 404 and 412
            // are ordinary control flow for a store with preconditions.
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self {
            agent,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket: bucket.to_string(),
            authorizer,
            retry,
        }
    }

    fn object_url(&self, key: &str, suffix: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}{suffix}",
            self.endpoint,
            encode(&self.bucket),
            encode(key),
        )
    }

    /// Runs `attempt` until it succeeds, fails non-retryably, or runs out of
    /// attempts. A [`ObjectStoreError::Conflict`] is never retried: the caller
    /// has to re-read and re-merge, and repeating the same body would either
    /// fail identically or overwrite a write it never saw.
    fn with_retry<T>(&self, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempts = 0;
        loop {
            match attempt() {
                Ok(value) => return Ok(value),
                Err(error) => {
                    attempts += 1;
                    if attempts >= self.retry.max_attempts || !error.is_transport_retryable() {
                        return Err(error);
                    }
                    let delay = self.retry.backoff(attempts - 1, error.retry_after_ms());
                    if !delay.is_zero() {
                        thread::sleep(delay);
                    }
                }
            }
        }
    }

    fn get_like(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) -> Result<Response> {
        let mut request = request.header("accept", "application/json");
        if let Some(authorization) = self.authorizer.authorization()? {
            request = request.header("authorization", &authorization);
        }
        finish(request.call())
    }

    fn post_body(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithBody>,
        body: &[u8],
        content_type: &str,
    ) -> Result<Response> {
        let mut request = request
            .header("accept", "application/json")
            .header("content-type", content_type);
        if let Some(authorization) = self.authorizer.authorization()? {
            request = request.header("authorization", &authorization);
        }
        finish(request.send(body))
    }
}

/// A response reduced to the parts the store reads. Deliberately excludes the
/// request, whose `authorization` header must never reach a log or an error.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn finish(
    response: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Response> {
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return Err(ObjectStoreError::Network {
                detail: error.to_string(),
            });
        }
    };
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    let mut body = Vec::new();
    response
        .body_mut()
        .with_config()
        .limit(MAX_OBJECT_BYTES)
        .reader()
        .read_to_end(&mut body)
        .map_err(|error| ObjectStoreError::Network {
            detail: error.to_string(),
        })?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| value.as_str())
}

/// Maps a non-2xx response onto the error taxonomy, keeping the server's
/// message as detail but never the request headers, which carry the token.
fn status_error(response: &Response, key: &str) -> Option<ObjectStoreError> {
    if (200..300).contains(&response.status) {
        return None;
    }
    let detail = error_message(&response.body);
    let error = ObjectStoreError::from_status(response.status, key, &detail);
    if let ObjectStoreError::RateLimited { .. } = error {
        let retry_after_ms = header(&response.headers, "retry-after")
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds * 1_000);
        return Some(ObjectStoreError::RateLimited { retry_after_ms });
    }
    Some(error)
}

fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

/// Percent-encodes a path segment, including `/`, which is what the JSON API
/// wants for an object name embedded in the path.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn precondition_query(precondition: &Precondition) -> Option<String> {
    match precondition {
        Precondition::None => None,
        Precondition::IfAbsent => Some("ifGenerationMatch=0".to_string()),
        Precondition::IfGenerationMatch(generation) => {
            Some(format!("ifGenerationMatch={}", encode(generation)))
        }
    }
}

fn meta_from_resource(key: &str, body: &[u8]) -> Result<ObjectMeta> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| ObjectStoreError::Other {
            detail: format!("unreadable object resource for {key}: {error}"),
        })?;
    Ok(meta_from_value(key, &value))
}

fn meta_from_value(key: &str, value: &serde_json::Value) -> ObjectMeta {
    ObjectMeta {
        key: value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(key)
            .to_string(),
        generation: value
            .get("generation")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        etag: value
            .get("etag")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        size: value
            .get("size")
            .and_then(serde_json::Value::as_str)
            .and_then(|size| size.parse().ok())
            .unwrap_or(0),
        updated_ms: value
            .get("updated")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc3339_ms),
    }
}

/// Parses the subset of RFC 3339 the JSON API emits (`2024-05-06T07:08:09.123Z`).
fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let (date, rest) = value.split_once('T')?;
    let time = rest.trim_end_matches('Z');
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    Some(
        days_from_civil(year, month, day) * 86_400_000
            + (hour * 3_600 + minute * 60 + second) * 1_000
            + millis,
    )
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

impl ObjectStore for GcsStore {
    fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, ObjectMeta)>> {
        let url = self.object_url(key.path(), "?alt=media");
        self.with_retry(|| {
            let response = self.get_like(self.agent.get(&url))?;
            if response.status == 404 {
                return Ok(None);
            }
            if let Some(error) = status_error(&response, key.path()) {
                return Err(error);
            }
            let meta = ObjectMeta {
                key: key.path().to_string(),
                generation: header(&response.headers, "x-goog-generation").map(str::to_string),
                etag: header(&response.headers, "etag").map(str::to_string),
                size: response.body.len() as u64,
                updated_ms: None,
            };
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
        let mut query = format!("uploadType=media&name={}", encode(key.path()));
        if let Some(condition) = precondition_query(precondition) {
            query.push('&');
            query.push_str(&condition);
        }
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?{query}",
            self.endpoint,
            encode(&self.bucket),
        );
        self.with_retry(|| {
            let response = self.post_body(self.agent.post(&url), body, content_type)?;
            if let Some(error) = status_error(&response, key.path()) {
                return Err(error);
            }
            meta_from_resource(key.path(), &response.body)
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut page_token: Option<String> = None;
        let mut objects = Vec::new();
        loop {
            let mut query = format!("prefix={}&maxResults={LIST_PAGE_SIZE}", encode(prefix));
            if let Some(token) = page_token.as_ref() {
                query.push_str(&format!("&pageToken={}", encode(token)));
            }
            let url = format!(
                "{}/storage/v1/b/{}/o?{query}",
                self.endpoint,
                encode(&self.bucket),
            );
            let page = self.with_retry(|| {
                let response = self.get_like(self.agent.get(&url))?;
                if let Some(error) = status_error(&response, prefix) {
                    return Err(error);
                }
                serde_json::from_slice::<serde_json::Value>(&response.body).map_err(|error| {
                    ObjectStoreError::Other {
                        detail: format!("unreadable list response for {prefix}: {error}"),
                    }
                })
            })?;
            if let Some(items) = page.get("items").and_then(serde_json::Value::as_array) {
                objects.extend(items.iter().map(|item| meta_from_value(prefix, item)));
            }
            page_token = page
                .get("nextPageToken")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                return Ok(objects);
            }
        }
    }

    fn delete(&self, key: &Key, precondition: &Precondition) -> Result<()> {
        let suffix = precondition_query(precondition)
            .map(|condition| format!("?{condition}"))
            .unwrap_or_default();
        let url = self.object_url(key.path(), &suffix);
        self.with_retry(|| {
            let response = self.get_like(self.agent.delete(&url))?;
            if response.status == 404 {
                return Ok(());
            }
            if let Some(error) = status_error(&response, key.path()) {
                return Err(error);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Anonymous, BearerToken, GcsStore, RetryPolicy, parse_rfc3339_ms};
    use ccusage_objectstore::{KeySpace, ObjectStore, ObjectStoreError, Precondition};
    use ccusage_test_support::http_server::{ScriptedServer, json_response as json, response};
    use std::time::Duration;

    fn store_for(server: &ScriptedServer) -> GcsStore {
        GcsStore::with_endpoint(
            server.endpoint(),
            "usage-bucket",
            Box::new(BearerToken::new("test-token")),
            no_retry(),
        )
    }

    /// Retries still happen; only the sleeping between them is removed.
    fn no_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    const OBJECT_RESOURCE: &str = r#"{"name":"ccusage/manifest.json","generation":"1712345678","etag":"CAE=","size":"12","updated":"2024-05-06T07:08:09.123Z"}"#;

    fn keys() -> KeySpace {
        KeySpace::new("ccusage").expect("key space")
    }

    #[test]
    fn get_returns_the_body_and_the_generation_the_server_reported() {
        let mut fake = ScriptedServer::serving(vec![response(
            200,
            "OK",
            "x-goog-generation: 1712345678\r\netag: \"CAE=\"\r\n",
            "{\"hello\":1}",
        )]);
        let store = store_for(&fake);

        let (body, meta) = store
            .get(&keys().manifest())
            .expect("get")
            .expect("object present");

        assert_eq!(body, b"{\"hello\":1}");
        assert_eq!(meta.generation.as_deref(), Some("1712345678"));
        let request = fake.requests().remove(0);
        assert!(
            request.contains("/storage/v1/b/usage-bucket/o/ccusage%2Fmanifest.json?alt=media"),
            "unexpected request line: {request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        );
    }

    #[test]
    fn a_missing_object_is_absence_rather_than_an_error() {
        let fake =
            ScriptedServer::serving(vec![json(404, r#"{"error":{"message":"No such object"}}"#)]);
        let store = store_for(&fake);

        assert!(store.get(&keys().manifest()).expect("get").is_none());
    }

    #[test]
    fn put_sends_the_create_if_absent_precondition_as_generation_zero() {
        let mut fake = ScriptedServer::serving(vec![json(200, OBJECT_RESOURCE)]);
        let store = store_for(&fake);

        let meta = store
            .put(
                &keys().manifest(),
                b"{}",
                "application/json",
                &Precondition::IfAbsent,
            )
            .expect("put");

        assert_eq!(meta.generation.as_deref(), Some("1712345678"));
        assert_eq!(meta.size, 12);
        let request = fake.requests().remove(0);
        assert!(request.contains("uploadType=media"), "{request}");
        assert!(request.contains("ifGenerationMatch=0"), "{request}");
        assert!(
            request.contains("name=ccusage%2Fmanifest.json"),
            "{request}"
        );
    }

    #[test]
    fn put_sends_the_expected_generation_for_a_compare_and_swap() {
        let mut fake = ScriptedServer::serving(vec![json(200, OBJECT_RESOURCE)]);
        let store = store_for(&fake);

        store
            .put(
                &keys().manifest(),
                b"{}",
                "application/json",
                &Precondition::IfGenerationMatch("1712345678".to_string()),
            )
            .expect("put");

        assert!(
            fake.requests()
                .remove(0)
                .contains("ifGenerationMatch=1712345678")
        );
    }

    #[test]
    fn a_failed_precondition_is_a_conflict_and_is_never_retried() {
        let mut fake = ScriptedServer::serving(vec![
            json(412, r#"{"error":{"message":"Precondition Failed"}}"#),
            json(200, OBJECT_RESOURCE),
        ]);
        let store = store_for(&fake);

        let error = store
            .put(
                &keys().manifest(),
                b"{}",
                "application/json",
                &Precondition::IfGenerationMatch("1".to_string()),
            )
            .expect_err("412 must surface");

        assert!(matches!(error, ObjectStoreError::Conflict { .. }));
        assert_eq!(
            fake.requests().len(),
            1,
            "retrying a 412 would overwrite a write we never read"
        );
    }

    #[test]
    fn a_throttled_write_is_retried_until_it_succeeds() {
        let mut fake = ScriptedServer::serving(vec![
            json(429, r#"{"error":{"message":"slow down"}}"#),
            json(503, r#"{"error":{"message":"backend"}}"#),
            json(200, OBJECT_RESOURCE),
        ]);
        let store = store_for(&fake);

        store
            .put(
                &keys().manifest(),
                b"{}",
                "application/json",
                &Precondition::None,
            )
            .expect("retries should reach the 200");

        assert_eq!(fake.requests().len(), 3);
    }

    #[test]
    fn retries_stop_at_the_attempt_limit() {
        let mut fake = ScriptedServer::serving(vec![json(503, "{}"); 4]);
        let store = store_for(&fake);

        let error = store
            .get(&keys().manifest())
            .expect_err("a persistent 503 must surface");

        assert!(matches!(
            error,
            ObjectStoreError::Server { status: 503, .. }
        ));
        assert_eq!(fake.requests().len(), 4);
    }

    #[test]
    fn a_403_names_the_object_and_keeps_the_server_message() {
        let fake = ScriptedServer::serving(vec![json(
            403,
            r#"{"error":{"message":"does not have storage.objects.get access"}}"#,
        )]);
        let store = store_for(&fake);

        let error = store.get(&keys().manifest()).expect_err("403 must surface");

        let rendered = error.to_string();
        assert!(rendered.contains("ccusage/manifest.json"), "{rendered}");
        assert!(rendered.contains("storage.objects.get"), "{rendered}");
        assert!(
            !rendered.contains("test-token"),
            "the token must never reach an error"
        );
    }

    #[test]
    fn list_follows_page_tokens() {
        let mut fake = ScriptedServer::serving(vec![
            json(
                200,
                r#"{"items":[{"name":"ccusage/a.json","generation":"1","size":"3"}],"nextPageToken":"page-2"}"#,
            ),
            json(
                200,
                r#"{"items":[{"name":"ccusage/b.json","generation":"2","size":"4"}]}"#,
            ),
        ]);
        let store = store_for(&fake);

        let objects = store.list("ccusage/").expect("list");

        assert_eq!(
            objects
                .iter()
                .map(|object| object.key.as_str())
                .collect::<Vec<_>>(),
            vec!["ccusage/a.json", "ccusage/b.json"]
        );
        let requests = fake.requests();
        assert!(requests[0].contains("prefix=ccusage%2F"), "{}", requests[0]);
        assert!(requests[1].contains("pageToken=page-2"), "{}", requests[1]);
    }

    #[test]
    fn delete_is_idempotent_and_can_be_conditional() {
        let mut fake = ScriptedServer::serving(vec![
            response(204, "No Content", "", ""),
            json(404, r#"{"error":{"message":"No such object"}}"#),
        ]);
        let store = store_for(&fake);

        store
            .delete(
                &keys().manifest(),
                &Precondition::IfGenerationMatch("7".to_string()),
            )
            .expect("conditional delete");
        store
            .delete(&keys().manifest(), &Precondition::None)
            .expect("deleting an absent object is not an error");

        let requests = fake.requests();
        assert!(
            requests[0].contains("ifGenerationMatch=7"),
            "{}",
            requests[0]
        );
        assert!(
            !requests[1].contains("ifGenerationMatch"),
            "{}",
            requests[1]
        );
    }

    #[test]
    fn an_anonymous_store_sends_no_authorization_header() {
        let mut fake = ScriptedServer::serving(vec![response(200, "OK", "", "asset")]);
        let store = GcsStore::with_endpoint(
            fake.endpoint(),
            "usage-bucket",
            Box::new(Anonymous),
            no_retry(),
        );

        store
            .get(&keys().dashboard_asset("index.html").expect("asset key"))
            .expect("public read");

        assert!(
            !fake.requests()[0]
                .to_ascii_lowercase()
                .contains("authorization:")
        );
    }

    #[test]
    fn parses_the_json_api_timestamp() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2024-05-06T07:08:09.123Z"),
            Some(1_714_979_289_123)
        );
        assert_eq!(parse_rfc3339_ms("not a timestamp"), None);
    }
}
