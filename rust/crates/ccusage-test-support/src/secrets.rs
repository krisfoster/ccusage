//! One assertion every sync feature reuses: whatever this code rendered, no
//! credential material is in it.
//!
//! Sync grows secret-bearing paths faster than it grows tests for them — bearer
//! tokens, HMAC secrets, signed-URL signatures, private keys — and each one is
//! forgettable in isolation. Keeping the shapes in a single helper means a new
//! secret kind is taught to every existing test at once, and that a test written
//! against a literal value ("my-secret") also catches the generic shapes it did
//! not think to look for.

/// Credential shapes that must never reach output, a log line, or a config file.
const SECRET_SHAPES: &[(&str, &str)] = &[
    ("Google OAuth access token", "ya29."),
    ("HMAC access id", "GOOG1E"),
    ("authorization header", "Authorization: Bearer"),
    ("authorization header", "authorization: bearer"),
    ("signed URL signature", "X-Goog-Signature="),
    ("signed URL credential", "X-Goog-Credential="),
    ("private key", "-----BEGIN PRIVATE KEY-----"),
    ("private key", "-----BEGIN RSA PRIVATE KEY-----"),
    ("OAuth refresh token", "\"refresh_token\""),
    ("OAuth client secret", "\"client_secret\""),
];

/// Fails when `rendered` contains any known credential shape, or any of the
/// literal secrets the test itself planted.
///
/// Pass the exact values the test fed in via `planted`: the generic shapes catch
/// the credentials that look like credentials, and `planted` catches the ones
/// that look like nothing in particular.
#[track_caller]
pub fn assert_no_secrets(rendered: &str, planted: &[&str]) {
    for (kind, needle) in SECRET_SHAPES {
        assert!(
            !rendered.contains(needle),
            "output leaks a {kind} ({needle:?}):\n{rendered}"
        );
    }
    for secret in planted {
        assert!(
            !secret.is_empty(),
            "an empty string is in every output; pass the real secret value"
        );
        assert!(
            !rendered.contains(secret),
            "output leaks the secret it was given:\n{rendered}"
        );
    }
}

/// The same check for anything with a `Debug` or `Display` rendering, which is
/// where secrets usually escape: an error formatted into a message.
#[track_caller]
pub fn assert_debug_redacted(value: &impl std::fmt::Debug, planted: &[&str]) {
    assert_no_secrets(&format!("{value:?}"), planted);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_passes() {
        assert_no_secrets("bucket ccusage-abc123 in project demo", &["hunter2"]);
    }

    #[test]
    #[should_panic(expected = "leaks a Google OAuth access token")]
    fn a_leaked_access_token_is_caught_without_the_test_naming_it() {
        assert_no_secrets("using token ya29.a0AfH6SMB", &[]);
    }

    #[test]
    #[should_panic(expected = "leaks a signed URL signature")]
    fn a_signed_url_is_caught_because_the_signature_is_a_bearer_credential() {
        assert_no_secrets("https://storage.googleapis.com/o?X-Goog-Signature=abc", &[]);
    }

    #[test]
    #[should_panic(expected = "leaks the secret it was given")]
    fn an_unremarkable_looking_secret_is_caught_when_planted() {
        assert_no_secrets("secret=correct-horse", &["correct-horse"]);
    }

    #[test]
    fn debug_renderings_are_checked_the_same_way() {
        #[derive(Debug)]
        struct Credential {
            #[allow(dead_code)]
            secret: &'static str,
        }

        assert_debug_redacted(
            &Credential {
                secret: "<redacted>",
            },
            &["correct-horse"],
        );
    }
}
