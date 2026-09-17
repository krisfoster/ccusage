//! Google credential resolution for cloud sync.
//!
//! Lives in the binary for the same reason `gcs` does: refreshing a token is an
//! HTTP call, and `ccusage-objectstore` must stay free of a TLS stack.
//!
//! The ladder is deliberately short (DR-05 cut the built-in OAuth client, so
//! there is no browser flow to own here) and is tried in this order:
//!
//! 1. `CCUSAGE_SYNC_ACCESS_TOKEN` — a token the caller already has, for CI.
//! 2. `CCUSAGE_SYNC_HMAC_ACCESS_ID` + `CCUSAGE_SYNC_HMAC_SECRET` — headless.
//! 3. `GOOGLE_APPLICATION_CREDENTIALS` — an explicitly pointed-at file.
//! 4. The ADC well-known file written by `gcloud auth application-default login`.
//! 5. `gcloud auth print-access-token` — covers a user who ran plain
//!    `gcloud auth login` and never created ADC.
//! 6. The GCE/Cloud Run metadata server.
//!
//! When every rung fails the error names each one that was tried, because the
//! usual support question is "which credential did it even look at". Secrets
//! never appear in those errors, in `Debug`, or in a log line.
//!
//! Nothing outside the tests calls this yet; `sync setup` in Phase 2 is the
//! first caller, so the module allows dead code until then.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    fmt,
    io::Read as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ccusage_cli::SyncAuthMode;
use ccusage_objectstore::{HmacKey, ObjectStoreError, Result};

use crate::gcs::Authorizer;

/// Google's ADC token endpoint. Overridable only in tests.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// The metadata server, addressed by IP so a broken resolver cannot hang us.
const DEFAULT_METADATA_ENDPOINT: &str = "http://169.254.169.254";
/// A token is refreshed this far before it expires, so an in-flight request
/// cannot be the one that discovers the expiry.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);
/// Applied to the token endpoint and, more importantly, to the metadata server,
/// which silently blackholes rather than refusing off GCE.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOKEN_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 1024 * 1024;

/// Where a credential came from. Carried into errors so a failure names the
/// file or command that produced it, never its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    EnvAccessToken,
    EnvHmac,
    CredentialFile(PathBuf),
    ApplicationDefault(PathBuf),
    GcloudCli,
    MetadataServer,
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvAccessToken => write!(formatter, "CCUSAGE_SYNC_ACCESS_TOKEN"),
            Self::EnvHmac => write!(formatter, "CCUSAGE_SYNC_HMAC_ACCESS_ID/SECRET"),
            Self::CredentialFile(path) => {
                write!(
                    formatter,
                    "GOOGLE_APPLICATION_CREDENTIALS ({})",
                    path.display()
                )
            }
            Self::ApplicationDefault(path) => {
                write!(
                    formatter,
                    "application default credentials ({})",
                    path.display()
                )
            }
            Self::GcloudCli => write!(formatter, "gcloud auth print-access-token"),
            Self::MetadataServer => write!(formatter, "GCE metadata server"),
        }
    }
}

/// How a bearer token is obtained, once a source has been chosen. HMAC is the
/// odd one out: it signs XML API requests directly and yields no token, so it
/// is carried here but consumed by the signer rather than by `GcsStore`.
enum TokenKind {
    Static(String),
    RefreshToken {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    Gcloud,
    Metadata {
        endpoint: String,
    },
    Hmac(HmacKey),
}

/// Deliberately hand-written: the derived form would print refresh tokens.
impl fmt::Debug for TokenKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Static(_) => "Static",
            Self::RefreshToken { .. } => "RefreshToken",
            Self::Gcloud => "Gcloud",
            Self::Metadata { .. } => "Metadata",
            Self::Hmac(_) => "Hmac",
        };
        write!(formatter, "{name}(<redacted>)")
    }
}

struct CachedToken {
    value: String,
    expires_at: Option<Instant>,
}

/// A resolved credential, plus the token it last minted.
pub(crate) struct Credentials {
    source: CredentialSource,
    kind: TokenKind,
    cached: Mutex<Option<CachedToken>>,
    agent: ureq::Agent,
    runner: CommandRunner,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("source", &self.source)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Runs `gcloud`, or whatever a test substitutes for it.
type CommandRunner = Arc<dyn Fn() -> std::result::Result<String, String> + Send + Sync>;

impl Credentials {
    pub(crate) fn source(&self) -> &CredentialSource {
        &self.source
    }

