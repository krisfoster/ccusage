//! `ccusage sync dashboard`: publish the page, or host it locally.
//!
//! Two access models, and the difference is where the *data* comes from — the
//! page itself is the same bytes in both:
//!
//! - **Local** (the default): a server bound to loopback reads the rollups with
//!   this machine's own credentials and serves them alongside the bundle.
//!   Nothing becomes public, and no token leaves the process.
//! - **Deployed** (`--deploy`): the bundle is uploaded to a *second* bucket,
//!   `<data-bucket>-dashboard`, which is world-readable and holds nothing else.
//!   A prefix-scoped public binding inside the data bucket would be neater, but
//!   GCS refuses an IAM condition on an `allUsers` member, so the only boundary
//!   it will actually enforce is the bucket. The rollups stay in the private
//!   data bucket, so a deployed page shows nothing until it is opened with a
//!   `--share` link, whose signed URLs ride in the location fragment and
//!   therefore never reach a server log.
//!
//! The public/private split is enforced twice over: by the bucket, and by the
//! key space — `dashboard_asset` is the only constructor that yields a public
//! key, and every rollup key is private by construction.

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
        bucket::{BucketAdmin, BucketSpec, PublicAccessPrevention},
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

    let assets_bucket = assets_bucket(&bucket);

    if args.deploy {
        let Some(project) = status.project_id.clone() else {
            return Err(cli_error(
                "sync has no project recorded. Re-run 'ccusage sync setup'.".to_string(),
            ));
        };
        deploy(
            &keys,
            &assets_bucket,
            &project,
            status
                .location
                .as_deref()
                .unwrap_or(super::DEFAULT_LOCATION),
            &credentials,
            &public,
        )?;
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
        Some(share_link(
            &bucket,
            &assets_bucket,
            &keys,
            hmac,
            args.share_ttl_seconds,
        )?)
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
            let url = public_url(&assets_bucket, &keys);
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

/// The page lives in its own bucket, next to the data bucket and named after
/// it.
///
/// GCS refuses an IAM condition on an `allUsers` binding, so a public *prefix*
/// inside the data bucket is not expressible: making the shell readable would
/// make every object readable. Splitting buckets makes the boundary the thing
/// GCS actually enforces, and leaves the data bucket able to keep public access
/// prevention enforced.
fn assets_bucket(data_bucket: &str) -> String {
    /// Cloud Storage caps a bucket name at 63 characters, so a long data bucket
    /// loses its tail rather than producing a name the API refuses.
    const MAX_NAME: usize = 63;
    const SUFFIX: &str = "-dashboard";

    let head_len = MAX_NAME - SUFFIX.len();
    let head = data_bucket
        .get(..head_len)
        .unwrap_or(data_bucket)
        .trim_end_matches(['-', '.', '_']);
    format!("{head}{SUFFIX}")
}

fn deploy(
    keys: &KeySpace,
    assets_bucket: &str,
    project: &str,
    location: &str,
    credentials: &Arc<crate::credentials::Credentials>,
    public: &[(String, Vec<u8>, &'static str)],
) -> Result<()> {
    let admin = BucketAdmin::new(
        JsonApi::new(
            STORAGE_ENDPOINT,
            Box::new(Arc::clone(credentials)),
            RetryPolicy::default(),
        ),
        assets_bucket,
    );
    let mut spec = BucketSpec::new(project, location);
    spec.public_access_prevention = PublicAccessPrevention::Inherited;
    admin.ensure(&spec).map_err(|error| {
        cli_error(format!(
            "could not create the dashboard bucket {assets_bucket}: {error}"
        ))
    })?;
    // An org policy may enforce prevention on new buckets, which would refuse
    // the public binding below with a bare 403.
    if let Ok(Some(info)) = admin.get()
        && info.public_access_prevention == Some(PublicAccessPrevention::Enforced)
    {
        admin
            .set_public_access_prevention(PublicAccessPrevention::Inherited)
            .map_err(|error| {
                cli_error(format!(
                    "{assets_bucket} prevents public access and it could not be relaxed: {error}. \
                     A published dashboard needs public access prevention set to 'inherited' on \
                     that bucket; your usage data is in a different bucket and stays private."
                ))
            })?;
    }

    let store = GcsStore::new(assets_bucket, Box::new(Arc::clone(credentials)));
    for asset in ASSETS {
        put_public(&store, keys, asset.path, asset.bytes, asset.content_type)?;
    }
    for (path, bytes, content_type) in public {
        put_public(&store, keys, path, bytes, content_type)?;
    }

    let granted = admin
        .grant_public_read()
        .map_err(|error| cli_error(error.to_string()))?;
    println!(
        "Uploaded {} file(s) to gs://{assets_bucket}/{}{}",
        ASSETS.len() + public.len(),
        keys.public_prefix(),
        if granted {
            ", and made that bucket world-readable. It holds the page only; your usage data is in \
             a separate, private bucket."
        } else {
            "; that bucket was already world-readable."
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
fn share_link(
    bucket: &str,
    assets_bucket: &str,
    keys: &KeySpace,
    hmac: &HmacKey,
    ttl_seconds: u64,
) -> Result<String> {
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
        public_url(assets_bucket, keys),
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
        for (provider_id, target) in &tier.models {
            if models.contains_key(target) {
                continue;
            }
            let Some(rates) = pricing.find_exact_with_fallback(target) else {
                continue;
            };
            let provider = map.provider(provider_id);
            const PER_MILLION: f64 = 1_000_000.0;
            models.insert(
                target.clone(),
                json!({
                    "provider": provider.map(|provider| provider.label.clone()),
                    "tier": tier.label,
                    // The rate itself comes from the licensed snapshot; the
                    // link is where a reader can check it against the vendor.
                    "source": provider.and_then(|provider| provider.pricing_url.clone()),
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

    let mut host = String::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("host")
        {
            host = value.trim().to_string();
        }
    }
    if !host_is_loopback(&host) {
        let body = b"the ccusage dashboard answers loopback names only".as_slice();
        stream.write_all(
            format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )?;
        stream.write_all(body)?;
        return stream.flush();
    }
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

/// The server binds loopback, but a name that *resolves* to loopback is enough
/// for a page on the internet to talk to it (DNS rebinding), and this one hands
/// out the user's spend. Only the names a local browser would actually send are
/// answered.
fn host_is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        // An IPv6 literal is bracketed, so the colons inside it are not a port.
        Some(rest) => rest.split_once(']').map_or(rest, |(name, _)| name),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
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
    // The URL-safe alphabet, and not merely as a nicety: the payload rides in a
    // fragment the page reads with `URLSearchParams`, which decodes `+` as a
    // space and would quietly corrupt every link containing one.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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

    /// A target the price map cannot resolve drops out of the table without a
    /// word, so the provider just looks like one nobody can be compared to.
    #[test]
    fn every_comparison_target_has_a_published_price_and_a_source() {
        let published = public_data(&pricing());
        let (_, bytes, _) = published
            .iter()
            .find(|(name, _, _)| name == "pricing.json")
            .expect("pricing.json is published");
        let value: Value = serde_json::from_slice(bytes).expect("valid JSON");
        let models = value["models"].as_object().expect("a model table");

        for tier in EquivalenceMap::embedded().tiers {
            for target in tier.models.values() {
                let row = models
                    .get(target)
                    .unwrap_or_else(|| panic!("{target} has no published price"));
                assert!(
                    row["source"]
                        .as_str()
                        .is_some_and(|url| url.starts_with("https://")),
                    "{target} has no source link"
                );
                assert!(row["input"].as_f64().is_some_and(|rate| rate > 0.0));
                assert!(row["output"].as_f64().is_some_and(|rate| rate > 0.0));
            }
        }
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

        let link = share_link("my-bucket", &assets_bucket("my-bucket"), &keys, &hmac, 3600)
            .expect("a link");

        let (base, fragment) = link.split_once('#').expect("a fragment");
        assert!(!base.contains('?'), "the page URL carries no query string");
        let encoded = fragment.strip_prefix("s=").expect("the sources parameter");
        assert!(!encoded.is_empty());
        assert!(base.ends_with("/dashboard/index.html"));
        assert!(base.contains("/my-bucket-dashboard/"), "{base}");
    }

    /// The page is world-readable and the data is not, which on GCS can only be
    /// two buckets: a condition on an `allUsers` binding is rejected outright.
    #[test]
    fn the_page_is_served_from_a_different_bucket_than_the_data() {
        let assets = assets_bucket("my-bucket");

        assert_ne!(assets, "my-bucket");
        assert!(assets.starts_with("my-bucket"), "{assets}");
        assert!(assets.len() <= 63, "bucket names cap at 63 characters");

        let long = assets_bucket(&"a".repeat(63));
        assert_eq!(long.len(), 63, "{long}");
        assert!(long.ends_with("-dashboard"), "{long}");
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

    /// `URLSearchParams` reads `+` as a space, so a payload encoded with the
    /// standard alphabet decodes to rubbish for the links unlucky enough to
    /// contain one.
    #[test]
    fn the_fragment_payload_uses_url_safe_characters_only() {
        let awkward: Vec<u8> = (0u8..=255).collect();

        let encoded = base64url(&awkward);

        assert!(
            encoded
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '=')),
            "{encoded}"
        );
    }

    /// Drives `respond` over a real socket: the header parsing, the Host
    /// check and the framing are the parts a browser meets first, and a test
    /// that called the helpers directly would skip all three.
    fn request(line: &str, host: Option<&str>) -> String {
        use std::io::Read as _;

        let listener =
            TcpListener::bind(SocketAddr::from((LOCAL_HOST, 0))).expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let request = match host {
            Some(host) => format!("{line} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
            None => format!("{line} HTTP/1.1\r\n\r\n"),
        };
        let client = std::thread::spawn(move || {
            let mut stream =
                TcpStream::connect(SocketAddr::from((LOCAL_HOST, port))).expect("a connection");
            stream.write_all(request.as_bytes()).expect("a request");
            let mut response = Vec::new();
            stream.read_to_end(&mut response).expect("a response");
            String::from_utf8_lossy(&response).into_owned()
        });

        let (stream, _) = listener.accept().expect("a connection");
        let data = vec![(
            "data/daily.json".to_string(),
            br#"{"schema":1,"days":{}}"#.to_vec(),
        )];
        let public = vec![(
            "pricing.json".to_string(),
            br#"{"models":{}}"#.to_vec(),
            "application/json",
        )];
        respond(stream, &data, &public).expect("a served response");
        client.join().expect("the client thread")
    }

    #[test]
    fn a_bare_request_is_answered_with_the_page_and_no_store() {
        let response = request("GET /", Some("127.0.0.1:8787"));

        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Content-Type: text/html"), "{response}");
        assert!(response.contains("Cache-Control: no-store"), "{response}");
        assert!(
            response.contains("X-Content-Type-Options: nosniff"),
            "{response}"
        );
        // Byte-identical to the file the HTML linter checks, so linting the
        // source lints what a browser is actually handed.
        let page = std::str::from_utf8(ASSETS[0].bytes).expect("UTF-8");
        assert!(response.ends_with(page), "{response}");
    }

    #[test]
    fn the_rollups_and_the_price_table_are_served_alongside_the_page() {
        let rollup = request("GET /data/daily.json", Some("localhost:8787"));
        let prices = request("GET /pricing.json?v=2", Some("localhost:8787"));

        assert!(rollup.contains("application/json"), "{rollup}");
        assert!(rollup.ends_with(r#"{"schema":1,"days":{}}"#), "{rollup}");
        assert!(prices.ends_with(r#"{"models":{}}"#), "{prices}");
    }

    #[test]
    fn an_unknown_path_is_a_404_rather_than_the_page() {
        let response = request("GET /../etc/passwd", Some("localhost"));

        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
        assert!(response.ends_with("not found"), "{response}");
    }

    /// A page on the internet can point a name it owns at 127.0.0.1 and then
    /// read whatever answers, which here is the user's spend.
    #[test]
    fn a_request_addressed_to_someone_elses_name_is_refused() {
        let rebound = request("GET /data/daily.json", Some("spend.attacker.example:8787"));
        let anonymous = request("GET /", None);

        assert!(rebound.starts_with("HTTP/1.1 403 Forbidden"), "{rebound}");
        assert!(!rebound.contains("schema"), "{rebound}");
        assert!(
            anonymous.starts_with("HTTP/1.1 403 Forbidden"),
            "{anonymous}"
        );
    }

    #[test]
    fn the_local_server_binds_loopback_only() {
        let listener = bind_local().expect("a listener");

        let address = listener.local_addr().expect("an address");

        assert!(address.ip().is_loopback(), "{address}");
        assert!(
            (FIRST_PORT..=LAST_PORT).contains(&address.port()) || address.port() > 0,
            "{address}"
        );
    }

    #[test]
    fn a_link_lifetime_is_described_in_the_largest_unit_that_fits() {
        assert_eq!(human_ttl(600), "10 minute(s)");
        assert_eq!(human_ttl(7_200), "2 hour(s)");
        assert_eq!(human_ttl(172_800), "2 day(s)");
    }

    #[test]
    fn the_published_page_url_points_at_the_public_prefix() {
        let keys = KeySpace::new("ccusage/v1").expect("key space");

        let url = public_url("my-bucket-dashboard", &keys);

        assert_eq!(
            url,
            "https://storage.googleapis.com/my-bucket-dashboard/ccusage/v1/dashboard/index.html"
        );
    }

    /// V4 signing wants a basic-format timestamp, and a signature built from a
    /// stamp the server reads differently fails with an opaque 403.
    #[test]
    fn the_signing_timestamp_drops_its_separators_and_its_milliseconds() {
        assert_eq!(rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(basic_timestamp(1_767_225_600), "20260101T000000Z");
        assert!(now_secs() > 1_700_000_000);
    }

    #[test]
    fn only_loopback_host_names_are_answered() {
        assert!(host_is_loopback("127.0.0.1:8787"));
        assert!(host_is_loopback("localhost:8787"));
        assert!(host_is_loopback("[::1]:8787"));
        assert!(host_is_loopback("localhost"));

        assert!(!host_is_loopback(""), "a request without a Host header");
        assert!(!host_is_loopback("spend.attacker.example:8787"));
        assert!(!host_is_loopback("127.0.0.1.attacker.example"));
    }
}
