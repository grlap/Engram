//! Store-independent endpoint observations of a local committed input.
//!
//! A clean observation is not execution evidence or an attestation. Consumers
//! retain their own root, authority, execution and before/after checks.

use serde::Serialize;
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_BUDGET: Duration = Duration::from_secs(8);
pub const MAX_JSON_BYTES: usize = 1024 * 1024;

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
            deadline: None,
        }
    }

    /// Return the final status and JSON together, after enforcing the deadline
    /// and output bound. Callers use this status for their process exit code.
    ///
    /// # Errors
    /// Returns a JSON serialization error if the observation cannot be encoded.
    pub fn json(&self) -> serde_json::Result<(Status, String)> {
        self.json_with_clock(Instant::now)
    }

    fn json_with_clock(
        &self,
        mut now: impl FnMut() -> Instant,
    ) -> serde_json::Result<(Status, String)> {
        let expired = |at| self.deadline.is_some_and(|deadline| at >= deadline);
        if expired(now()) {
            return self.refusal_json(
                RefusalCode::Deadline,
                "source observation exceeded its deadline",
            );
        }
        let json = serde_json::to_string(self)?;
        if expired(now()) {
            return self.refusal_json(
                RefusalCode::Deadline,
                "source JSON serialization exceeded its deadline",
            );
        }
        if json.len() <= MAX_JSON_BYTES {
            return Ok((self.status, json));
        }
        self.refusal_json(
            RefusalCode::OutputLimit,
            "source JSON exceeded its one-MiB output bound",
        )
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
    crate::committed_input::observe(root, budget)
}

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
            entries: vec![Entry {
                status: "??".to_owned(),
                path: "\u{1}".repeat(MAX_JSON_BYTES / 2),
                origin: None,
            }],
            refusal: None,
            deadline: None,
        };
        let (status, json) = observation.json().unwrap();
        assert_eq!(status, Status::Unavailable);
        assert!(json.len() <= MAX_JSON_BYTES);
        let result: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(result["status"], "unavailable");
        assert!(result["source"].is_null());
        assert!(result["tree"].is_null());
        assert_eq!(result["refusal"]["code"], "output_limit");
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
            assert_eq!(result["entries"], serde_json::json!([]));
        }
    }
}
