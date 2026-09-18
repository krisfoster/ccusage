//! Writing the values `sync setup` discovers back into `ccusage.json`.
//!
//! Setup is the only thing in ccusage that edits the user's config, so it edits
//! as little as possible: the file is re-read, the `sync` object is merged key by
//! key, and everything else — including comments-as-keys, unknown future options
//! and the user's own formatting choices beyond whitespace — survives untouched.
//! Nothing written here is a credential; the values are a bucket name, a project,
//! and two identifiers, and `config::from_args` rejects the file outright if a
//! secret-looking key ever appears in it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

use crate::config::discovered_config_paths;

/// The values setup resolves. `None` leaves whatever the file already has.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncWriteback {
    pub provider: Option<String>,
    pub project_id: Option<String>,
    pub bucket: Option<String>,
    pub location: Option<String>,
    pub prefix: Option<String>,
    pub machine_id: Option<String>,
    pub user_id: Option<String>,
    /// Not a credential: it only makes the bucket's hashes unguessable, and
    /// every machine writing to the bucket needs the same one.
    pub salt: Option<String>,
}

impl SyncWriteback {
    fn entries(&self) -> Vec<(&'static str, &String)> {
        [
            ("provider", self.provider.as_ref()),
            ("projectId", self.project_id.as_ref()),
            ("bucket", self.bucket.as_ref()),
            ("location", self.location.as_ref()),
            ("prefix", self.prefix.as_ref()),
            ("machineId", self.machine_id.as_ref()),
            ("userId", self.user_id.as_ref()),
            ("salt", self.salt.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, value)))
        .collect()
    }
}

/// Where setup should write when the user did not pass `--config`.
///
/// The first existing discovered file wins, so a project-local `.ccusage` keeps
/// owning the settings of a machine that has one. Otherwise the home-scoped file
/// is created: a machine ID that lived in one checkout would be minted again in
/// the next, and two shards would then claim the same machine.
pub fn sync_writeback_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let candidates = discovered_config_paths();
    candidates
        .iter()
        .find(|path| path.is_file())
        .or_else(|| candidates.last())
        .cloned()
}

pub fn persist_sync(path: &Path, values: &SyncWriteback) -> Result<(), String> {
    let mut document = read_document(path)?;
    let sync = document
        .entry("sync")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("{}: 'sync' is not an object", path.display()))?;
    for (key, value) in values.entries() {
        sync.insert(key.to_string(), Value::String(value.clone()));
    }
    write_document(path, &document)
}

/// Drops the whole `sync` block, so the machine no longer points at a bucket.
///
/// Returns whether there was one to drop, which is what tells `sync remove`
/// whether to say it cleaned the config or that there was nothing in it.
pub fn clear_sync(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut document = read_document(path)?;
    if document.remove("sync").is_none() {
        return Ok(false);
    }
    write_document(path, &document)?;
    Ok(true)
}

fn read_document(path: &Path) -> Result<Map<String, Value>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if contents.trim().is_empty() {
        return Ok(Map::new());
    }
    // Refusing to parse is the right answer: overwriting a file the user hand-
    // edited into invalidity would lose settings they still want.
    match serde_json::from_str::<Value>(&contents) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(format!("{}: expected a JSON object", path.display())),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Written through a temporary file in the same directory, so an interrupted
