//! `ccusage sync dashboard`: publish the page, or host it locally.
//!
//! Two access models, and the difference is where the *data* comes from — the
//! page itself is the same bytes in both:
//!
//! - **Local** (the default): a server bound to loopback reads the rollups with
//!   this machine's own credentials and serves them alongside the bundle.
//!   Nothing becomes public, and no token leaves the process.
//! - **Deployed** (`--deploy`): the bundle is uploaded under the one prefix
//!   `allUsers` can read. The rollups are *not* uploaded there — they are
//!   already in the bucket, private — so a deployed page shows nothing until it
//!   is opened with a `--share` link, whose signed URLs ride in the location
//!   fragment and therefore never reach a server log.
//!
//! The public/private split is enforced by the key space, not by care taken
//! here: `dashboard_asset` is the only constructor that yields a public key,
//! and every rollup key is private by construction.

use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ccusage_compare::EquivalenceMap;
use ccusage_config::ConfigContext;
use ccusage_dashboard::{ASSETS, Asset};
use ccusage_objectstore::{
    GOOG4_HMAC_SHA256, HmacKey, Key, KeySpace, ObjectStore, Precondition, RollupKind, Signer,
    SigningTime,
};
use ccusage_sync::rollup::ROLLUP_SCHEMA;
use serde_json::{Value, json};

use super::{STORAGE_ENDPOINT, status};
use crate::{
    Result,
    cli::SyncDashboardArgs,
    cli_error,
    gcs::{
        GcsStore, JsonApi, RetryPolicy,
        bucket::{BucketAdmin, PublicAccessPrevention},
    },
    pricing::PricingMap,
};

/// Where the dashboard listens when hosting locally. Loopback only: the data it
/// serves is the user's spend, and binding `0.0.0.0` would put it on the
/// network of whichever café they happen to be in.
const LOCAL_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const FIRST_PORT: u16 = 8787;
const LAST_PORT: u16 = 8807;

const ATTRIBUTION: &str = "Model prices from LiteLLM's model_prices_and_context_window.json \
                           (MIT licence), with models.dev as the offline fallback. \
                           Model equivalences are ccusage's own editorial judgement.";

pub(crate) fn execute(config: &ConfigContext, args: &SyncDashboardArgs) -> Result<()> {
    let status = status::Status::from_config(config.sync());
    let Some(bucket) = status.bucket.clone() else {
        return Err(cli_error(
            "sync is not configured. Run 'ccusage sync setup' first.".to_string(),
        ));
    };
    let keys = KeySpace::new(&status.prefix).map_err(|error| cli_error(error.to_string()))?;
    let credentials = Arc::new(
        super::auth::resolve(
            super::config_auth_mode(config),
            false,
            &mut super::auth::TerminalPrompt,
        )
        .map_err(|error| cli_error(error.to_string()))?,
    );
    let store = GcsStore::new(&bucket, Box::new(Arc::clone(&credentials)));

    let public = public_data(&PricingMap::load_with_overrides(
        false,
        false,
        std::collections::BTreeMap::new().iter(),
    ));

    if args.deploy {
        deploy(&store, &keys, &bucket, &credentials, &public)?;
    }

    let link = if args.share {
        let hmac = credentials.hmac_key().ok_or_else(|| {
            cli_error(
                "share links need an HMAC credential, and this machine is authenticated with \
                 application-default credentials. Either host the dashboard locally \
                 (`ccusage sync dashboard`), or create an HMAC key for a service account with \
                 read access and set CCUSAGE_SYNC_HMAC_ACCESS_ID / CCUSAGE_SYNC_HMAC_SECRET."
                    .to_string(),
            )
        })?;
        Some(share_link(&bucket, &keys, hmac, args.share_ttl_seconds)?)
    } else {
        None
    };

    match (args.deploy || args.share, &link) {
        (true, Some(link)) => {
            println!(
                "\nShare link (expires in {}):",
                human_ttl(args.share_ttl_seconds)
            );
            println!("{link}");
            println!(
                "Anyone holding this link can read the usage data it points at until it expires. \
                 It is a bearer token: treat it like one."
            );
            if args.open {
                open_in_browser(link);
            }
            Ok(())
        }
        (true, None) => {
            let url = public_url(&bucket, &keys);
            println!("\nDashboard: {url}");
            println!(
                "The page is public; the usage data is not. Run `ccusage sync dashboard --share` \
                 to mint a link that can read it."
            );
            if args.open {
                open_in_browser(&url);
            }
            Ok(())
        }
        (false, _) => serve_locally(&store, &keys, &public, args.open),
    }
}