    /// The HMAC key, when the resolved credential is one. Signed share links
    /// take this path; `GcsStore` does not.
    pub(crate) fn hmac_key(&self) -> Option<&HmacKey> {
        match &self.kind {
            TokenKind::Hmac(key) => Some(key),
            _ => None,
        }
    }

    /// A valid access token, minted or served from cache.
    pub(crate) fn access_token(&self) -> Result<String> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(token) = cached.as_ref()
            && token
                .expires_at
                .is_none_or(|expiry| Instant::now() + EXPIRY_SKEW < expiry)
        {
            return Ok(token.value.clone());
        }
        let minted = self.mint()?;
        let value = minted.value.clone();
        *cached = Some(minted);
        Ok(value)
    }

    fn mint(&self) -> Result<CachedToken> {
        match &self.kind {
            TokenKind::Static(value) => Ok(CachedToken {
                value: value.clone(),
                expires_at: None,
            }),
            TokenKind::RefreshToken {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
            } => {
                let form = format!(
                    "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
                    form_encode(client_id),
                    form_encode(client_secret),
                    form_encode(refresh_token),
                );
                let response = self
                    .agent
                    .post(token_uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .send(form.as_bytes());
                self.token_from_response(response)
            }
            TokenKind::Gcloud => {
                let token = (self.runner)().map_err(|detail| self.unauthenticated(&detail))?;
                let token = token.trim();
                if token.is_empty() {
                    return Err(self.unauthenticated("printed an empty token"));
                }
                Ok(CachedToken {
                    // gcloud does not report an expiry, so the token is treated
                    // as short-lived and re-fetched each hour.
                    value: token.to_string(),
                    expires_at: Some(Instant::now() + Duration::from_secs(3_600)),
                })
            }
            TokenKind::Metadata { endpoint } => {
                let url = format!(
                    "{endpoint}/computeMetadata/v1/instance/service-accounts/default/token"
                );
                let response = self
                    .agent
                    .get(&url)
                    .header("metadata-flavor", "Google")
                    .call();
                self.token_from_response(response)
            }
            TokenKind::Hmac(_) => Err(self.unauthenticated(
                "HMAC credentials sign XML API requests and cannot mint an access token",
            )),
        }
    }

    fn token_from_response(
        &self,
        response: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<CachedToken> {
        let mut response = response.map_err(|error| ObjectStoreError::Network {
            detail: format!("token request to {} failed: {error}", self.source),
        })?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_TOKEN_RESPONSE_BYTES)
            .read_to_end(&mut body)
            .map_err(|error| ObjectStoreError::Network {
                detail: format!("unreadable token response from {}: {error}", self.source),
            })?;
        if !(200..300).contains(&status) {
            // The body of a token error carries `error_description`, which is
            // the actionable part ("Token has been expired or revoked"), and
            // never the credential itself.
            return Err(self.unauthenticated(&format!(
                "token endpoint returned {status}: {}",
                token_error(&body)
            )));
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| ObjectStoreError::Other {
                detail: format!("unparseable token response from {}: {error}", self.source),
            })?;
        let value = parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| self.unauthenticated("token response carried no access_token"))?;
        let expires_in = parsed
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .map(Duration::from_secs);
        Ok(CachedToken {
            value: value.to_string(),
            expires_at: expires_in.map(|lifetime| Instant::now() + lifetime),
        })
    }

    fn unauthenticated(&self, detail: &str) -> ObjectStoreError {
        ObjectStoreError::Unauthenticated {
            source: self.source.to_string(),
            detail: detail.to_string(),
        }
    }
}

impl Authorizer for Credentials {
    fn authorization(&self) -> Result<Option<String>> {
        Ok(Some(format!("Bearer {}", self.access_token()?)))
    }
}

