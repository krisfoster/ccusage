//! The unit of synced usage: one agent's activity on one machine for one UTC day.
//!
//! Two properties drive the shape. Cells are keyed by a 15-minute UTC bucket
//! because that is the smallest interval every real IANA offset re-aggregates
//! cleanly — `+05:45` included — so a viewer can show local days without the
//! writer having picked a timezone. And the dedupe keys hang off each
//! `(bucket, model)` cell rather than off the shard, because an intersection has
//! to say *which* tokens two machines both reported; a shard-level set only says
//! "something in this day overlaps", which cannot be acted on without
//! over-subtracting real usage.
//!
//! `content_hash` is computed over a canonical byte encoding written by hand
//! rather than over the JSON, so two machines that folded the same entries agree
//! on the hash regardless of serializer version, map ordering, or float
//! formatting. Everything sorts before it is hashed or serialized for the same
//! reason.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped when a reader written against the current shape would misread a shard.
/// Readers never parse a higher version leniently — see [`ParsedShard`].
pub const SHARD_SCHEMA: u32 = 1;

/// 96 fifteen-minute buckets in a UTC day.
pub const BUCKETS_PER_DAY: u16 = 96;

/// Costs are a convenience snapshot, not the authority, so the hash pins them at
/// a tenth of a microdollar and stops worrying about float formatting.
const COST_HASH_SCALE: f64 = 1e7;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShardError {
    BucketOutOfRange { bucket: u16 },
    EmptyModel,
    Malformed { detail: String },
}

impl std::fmt::Display for ShardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BucketOutOfRange { bucket } => write!(
                formatter,
                "bucket {bucket} is outside the {BUCKETS_PER_DAY} fifteen-minute buckets of a UTC day"
            ),
            Self::EmptyModel => formatter.write_str("a usage cell must name a model"),
            Self::Malformed { detail } => write!(formatter, "malformed shard: {detail}"),
        }
    }
}

impl std::error::Error for ShardError {}

/// One model's usage inside one 15-minute UTC bucket.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Cell {
    /// Bucket index within the UTC day, `0..96`.
    #[serde(rename = "i")]
    pub bucket: u16,
    #[serde(rename = "m")]
    pub model: String,
    #[serde(rename = "in", default, skip_serializing_if = "is_zero")]
    pub input_tokens: u64,
    #[serde(rename = "out", default, skip_serializing_if = "is_zero")]
    pub output_tokens: u64,
    #[serde(rename = "cw", default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
    #[serde(rename = "cr", default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u64,
    #[serde(rename = "cost", default)]
    pub cost: f64,
    #[serde(rename = "msgs", default, skip_serializing_if = "is_zero_u32")]
    pub messages: u32,
    /// Salted, truncated dedupe keys for the entries folded into this cell,
    /// sorted and deduplicated. Rendered as 16 hex characters each.
    #[serde(rename = "k", default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<DedupeKey>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// A 64-bit salted digest of an entry's `message.id` + `request_id`.
///
/// Stored as hex rather than as a number because JSON numbers above 2^53 do not
/// survive a round trip through a browser, and the dashboard reads these.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DedupeKey(pub u64);

impl std::fmt::Display for DedupeKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

impl Serialize for DedupeKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DedupeKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let rendered = String::deserialize(deserializer)?;
        u64::from_str_radix(&rendered, 16)
            .map(Self)
            .map_err(|_| serde::de::Error::custom(format!("not a dedupe key: {rendered}")))
    }
}

/// Enough of a session to attribute spend to a project without naming it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub id: String,
    /// Salted hash of the project path, never the path itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// First and last activity as epoch milliseconds.
    pub first: i64,
    pub last: i64,
    #[serde(default)]
    pub cost: f64,
}

