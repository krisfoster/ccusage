//! `ccusage sync serve`: a hub that lets sandboxes sync into a directory on the host.
//!
//! The hub is the only process that touches the data directory, which is what makes compare-and-
//! swap safe: every write from every sandbox passes through one `FsStore`. It speaks the protocol
//! documented in `http_store.rs`.
//!
//! Everything the hub receives is untrusted. A sandbox runs an agent, and an agent can be talked
//! into writing anything, so each request is checked before it reaches the store:
//!
//! - a bearer token, compared in constant time, on every request;
//! - key paths are validated segment by segment and must sit under the hub's prefix, so a request
//!   cannot name a file outside the data directory;
//! - the dashboard prefix is read-only, since it is the world-readable one in a bucket;
//! - bodies are capped, connections are capped, and sockets time out;
//! - deletes are refused unless the hub was started with `--allow-delete`, apart from the doctor
//!   probe object, so a compromised sandbox cannot erase history.
//!
//! What the hub does not do is tell sandboxes apart: one token serves them all, and the shared
//! rollup objects are writable by any of them. A sandbox can therefore corrupt the aggregate, but
//! `ccusage sync repair` rebuilds the rollups from the shards.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use ccusage_objectstore::{
    Key, KeySpace, ObjectMeta, ObjectStore, ObjectStoreError, Precondition, sha256_hex,
};
use serde_json::{Value, json};

use crate::{
    Result, cli_error,
    fs_store::FsStore,
    http_store::{
        HEALTH_PATH, IF_GENERATION_HEADER, LIST_PATH, OBJECT_NOT_FOUND, OBJECTS_PATH,
        PROTOCOL_VERSION,
    },
};

/// Shards and rollups are small; this only has to stop a runaway body.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_KEY_BYTES: usize = 512;
const MAX_SEGMENT_BYTES: usize = 64;
const MAX_CONNECTIONS: usize = 32;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Hub {
    store: FsStore,
    keys: KeySpace,
    token_digest: String,
    allow_delete: bool,
}

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "application/json".to_string())],
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(
            status,
            &json!({ "error": { "code": code, "message": message } }),
        )
    }

    fn with_meta(mut self, meta: &ObjectMeta) -> Self {
        if let Some(generation) = &meta.generation {
            self.headers
                .push((GENERATION_HEADER_CASED, generation.clone()));
        }
        if let Some(etag) = &meta.etag {
            self.headers.push(("ETag", etag.clone()));
        }
        if let Some(updated) = meta.updated_ms {
            self.headers
                .push((UPDATED_HEADER_CASED, updated.to_string()));
        }
        self
    }
}

const GENERATION_HEADER_CASED: &str = "X-Ccusage-Generation";
const UPDATED_HEADER_CASED: &str = "X-Ccusage-Updated-Ms";

impl Hub {
    pub(crate) fn new(dir: &Path, prefix: &str, token: &str, allow_delete: bool) -> Result<Self> {
        let keys = KeySpace::new(prefix).map_err(|error| cli_error(error.to_string()))?;
        Ok(Self {
            store: FsStore::new(dir),
            keys,
            token_digest: sha256_hex(token.as_bytes()),
            allow_delete,
        })
    }

