//! Store-independent endpoint observations of a local committed input.
//!
//! A clean observation is not execution evidence or an attestation. Consumers
//! retain their own root, authority, execution and before/after checks.

use serde::{Deserialize, Serialize};
mod validation;
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_BUDGET_MS: u64 = 60_000;
/// No public override may end a source read sooner: the operation-limit floor.
/// Internal callers still pass what remains of their own absolute deadlines.
pub const MIN_PUBLIC_BUDGET_MS: u64 = 60_000;
pub const MAX_BUDGET: Duration = Duration::from_millis(MAX_BUDGET_MS);

/// The budget a public caller's override actually receives: a shorter request
/// is still accepted, for existing callers, but is raised to the floor.
#[must_use]
pub fn public_budget(requested_ms: u64) -> Duration {
    Duration::from_millis(requested_ms.max(MIN_PUBLIC_BUDGET_MS))
}
pub const MAX_JSON_BYTES: usize = 1024 * 1024;

type BoundedJson = Result<String, (RefusalCode, &'static str)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Clean,
    NotClean,
    Unavailable,
}

/// One record from Git's single NUL-delimited porcelain status response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Entry {
    pub status: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// The complete measured source state. Paths are relative to their repository;
/// absolute host paths and observation metadata are not comparison inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceId {
    pub commit: String,
    pub tree: String,
    pub submodules: Vec<Submodule>,
    pub changes: Vec<Change>,
}

/// Decode a present identity without reflecting malformed caller values in a
/// front-door diagnostic. Absence is handled by the containing field's default.
///
/// # Errors
/// Refuses a malformed or null present object without reflecting its values.
pub fn present_source_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SourceId>, D::Error> {
    SourceId::deserialize(deserializer)
        .map(Some)
        .map_err(|_| serde::de::Error::custom("invalid source_id object"))
}

/// Bounded presentation of a source object. Equality always uses `SourceId`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceIdSummary {
    pub commit: String,
    pub tree: String,
    pub changes: usize,
    pub submodules: usize,
}

impl SourceIdSummary {
    #[must_use]
    pub fn description(&self) -> String {
        format!(
            "source_id commit {}; tree {}; {} changes; {} submodules",
            self.commit, self.tree, self.changes, self.submodules
        )
    }
}

