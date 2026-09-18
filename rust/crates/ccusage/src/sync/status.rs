//! `ccusage sync status`: what this machine would sync, where, and as whom.
//!
//! Deliberately offline. Status is the command a user runs when something looks
//! wrong, and a network round trip there turns a config question into a
//! credential question; `sync doctor` is the one that talks to Google. Rendering
//! is separated from reading so both shapes are asserted without a config file.

use ccusage_config::config_schema::SyncConfig;
use serde_json::{Value, json};

use super::DEFAULT_PREFIX;

/// The resolved view of the `sync` config block.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Status {
    pub(crate) provider: Option<String>,
    pub(crate) project_id: Option<String>,
    pub(crate) bucket: Option<String>,
    pub(crate) location: Option<String>,
    pub(crate) prefix: String,
    pub(crate) machine_id: Option<String>,
    pub(crate) machine_label: Option<String>,
    pub(crate) user_id: Option<String>,
}

impl Status {
    pub(crate) fn from_config(config: Option<&SyncConfig>) -> Self {
        let Some(config) = config else {
            return Self {
                prefix: DEFAULT_PREFIX.to_string(),
                ..Self::default()
            };
        };
        Self {
            provider: config.provider.as_ref().map(|_| "gcs".to_string()),
            project_id: config.project_id.clone(),
            bucket: config.bucket.clone(),
            location: config.location.clone(),
            prefix: config
                .prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
            machine_id: config.machine_id.clone(),
            machine_label: config.machine_label.clone(),
            user_id: config.user_id.clone(),
        }
    }

    /// A bucket is what makes sync possible; everything else has a default.
    pub(crate) fn is_configured(&self) -> bool {
        self.bucket.is_some()
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "configured": self.is_configured(),
            "provider": self.provider,
            "projectId": self.project_id,
            "bucket": self.bucket,
            "location": self.location,
            "prefix": self.prefix,
            "machineId": self.machine_id,
            "machineLabel": self.machine_label,
            "userId": self.user_id,
            // The last sync time lives in the bucket's machine record, and
            // status does not read the bucket. The field is kept so the shape
            // does not change for a caller once it does.
            "lastSyncAt": Value::Null,
        })
    }

    pub(crate) fn to_text(&self) -> String {
        if !self.is_configured() {
            return "Sync is not configured. Run 'ccusage sync setup' to create a bucket."
                .to_string();
        }
        let unset = "not yet assigned";
        let mut lines = vec![
            format!(
                "Provider    {}",
                self.provider.as_deref().unwrap_or("gcs (assumed)")
            ),
            format!(
                "Project     {}",
                self.project_id.as_deref().unwrap_or("unknown")
            ),
            format!(
                "Bucket      gs://{}",
                self.bucket.as_deref().unwrap_or_default()
            ),
            format!(
                "Location    {}",
                self.location.as_deref().unwrap_or("unknown")
            ),
            format!("Prefix      {}", self.prefix),
            format!("User        {}", self.user_id.as_deref().unwrap_or(unset)),
        ];
        let machine = match (&self.machine_id, &self.machine_label) {
            (Some(id), Some(label)) => format!("{id} ({label})"),
            (Some(id), None) => id.clone(),
            (None, _) => unset.to_string(),
        };
        lines.push(format!("Machine     {machine}"));
        lines.push("Last sync   never".to_string());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::secrets::assert_no_secrets;

    use super::*;

    fn configured() -> SyncConfig {
        SyncConfig {
            project_id: Some("my-project".to_string()),
            bucket: Some("ccusage-9f3a1c2b4405".to_string()),
            location: Some("US".to_string()),
            machine_id: Some("a1b2c3".to_string()),
            machine_label: Some("laptop".to_string()),
            user_id: Some("u-1234".to_string()),
            ..SyncConfig::default()
        }
    }

    #[test]
    fn an_unconfigured_machine_is_told_how_to_configure_itself() {
        let status = Status::from_config(None);

        assert!(!status.is_configured());
        assert!(
            status.to_text().contains("ccusage sync setup"),
            "{status:?}"
        );
        assert_eq!(status.to_json()["configured"], json!(false));
    }

    #[test]
    fn the_prefix_falls_back_to_the_default_layout() {
        let status = Status::from_config(Some(&SyncConfig::default()));

        assert_eq!(status.prefix, DEFAULT_PREFIX);
    }

    #[test]
    fn reports_the_configured_target_and_identity() {
        let status = Status::from_config(Some(&configured()));

        let text = status.to_text();
        assert!(text.contains("gs://ccusage-9f3a1c2b4405"), "{text}");
        assert!(text.contains("my-project"), "{text}");
        assert!(text.contains("a1b2c3 (laptop)"), "{text}");
    }

    /// Status is the command users paste into issues, so it renders where the
    /// target came from but never what authenticated to it.
    #[test]
    fn neither_rendering_carries_credential_material() {
        let status = Status::from_config(Some(&configured()));

        assert_no_secrets(&status.to_text(), &[]);
        assert_no_secrets(&status.to_json().to_string(), &[]);
    }

    #[test]
    fn the_json_shape_is_stable_for_scripts() {
        let status = Status::from_config(Some(&configured()));

        assert_eq!(
            status.to_json(),
            json!({
                "configured": true,
                "provider": Value::Null,
                "projectId": "my-project",
                "bucket": "ccusage-9f3a1c2b4405",
                "location": "US",
                "prefix": "ccusage/v1",
                "machineId": "a1b2c3",
                "machineLabel": "laptop",
                "userId": "u-1234",
                "lastSyncAt": Value::Null,
            })
        );
    }

    #[test]
    fn an_identity_that_setup_has_not_minted_yet_is_named_as_such() {
        let status = Status::from_config(Some(&SyncConfig {
            bucket: Some("ccusage-9f3a1c2b4405".to_string()),
            ..SyncConfig::default()
        }));

        assert!(status.to_text().contains("not yet assigned"));
    }
}
