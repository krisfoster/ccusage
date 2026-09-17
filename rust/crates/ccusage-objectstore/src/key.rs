//! Every key in the bucket is built here.
//!
//! Two invariants are enforced at construction rather than by convention, because both are
//! silent-failure shaped: an identifier that escapes its path segment, and a data object whose
//! key falls inside the world-readable dashboard prefix.

use crate::error::{ObjectStoreError, Result};

/// Whether an object is readable by anyone on the internet.
///
/// Carried by the key rather than passed alongside it, so a store implementation cannot write an
/// object without having stated which access domain it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// The dashboard shell: HTML, JS, CSS, and third-party price data.
    Public,
    /// Everything derived from the user's usage.
    Private,
}

/// A validated object key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    path: String,
    visibility: Visibility,
}

impl Key {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    pub fn is_public(&self) -> bool {
        self.visibility == Visibility::Public
    }
}

/// Which rollup object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupKind {
    Daily,
    Weekly,
    Monthly,
    Models,
}

/// A calendar date in UTC. Shards are keyed by UTC date so the uploader's timezone never leaks
/// into the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcDate {
    year: i16,
    month: u8,
    day: u8,
}

impl UtcDate {
    pub fn new(year: i16, month: u8, day: u8) -> Result<Self> {
        let valid = (1970..=9999).contains(&year)
            && (1..=12).contains(&month)
            && day >= 1
            && day <= days_in_month(year, month);
        if !valid {
            return Err(ObjectStoreError::InvalidKey {
                key: format!("{year:04}-{month:02}-{day:02}"),
                reason: "not a real UTC date".to_string(),
            });
        }
        Ok(Self { year, month, day })
    }

    pub fn year(&self) -> i16 {
        self.year
    }

    pub fn month(&self) -> u8 {
        self.month
    }

    pub fn day(&self) -> u8 {
        self.day
    }
}