/// How the dedupe keys in this shard were produced. A reader that cannot
/// reproduce the algorithm or does not hold the salt must not intersect them.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Dedupe {
    pub algo: String,
    /// Identifier of the per-bucket salt, never the salt itself.
    pub salt: String,
    pub count: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shard {
    pub schema: u32,
    pub agent: String,
    pub machine_id: String,
    pub user_id: String,
    /// `YYYY-MM-DD`, always UTC.
    pub utc_date: String,
    pub generated_at: String,
    pub ccusage_version: String,
    pub cost_mode: String,
    pub pricing_snapshot: String,
    #[serde(rename = "buckets")]
    pub cells: Vec<Cell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Session>,
    pub dedupe: Dedupe,
    /// Set by [`Shard::finish`]; covers every field above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

/// The result of reading an object out of the bucket.
///
/// A machine that auto-updated first writes shards this build cannot read. The
/// only safe readings are "all of it" and "none of it": parsing a newer shard
/// leniently would drop the fields it does not know about and silently
/// under-report the user's spend, so a newer shard is carried as an opaque
/// blob that rollups exclude and the CLI counts out loud.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedShard {
    Known(Box<Shard>),
    Newer { schema: u32, bytes: Vec<u8> },
}

impl ParsedShard {
    pub fn known(self) -> Option<Shard> {
        match self {
            Self::Known(shard) => Some(*shard),
            Self::Newer { .. } => None,
        }
    }
}

impl Shard {
    /// Sorts every collection into the one order all machines agree on.
    ///
    /// Folding is order-dependent — two machines walk their logs differently —
    /// so normalization is what makes `content_hash` a statement about content
    /// rather than about iteration order.
    pub fn normalize(&mut self) {
        for cell in &mut self.cells {
            cell.keys.sort_unstable();
            cell.keys.dedup();
        }
        self.cells.retain(|cell| !cell.is_empty());
        self.cells
            .sort_by(|left, right| (left.bucket, &left.model).cmp(&(right.bucket, &right.model)));
        self.sessions.sort_by(|left, right| left.id.cmp(&right.id));
        self.dedupe.count = self.cells.iter().map(|cell| cell.keys.len() as u64).sum();
    }

    pub fn validate(&self) -> Result<(), ShardError> {
        for cell in &self.cells {
            if cell.bucket >= BUCKETS_PER_DAY {
                return Err(ShardError::BucketOutOfRange {
                    bucket: cell.bucket,
                });
            }
            if cell.model.is_empty() {
                return Err(ShardError::EmptyModel);
            }
        }
        Ok(())
    }

    /// Normalizes, validates, and stamps the content hash. Call before writing.
    pub fn finish(&mut self) -> Result<&str, ShardError> {
        self.normalize();
        self.validate()?;
        self.content_hash = None;
        let hash = self.compute_content_hash();
        self.content_hash = Some(hash);
        Ok(self
            .content_hash
            .as_deref()
            .expect("the hash was just assigned"))
    }

    /// A canonical encoding of everything except `content_hash`, hashed.
    ///
    /// Hand-rolled rather than "hash the JSON" so the digest survives a
    /// serializer upgrade, and length-prefixed so no pair of adjacent fields can
    /// be shifted between each other without changing the digest.
    pub fn compute_content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        };
        field(&self.schema.to_be_bytes());
        field(self.agent.as_bytes());
        field(self.machine_id.as_bytes());
        field(self.user_id.as_bytes());
        field(self.utc_date.as_bytes());
        field(self.cost_mode.as_bytes());
        field(self.pricing_snapshot.as_bytes());
        field(self.dedupe.algo.as_bytes());
        field(self.dedupe.salt.as_bytes());
        field(&(self.cells.len() as u64).to_be_bytes());
        for cell in &self.cells {
            field(&cell.bucket.to_be_bytes());
            field(cell.model.as_bytes());
            field(&cell.input_tokens.to_be_bytes());
            field(&cell.output_tokens.to_be_bytes());
            field(&cell.cache_write_tokens.to_be_bytes());
            field(&cell.cache_read_tokens.to_be_bytes());
            field(&scaled_cost(cell.cost).to_be_bytes());
            field(&cell.messages.to_be_bytes());
            field(&(cell.keys.len() as u64).to_be_bytes());
            for key in &cell.keys {
                field(&key.0.to_be_bytes());
            }
        }
        field(&(self.sessions.len() as u64).to_be_bytes());
        for session in &self.sessions {
            field(session.id.as_bytes());
            field(session.project.as_deref().unwrap_or("").as_bytes());
            field(&session.first.to_be_bytes());
            field(&session.last.to_be_bytes());
            field(&scaled_cost(session.cost).to_be_bytes());
        }
        let digest: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{digest}")
    }

    /// Whether the stamped hash still describes the contents.
    pub fn content_hash_matches(&self) -> bool {
        let Some(stamped) = self.content_hash.as_deref() else {
            return false;
        };
        let mut copy = self.clone();
        copy.content_hash = None;
        copy.compute_content_hash() == stamped
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ShardError> {
        serde_json::to_vec(self).map_err(|error| ShardError::Malformed {
            detail: error.to_string(),
        })
    }

    /// Reads a shard, or preserves it verbatim when it was written by a newer
    /// ccusage. Only a shard this build fully understands is parsed.
    pub fn parse(bytes: &[u8]) -> Result<ParsedShard, ShardError> {
        let schema = schema_of(bytes)?;
        if schema > SHARD_SCHEMA {
            return Ok(ParsedShard::Newer {
                schema,
                bytes: bytes.to_vec(),
            });
        }
        let shard: Self = serde_json::from_slice(bytes).map_err(|error| ShardError::Malformed {
            detail: error.to_string(),
        })?;
        shard.validate()?;
        Ok(ParsedShard::Known(Box::new(shard)))
    }
}