/// Walks the credential ladder. Every input it reads — environment, home
/// directory, `gcloud`, and the two endpoints — is injectable, so the tests
/// never depend on the machine they run on.
pub(crate) struct Resolver {
    env: HashMap<String, String>,
    home: Option<PathBuf>,
    token_uri: String,
    metadata_endpoint: String,
    probe_metadata: bool,
    runner: CommandRunner,
    mode: SyncAuthMode,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    pub(crate) fn new() -> Self {
        Self {
            env: std::env::vars().collect(),
            home: ccusage_core::home::home_dir(),
            token_uri: DEFAULT_TOKEN_URI.to_string(),
            metadata_endpoint: DEFAULT_METADATA_ENDPOINT.to_string(),
            // Off unless something says we are on GCE: probing costs a
            // connection attempt to a link-local address on every sync.
            probe_metadata: false,
            runner: Arc::new(run_gcloud),
            mode: SyncAuthMode::Auto,
        }
    }

    /// Narrows the ladder to the rungs the user asked for. An explicit mode that
    /// finds nothing fails rather than quietly authenticating as somebody else.
    pub(crate) fn with_mode(mut self, mode: SyncAuthMode) -> Self {
        self.mode = mode;
        self
    }

    pub(crate) fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub(crate) fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    pub(crate) fn with_token_uri(mut self, token_uri: &str) -> Self {
        self.token_uri = token_uri.to_string();
        self
    }

    pub(crate) fn with_metadata_endpoint(mut self, endpoint: &str) -> Self {
        self.metadata_endpoint = endpoint.to_string();
        self.probe_metadata = true;
        self
    }

    pub(crate) fn with_gcloud(mut self, runner: CommandRunner) -> Self {
        self.runner = runner;
        self
    }

    /// Returns the first credential that resolves, or an error naming every
    /// rung that was tried.
    pub(crate) fn resolve(self) -> Result<Credentials> {
        let mut tried: Vec<String> = Vec::new();
        let env_rungs = matches!(self.mode, SyncAuthMode::Auto | SyncAuthMode::Hmac);
        let adc_rungs = matches!(self.mode, SyncAuthMode::Auto | SyncAuthMode::Adc);

        if let Some(token) = self
            .non_empty("CCUSAGE_SYNC_ACCESS_TOKEN")
            .filter(|_| matches!(self.mode, SyncAuthMode::Auto))
        {
            return Ok(self.credentials(
                CredentialSource::EnvAccessToken,
                TokenKind::Static(token.to_string()),
            ));
        }
        tried.push(CredentialSource::EnvAccessToken.to_string());

        if let (Some(access_id), Some(secret)) = (
            self.non_empty("CCUSAGE_SYNC_HMAC_ACCESS_ID")
                .filter(|_| env_rungs),
            self.non_empty("CCUSAGE_SYNC_HMAC_SECRET")
                .filter(|_| env_rungs),
        ) {
            let key = HmacKey::new(access_id, secret);
            return Ok(self.credentials(CredentialSource::EnvHmac, TokenKind::Hmac(key)));
        }
        tried.push(CredentialSource::EnvHmac.to_string());

        if let Some(path) = self
            .non_empty("GOOGLE_APPLICATION_CREDENTIALS")
            .filter(|_| adc_rungs)
        {
            let path = PathBuf::from(path);
            let source = CredentialSource::CredentialFile(path.clone());
            // An explicitly pointed-at file that does not work is an error, not
            // a reason to fall through to somebody else's credentials.
            let kind = self.read_credential_file(&path, &source)?;
            return Ok(self.credentials(source, kind));
        }
        tried.push("GOOGLE_APPLICATION_CREDENTIALS".to_string());

        if let Some(path) = self.adc_path().filter(|_| adc_rungs) {
            let source = CredentialSource::ApplicationDefault(path.clone());
            if path.exists() {
                let kind = self.read_credential_file(&path, &source)?;
                return Ok(self.credentials(source, kind));
            }
            tried.push(source.to_string());
        }

        match if adc_rungs {
            (self.runner)()
        } else {
            Err(format!("skipped by --auth {}", mode_flag(self.mode)))
        } {
            Ok(token) if !token.trim().is_empty() => {
                return Ok(self.credentials(CredentialSource::GcloudCli, TokenKind::Gcloud));
            }
            Ok(_) => tried.push(format!("{} (no token)", CredentialSource::GcloudCli)),
            Err(detail) => tried.push(format!("{} ({detail})", CredentialSource::GcloudCli)),
        }

        if adc_rungs && (self.probe_metadata || self.env.contains_key("GCE_METADATA_HOST")) {
            let endpoint = self.non_empty("GCE_METADATA_HOST").map_or_else(
                || self.metadata_endpoint.clone(),
                |host| format!("http://{host}"),
            );
            return Ok(self.credentials(
                CredentialSource::MetadataServer,
                TokenKind::Metadata { endpoint },
            ));
        }
        tried.push(CredentialSource::MetadataServer.to_string());

        Err(ObjectStoreError::Unauthenticated {
            source: "credential ladder".to_string(),
            detail: format!(
                "no Google credentials found for --auth {}; tried {}. {}",
                mode_flag(self.mode),
                tried.join(", "),
                remediation(self.mode)
            ),
        })
    }