/* ------------------------------------------------------------------ deploy */

fn deploy(
    store: &GcsStore,
    keys: &KeySpace,
    bucket: &str,
    credentials: &Arc<crate::credentials::Credentials>,
    public: &[(String, Vec<u8>, &'static str)],
) -> Result<()> {
    let admin = BucketAdmin::new(
        JsonApi::new(
            STORAGE_ENDPOINT,
            Box::new(Arc::clone(credentials)),
            RetryPolicy::default(),
        ),
        bucket,
    );
    // An org policy may have re-enforced prevention since setup, and the public
    // binding below would then be refused with a bare 403.
    if let Ok(Some(info)) = admin.get()
        && info.public_access_prevention == Some(PublicAccessPrevention::Enforced)
    {
        admin
            .set_public_access_prevention(PublicAccessPrevention::Inherited)
            .map_err(|error| {
                cli_error(format!(
                    "this bucket prevents public access and it could not be relaxed: {error}. \
                         Deploying the dashboard needs public access prevention set to \
                         'inherited'; the data stays private either way."
                ))
            })?;
    }

    for asset in ASSETS {
        put_public(store, keys, asset.path, asset.bytes, asset.content_type)?;
    }
    for (path, bytes, content_type) in public {
        put_public(store, keys, path, bytes, content_type)?;
    }

    let granted = admin
        .grant_public_dashboard_read(keys)
        .map_err(|error| cli_error(error.to_string()))?;
    println!(
        "Uploaded {} file(s) to {}{}",
        ASSETS.len() + public.len(),
        keys.public_prefix(),
        if granted {
            ", and granted public read on that prefix only."
        } else {
            "; public read on that prefix was already granted."
        }
    );
    Ok(())
}

fn put_public(
    store: &GcsStore,
    keys: &KeySpace,
    path: &str,
    bytes: &[u8],
    content_type: &str,
) -> Result<()> {
    let key: Key = keys
        .dashboard_asset(path)
        .map_err(|error| cli_error(error.to_string()))?;
    store
        .put(&key, bytes, content_type, &Precondition::None)
        .map_err(|error| cli_error(format!("could not upload {path}: {error}")))?;
    Ok(())
}

fn public_url(bucket: &str, keys: &KeySpace) -> String {
    format!(
        "{STORAGE_ENDPOINT}/{bucket}/{}index.html",
        keys.public_prefix()
    )
}

/* -------------------------------------------------------------- share link */

/// The rollups a viewer needs, as signed URLs carried in the fragment.
fn share_link(bucket: &str, keys: &KeySpace, hmac: &HmacKey, ttl_seconds: u64) -> Result<String> {
    let signer = Signer::new(GOOG4_HMAC_SHA256, "auto", "storage");
    let now = now_secs();
    let time =
        SigningTime::parse(&basic_timestamp(now)).map_err(|error| cli_error(error.to_string()))?;
    let expires = u32::try_from(ttl_seconds)
        .map_err(|_| cli_error("share link lifetime is too long".to_string()))?;

    let mut sources = serde_json::Map::new();
    for (name, kind) in [
        ("daily", RollupKind::Daily),
        ("weekly", RollupKind::Weekly),
        ("monthly", RollupKind::Monthly),
        ("models", RollupKind::Models),
    ] {
        let path = format!("/{bucket}/{}", keys.rollup(kind).path());
        let url = signer
            .signed_url(hmac, &time, "storage.googleapis.com", &path, expires)
            .map_err(|error| cli_error(error.to_string()))?;
        sources.insert(name.to_string(), Value::String(url));
    }
    sources.insert(
        "expiresAt".to_string(),
        Value::String(rfc3339(now + ttl_seconds)),
    );

    let payload = serde_json::to_vec(&Value::Object(sources))
        .map_err(|error| cli_error(format!("could not build the share link: {error}")))?;
    Ok(format!(
        "{}#s={}",
        public_url(bucket, keys),
        base64url(&payload)
    ))
}

fn human_ttl(seconds: u64) -> String {
    match seconds {
        0..=3599 => format!("{} minute(s)", seconds / 60),
        3600..=86399 => format!("{} hour(s)", seconds / 3600),
        _ => format!("{} day(s)", seconds / 86_400),
    }
}

/* ------------------------------------------------------------ public data */

/// The third-party price table and the equivalence map, which the page needs in
/// order to draw the comparison table before anyone has signed in.
///
/// Deliberately public: it is published price data, not the user's usage. The
/// table is filtered to the models the equivalence map can actually target, so
/// it stays small and so no user-derived model name can reach it.
fn public_data(pricing: &PricingMap) -> Vec<(String, Vec<u8>, &'static str)> {
    let map = EquivalenceMap::embedded();
    let mut models = serde_json::Map::new();
    for tier in &map.tiers {
        for target in tier.models.values() {
            if models.contains_key(target) {
                continue;
            }
            let Some(rates) = pricing.find_exact_with_fallback(target) else {
                continue;
            };
            const PER_MILLION: f64 = 1_000_000.0;
            models.insert(
                target.clone(),
                json!({
                    "input": rates.input * PER_MILLION,
                    "output": rates.output * PER_MILLION,
                    "cacheWrite": rates
                        .has_explicit_cache_creation_cost()
                        .then(|| rates.cache_creation_input_token_cost() * PER_MILLION),
                    "cacheRead": rates
                        .cache_read_explicit
                        .then_some(rates.cache_read * PER_MILLION),
                }),
            );
        }
    }

    let pricing_json = json!({
        "schema": 1,
        "attribution": ATTRIBUTION,
        "models": models,
    });
    vec![
        (
            "pricing.json".to_string(),
            serde_json::to_vec(&pricing_json).unwrap_or_default(),
            "application/json",
        ),
        (
            "model-equivalence.json".to_string(),
            serde_json::to_vec(&map).unwrap_or_default(),
            "application/json",
        ),
    ]
}

/* ------------------------------------------------------------ local server */

fn serve_locally(
    store: &GcsStore,
    keys: &KeySpace,
    public: &[(String, Vec<u8>, &'static str)],
    open: bool,
) -> Result<()> {
    let mut data: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, kind) in [
        ("daily", RollupKind::Daily),
        ("weekly", RollupKind::Weekly),
        ("monthly", RollupKind::Monthly),
        ("models", RollupKind::Models),
    ] {
        let key = keys.rollup(kind);
        match store.get(&key) {
            Ok(Some((bytes, _))) => data.push((format!("data/{name}.json"), bytes)),
            Ok(None) if name == "daily" => {
                return Err(cli_error(
                    "this bucket has no rollups yet. Run 'ccusage sync run' first.".to_string(),
                ));
            }
            Ok(None) => {}
            Err(error) => return Err(cli_error(format!("could not read {name}.json: {error}"))),
        }
    }
    check_schema(&data)?;

    let listener = bind_local()?;
    let port = listener
        .local_addr()
        .map_err(|error| cli_error(error.to_string()))?
        .port();
    let url = format!("http://127.0.0.1:{port}/");
    println!("Serving your dashboard at {url}");
    println!("The data is read with your own credentials and never leaves this machine.");
    println!("Press Ctrl-C to stop.");
    if open {
        open_in_browser(&url);
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = respond(stream, &data, public);
    }
    Ok(())
}

/// A rollup from a newer ccusage may mean something different by the same
/// field, so the page is not shown data it might misread.
fn check_schema(data: &[(String, Vec<u8>)]) -> Result<()> {
    for (name, bytes) in data {
        let parsed: Value = serde_json::from_slice(bytes)
            .map_err(|error| cli_error(format!("{name} is not valid JSON: {error}")))?;
        let schema = parsed.get("schema").and_then(Value::as_u64).unwrap_or(0);
        if schema > u64::from(ROLLUP_SCHEMA) {
            return Err(cli_error(format!(
                "{name} was written by a newer ccusage (rollup schema {schema}); upgrade to read it."
            )));
        }
    }
    Ok(())
}

fn bind_local() -> Result<TcpListener> {
    for port in FIRST_PORT..=LAST_PORT {
        if let Ok(listener) = TcpListener::bind(SocketAddr::from((LOCAL_HOST, port))) {
            return Ok(listener);
        }
    }
    TcpListener::bind(SocketAddr::from((LOCAL_HOST, 0)))
        .map_err(|error| cli_error(format!("could not listen on localhost: {error}")))
}

fn respond(
    mut stream: TcpStream,
    data: &[(String, Vec<u8>)],
    public: &[(String, Vec<u8>, &'static str)],
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let requested = path
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    let requested = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };

    let found: Option<(&[u8], &str)> = ASSETS
        .iter()
        .find(|asset: &&Asset| asset.path == requested)
        .map(|asset| (asset.bytes, asset.content_type))
        .or_else(|| {
            public
                .iter()
                .find(|(name, _, _)| name == requested)
                .map(|(_, bytes, content_type)| (bytes.as_slice(), *content_type))
        })
        .or_else(|| {
            data.iter()
                .find(|(name, _)| name == requested)
                .map(|(_, bytes)| (bytes.as_slice(), "application/json"))
        });

    let (status, body, content_type) = match found {
        Some((bytes, content_type)) => ("200 OK", bytes, content_type),
        None => ("404 Not Found", b"not found".as_slice(), "text/plain"),
    };
    // No caching locally: a second `sync run` should show up on reload, and
    // these bytes are the user's spend, so a disk cache is not wanted either.
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener).arg(url).spawn();
}

/* ------------------------------------------------------------------ timing */

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn basic_timestamp(epoch_secs: u64) -> String {
    let stamp = rfc3339(epoch_secs);
    stamp.replace(['-', ':'], "")
}

fn rfc3339(epoch_secs: u64) -> String {
    let timestamp = ccusage_core::TimestampMs::from_millis(
        i64::try_from(epoch_secs).unwrap_or_default() * 1000,
    );
    // Millisecond precision is not wanted in a V4 timestamp, and the fragment
    // reads better without it either.
    let full = ccusage_core::format_rfc3339_millis(timestamp);
    full.replace(".000", "")
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        let indices = [
            (packed >> 18) & 63,
            (packed >> 12) & 63,
            (packed >> 6) & 63,
            packed & 63,
        ];
        for (position, index) in indices.iter().enumerate() {
            if position <= chunk.len() {
                out.push(char::from(ALPHABET[*index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> PricingMap {
        PricingMap::load_with_overrides(true, false, std::collections::BTreeMap::new().iter())
    }

    #[test]
    fn the_published_price_table_holds_only_models_the_map_can_target() {
        let published = public_data(&pricing());
        let (_, bytes, _) = published
            .iter()
            .find(|(name, _, _)| name == "pricing.json")
            .expect("pricing.json is published");
        let value: Value = serde_json::from_slice(bytes).expect("valid JSON");

        let targets: Vec<String> = EquivalenceMap::embedded()
            .tiers
            .iter()
            .flat_map(|tier| tier.models.values().cloned())
            .collect();
        let models = value["models"].as_object().expect("a model table");
        assert!(!models.is_empty());
        for model in models.keys() {
            assert!(
                targets.contains(model),
                "{model} is not a comparison target"
            );
        }
        assert!(
            value["attribution"]
                .as_str()
                .is_some_and(|text| text.contains("LiteLLM"))
        );
    }

    /// Everything this function returns is uploaded under the world-readable
    /// prefix, so a machine id or a total reaching it would be a leak.
    #[test]
    fn nothing_published_is_derived_from_the_user() {
        for (_, bytes, _) in public_data(&pricing()) {
            let text = String::from_utf8(bytes).expect("UTF-8");
            for forbidden in ["machine", "userId", "cost", "shards"] {
                assert!(
                    !text.contains(forbidden),
                    "published data mentions {forbidden}"
                );
            }
        }
    }

    #[test]
    fn a_share_link_carries_its_urls_in_the_fragment_and_never_the_query() {
        let keys = KeySpace::new("ccusage/v1").expect("key space");
        let hmac = HmacKey::new("GOOG1EXAMPLE", "c2VjcmV0");

        let link = share_link("my-bucket", &keys, &hmac, 3600).expect("a link");

        let (base, fragment) = link.split_once('#').expect("a fragment");
        assert!(!base.contains('?'), "the page URL carries no query string");
        let encoded = fragment.strip_prefix("s=").expect("the sources parameter");
        assert!(!encoded.is_empty());
        assert!(base.ends_with("/dashboard/index.html"));
    }

    #[test]
    fn a_rollup_written_by_a_newer_ccusage_is_refused_rather_than_misread() {
        let newer = vec![(
            "data/daily.json".to_string(),
            br#"{"schema":99,"days":{}}"#.to_vec(),
        )];

        let error = check_schema(&newer).expect_err("refused");

        assert!(format!("{error}").contains("newer ccusage"));
    }

    #[test]
    fn the_base64url_encoding_round_trips_through_the_browsers_atob() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"a"), "YQ==");
        assert_eq!(base64url(b"ab"), "YWI=");
        assert_eq!(base64url(b"abc"), "YWJj");
        assert_eq!(base64url(br#"{"a":1}"#), "eyJhIjoxfQ==");
    }
}
