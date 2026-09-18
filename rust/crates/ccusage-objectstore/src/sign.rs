//! V4 request signing: `GOOG4-HMAC-SHA256` for Cloud Storage, `AWS4-HMAC-SHA256` for the S3-shaped
//! providers we expect to add later.
//!
//! The two are the same algorithm with different literals, so the scheme is a parameter rather
//! than a second implementation. Keeping the signer here — away from the HTTP client — means it
//! is testable against published vectors without a socket.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::error::{ObjectStoreError, Result};

/// The provider-specific literals of the V4 scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    /// Value of the `Authorization` prefix and the `X-…-Algorithm` query parameter.
    pub algorithm: &'static str,
    /// Prefix of the seed signing key.
    pub key_prefix: &'static str,
    /// Terminator of the credential scope.
    pub request_type: &'static str,
    /// Header and query-parameter namespace, e.g. `x-goog`.
    pub param_prefix: &'static str,
}

/// Cloud Storage with an HMAC key.
pub const GOOG4_HMAC_SHA256: Scheme = Scheme {
    algorithm: "GOOG4-HMAC-SHA256",
    key_prefix: "GOOG4",
    request_type: "goog4_request",
    param_prefix: "x-goog",
};

/// S3 and the S3-compatible providers (R2, MinIO).
pub const AWS4_HMAC_SHA256: Scheme = Scheme {
    algorithm: "AWS4-HMAC-SHA256",
    key_prefix: "AWS4",
    request_type: "aws4_request",
    param_prefix: "x-amz",
};

/// An HMAC credential. `Debug` is redacted because these end up in error paths.
#[derive(Clone)]
pub struct HmacKey {
    pub access_id: String,
    secret: String,
}

impl HmacKey {
    pub fn new(access_id: &str, secret: &str) -> Self {
        Self {
            access_id: access_id.to_string(),
            secret: secret.to_string(),
        }
    }
}

impl fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HmacKey")
            .field("access_id", &self.access_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// A signing instant, as the two formats V4 needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningTime {
    date_stamp: String,
    timestamp: String,
}

impl SigningTime {
    /// `timestamp` is basic-format UTC, `YYYYMMDDTHHMMSSZ`.
    pub fn parse(timestamp: &str) -> Result<Self> {
        let bytes = timestamp.as_bytes();
        let shaped = bytes.len() == 16
            && bytes[..8].iter().all(u8::is_ascii_digit)
            && bytes[8] == b'T'
            && bytes[9..15].iter().all(u8::is_ascii_digit)
            && bytes[15] == b'Z';
        if !shaped {
            return Err(ObjectStoreError::Other {
                detail: format!("signing timestamp {timestamp:?} is not YYYYMMDDTHHMMSSZ"),
            });
        }
        Ok(Self {
            date_stamp: timestamp[..8].to_string(),
            timestamp: timestamp.to_string(),
        })
    }

    pub fn date_stamp(&self) -> &str {
        &self.date_stamp
    }

    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

/// A request to sign. Headers and query parameters are canonicalized here, so callers pass them
/// in whatever order is convenient.
#[derive(Debug, Clone)]
pub struct CanonicalRequest<'a> {
    pub method: &'a str,
    /// Already-decoded object path, e.g. `/bucket/ccusage/v1/manifest.json`.
    pub path: &'a str,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    /// Hex SHA-256 of the body, or `UNSIGNED-PAYLOAD`.
    pub payload_hash: &'a str,
}

/// Where the signature is carried.
#[derive(Debug, Clone)]
pub struct Signer {
    scheme: Scheme,
    region: String,
    service: String,
}

impl Signer {
    pub fn new(scheme: Scheme, region: &str, service: &str) -> Self {
        Self {
            scheme,
            region: region.to_string(),
            service: service.to_string(),
        }
    }

    pub fn credential_scope(&self, time: &SigningTime) -> String {
        format!(
            "{}/{}/{}/{}",
            time.date_stamp, self.region, self.service, self.scheme.request_type
        )
    }

    pub fn canonical_request(&self, request: &CanonicalRequest<'_>) -> String {
        let headers = canonical_headers(&request.headers);
        let signed = signed_headers(&request.headers);
        let mut query: Vec<(String, String)> = request
            .query
            .iter()
            .map(|(name, value)| (uri_encode(name, true), uri_encode(value, true)))
            .collect();
        query.sort();
        let query = query
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");

        format!(
            "{}\n{}\n{query}\n{headers}\n{signed}\n{}",
            request.method,
            uri_encode(request.path, false),
            request.payload_hash
        )
    }