impl Cell {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_write_tokens + self.cache_read_tokens
    }

    fn is_empty(&self) -> bool {
        self.total_tokens() == 0 && self.messages == 0 && self.keys.is_empty()
    }
}

fn scaled_cost(cost: f64) -> i64 {
    (cost * COST_HASH_SCALE).round() as i64
}

/// The schema field alone, read without committing to the rest of the shape.
fn schema_of(bytes: &[u8]) -> Result<u32, ShardError> {
    #[derive(Deserialize)]
    struct SchemaOnly {
        schema: u32,
    }

    serde_json::from_slice::<SchemaOnly>(bytes)
        .map(|parsed| parsed.schema)
        .map_err(|error| ShardError::Malformed {
            detail: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(bucket: u16, model: &str, keys: &[u64]) -> Cell {
        Cell {
            bucket,
            model: model.to_string(),
            input_tokens: 1_200,
            output_tokens: 830,
            cache_write_tokens: 4_000,
            cache_read_tokens: 91_000,
            cost: 0.042_1,
            messages: 3,
            keys: keys.iter().copied().map(DedupeKey).collect(),
        }
    }

    fn shard(cells: Vec<Cell>) -> Shard {
        Shard {
            schema: SHARD_SCHEMA,
            agent: "claude".to_string(),
            machine_id: "a1b2c3d4".to_string(),
            user_id: "u-1234".to_string(),
            utc_date: "2026-09-17".to_string(),
            generated_at: "2026-09-17T18:12:03Z".to_string(),
            ccusage_version: "20.0.21".to_string(),
            cost_mode: "auto".to_string(),
            pricing_snapshot: "litellm@2026-09-16".to_string(),
            cells,
            sessions: vec![Session {
                id: "session-1".to_string(),
                project: Some("sha256:ab12".to_string()),
                first: 1_758_000_000_000,
                last: 1_758_000_900_000,
                cost: 1.23,
            }],
            dedupe: Dedupe {
                algo: "fxhash64/v1".to_string(),
                salt: "bucket-salt/v1".to_string(),
                count: 0,
            },
            content_hash: None,
        }
    }

    #[test]
    fn a_shard_survives_a_round_trip_through_json() {
        let mut original = shard(vec![cell(54, "claude-sonnet-4-5", &[7, 9])]);
        original.finish().expect("finish");

        let parsed = Shard::parse(&original.to_json().expect("serialize"))
            .expect("parse")
            .known()
            .expect("this build wrote it");

        assert_eq!(parsed, original);
        assert!(parsed.content_hash_matches());
    }

    /// Two machines fold the same entries in whatever order they walk their
    /// logs; if that changed the hash, every sync would re-upload every shard.
    #[test]
    fn the_hash_ignores_the_order_the_data_was_folded_in() {
        let mut one = shard(vec![
            cell(54, "claude-sonnet-4-5", &[9, 7, 9]),
            cell(12, "claude-haiku-4-5", &[3]),
        ]);
        let mut other = shard(vec![
            cell(12, "claude-haiku-4-5", &[3]),
            cell(54, "claude-sonnet-4-5", &[7, 9]),
        ]);

        assert_eq!(
            one.finish().expect("finish"),
            other.finish().expect("finish")
        );
    }

    #[test]
    fn a_changed_token_count_changes_the_hash() {
        let mut before = shard(vec![cell(54, "claude-sonnet-4-5", &[7])]);
        let before_hash = before.finish().expect("finish").to_string();
        let mut after = shard(vec![cell(54, "claude-sonnet-4-5", &[7])]);
        after.cells[0].output_tokens += 1;

        assert_ne!(after.finish().expect("finish"), before_hash);
    }

    /// The generation timestamp is not content: hashing it would make every
    /// sync look like a change and rewrite the whole history.
    #[test]
    fn regenerating_the_same_day_later_does_not_change_the_hash() {
        let mut first = shard(vec![cell(54, "claude-sonnet-4-5", &[7])]);
        let first_hash = first.finish().expect("finish").to_string();
        let mut second = shard(vec![cell(54, "claude-sonnet-4-5", &[7])]);
        second.generated_at = "2026-09-18T09:00:00Z".to_string();
        second.ccusage_version = "20.1.0".to_string();

        assert_eq!(second.finish().expect("finish"), first_hash);
    }

    #[test]
    fn dedupe_keys_are_sorted_deduplicated_and_counted() {
        let mut shard = shard(vec![cell(54, "claude-sonnet-4-5", &[9, 7, 9, 7])]);

        shard.finish().expect("finish");

        assert_eq!(shard.cells[0].keys, vec![DedupeKey(7), DedupeKey(9)]);
        assert_eq!(shard.dedupe.count, 2);
    }

    #[test]
    fn an_empty_cell_is_dropped_rather_than_stored() {
        let mut shard = shard(vec![Cell {
            bucket: 3,
            model: "claude-sonnet-4-5".to_string(),
            ..Cell::default()
        }]);

        shard.finish().expect("finish");

        assert!(shard.cells.is_empty());
    }

    #[test]
    fn a_bucket_beyond_the_day_is_refused() {
        let mut shard = shard(vec![cell(96, "claude-sonnet-4-5", &[1])]);

        assert_eq!(
            shard.finish(),
            Err(ShardError::BucketOutOfRange { bucket: 96 })
        );
    }

    /// Reading a newer shard leniently would drop fields this build does not
    /// know about and under-report spend, so it stays opaque.
    #[test]
    fn a_shard_from_a_newer_ccusage_is_kept_whole_rather_than_half_read() {
        let bytes = br#"{"schema":99,"agent":"claude","buckets":[],"somethingNew":true}"#;

        let parsed = Shard::parse(bytes).expect("a newer shard is not an error");

        assert_eq!(
            parsed,
            ParsedShard::Newer {
                schema: 99,
                bytes: bytes.to_vec(),
            }
        );
    }

    #[test]
    fn an_object_that_is_not_a_shard_is_an_error() {
        assert!(matches!(
            Shard::parse(b"{\"nope\":1}"),
            Err(ShardError::Malformed { .. })
        ));
    }

    #[test]
    fn dedupe_keys_serialize_as_hex_strings_that_a_browser_can_read() {
        let mut shard = shard(vec![cell(54, "claude-sonnet-4-5", &[u64::MAX])]);
        shard.finish().expect("finish");

        let rendered = String::from_utf8(shard.to_json().expect("serialize")).expect("utf8");

        assert!(rendered.contains("\"ffffffffffffffff\""), "{rendered}");
    }

    /// The cost envelope in the design assumes ~35 KB for a heavy day; a shard
    /// that outgrows that quietly turns a cents-per-month bill into dollars.
    #[test]
    fn a_heavy_day_stays_inside_the_size_envelope() {
        let models = ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"];
        let mut cells = Vec::new();
        let mut key = 0_u64;
        for bucket in 0..BUCKETS_PER_DAY {
            for model in models {
                key += 1;
                cells.push(cell(bucket, model, &[key, key + 1_000_000]));
            }
        }
        let mut shard = shard(cells);
        shard.sessions = (0..40)
            .map(|index| Session {
                id: format!("session-{index:04}"),
                project: Some(format!("sha256:{index:012x}")),
                first: 1_758_000_000_000,
                last: 1_758_000_900_000,
                cost: 1.23,
            })
            .collect();
        shard.finish().expect("finish");

        let size = shard.to_json().expect("serialize").len();

        assert!(size < 64 * 1024, "a heavy day serialized to {size} bytes");
    }
}
