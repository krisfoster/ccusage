//! A hashed hardware-ish fingerprint, used only to notice that a config was copied
//! to another machine.
//!
//! It is never an identity: DR-03 rules the hardware sources out for that because
//! they collide across cloned VMs and containers. A collision here costs a missed
//! warning, and an absent source costs the warning entirely, which is why every
//! read failure degrades to `None` instead of an error.

use std::{env, fs, path::PathBuf};

use sha2::{Digest, Sha256};

/// Hashed and truncated so a serial number never reaches config, a bucket, or a log.
const FINGERPRINT_HEX_LENGTH: usize = 16;

/// Candidate sources in priority order, injectable so the reader is testable
/// without a host that happens to have the files.
#[derive(Clone, Debug, Default)]
pub struct FingerprintSources {
    pub files: Vec<PathBuf>,
    pub env_vars: Vec<String>,
}

impl FingerprintSources {
    /// Linux and the BSDs keep a stable ID in a readable file. macOS hides
    /// `IOPlatformUUID` behind IOKit and Windows keeps `MachineGuid` in the
    /// registry; neither is worth a dependency or a subprocess for a warning, so
    /// those platforms fall back to the host name and otherwise go without.
    pub fn platform_default() -> Self {
        Self {
            files: vec![
                PathBuf::from("/etc/machine-id"),
                PathBuf::from("/var/lib/dbus/machine-id"),
            ],
            env_vars: vec!["COMPUTERNAME".to_string(), "HOSTNAME".to_string()],
        }
    }
}

pub fn observe_fingerprint(sources: &FingerprintSources) -> Option<String> {
    let from_file = sources.files.iter().find_map(|path| {
        let contents = fs::read_to_string(path).ok()?;
        let trimmed = contents.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });
    let raw = from_file.or_else(|| {
        sources
            .env_vars
            .iter()
            .find_map(|name| env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    })?;
    Some(hash_fingerprint(&raw))
}

fn hash_fingerprint(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ccusage-machine-fingerprint\0");
    hasher.update(raw.trim().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(FINGERPRINT_HEX_LENGTH)
        .collect()
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::fs_fixture;

    use super::*;

    #[test]
    fn hashes_the_first_readable_source() {
        let fixture = fs_fixture!({
            "machine-id": "3f8d2a1c9b7e4f60\n",
        });

        let fingerprint = observe_fingerprint(&sources(&[
            fixture.path("missing"),
            fixture.path("machine-id"),
        ]))
        .expect("a readable source yields a fingerprint");

        assert_eq!(fingerprint.len(), FINGERPRINT_HEX_LENGTH);
        assert!(!fingerprint.contains("3f8d2a1c9b7e4f60"));
    }

    #[test]
    fn the_same_machine_reads_the_same_fingerprint() {
        let fixture = fs_fixture!({
            "machine-id": "3f8d2a1c9b7e4f60",
            "other-machine-id": "0000000000000000",
        });

        let sources = sources(&[fixture.path("machine-id")]);
        assert_eq!(
            observe_fingerprint(&sources),
            observe_fingerprint(&sources.clone())
        );
        assert_ne!(
            observe_fingerprint(&sources),
            observe_fingerprint(&self::sources(&[fixture.path("other-machine-id")]))
        );
    }

    #[test]
    fn an_empty_file_is_not_a_fingerprint() {
        let fixture = fs_fixture!({
            "machine-id": "   \n",
        });

        assert_eq!(
            observe_fingerprint(&sources(&[fixture.path("machine-id")])),
            None
        );
    }

    #[test]
    fn no_readable_source_degrades_to_no_fingerprint() {
        let fixture = fs_fixture!({});

        assert_eq!(
            observe_fingerprint(&sources(&[fixture.path("missing")])),
            None
        );
    }

    fn sources(files: &[PathBuf]) -> FingerprintSources {
        FingerprintSources {
            files: files.to_vec(),
            env_vars: Vec::new(),
        }
    }
}