    pub(crate) fn handle(&self, request: &Request) -> Reply {
        if !self.authorized(request) {
            let mut reply =
                Reply::error(401, "unauthenticated", "a valid bearer token is required");
            reply
                .headers
                .push(("WWW-Authenticate", "Bearer".to_string()));
            return reply;
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", HEALTH_PATH) => Reply::json(
                200,
                &json!({ "protocol": PROTOCOL_VERSION, "prefix": self.keys.prefix() }),
            ),
            ("GET", LIST_PATH) => self.list(request),
            (method, path) if path.starts_with(OBJECTS_PATH) => {
                let key_path = &path[OBJECTS_PATH.len()..];
                match method {
                    "GET" | "PUT" | "DELETE" => match self.parse_key(key_path) {
                        Ok(key) => self.object(method, &key, request),
                        Err(reply) => reply,
                    },
                    _ => Reply::error(405, "method_not_allowed", "use GET, PUT or DELETE"),
                }
            }
            _ => Reply::error(404, "not_found", "no such route"),
        }
    }

    fn authorized(&self, request: &Request) -> bool {
        let Some(presented) = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        // Digests are the same length whatever was presented, so the comparison leaks nothing
        // about the token's length or about how much of a guess was right.
        let presented = sha256_hex(presented.as_bytes());
        presented
            .bytes()
            .zip(self.token_digest.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }

    fn parse_key(&self, path: &str) -> std::result::Result<Key, Reply> {
        let bad = |message: &str| Reply::error(400, "invalid_key", message);
        if path.is_empty() || path.len() > MAX_KEY_BYTES {
            return Err(bad("the key is empty or too long"));
        }
        for segment in path.split('/') {
            let valid = !segment.is_empty()
                && segment.len() <= MAX_SEGMENT_BYTES
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                && !segment.bytes().all(|byte| byte == b'.');
            if !valid {
                return Err(bad(
                    "key segments must be ASCII alphanumerics, '-', '_' or '.', and not '.' or '..'",
                ));
            }
        }
        let key = self.keys.listed(path).map_err(|_| {
            Reply::error(
                403,
                "outside_prefix",
                &format!("this hub only serves keys under '{}/'", self.keys.prefix()),
            )
        })?;
        if key.is_public() {
            return Err(Reply::error(
                403,
                "read_only_prefix",
                "the dashboard prefix cannot be written through the hub",
            ));
        }
        Ok(key)
    }

    fn object(&self, method: &str, key: &Key, request: &Request) -> Reply {
        match method {
            "GET" => match self.store.get(key) {
                Ok(Some((body, meta))) => {
                    let mut reply = Reply {
                        status: 200,
                        headers: vec![("Content-Type", content_type_for(key.path()).to_string())],
                        body,
                    };
                    reply = reply.with_meta(&meta);
                    reply
                }
                Ok(None) => Reply::error(404, OBJECT_NOT_FOUND, "no such object"),
                Err(error) => store_error(&error),
            },
            "PUT" => {
                let precondition = match precondition_of(request, true) {
                    Ok(precondition) => precondition,
                    Err(reply) => return reply,
                };
                let content_type = request
                    .header("content-type")
                    .unwrap_or("application/octet-stream");
                match self
                    .store
                    .put(key, &request.body, content_type, &precondition)
                {
                    Ok(meta) => Reply::json(200, &json!({})).with_meta(&meta),
                    Err(error) => store_error(&error),
                }
            }
            _ => {
                if !self.allow_delete && !is_probe(key) {
                    return Reply::error(
                        403,
                        "deletes_disabled",
                        "this hub refuses deletes; start it with --allow-delete to permit them",
                    );
                }
                let precondition = match precondition_of(request, false) {
                    Ok(precondition) => precondition,
                    Err(reply) => return reply,
                };
                match self.store.delete(key, &precondition) {
                    Ok(()) => Reply {
                        status: 204,
                        headers: Vec::new(),
                        body: Vec::new(),
                    },
                    Err(error) => store_error(&error),
                }
            }
        }
    }

    fn list(&self, request: &Request) -> Reply {
        let prefix = query_value(&request.query, "prefix").unwrap_or_default();
        let inside = prefix.is_empty() || prefix.starts_with(&format!("{}/", self.keys.prefix()));
        if !inside {
            return Reply::error(
                403,
                "outside_prefix",
                &format!("this hub only serves keys under '{}/'", self.keys.prefix()),
            );
        }
        match self.store.list(&prefix) {
            Ok(objects) => Reply::json(
                200,
                &json!({
                    "objects": objects
                        .iter()
                        .map(|meta| json!({
                            "key": meta.key,
                            "generation": meta.generation,
                            "etag": meta.etag,
                            "size": meta.size,
                            "updatedMs": meta.updated_ms,
                        }))
                        .collect::<Vec<_>>(),
                }),
            ),
            Err(error) => store_error(&error),
        }
    }
}

fn is_probe(key: &Key) -> bool {
    key.path().contains("/.probe/")
}

fn content_type_for(key: &str) -> &'static str {
    if key.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn precondition_of(
    request: &Request,
    allow_if_absent: bool,
) -> std::result::Result<Precondition, Reply> {
    let if_absent = request.header("if-none-match");
    let generation = request.header(IF_GENERATION_HEADER);
    match (if_absent, generation) {
        (None, None) => Ok(Precondition::None),
        (Some("*"), None) if allow_if_absent => Ok(Precondition::IfAbsent),
        (None, Some(generation)) if !generation.is_empty() => {
            Ok(Precondition::IfGenerationMatch(generation.to_string()))
        }
        _ => Err(Reply::error(
            400,
            "invalid_precondition",
            "use 'If-None-Match: *' or 'X-Ccusage-If-Generation-Match: <generation>', not both",
        )),
    }
}

