//! What a target holds beside each copy and what this home records about the
//! copies it put: stored manifests, receipts and attempts. The core keeps
//! these records; an adapter fills them in and reads them back.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::CaptureManifest;
use crate::ObjectId;

/// The format of a stored copy's manifest at a target.
pub const STORED_FORMAT_VERSION: u32 = 1;

/// How a stored copy's bytes are encoded at the target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Gzip,
}

/// What a target holds beside each copy: the capture's manifest, how the
/// stored file encodes it, and the name both files carry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredManifest {
    pub format_version: u32,
    /// The copy's name at the target, which carries its attempt id.
    pub copy: String,
    /// The identity of the target the copy was put for.
    pub target_identity: ObjectId,
    pub encoding: Encoding,
    /// Bytes of the stored, encoded file.
    pub stored_bytes: u64,
    pub capture: CaptureManifest,
}

/// How a target acknowledged a copy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Acknowledgement {
    /// The bytes were written and read back equal.
    ReadBack,
}

/// How far the target is known to lie off this machine.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OffHost {
    /// The operator asserted it; Engram did not verify it.
    Asserted,
}

/// What a target answered for one stored copy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupReceipt {
    /// SHA-256 of the artifact, uncompressed.
    pub sha256: String,
    /// The identity of the target that issued the receipt.
    pub target_identity: ObjectId,
    pub at: DateTime<Utc>,
    pub acknowledgement: Acknowledgement,
    pub off_host: OffHost,
    pub manifest: StoredManifest,
}

/// One attempt to put a copy, recorded before the put. Only the files an
/// attempt names are ever removed or completed for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub id: uuid::Uuid,
    /// The stored manifest the attempt puts; it names the target identity.
    pub manifest: StoredManifest,
    /// The final name of the attempt's data file at the target.
    pub data_file: String,
    /// The temporary name its data file is written under before the move.
    pub temporary_data_file: String,
}

/// Names one stored copy: its name at the target and the identity of the
/// target it was put for.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyRef {
    pub copy: String,
    pub target_identity: ObjectId,
}

impl CopyRef {
    /// The copy a receipt names.
    #[must_use]
    pub fn of(receipt: &BackupReceipt) -> Self {
        Self {
            copy: receipt.manifest.copy.clone(),
            target_identity: receipt.target_identity.clone(),
        }
    }
}

/// The target's last confirmation that it holds a copy, read back in full.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyConfirmed {
    pub copy: CopyRef,
    pub at: DateTime<Utc>,
}

/// A finding that the target no longer holds a copy, or holds other bytes
/// under its name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopyMissing {
    pub copy: CopyRef,
    pub at: DateTime<Utc>,
    pub reason: String,
}

/// How a push ended.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// A copy was put and its receipt recorded.
    Uploaded,
    /// A fresh capture equalled the newest receipt's copy, which the target
    /// confirmed; nothing was uploaded.
    Unchanged,
    /// The push failed; nothing it did counts.
    Failed,
}

/// The last push of a kind, as this home recorded it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LastAttempt {
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub outcome: AttemptOutcome,
    /// The typed code of a failure; present as null otherwise.
    #[serde(deserialize_with = "Option::deserialize")]
    pub code: Option<String>,
    /// What went wrong, for a failure; present as null otherwise.
    #[serde(deserialize_with = "Option::deserialize")]
    pub message: Option<String>,
}