    fn credentials(&self, source: CredentialSource, kind: TokenKind) -> Credentials {
        Credentials {
            source,
            kind,
            cached: Mutex::new(None),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(TOKEN_TIMEOUT))
                // A 400 from the token endpoint carries the reason the refresh
                // token stopped working, which is the whole diagnostic.
                .http_status_as_error(false)
                .build()
                .new_agent(),
            runner: Arc::clone(&self.runner),
        }
    }

    fn non_empty(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// `$CLOUDSDK_CONFIG` wins, then the platform's gcloud config directory.
    fn adc_path(&self) -> Option<PathBuf> {
        let base = match self.non_empty("CLOUDSDK_CONFIG") {
            Some(configured) => PathBuf::from(configured),
            None if cfg!(windows) => PathBuf::from(self.non_empty("APPDATA")?).join("gcloud"),
            None => self.home.clone()?.join(".config").join("gcloud"),
        };
        Some(base.join("application_default_credentials.json"))
    }

    fn read_credential_file(&self, path: &Path, source: &CredentialSource) -> Result<TokenKind> {
        let metadata =
            std::fs::metadata(path).map_err(|error| ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: format!("unreadable: {error}"),
            })?;
        if metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err(ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: "implausibly large for a credential file".to_string(),
            });
        }
        let contents = std::fs::read(path).map_err(|error| ObjectStoreError::Unauthenticated {
            source: source.to_string(),
            detail: format!("unreadable: {error}"),
        })?;
        // The parse error is not quoted: a malformed credential file's error
        // text can include the bytes around the fault, which are secret.
        let parsed: serde_json::Value =
            serde_json::from_slice(&contents).map_err(|_| ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: "not valid JSON".to_string(),
            })?;
        let kind = parsed
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match kind {
            "authorized_user" => Ok(TokenKind::RefreshToken {
                client_id: self.field(&parsed, "client_id", source)?,
                client_secret: self.field(&parsed, "client_secret", source)?,
                refresh_token: self.field(&parsed, "refresh_token", source)?,
                token_uri: parsed
                    .get("token_uri")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&self.token_uri)
                    .to_string(),
            }),
            // Both need an RSA signer, which DR-05 kept out of the binary.
            "service_account" | "external_account" => Err(ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: format!(
                    "{kind} credentials are not supported yet; run \
                     `gcloud auth application-default login`, or set CCUSAGE_SYNC_HMAC_ACCESS_ID \
                     and CCUSAGE_SYNC_HMAC_SECRET"
                ),
            }),
            other => Err(ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: format!("unrecognized credential type {other:?}"),
            }),
        }
    }

    fn field(
        &self,
        parsed: &serde_json::Value,
        name: &str,
        source: &CredentialSource,
    ) -> Result<String> {
        parsed
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ObjectStoreError::Unauthenticated {
                source: source.to_string(),
                detail: format!("missing {name}"),
            })
    }
}

fn mode_flag(mode: SyncAuthMode) -> &'static str {
    match mode {
        SyncAuthMode::Auto => "auto",
        SyncAuthMode::Adc => "adc",
        SyncAuthMode::Hmac => "hmac",
    }
}