fn days_in_month(year: i16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// A path segment supplied by config or discovered identity, so it is untrusted: anything that
/// could add a segment, climb out of one, or confuse a signer is refused.
fn validate_segment(value: &str, what: &str) -> Result<()> {
    let invalid = |reason: &str| ObjectStoreError::InvalidKey {
        key: value.to_string(),
        reason: format!("{what} {reason}"),
    };
    if value.is_empty() {
        return Err(invalid("is empty"));
    }
    if value.len() > 64 {
        return Err(invalid("is longer than 64 characters"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(invalid("must be ASCII alphanumerics, '-', '_' or '.' only"));
    }
    if value.bytes().all(|b| b == b'.') {
        return Err(invalid("is a relative path segment"));
    }
    Ok(())
}

/// Builds every key under one bucket prefix.
#[derive(Debug, Clone)]
pub struct KeySpace {
    prefix: String,
}

const DASHBOARD_SEGMENT: &str = "dashboard";

impl KeySpace {
    pub fn new(prefix: &str) -> Result<Self> {
        let invalid = |reason: &str| ObjectStoreError::InvalidKey {
            key: prefix.to_string(),
            reason: reason.to_string(),
        };
        if prefix.is_empty() {
            return Err(invalid("prefix is empty"));
        }
        if prefix.starts_with('/') || prefix.ends_with('/') {
            return Err(invalid("prefix must not start or end with '/'"));
        }
        for segment in prefix.split('/') {
            validate_segment(segment, "prefix segment")?;
        }
        Ok(Self {
            prefix: prefix.to_string(),
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Every key that is world-readable starts with this, and no other key does.
    ///
    /// The bucket's public IAM binding is built from this, so the grant and the
    /// keys cannot drift apart.
    pub fn public_prefix(&self) -> String {
        format!("{}/{DASHBOARD_SEGMENT}/", self.prefix)
    }

    pub fn manifest(&self) -> Key {
        self.private("manifest.json")
    }

    pub fn machine(&self, user_id: &str, machine_id: &str) -> Result<Key> {
        Ok(self.private(&format!(
            "{}machine.json",
            self.machine_prefix_unchecked(user_id, machine_id)?
        )))
    }

    pub fn machine_index(&self, user_id: &str, machine_id: &str) -> Result<Key> {
        Ok(self.private(&format!(
            "{}index.json",
            self.machine_prefix_unchecked(user_id, machine_id)?
        )))
    }

    pub fn shard(
        &self,
        user_id: &str,
        machine_id: &str,
        agent: &str,
        date: UtcDate,
    ) -> Result<Key> {
        validate_segment(agent, "agent")?;
        let base = self.machine_prefix_unchecked(user_id, machine_id)?;
        Ok(self.private(&format!(
            "{base}shards/{agent}/{:04}/{:02}/{:02}.json",
            date.year, date.month, date.day
        )))
    }

    pub fn rollup(&self, kind: RollupKind) -> Key {
        let name = match kind {
            RollupKind::Daily => "daily",
            RollupKind::Weekly => "weekly",
            RollupKind::Monthly => "monthly",
            RollupKind::Models => "models",
        };
        self.private(&format!("rollup/{name}.json"))
    }

    /// The only constructor that produces a world-readable key.
    pub fn dashboard_asset(&self, relative: &str) -> Result<Key> {
        let invalid = |reason: &str| ObjectStoreError::InvalidKey {
            key: relative.to_string(),
            reason: reason.to_string(),
        };
        if relative.is_empty() {
            return Err(invalid("asset path is empty"));
        }
        if relative.starts_with('/') || relative.ends_with('/') {
            return Err(invalid("asset path must not start or end with '/'"));
        }
        for segment in relative.split('/') {
            validate_segment(segment, "asset path segment")?;
        }
        Ok(Key {
            path: format!("{}{relative}", self.public_prefix()),
            visibility: Visibility::Public,
        })
    }

    pub fn probe(&self, machine_id: &str) -> Result<Key> {
        validate_segment(machine_id, "machineId")?;
        Ok(self.private(&format!(".probe/{machine_id}")))
    }

    pub fn machine_prefix(&self, user_id: &str, machine_id: &str) -> Result<String> {
        let relative = self.machine_prefix_unchecked(user_id, machine_id)?;
        Ok(format!("{}/{relative}", self.prefix))
    }

    pub fn dashboard_prefix(&self) -> String {
        format!("{}/dashboard/", self.prefix)
    }

    fn machine_prefix_unchecked(&self, user_id: &str, machine_id: &str) -> Result<String> {
        validate_segment(user_id, "userId")?;
        validate_segment(machine_id, "machineId")?;
        Ok(format!("users/{user_id}/machines/{machine_id}/"))
    }

    fn private(&self, relative: &str) -> Key {
        debug_assert!(
            !relative.starts_with("dashboard/"),
            "a private object must never be keyed into the public dashboard prefix"
        );
        Key {
            path: format!("{}/{relative}", self.prefix),
            visibility: Visibility::Private,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> KeySpace {
        KeySpace::new("ccusage/v1").unwrap()
    }

    #[test]
    fn builds_the_documented_layout() {
        let s = space();
        assert_eq!(s.manifest().path(), "ccusage/v1/manifest.json");
        assert_eq!(
            s.machine("u1", "m1").unwrap().path(),
            "ccusage/v1/users/u1/machines/m1/machine.json"
        );
        assert_eq!(
            s.machine_index("u1", "m1").unwrap().path(),
            "ccusage/v1/users/u1/machines/m1/index.json"
        );
        assert_eq!(
            s.shard("u1", "m1", "claude", UtcDate::new(2026, 9, 7).unwrap())
                .unwrap()
                .path(),
            "ccusage/v1/users/u1/machines/m1/shards/claude/2026/09/07.json"
        );
        assert_eq!(
            s.rollup(RollupKind::Daily).path(),
            "ccusage/v1/rollup/daily.json"
        );
        assert_eq!(
            s.dashboard_asset("assets/app-9f3a.js").unwrap().path(),
            "ccusage/v1/dashboard/assets/app-9f3a.js"
        );
    }

    #[test]
    fn classifies_only_dashboard_assets_as_public() {
        let s = space();
        assert_eq!(s.manifest().visibility(), Visibility::Private);
        assert_eq!(
            s.machine("u1", "m1").unwrap().visibility(),
            Visibility::Private
        );
        assert_eq!(
            s.rollup(RollupKind::Models).visibility(),
            Visibility::Private
        );
        assert_eq!(
            s.dashboard_asset("index.html").unwrap().visibility(),
            Visibility::Public
        );
    }

    #[test]
    fn keeps_the_probe_object_out_of_the_public_prefix() {
        let s = space();
        let probe = s.probe("m1").unwrap();
        assert_eq!(probe.path(), "ccusage/v1/.probe/m1");
        assert_eq!(probe.visibility(), Visibility::Private);
        assert!(!probe.path().starts_with(&s.dashboard_prefix()));
    }

    #[test]
    fn rejects_identifiers_that_would_escape_their_segment() {
        let s = space();
        for bad in ["../admin", "a/b", "m1/../../etc", "", "  ", "a\u{0}b"] {
            assert!(
                s.machine("u1", bad).is_err(),
                "machineId {bad:?} should be rejected"
            );
            assert!(
                s.machine(bad, "m1").is_err(),
                "userId {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_dashboard_asset_paths_that_climb_out_of_the_prefix() {
        let s = space();
        for bad in [
            "../rollup/daily.json",
            "assets/../../manifest.json",
            "/etc/passwd",
            "",
        ] {
            assert!(
                s.dashboard_asset(bad).is_err(),
                "asset path {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_a_prefix_that_collides_with_the_public_domain() {
        assert!(KeySpace::new("").is_err());
        assert!(KeySpace::new("/leading").is_err());
        assert!(KeySpace::new("trailing/").is_err());
        assert!(KeySpace::new("has/../dots").is_err());
    }

    #[test]
    fn rejects_impossible_dates() {
        assert!(UtcDate::new(2026, 0, 1).is_err());
        assert!(UtcDate::new(2026, 13, 1).is_err());
        assert!(UtcDate::new(2026, 2, 30).is_err());
        assert!(UtcDate::new(2026, 9, 0).is_err());
        assert!(UtcDate::new(2026, 2, 29).is_err());
        assert!(UtcDate::new(2024, 2, 29).is_ok());
    }

    #[test]
    fn scopes_a_machine_prefix_for_deletion() {
        let s = space();
        assert_eq!(
            s.machine_prefix("u1", "m1").unwrap(),
            "ccusage/v1/users/u1/machines/m1/"
        );
    }

    #[test]
    fn reports_the_error_key_so_failures_name_the_object() {
        let s = space();
        let err = s.machine("u1", "../x").unwrap_err();
        assert!(
            matches!(err, ObjectStoreError::InvalidKey { .. }),
            "expected InvalidKey, got {err:?}"
        );
    }
}
