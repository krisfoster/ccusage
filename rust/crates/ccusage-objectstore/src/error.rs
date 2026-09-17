//! The error taxonomy every store implementation maps onto.
//!
//! The distinction that matters most is between errors the transport may retry on its own and a
//! CAS conflict, which it must never retry: a conflict means the caller's read is stale, so only
//! the caller can re-read, re-merge, and try again.

use std::fmt;

pub type Result<T> = std::result::Result<T, ObjectStoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStoreError {
    /// A precondition failed (HTTP 412). The object changed under us.
    Conflict {
        key: String,
    },
    NotFound {
        key: String,
    },
    /// Authenticated, but not allowed. `detail` should name the missing role where known.
    Forbidden {
        key: String,
        detail: String,
    },
    /// No usable credentials, or they expired. `source` names which credential source was used,
    /// because "permission denied" with no provenance is the least actionable error there is.
    Unauthenticated {
        source: String,
        detail: String,
    },
    RateLimited {
        retry_after_ms: Option<u64>,
    },
    Server {
        status: u16,
        detail: String,
    },
    Network {
        detail: String,
    },
    InvalidKey {
        key: String,
        reason: String,
    },
    Other {
        detail: String,
    },
}

impl ObjectStoreError {
    /// Whether the transport may retry this request unchanged.
    ///
    /// `Conflict` is deliberately false: retrying the same body against a changed object would
    /// clobber whatever the other writer just wrote, which is the exact thing CAS exists to stop.
    pub fn is_transport_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Server { .. } | Self::Network { .. }
        )
    }

    /// Whether the caller can retry after re-reading the object.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }

    /// Maps an HTTP status onto the taxonomy. `key` is the object the request was for.
    pub fn from_status(status: u16, key: &str, detail: &str) -> Self {
        match status {
            401 => Self::Unauthenticated {
                source: String::new(),
                detail: detail.to_string(),
            },
            403 => Self::Forbidden {
                key: key.to_string(),
                detail: detail.to_string(),
            },
            404 => Self::NotFound {
                key: key.to_string(),
            },
            412 => Self::Conflict {
                key: key.to_string(),
            },
            429 => Self::RateLimited {
                retry_after_ms: None,
            },
            500..=599 => Self::Server {
                status,
                detail: detail.to_string(),
            },
            _ => Self::Other {
                detail: detail.to_string(),
            },
        }
    }

    /// Records which credential source produced an authentication failure.
    pub fn with_credential_source(self, source: &str) -> Self {
        match self {
            Self::Unauthenticated { detail, .. } => Self::Unauthenticated {
                source: source.to_string(),
                detail,
            },
            other => other,
        }
    }
}

impl fmt::Display for ObjectStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { key } => {
                write!(f, "{key} changed while we were writing it")
            }
            Self::NotFound { key } => write!(f, "{key} does not exist"),
            Self::Forbidden { key, detail } => {
                write!(f, "not allowed to access {key}: {detail}")
            }
            Self::Unauthenticated { source, detail } => {
                write!(f, "credentials from {source} were rejected: {detail}")
            }
            Self::RateLimited { retry_after_ms } => match retry_after_ms {
                Some(ms) => write!(f, "rate limited, retry after {ms}ms"),
                None => write!(f, "rate limited"),
            },
            Self::Server { status, detail } => {
                write!(f, "storage returned {status}: {detail}")
            }
            Self::Network { detail } => write!(f, "network error: {detail}"),
            Self::InvalidKey { key, reason } => {
                write!(f, "invalid object key {key:?}: {reason}")
            }
            Self::Other { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ObjectStoreError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_statuses_onto_the_taxonomy() {
        let cases: [(u16, ObjectStoreError); 6] = [
            (
                404,
                ObjectStoreError::NotFound {
                    key: "k".to_string(),
                },
            ),
            (
                412,
                ObjectStoreError::Conflict {
                    key: "k".to_string(),
                },
            ),
            (
                403,
                ObjectStoreError::Forbidden {
                    key: "k".to_string(),
                    detail: "d".to_string(),
                },
            ),
            (
                429,
                ObjectStoreError::RateLimited {
                    retry_after_ms: None,
                },
            ),
            (
                503,
                ObjectStoreError::Server {
                    status: 503,
                    detail: "d".to_string(),
                },
            ),
            (
                418,
                ObjectStoreError::Other {
                    detail: "d".to_string(),
                },
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(
                ObjectStoreError::from_status(status, "k", "d"),
                expected,
                "status {status}"
            );
        }
    }

    #[test]
    fn maps_401_to_unauthenticated_so_the_message_can_name_the_credential_source() {
        assert!(matches!(
            ObjectStoreError::from_status(401, "k", "d"),
            ObjectStoreError::Unauthenticated { .. }
        ));
    }

    #[test]
    fn never_lets_the_transport_retry_a_conflict() {
        let conflict = ObjectStoreError::from_status(412, "k", "d");
        assert!(!conflict.is_transport_retryable());
        assert!(conflict.is_conflict());
    }

    #[test]
    fn marks_throttling_and_server_and_network_errors_as_retryable() {
        assert!(ObjectStoreError::from_status(429, "k", "d").is_transport_retryable());
        assert!(ObjectStoreError::from_status(500, "k", "d").is_transport_retryable());
        assert!(
            ObjectStoreError::Network {
                detail: "reset".to_string()
            }
            .is_transport_retryable()
        );
        assert!(!ObjectStoreError::from_status(404, "k", "d").is_transport_retryable());
        assert!(!ObjectStoreError::from_status(403, "k", "d").is_transport_retryable());
    }

    #[test]
    fn names_the_object_and_the_credential_source_when_displayed() {
        let forbidden = ObjectStoreError::Forbidden {
            key: "ccusage/v1/manifest.json".to_string(),
            detail: "missing storage.objects.create".to_string(),
        };
        let shown = forbidden.to_string();
        assert!(shown.contains("ccusage/v1/manifest.json"), "{shown}");
        assert!(shown.contains("missing storage.objects.create"), "{shown}");

        let unauth = ObjectStoreError::Unauthenticated {
            source: "ADC (~/.config/gcloud/application_default_credentials.json)".to_string(),
            detail: "token expired".to_string(),
        };
        assert!(
            unauth.to_string().contains("application_default"),
            "{unauth}"
        );
    }
}