/// setup cannot leave a truncated config behind.
fn write_document(path: &Path, document: &Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let mut serialized = serde_json::to_string_pretty(&Value::Object(document.clone()))
        .map_err(|error| error.to_string())?;
    serialized.push('\n');
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serialized)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::fs_fixture;
    use ccusage_test_support::secrets::assert_no_secrets;

    use super::*;

    #[test]
    fn creates_the_file_with_only_the_sync_block() {
        let fixture = fs_fixture!({});
        let path = fixture.path("ccusage.json");

        persist_sync(
            &path,
            &SyncWriteback {
                bucket: Some("ccusage-9f3a1c2b".to_string()),
                project_id: Some("my-project".to_string()),
                ..SyncWriteback::default()
            },
        )
        .expect("persist");

        assert_eq!(
            read(&path),
            json!({
                "sync": {
                    "bucket": "ccusage-9f3a1c2b",
                    "projectId": "my-project",
                }
            })
        );
    }

    #[test]
    fn clearing_sync_leaves_every_other_setting_behind() {
        let fixture = fs_fixture!({
            "ccusage.json": r#"{
                "mode": "calculate",
                "sync": { "bucket": "ccusage-9f3a1c2b" }
            }"#,
        });
        let path = fixture.path("ccusage.json");

        assert!(clear_sync(&path).expect("clear"));

        assert_eq!(read(&path), json!({ "mode": "calculate" }));
    }

    /// `sync remove` calls this even when setup never wrote anything, and a
    /// missing block is not a failure to report.
    #[test]
    fn clearing_a_config_with_no_sync_block_reports_nothing_to_do() {
        let fixture = fs_fixture!({ "ccusage.json": r#"{ "mode": "calculate" }"# });
        let path = fixture.path("ccusage.json");

        assert!(!clear_sync(&path).expect("clear"));
        assert!(!clear_sync(&fixture.path("absent.json")).expect("clear"));
        assert_eq!(read(&path), json!({ "mode": "calculate" }));
    }

    #[test]
    fn keeps_every_setting_the_user_already_had() {
        let fixture = fs_fixture!({
            "ccusage.json": r#"{
                "mode": "calculate",
                "sync": { "prefix": "ccusage/v1", "redactProjects": true }
            }"#,
        });
        let path = fixture.path("ccusage.json");

        persist_sync(
            &path,
            &SyncWriteback {
                bucket: Some("ccusage-9f3a1c2b".to_string()),
                ..SyncWriteback::default()
            },
        )
        .expect("persist");

        assert_eq!(
            read(&path),
            json!({
                "mode": "calculate",
                "sync": {
                    "prefix": "ccusage/v1",
                    "redactProjects": true,
                    "bucket": "ccusage-9f3a1c2b",
                }
            })
        );
    }

    #[test]
    fn an_absent_value_leaves_the_existing_one_alone() {
        let fixture = fs_fixture!({
            "ccusage.json": r#"{ "sync": { "bucket": "ccusage-existing" } }"#,
        });
        let path = fixture.path("ccusage.json");

        persist_sync(
            &path,
            &SyncWriteback {
                project_id: Some("my-project".to_string()),
                ..SyncWriteback::default()
            },
        )
        .expect("persist");

        assert_eq!(read(&path)["sync"]["bucket"], json!("ccusage-existing"));
    }

    #[test]
    fn refuses_to_overwrite_a_config_it_cannot_parse() {
        let fixture = fs_fixture!({
            "ccusage.json": "{ not json",
        });
        let path = fixture.path("ccusage.json");

        let error = persist_sync(&path, &SyncWriteback::default())
            .expect_err("a config that cannot be read cannot be safely rewritten");

        assert!(error.contains("ccusage.json"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{ not json");
    }

    #[test]
    fn leaves_no_temporary_file_behind() {
        let fixture = fs_fixture!({});
        let path = fixture.path("ccusage.json");

        persist_sync(
            &path,
            &SyncWriteback {
                bucket: Some("ccusage-9f3a1c2b".to_string()),
                ..SyncWriteback::default()
            },
        )
        .expect("persist");

        assert!(!fixture.path("ccusage.json.tmp").exists());
    }

    /// Setup holds live credentials while it runs; the file it writes is
    /// world-readable by the user's other tools and often committed by accident,
    /// so the set of keys it may contain is an allowlist rather than a habit.
    #[test]
    fn the_written_config_can_only_contain_non_secret_settings() {
        let fixture = fs_fixture!({});
        let path = fixture.path("ccusage.json");

        persist_sync(
            &path,
            &SyncWriteback {
                provider: Some("gcs".to_string()),
                project_id: Some("my-project".to_string()),
                bucket: Some("ccusage-9f3a1c2b".to_string()),
                location: Some("US".to_string()),
                prefix: Some("ccusage/v1".to_string()),
                machine_id: Some("a1b2c3".to_string()),
                user_id: Some("u-1234".to_string()),
                salt: Some("00112233445566778899aabbccddeeff".to_string()),
            },
        )
        .expect("persist");

        let contents = fs::read_to_string(&path).expect("read");
        assert_no_secrets(&contents, &[]);
        let written = read(&path);
        let allowed = [
            "provider",
            "projectId",
            "bucket",
            "location",
            "prefix",
            "machineId",
            "userId",
            "salt",
        ];
        for key in written["sync"].as_object().expect("sync block").keys() {
            assert!(allowed.contains(&key.as_str()), "setup wrote '{key}'");
        }
    }

    #[test]
    fn an_explicit_config_path_is_the_writeback_target() {
        let path = PathBuf::from("/tmp/somewhere/ccusage.json");

        assert_eq!(sync_writeback_path(Some(&path)), Some(path));
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("parse")
    }
}