    pub fn string_to_sign(&self, time: &SigningTime, canonical_request: &str) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.scheme.algorithm,
            time.timestamp,
            self.credential_scope(time),
            sha256_hex(canonical_request.as_bytes())
        )
    }

    pub fn signature(&self, key: &HmacKey, time: &SigningTime, string_to_sign: &str) -> String {
        let seed = format!("{}{}", self.scheme.key_prefix, key.secret);
        let date_key = hmac_sha256(seed.as_bytes(), time.date_stamp.as_bytes());
        let region_key = hmac_sha256(&date_key, self.region.as_bytes());
        let service_key = hmac_sha256(&region_key, self.service.as_bytes());
        let signing_key = hmac_sha256(&service_key, self.scheme.request_type.as_bytes());
        hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()))
    }

    /// The full `Authorization` header value for a header-signed request.
    pub fn authorization_header(
        &self,
        key: &HmacKey,
        time: &SigningTime,
        request: &CanonicalRequest<'_>,
    ) -> String {
        let string_to_sign = self.string_to_sign(time, &self.canonical_request(request));
        format!(
            "{} Credential={}/{}, SignedHeaders={}, Signature={}",
            self.scheme.algorithm,
            key.access_id,
            self.credential_scope(time),
            signed_headers(&request.headers),
            self.signature(key, time, &string_to_sign)
        )
    }

    /// A query-signed URL. `expires_secs` is clamped to the V4 maximum of 7 days.
    pub fn signed_url(
        &self,
        key: &HmacKey,
        time: &SigningTime,
        host: &str,
        path: &str,
        expires_secs: u32,
    ) -> Result<String> {
        const MAX_EXPIRES_SECS: u32 = 7 * 24 * 3600;
        if expires_secs == 0 || expires_secs > MAX_EXPIRES_SECS {
            return Err(ObjectStoreError::Other {
                detail: format!(
                    "signed link lifetime {expires_secs}s is outside 1..={MAX_EXPIRES_SECS}s"
                ),
            });
        }

        // Signed under exactly the names the URL carries. The canonical query
        // string is the query string as sent, and a provider matches parameter
        // names by code point, so signing `x-goog-date` and sending
        // `X-Goog-Date` yields a signature the far end cannot reproduce.
        let prefix = header_case(self.scheme.param_prefix);
        let credential = format!("{}/{}", key.access_id, self.credential_scope(time));
        let query = vec![
            (
                format!("{prefix}-Algorithm"),
                self.scheme.algorithm.to_string(),
            ),
            (format!("{prefix}-Credential"), credential),
            (format!("{prefix}-Date"), time.timestamp.clone()),
            (format!("{prefix}-Expires"), expires_secs.to_string()),
            (format!("{prefix}-SignedHeaders"), "host".to_string()),
        ];
        let headers = vec![("host".to_string(), host.to_string())];
        let canonical = self.canonical_request(&CanonicalRequest {
            method: "GET",
            path,
            query: query.clone(),
            headers: headers.clone(),
            payload_hash: "UNSIGNED-PAYLOAD",
        });
        let signature = self.signature(key, time, &self.string_to_sign(time, &canonical));

        let mut rendered: Vec<String> = query
            .iter()
            .map(|(name, value)| format!("{}={}", uri_encode(name, true), uri_encode(value, true)))
            .collect();
        rendered.sort();
        let rendered = rendered.join("&");
        Ok(format!(
            "https://{host}{}?{rendered}&{prefix}-Signature={signature}",
            uri_encode(path, false),
        ))
    }
}

/// Hex-encoded SHA-256, used for payload hashes and for content hashes elsewhere in sync.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `x-goog` -> `X-Goog`: the casing providers document for the signed-URL
/// parameters, and therefore the casing they must be signed under.
fn header_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Lowercased names, trimmed values, internal runs of spaces folded to one.
fn canonical_headers(headers: &[(String, String)]) -> String {
    let mut rows: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    rows.sort();
    let mut out = String::new();
    for (name, value) in rows {
        out.push_str(&format!("{name}:{value}\n"));
    }
    out
}