fn store_error(error: &ObjectStoreError) -> Reply {
    match error {
        ObjectStoreError::Conflict { .. } => Reply::error(
            412,
            "precondition_failed",
            "the object changed since it was read",
        ),
        ObjectStoreError::NotFound { .. } => Reply::error(404, OBJECT_NOT_FOUND, "no such object"),
        ObjectStoreError::InvalidKey { .. } => Reply::error(400, "invalid_key", "invalid key"),
        ObjectStoreError::Forbidden { .. } => {
            Reply::error(403, "forbidden", "the hub cannot access its data directory")
        }
        other => Reply::error(500, "store_error", &other.to_string()),
    }
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = value.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/* ------------------------------------------------------------------ socket */

/// Parses one request. The error is the reply to send instead of handling it.
fn read_request(stream: &TcpStream) -> std::result::Result<Request, Reply> {
    let bad = |message: &str| Reply::error(400, "bad_request", message);
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    read_bounded_line(&mut reader, &mut line).map_err(|()| bad("unreadable request line"))?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = Vec::new();
    let mut header_bytes = line.len();
    loop {
        let mut header = String::new();
        read_bounded_line(&mut reader, &mut header).map_err(|()| bad("unreadable header"))?;
        header_bytes += header.len();
        if header.trim().is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS || header_bytes > MAX_HEADER_BYTES {
            return Err(Reply::error(431, "headers_too_large", "too many headers"));
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(Reply::error(
            501,
            "unsupported",
            "chunked request bodies are not supported; send a Content-Length",
        ));
    }
    let length = match headers.iter().find(|(name, _)| name == "content-length") {
        Some((_, value)) => value
            .parse::<usize>()
            .map_err(|_| bad("invalid Content-Length"))?,
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(Reply::error(
            413,
            "body_too_large",
            &format!("bodies are limited to {MAX_BODY_BYTES} bytes"),
        ));
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|_| bad("the body ended before Content-Length bytes"))?;

    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

/// Reads one line without letting a client that never sends a newline grow it without bound.
fn read_bounded_line(
    reader: &mut BufReader<&TcpStream>,
    line: &mut String,
) -> std::result::Result<(), ()> {
    let mut limited = reader.by_ref().take(MAX_HEADER_BYTES as u64);
    match limited.read_line(line) {
        Ok(0) | Err(_) => Err(()),
        Ok(_) if line.ends_with('\n') => Ok(()),
        Ok(_) => Err(()),
    }
}

fn write_reply(mut stream: &TcpStream, reply: &Reply) -> std::io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        reply.body.len()
    ));
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn serve_connection(stream: &TcpStream, hub: &Hub) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let reply = match read_request(stream) {
        Ok(request) => hub.handle(&request),
        Err(reply) => reply,
    };
    let _ = write_reply(stream, &reply);
}