fn remediation(mode: SyncAuthMode) -> &'static str {
    match mode {
        SyncAuthMode::Auto => {
            "Run `gcloud auth application-default login`, or set CCUSAGE_SYNC_HMAC_ACCESS_ID and \
             CCUSAGE_SYNC_HMAC_SECRET for a headless machine"
        }
        SyncAuthMode::Adc => "Run `gcloud auth application-default login`",
        SyncAuthMode::Hmac => {
            "Set CCUSAGE_SYNC_HMAC_ACCESS_ID and CCUSAGE_SYNC_HMAC_SECRET, or drop --auth hmac to \
             use application default credentials"
        }
    }
}

/// gcloud is always spawned from a resolved absolute path and never through a
/// shell, so a writable relative `PATH` entry cannot substitute a different
/// binary between the check and the call.
pub(crate) fn gcloud_program(env: &HashMap<String, String>) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["gcloud.cmd", "gcloud.exe", "gcloud"]
    } else {
        &["gcloud"]
    };
    std::env::split_paths(env.get("PATH").map_or("", String::as_str))
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| {
            names
                .iter()
                .map(move |name| directory.join(name))
                .collect::<Vec<_>>()
        })
        .find(|candidate| candidate.is_file())
}

fn run_gcloud() -> std::result::Result<String, String> {
    let program =
        gcloud_program(&std::env::vars().collect()).ok_or_else(|| "not on PATH".to_string())?;
    let output = Command::new(program)
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        // stderr from gcloud is diagnostic ("You do not currently have an
        // active account selected") and carries no token.
        return Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn token_error(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("error_description")
                .or_else(|| value.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
}

fn form_encode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{CredentialSource, Resolver};
    use ccusage_cli::SyncAuthMode;
    use ccusage_objectstore::ObjectStoreError;
    use ccusage_test_support::http_server::{ScriptedServer, json_response as json};
    use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// A resolver that finds nothing unless a test gives it something: no
    /// inherited environment, no home directory, and a `gcloud` that fails.
    fn resolver(pairs: &[(&str, &str)]) -> Resolver {
        Resolver::new()
            .with_env(env(pairs))
            .with_home(None)
            .with_gcloud(Arc::new(|| Err("gcloud: command not found".to_string())))
    }

    fn write_credential_file(name: &str, contents: &str) -> (assert_fs::TempDir, PathBuf) {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = dir.path().join(name);
        fs::write(&path, contents).expect("write credential file");
        (dir, path)
    }

    const AUTHORIZED_USER: &str = r#"{
        "type": "authorized_user",
        "client_id": "client.apps.googleusercontent.com",
        "client_secret": "shhh",
        "refresh_token": "1//refresh"
    }"#;

    #[test]
    fn an_access_token_in_the_environment_wins() {
        let credentials = resolver(&[
            ("CCUSAGE_SYNC_ACCESS_TOKEN", "ya29.static"),
            ("CCUSAGE_SYNC_HMAC_ACCESS_ID", "GOOG1EXAMPLE"),
            ("CCUSAGE_SYNC_HMAC_SECRET", "secret"),
        ])
        .resolve()
        .expect("resolve");

        assert_eq!(credentials.source(), &CredentialSource::EnvAccessToken);
        assert_eq!(credentials.access_token().expect("token"), "ya29.static");
    }

    #[test]
    fn adc_mode_ignores_the_environment_credentials() {
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);

        let credentials = resolver(&[
            ("CCUSAGE_SYNC_ACCESS_TOKEN", "ya29.static"),
            ("CCUSAGE_SYNC_HMAC_ACCESS_ID", "GOOG1EXAMPLE"),
            ("CCUSAGE_SYNC_HMAC_SECRET", "secret"),
            ("GOOGLE_APPLICATION_CREDENTIALS", &path.to_string_lossy()),
        ])
        .with_mode(SyncAuthMode::Adc)
        .resolve()
        .expect("resolve");

        assert_eq!(
            credentials.source(),
            &CredentialSource::CredentialFile(path)
        );
    }

    #[test]
    fn hmac_mode_does_not_fall_back_to_application_default_credentials() {
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);

        let error = resolver(&[("GOOGLE_APPLICATION_CREDENTIALS", &path.to_string_lossy())])
            .with_mode(SyncAuthMode::Hmac)
            .resolve()
            .expect_err("--auth hmac must not authenticate as somebody else");

        let rendered = error.to_string();
        assert!(
            rendered.contains("CCUSAGE_SYNC_HMAC_ACCESS_ID"),
            "{rendered}"
        );
        assert!(!rendered.contains("adc.json"), "{rendered}");
    }

    #[test]
    fn an_hmac_pair_resolves_to_a_signing_key_rather_than_a_token() {
        let credentials = resolver(&[
            ("CCUSAGE_SYNC_HMAC_ACCESS_ID", "GOOG1EXAMPLE"),
            ("CCUSAGE_SYNC_HMAC_SECRET", "secret"),
        ])
        .resolve()
        .expect("resolve");

        assert_eq!(credentials.source(), &CredentialSource::EnvHmac);
        assert_eq!(
            credentials.hmac_key().expect("hmac key").access_id,
            "GOOG1EXAMPLE"
        );
        assert!(
            matches!(
                credentials.access_token(),
                Err(ObjectStoreError::Unauthenticated { .. })
            ),
            "an HMAC key signs the XML API; it cannot mint a bearer token"
        );
    }

    #[test]
    fn a_half_configured_hmac_pair_is_skipped_rather_than_half_used() {
        let error = resolver(&[("CCUSAGE_SYNC_HMAC_ACCESS_ID", "GOOG1EXAMPLE")])
            .resolve()
            .expect_err("an access id alone cannot sign anything");

        assert!(error.to_string().contains("no Google credentials found"));
    }

    #[test]
    fn an_authorized_user_file_exchanges_its_refresh_token_for_an_access_token() {
        let mut server = ScriptedServer::serving(vec![json(
            200,
            r#"{"access_token":"ya29.minted","expires_in":3599}"#,
        )]);
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);
        let credentials = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .with_token_uri(&format!("{}/token", server.endpoint()))
        .resolve()
        .expect("resolve");

        assert_eq!(credentials.access_token().expect("token"), "ya29.minted");

        let request = server.requests().remove(0);
        assert!(request.contains("grant_type=refresh_token"), "{request}");
        assert!(
            request.contains("refresh_token=1%2F%2Frefresh"),
            "{request}"
        );
    }

    #[test]
    fn a_minted_token_is_reused_until_it_is_close_to_expiry() {
        let mut server = ScriptedServer::serving(vec![json(
            200,
            r#"{"access_token":"ya29.minted","expires_in":3599}"#,
        )]);
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);
        let credentials = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .with_token_uri(&format!("{}/token", server.endpoint()))
        .resolve()
        .expect("resolve");

        credentials.access_token().expect("first token");
        credentials.access_token().expect("cached token");

        assert_eq!(
            server.requests().len(),
            1,
            "a sync makes many requests; each must not cost a token exchange"
        );
    }

    #[test]
    fn a_token_that_expires_inside_the_skew_is_minted_again() {
        let mut server = ScriptedServer::serving(vec![
            json(200, r#"{"access_token":"ya29.first","expires_in":30}"#),
            json(200, r#"{"access_token":"ya29.second","expires_in":3599}"#),
        ]);
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);
        let credentials = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .with_token_uri(&format!("{}/token", server.endpoint()))
        .resolve()
        .expect("resolve");

        assert_eq!(credentials.access_token().expect("first"), "ya29.first");
        assert_eq!(credentials.access_token().expect("second"), "ya29.second");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn a_revoked_refresh_token_reports_what_google_said_and_not_the_secret() {
        let server = ScriptedServer::serving(vec![json(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )]);
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);
        let credentials = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .with_token_uri(&format!("{}/token", server.endpoint()))
        .resolve()
        .expect("resolve");

        let rendered = credentials
            .access_token()
            .expect_err("a revoked token must surface")
            .to_string();

        assert!(rendered.contains("expired or revoked"), "{rendered}");
        assert!(
            rendered.contains("GOOGLE_APPLICATION_CREDENTIALS"),
            "{rendered}"
        );
        assert!(!rendered.contains("1//refresh"), "the refresh token leaked");
        assert!(!rendered.contains("shhh"), "the client secret leaked");
    }

    #[test]
    fn a_service_account_file_says_what_to_do_instead() {
        let (_dir, path) = write_credential_file(
            "sa.json",
            r#"{"type":"service_account","private_key":"-----BEGIN PRIVATE KEY-----"}"#,
        );

        let rendered = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .resolve()
        .expect_err("RSA signing is not shipped yet")
        .to_string();

        assert!(
            rendered.contains("service_account credentials are not supported"),
            "{rendered}"
        );
        assert!(
            rendered.contains("gcloud auth application-default login"),
            "{rendered}"
        );
        assert!(!rendered.contains("BEGIN PRIVATE KEY"), "the key leaked");
    }

    #[test]
    fn a_pointed_at_file_that_does_not_work_does_not_fall_through() {
        let error = resolver(&[("GOOGLE_APPLICATION_CREDENTIALS", "/nonexistent/adc.json")])
            .with_gcloud(Arc::new(|| Ok("ya29.gcloud".to_string())))
            .resolve()
            .expect_err("an explicit credential must not be silently replaced");

        assert!(matches!(error, ObjectStoreError::Unauthenticated { .. }));
        assert!(error.to_string().contains("/nonexistent/adc.json"));
    }

    #[test]
    fn the_well_known_adc_file_is_found_under_the_home_directory() {
        let mut server = ScriptedServer::serving(vec![json(
            200,
            r#"{"access_token":"ya29.adc","expires_in":3599}"#,
        )]);
        let home = assert_fs::TempDir::new().expect("temp home");
        let gcloud = home.path().join(".config").join("gcloud");
        fs::create_dir_all(&gcloud).expect("gcloud config dir");
        fs::write(
            gcloud.join("application_default_credentials.json"),
            AUTHORIZED_USER,
        )
        .expect("write adc");

        let credentials = resolver(&[])
            .with_home(Some(home.path().to_path_buf()))
            .with_token_uri(&format!("{}/token", server.endpoint()))
            .resolve()
            .expect("resolve");

        assert!(matches!(
            credentials.source(),
            CredentialSource::ApplicationDefault(_)
        ));
        assert_eq!(credentials.access_token().expect("token"), "ya29.adc");
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn gcloud_covers_a_user_who_never_created_adc() {
        let credentials = resolver(&[])
            .with_gcloud(Arc::new(|| Ok("ya29.gcloud\n".to_string())))
            .resolve()
            .expect("resolve");

        assert_eq!(credentials.source(), &CredentialSource::GcloudCli);
        assert_eq!(credentials.access_token().expect("token"), "ya29.gcloud");
    }

    #[test]
    fn the_metadata_server_is_used_on_a_gce_style_host() {
        let mut server = ScriptedServer::serving(vec![json(
            200,
            r#"{"access_token":"ya29.metadata","expires_in":3599}"#,
        )]);
        let credentials = resolver(&[])
            .with_metadata_endpoint(server.endpoint())
            .resolve()
            .expect("resolve");

        assert_eq!(credentials.source(), &CredentialSource::MetadataServer);
        assert_eq!(credentials.access_token().expect("token"), "ya29.metadata");

        let request = server.requests().remove(0);
        assert!(
            request.contains("/computeMetadata/v1/instance/service-accounts/default/token"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("metadata-flavor: google"),
            "the metadata server rejects a request without the header: {request}"
        );
    }

    #[test]
    fn exhausting_the_ladder_names_every_rung_that_was_tried() {
        let rendered = resolver(&[])
            .resolve()
            .expect_err("nothing was configured")
            .to_string();

        for rung in [
            "CCUSAGE_SYNC_ACCESS_TOKEN",
            "CCUSAGE_SYNC_HMAC_ACCESS_ID/SECRET",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "gcloud auth print-access-token (gcloud: command not found)",
            "GCE metadata server",
        ] {
            assert!(rendered.contains(rung), "{rung} missing from: {rendered}");
        }
    }

    #[test]
    fn debug_output_never_carries_the_credential() {
        let (_dir, path) = write_credential_file("adc.json", AUTHORIZED_USER);
        let credentials = resolver(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            path.to_str().expect("path"),
        )])
        .resolve()
        .expect("resolve");

        let rendered = format!("{credentials:?}");

        assert!(rendered.contains("RefreshToken(<redacted>)"), "{rendered}");
        assert!(!rendered.contains("1//refresh"), "{rendered}");
        assert!(!rendered.contains("shhh"), "{rendered}");
    }
}