fn signed_headers(headers: &[(String, String)]) -> String {
    let mut names: Vec<String> = headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    names.join(";")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// RFC 3986 encoding. `encode_slash` is false for object paths, where `/` is a separator, and
/// true everywhere else.
fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // aws-sig-v4-test-suite `get-vanilla`, as published by AWS and mirrored in aws-c-auth.
    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const TIMESTAMP: &str = "20150830T123600Z";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn aws_signer() -> Signer {
        Signer::new(AWS4_HMAC_SHA256, "us-east-1", "service")
    }

    fn get_vanilla<'a>() -> CanonicalRequest<'a> {
        CanonicalRequest {
            method: "GET",
            path: "/",
            query: vec![],
            headers: vec![
                ("Host".to_string(), "example.amazonaws.com".to_string()),
                ("X-Amz-Date".to_string(), TIMESTAMP.to_string()),
            ],
            payload_hash: EMPTY_SHA256,
        }
    }

    #[test]
    fn hashes_the_nist_vectors() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn builds_the_published_canonical_request() {
        let expected = concat!(
            "GET\n",
            "/\n",
            "\n",
            "host:example.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\n",
            "host;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(aws_signer().canonical_request(&get_vanilla()), expected);
    }

    #[test]
    fn builds_the_published_string_to_sign() {
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        let signer = aws_signer();
        let canonical = signer.canonical_request(&get_vanilla());
        let expected = concat!(
            "AWS4-HMAC-SHA256\n",
            "20150830T123600Z\n",
            "20150830/us-east-1/service/aws4_request\n",
            // sha256 of the canonical request above, which is itself pinned by the published
            // vector: the signature test below derives from this string and matches AWS's.
            "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(signer.string_to_sign(&time, &canonical), expected);
    }

    #[test]
    fn reproduces_the_published_signature() {
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        let signer = aws_signer();
        let sts = signer.string_to_sign(&time, &signer.canonical_request(&get_vanilla()));
        assert_eq!(
            signer.signature(&HmacKey::new(ACCESS_KEY, SECRET), &time, &sts),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn reproduces_the_published_authorization_header() {
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        assert_eq!(
            aws_signer().authorization_header(
                &HmacKey::new(ACCESS_KEY, SECRET),
                &time,
                &get_vanilla()
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn sorts_and_folds_headers_the_way_the_spec_requires() {
        let signer = aws_signer();
        let request = CanonicalRequest {
            method: "GET",
            path: "/",
            query: vec![],
            headers: vec![
                ("X-Amz-Date".to_string(), TIMESTAMP.to_string()),
                ("Host".to_string(), "  example.amazonaws.com  ".to_string()),
                ("My-Header".to_string(), "a     b".to_string()),
            ],
            payload_hash: EMPTY_SHA256,
        };
        let canonical = signer.canonical_request(&request);
        let lines: Vec<&str> = canonical.lines().collect();
        assert_eq!(lines[3], "host:example.amazonaws.com");
        assert_eq!(lines[4], "my-header:a b");
        assert_eq!(lines[5], "x-amz-date:20150830T123600Z");
        assert_eq!(lines[7], "host;my-header;x-amz-date");
    }

    #[test]
    fn sorts_and_encodes_the_query_string() {
        let signer = aws_signer();
        let request = CanonicalRequest {
            method: "GET",
            path: "/",
            query: vec![
                ("b".to_string(), "two words".to_string()),
                ("a".to_string(), "x/y".to_string()),
            ],
            headers: vec![("Host".to_string(), "example.amazonaws.com".to_string())],
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(
            signer.canonical_request(&request).lines().nth(2).unwrap(),
            "a=x%2Fy&b=two%20words"
        );
    }

    #[test]
    fn keeps_slashes_in_the_path_but_encodes_the_rest() {
        let signer = aws_signer();
        let request = CanonicalRequest {
            method: "GET",
            path: "/bucket/ccusage/v1/roll up.json",
            query: vec![],
            headers: vec![("Host".to_string(), "example.amazonaws.com".to_string())],
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(
            signer.canonical_request(&request).lines().nth(1).unwrap(),
            "/bucket/ccusage/v1/roll%20up.json"
        );
    }

    #[test]
    fn uses_the_google_literals_for_the_gcs_scheme() {
        let signer = Signer::new(GOOG4_HMAC_SHA256, "us-central1", "storage");
        let time = SigningTime::parse("20191201T190859Z").unwrap();
        assert_eq!(
            signer.credential_scope(&time),
            "20191201/us-central1/storage/goog4_request"
        );
        let sts = signer.string_to_sign(&time, "canonical");
        assert!(
            sts.starts_with("GOOG4-HMAC-SHA256\n20191201T190859Z\n"),
            "{sts}"
        );
    }

    #[test]
    fn builds_a_signed_url_carrying_every_required_parameter() {
        let signer = Signer::new(GOOG4_HMAC_SHA256, "auto", "storage");
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        let url = signer
            .signed_url(
                &HmacKey::new(ACCESS_KEY, SECRET),
                &time,
                "storage.googleapis.com",
                "/bucket/ccusage/v1/rollup/daily.json",
                900,
            )
            .unwrap();

        assert!(
            url.starts_with("https://storage.googleapis.com/bucket/ccusage/v1/rollup/daily.json?")
        );
        for expected in [
            "X-Goog-Algorithm=GOOG4-HMAC-SHA256",
            "X-Goog-Date=20150830T123600Z",
            "X-Goog-Expires=900",
            "X-Goog-SignedHeaders=host",
            "X-Goog-Credential=AKIDEXAMPLE%2F20150830%2Fauto%2Fstorage%2Fgoog4_request",
            "X-Goog-Signature=",
        ] {
            assert!(url.contains(expected), "{expected} missing from {url}");
        }
        assert!(!url.contains(SECRET), "the secret must never reach the URL");
    }

    /// The far end recomputes the signature over the query string it receives,
    /// so the names signed and the names sent have to be the same bytes. This
    /// re-derives the signature from the URL as written and compares.
    #[test]
    fn signs_the_query_string_it_actually_sends() {
        let signer = Signer::new(GOOG4_HMAC_SHA256, "auto", "storage");
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        let key = HmacKey::new(ACCESS_KEY, SECRET);
        let path = "/bucket/ccusage/v1/rollup/daily.json";
        let url = signer
            .signed_url(&key, &time, "storage.googleapis.com", path, 900)
            .unwrap();

        let (_, query) = url.split_once('?').expect("a query string");
        let mut sent: Vec<(String, String)> = Vec::new();
        let mut claimed = String::new();
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').expect("name=value");
            if name == "X-Goog-Signature" {
                claimed = value.to_string();
            } else {
                sent.push((name.to_string(), decode(value)));
            }
        }

        let canonical = signer.canonical_request(&CanonicalRequest {
            method: "GET",
            path,
            query: sent,
            headers: vec![("host".to_string(), "storage.googleapis.com".to_string())],
            payload_hash: "UNSIGNED-PAYLOAD",
        });
        assert_eq!(
            signer.signature(&key, &time, &signer.string_to_sign(&time, &canonical)),
            claimed
        );
    }

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = String::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let hex = &value[index + 1..index + 3];
                out.push(char::from(u8::from_str_radix(hex, 16).expect("hex")));
                index += 3;
            } else {
                out.push(char::from(bytes[index]));
                index += 1;
            }
        }
        out
    }

    #[test]
    fn refuses_an_expiry_beyond_the_seven_day_maximum() {
        let signer = Signer::new(GOOG4_HMAC_SHA256, "auto", "storage");
        let time = SigningTime::parse(TIMESTAMP).unwrap();
        let err = signer
            .signed_url(
                &HmacKey::new(ACCESS_KEY, SECRET),
                &time,
                "storage.googleapis.com",
                "/bucket/x.json",
                7 * 24 * 3600 + 1,
            )
            .unwrap_err();
        assert!(matches!(err, ObjectStoreError::Other { .. }), "{err:?}");
    }

    #[test]
    fn rejects_timestamps_that_are_not_basic_format_utc() {
        for bad in [
            "2015-08-30T12:36:00Z",
            "20150830T123600",
            "20150830t123600Z",
            "",
        ] {
            assert!(
                SigningTime::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        let good = SigningTime::parse(TIMESTAMP).unwrap();
        assert_eq!(good.date_stamp(), "20150830");
    }

    #[test]
    fn keeps_the_secret_out_of_debug_output() {
        let shown = format!("{:?}", HmacKey::new(ACCESS_KEY, SECRET));
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(shown.contains(ACCESS_KEY), "{shown}");
    }
}