/// Accepts connections until the process ends, one thread per connection up to a cap.
pub(crate) fn serve(listener: &TcpListener, hub: &Arc<Hub>) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = write_reply(
                &stream,
                &Reply::error(503, "busy", "too many connections; retry shortly"),
            );
            continue;
        }
        let hub = Arc::clone(hub);
        let active = Arc::clone(&active);
        thread::spawn(move || {
            serve_connection(&stream, &hub);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/* ----------------------------------------------------------------- command */

pub(crate) struct ServeOptions {
    pub(crate) dir: PathBuf,
    pub(crate) listen: SocketAddr,
    pub(crate) token_file: PathBuf,
    pub(crate) prefix: String,
    pub(crate) allow_delete: bool,
}

pub(crate) fn execute(options: &ServeOptions) -> Result<()> {
    let token = load_or_create_token(&options.token_file)?;
    let hub = Arc::new(Hub::new(
        &options.dir,
        &options.prefix,
        &token,
        options.allow_delete,
    )?);
    let listener = TcpListener::bind(options.listen)
        .map_err(|error| cli_error(format!("could not listen on {}: {error}", options.listen)))?;
    println!(
        "Serving usage from {} on http://{}",
        options.dir.display(),
        listener.local_addr().map_or_else(
            |_| options.listen.to_string(),
            |address| address.to_string()
        )
    );
    println!("Bearer token file: {}", options.token_file.display());
    if !options.listen.ip().is_loopback() {
        println!(
            "Warning: {} is not a loopback address, so other machines on the network can reach \
             this hub. The token is the only protection.",
            options.listen
        );
    }
    println!("Press Ctrl-C to stop.");
    serve(&listener, &hub);
    Ok(())
}

/// The token is generated once and kept in a file only its owner can read. Passing the token on
/// the command line would put it in the process list.
fn load_or_create_token(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) if !contents.trim().is_empty() => return Ok(contents.trim().to_string()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(cli_error(format!(
                "could not read the token file {}: {error}",
                path.display()
            )));
        }
    }
    let mut entropy = [0u8; 32];
    getrandom::fill(&mut entropy)
        .map_err(|error| cli_error(format!("no source of randomness: {error}")))?;
    let token: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            cli_error(format!("could not create {}: {error}", parent.display()))
        })?;
    }
    write_private(path, &token)?;
    println!("Created a new bearer token at {}.", path.display());
    Ok(token)
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .map_err(|error| cli_error(format!("could not write {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .map_err(|error| cli_error(format!("could not write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;
    use ccusage_objectstore::UtcDate;
    use ccusage_test_support::http_server::{ScriptedServer, response};

    use super::*;
    use crate::{
        gcs::RetryPolicy,
        http_store::HttpStore,
        sync::doctor::{Outcome, run_checks},
    };

    const TOKEN: &str = "test-token-0123456789";

    fn no_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn keys() -> KeySpace {
        KeySpace::new("ccusage/v1").expect("key space")
    }

    struct Running {
        _dir: TempDir,
        url: String,
        data: PathBuf,
    }

    fn start(allow_delete: bool) -> Running {
        let dir = TempDir::new().expect("temp dir");
        let data = dir.path().join("data");
        let hub = Arc::new(Hub::new(&data, "ccusage/v1", TOKEN, allow_delete).expect("hub"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        thread::spawn(move || serve(&listener, &hub));
        Running {
            _dir: dir,
            url,
            data,
        }
    }

    fn client(running: &Running) -> HttpStore {
        HttpStore::with_retry(&running.url, TOKEN, no_retry())
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
        let mut all = vec![("authorization".to_string(), format!("Bearer {TOKEN}"))];
        all.extend(
            headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: all,
            body: body.to_vec(),
        }
    }

    fn hub_for_handle() -> (TempDir, Hub) {
        let dir = TempDir::new().expect("temp dir");
        let hub = Hub::new(&dir.path().join("data"), "ccusage/v1", TOKEN, false).expect("hub");
        (dir, hub)
    }

    #[test]
    fn round_trips_an_object_with_its_generation() {
        let running = start(false);
        let store = client(&running);
        let key = keys().manifest();

        let written = store
            .put(
                &key,
                br#"{"a":1}"#,
                "application/json",
                &Precondition::IfAbsent,
            )
            .expect("put");
        let (body, meta) = store.get(&key).expect("get").expect("object");

        assert_eq!(body, br#"{"a":1}"#);
        assert!(written.generation.is_some());
        assert_eq!(meta.generation, written.generation);
        assert_eq!(meta.size, 7);
        assert!(meta.updated_ms.is_some());
        assert_eq!(
            std::fs::read(running.data.join("ccusage/v1/manifest.json")).expect("on disk"),
            br#"{"a":1}"#
        );
    }

    #[test]
    fn a_missing_object_reads_as_none() {
        let running = start(false);
        assert!(client(&running).get(&keys().salt()).expect("get").is_none());
    }

    #[test]
    fn preconditions_hold_over_the_wire() {
        let running = start(false);
        let store = client(&running);
        let key = keys().salt();

        let first = store
            .put(&key, b"one", "text/plain", &Precondition::IfAbsent)
            .expect("create");
        assert!(matches!(
            store.put(&key, b"two", "text/plain", &Precondition::IfAbsent),
            Err(ObjectStoreError::Conflict { .. })
        ));
        assert!(matches!(
            store.put(
                &key,
                b"two",
                "text/plain",
                &Precondition::IfGenerationMatch("stale".to_string())
            ),
            Err(ObjectStoreError::Conflict { .. })
        ));
        store
            .put(
                &key,
                b"two",
                "text/plain",
                &Precondition::IfGenerationMatch(first.generation.expect("generation")),
            )
            .expect("current generation is accepted");
        assert_eq!(store.get(&key).unwrap().unwrap().0, b"two");
    }

    #[test]
    fn lists_by_prefix() {
        let running = start(false);
        let store = client(&running);
        let space = keys();
        let date = UtcDate::new(2026, 10, 9).expect("date");
        let mine = space.shard("u1", "m1", "claude", date).expect("shard");
        let theirs = space.shard("u1", "m2", "claude", date).expect("shard");
        for key in [&mine, &theirs] {
            store
                .put(key, b"{}", "application/json", &Precondition::None)
                .expect("put");
        }

        let listed = store
            .list(&space.machine_prefix("u1", "m1").expect("prefix"))
            .expect("list");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, mine.path());
        assert!(listed[0].generation.is_some());
    }

    #[test]
    fn a_wrong_token_is_refused_and_names_the_variable() {
        let running = start(false);
        let store = HttpStore::with_retry(&running.url, "not-the-token", no_retry());

        let error = store.get(&keys().manifest()).expect_err("refused");

        assert!(matches!(error, ObjectStoreError::Unauthenticated { .. }));
        assert!(error.to_string().contains("CCUSAGE_SYNC_TOKEN"), "{error}");
    }

    #[test]
    fn ping_reports_the_hub_prefix() {
        let running = start(false);
        assert_eq!(client(&running).ping().expect("ping"), "ccusage/v1");
    }

    #[test]
    fn a_url_that_is_not_a_hub_is_an_error_not_an_empty_store() {
        let mut server =
            ScriptedServer::serving(vec![response(404, "Not Found", "", "no such page")]);
        let store = HttpStore::with_retry(server.endpoint(), TOKEN, no_retry());

        let error = store.get(&keys().manifest()).expect_err("not a hub");

        assert!(error.to_string().contains("does not look like"), "{error}");
        let _ = server.requests();
    }

    #[test]
    fn the_doctor_checks_pass_against_a_hub() {
        let running = start(false);
        let checks = run_checks(
            &client(&running),
            &keys(),
            "machine-1",
            crate::sync::now_ms(),
        );

        let failures: Vec<_> = checks
            .iter()
            .filter(|check| check.outcome == Outcome::Fail)
            .map(|check| (check.name, check.detail.clone()))
            .collect();
        assert!(failures.is_empty(), "failed checks: {failures:?}");
    }

    #[test]
    fn deletes_are_refused_unless_enabled_except_for_the_doctor_probe() {
        let running = start(false);
        let store = client(&running);
        let key = keys().manifest();
        store
            .put(&key, b"keep", "text/plain", &Precondition::None)
            .expect("put");

        assert!(matches!(
            store.delete(&key, &Precondition::None),
            Err(ObjectStoreError::Forbidden { .. })
        ));
        assert!(store.get(&key).unwrap().is_some());

        let permissive = start(true);
        let store = client(&permissive);
        store
            .put(&key, b"drop", "text/plain", &Precondition::None)
            .expect("put");
        store.delete(&key, &Precondition::None).expect("delete");
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn keys_outside_the_prefix_are_refused() {
        let running = start(true);
        let other = KeySpace::new("other/v1").expect("key space");

        let error = client(&running)
            .put(&other.manifest(), b"x", "text/plain", &Precondition::None)
            .expect_err("outside the prefix");

        assert!(
            matches!(error, ObjectStoreError::Forbidden { .. }),
            "{error}"
        );
        assert!(!running.data.join("other").exists());
    }

    #[test]
    fn the_dashboard_prefix_is_not_writable() {
        let (_dir, hub) = hub_for_handle();
        let reply = hub.handle(&request(
            "PUT",
            "/v1/objects/ccusage/v1/dashboard/index.html",
            &[],
            b"<script>",
        ));
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn path_traversal_and_odd_segments_are_rejected_before_the_store() {
        let (dir, hub) = hub_for_handle();
        std::fs::write(dir.path().join("secret.txt"), "secret").expect("secret");

        for path in [
            "/v1/objects/ccusage/v1/../../secret.txt",
            "/v1/objects/ccusage/v1/./manifest.json",
            "/v1/objects/ccusage/v1//manifest.json",
            "/v1/objects/ccusage/v1/%2e%2e/secret.txt",
            "/v1/objects/ccusage/v1/a%00b",
            "/v1/objects/ccusage/v1/~lock",
            "/v1/objects/",
        ] {
            for method in ["GET", "PUT", "DELETE"] {
                let reply = hub.handle(&request(method, path, &[], b"x"));
                assert_eq!(reply.status, 400, "{method} {path}");
            }
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("secret.txt")).expect("untouched"),
            "secret"
        );
    }

    #[test]
    fn list_cannot_name_a_prefix_outside_the_hub() {
        let (_dir, hub) = hub_for_handle();
        let reply = hub.handle(&request("GET", "/v1/list?prefix=other%2F", &[], b""));
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn conflicting_precondition_headers_are_a_bad_request() {
        let (_dir, hub) = hub_for_handle();
        let reply = hub.handle(&request(
            "PUT",
            "/v1/objects/ccusage/v1/salt.json",
            &[("if-none-match", "*"), (IF_GENERATION_HEADER, "abc")],
            b"x",
        ));
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn unknown_routes_and_methods_do_not_reach_the_store() {
        let (_dir, hub) = hub_for_handle();
        assert_eq!(hub.handle(&request("GET", "/", &[], b"")).status, 404);
        assert_eq!(
            hub.handle(&request(
                "POST",
                "/v1/objects/ccusage/v1/salt.json",
                &[],
                b"x"
            ))
            .status,
            405
        );
    }

    fn raw(url: &str, bytes: &[u8]) -> String {
        let address = url.trim_start_matches("http://");
        let mut stream = TcpStream::connect(address).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.write_all(bytes).expect("write");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[test]
    fn an_oversized_body_is_refused_without_being_read() {
        let running = start(false);
        let request = format!(
            "PUT /v1/objects/ccusage/v1/salt.json HTTP/1.1\r\nHost: x\r\n\
             Authorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(raw(&running.url, request.as_bytes()).starts_with("HTTP/1.1 413"));
    }

    #[test]
    fn chunked_bodies_are_refused() {
        let running = start(false);
        let request = format!(
            "PUT /v1/objects/ccusage/v1/salt.json HTTP/1.1\r\nHost: x\r\n\
             Authorization: Bearer {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        );
        assert!(raw(&running.url, request.as_bytes()).starts_with("HTTP/1.1 501"));
    }

    #[test]
    fn garbage_gets_a_400_and_the_hub_keeps_serving() {
        let running = start(false);
        assert!(raw(&running.url, b"garbage\r\n\r\n").starts_with("HTTP/1.1 400"));
        assert_eq!(
            client(&running).ping().expect("still serving"),
            "ccusage/v1"
        );
    }

    /// The shared manifest is written by every machine with compare-and-swap, so this is the
    /// property that matters most over the wire: many sandboxes registering at once lose nobody.
    /// A registration that loses all its attempts reports it and is safe to repeat, which is what
    /// the sandbox's sync loop does, so each worker retries the way that loop would.
    #[test]
    fn machines_registering_at_once_over_http_all_end_up_in_the_roster() {
        use crate::sync::bootstrap::{manifest_machines, register_machine};

        let running = start(false);
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let store = client(&running);
                thread::spawn(move || {
                    let machine = format!("sandbox-{index}");
                    for _ in 0..20 {
                        if register_machine(&store, &keys(), "user-1", &machine).is_ok() {
                            return true;
                        }
                    }
                    false
                })
            })
            .collect();
        for handle in handles {
            assert!(
                handle.join().expect("worker finished"),
                "a machine never registered"
            );
        }

        let roster = manifest_machines(&client(&running), &keys()).expect("roster");
        let expected: Vec<String> = (0..8).map(|index| format!("sandbox-{index}")).collect();
        assert_eq!(roster, expected, "every machine once, none lost");
    }

    #[test]
    fn the_token_file_is_created_private_and_reused() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("nested").join("token");

        let first = load_or_create_token(&path).expect("created");
        let second = load_or_create_token(&path).expect("reused");

        assert_eq!(first.len(), 64);
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