impl SourceId {
    #[must_use]
    pub fn summary(&self) -> SourceIdSummary {
        SourceIdSummary {
            commit: self.commit.clone(),
            tree: self.tree.clone(),
            changes: self.changes.len(),
            submodules: self.submodules.len(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Submodule {
    pub path: String,
    pub commit: String,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: String,
    pub status: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub origin: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub blob: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub mode: Option<Mode>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub enum Mode {
    #[serde(rename = "100644")]
    Regular,
    #[serde(rename = "100755")]
    Executable,
    #[serde(rename = "120000")]
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
}

/// Closed JSON v1 refusal set. Unknown codes must fail closed at the adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    InputUnavailable,
    Deadline,
    OutputLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub version: u8,
    pub status: Status,
    pub root: Option<String>,
    pub source: Option<String>,
    pub tree: Option<String>,
    pub entries: Vec<Entry>,
    pub refusal: Option<Refusal>,
    pub source_id: Option<SourceId>,
    #[serde(skip)]
    pub(crate) deadline: Option<Instant>,
}

impl Observation {
    pub(crate) fn unavailable(root: Option<String>, code: RefusalCode, message: String) -> Self {
        // Error chains can include arbitrary Git diagnostics. Preserve a bounded
        // explanation; entry data itself is never truncated into a usable result.
        let mut message = message;
        if message.len() > 4096 {
            let mut end = 4096;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self {
            version: 1,
            status: Status::Unavailable,
            root,
            source: None,
            tree: None,
            entries: Vec::new(),
            refusal: Some(Refusal { code, message }),
            source_id: None,
            deadline: None,
        }
    }

    /// Return the final status and JSON together, after enforcing the deadline
    /// and output bound. Callers use this status for their process exit code.
    ///
    /// # Errors
    /// Returns a JSON serialization error if the observation cannot be encoded.
    pub fn json(&self) -> serde_json::Result<(Status, String)> {
        self.json_with_clock(crate::operation_time::now)
    }

    fn json_with_clock(
        &self,
        mut now: impl FnMut() -> Instant,
    ) -> serde_json::Result<(Status, String)> {
        match self.bounded_json(&mut now)? {
            Ok(json) => Ok((self.status, json)),
            Err((code, message)) => self.refusal_json(code, message),
        }
    }

    fn bounded_json(&self, now: &mut impl FnMut() -> Instant) -> serde_json::Result<BoundedJson> {
        let expired = |at| self.deadline.is_some_and(|deadline| at >= deadline);
        if expired(now()) {
            return Ok(Err((
                RefusalCode::Deadline,
                "source observation exceeded its deadline",
            )));
        }
        let json = serde_json::to_string(self)?;
        if expired(now()) {
            return Ok(Err((
                RefusalCode::Deadline,
                "source JSON serialization exceeded its deadline",
            )));
        }
        if json.len() <= MAX_JSON_BYTES {
            return Ok(Ok(json));
        }
        Ok(Err((
            RefusalCode::OutputLimit,
            "source JSON exceeded its one-MiB output bound",
        )))
    }

    fn finalized(self) -> Self {
        match self.bounded_json(&mut crate::operation_time::now) {
            Ok(Ok(_)) => self,
            Ok(Err((code, message))) => Self::unavailable(self.root, code, message.to_owned()),
            Err(error) => Self::unavailable(
                self.root,
                RefusalCode::InputUnavailable,
                format!("source JSON serialization failed: {error}"),
            ),
        }
    }

    fn refusal_json(
        &self,
        code: RefusalCode,
        message: &str,
    ) -> serde_json::Result<(Status, String)> {
        let json = serde_json::to_string(&Self::unavailable(
            self.root.clone(),
            code,
            message.to_owned(),
        ))?;
        Ok((Status::Unavailable, json))
    }
}

/// Measure the sole committed-input predicate, with an optional shorter budget.
pub fn inspect(root: &Path, budget: Duration) -> Observation {
    inspect_with(root, budget, std::convert::identity)
}

/// Keep the observation's warning active through the caller's final processing.
pub fn inspect_with<T>(root: &Path, budget: Duration, finish: impl FnOnce(Observation) -> T) -> T {
    let _warning = crate::operation_time::Warning::start("source observation");
    finish(crate::committed_input::observe(root, budget).finalized())
}

#[cfg(test)]
mod warning_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_expansion_refuses_instead_of_truncating_usable_entries() {
        let fixture = crate::ObjectId::from_canonical_bytes(b"source output fixture").to_string();
        let fixture = &fixture[..40];
        let observation = Observation {
            version: 1,
            status: Status::NotClean,
            root: Some("/fixture".to_owned()),
            source: Some(format!("git-commit:{fixture}")),
            tree: Some(fixture.to_owned()),
            entries: Vec::new(),
            refusal: None,
            source_id: Some(SourceId {
                commit: fixture.to_owned(),
                tree: fixture.to_owned(),
                submodules: Vec::new(),
                changes: vec![Change {
                    path: "\u{1}".repeat(MAX_JSON_BYTES / 2),
                    status: "??".to_owned(),
                    origin: None,
                    blob: Some(fixture.to_owned()),
                    mode: Some(Mode::Regular),
                }],
            }),
            deadline: None,
        };
        let (status, json) = observation.json().unwrap();
        assert_eq!(status, Status::Unavailable);
        assert!(json.len() <= MAX_JSON_BYTES);
        let result: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(result["status"], "unavailable");
        assert!(result["source"].is_null());
        assert!(result["tree"].is_null());
        assert!(result["source_id"].is_null());
        assert_eq!(result["refusal"]["code"], "output_limit");
        let finalized = observation.finalized();
        assert_eq!(finalized.status, Status::Unavailable);
        assert!(finalized.source_id.is_none());
        assert_eq!(finalized.refusal.unwrap().code, RefusalCode::OutputLimit);
    }

    #[test]
    fn final_json_refuses_expiry_before_and_after_serialization() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let observation = Observation {
            version: 1,
            status: Status::Clean,
            root: Some("/fixture".to_owned()),
            source: Some("git-commit:fixture".to_owned()),
            tree: Some("fixture".to_owned()),
            entries: Vec::new(),
            refusal: None,
            source_id: None,
            deadline: Some(deadline),
        };
        for times in [
            [deadline, deadline],
            [
                deadline.checked_sub(Duration::from_millis(1)).unwrap(),
                deadline,
            ],
        ] {
            let mut times = times.into_iter();
            let (status, json) = observation
                .json_with_clock(|| times.next().unwrap())
                .unwrap();
            assert_eq!(status, Status::Unavailable);
            let result: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(result["refusal"]["code"], "deadline");
            assert!(result["source"].is_null());
            assert!(result["tree"].is_null());
            assert!(result["source_id"].is_null());
            assert_eq!(result["entries"], serde_json::json!([]));
        }
    }

    #[test]
    fn source_change_nullable_fields_are_required_and_types_are_closed() {
        let tombstone = serde_json::json!({
            "path": "deleted", "status": " D", "origin": null, "blob": null, "mode": null
        });
        let parsed: Change = serde_json::from_value(tombstone.clone()).unwrap();
        assert!(parsed.blob.is_none());
        for key in ["origin", "blob", "mode"] {
            let mut missing = tombstone.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<Change>(missing).is_err());
        }
        let mut unknown = tombstone;
        unknown["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Change>(unknown).is_err());
        assert!(serde_json::from_value::<Mode>(serde_json::json!(100_644)).is_err());
    }
}
