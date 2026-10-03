//! A host's read, before it asks for an acceptance evaluation, of whether a
//! run's named source root has the initial sighting that recording an
//! evaluation at a cut requires. It selects the root and the sighting with
//! the code recording uses, from one snapshot, and writes nothing. It is not
//! a verdict: recording an evaluation still decides, against everything that
//! happened since.

use serde::{Deserialize, Serialize};

use crate::ObjectId;

use super::{ProjectId, WorkId, WorkRunId};

/// The most bytes the serialized result holds.
pub const NAMED_ROOT_SIGHTING_READ_BYTES: usize = 16 * 1_024;

/// What the read found at its cut. Exactly three shapes exist: no root, a
/// root without a sighting, and a root with one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedRootSightingRead {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    /// The run-feed position the root and the sighting were read at: the
    /// requested cut, or the head when none was requested.
    pub read_cut: i64,
    /// The run-feed head in the same snapshot.
    pub head_cut: i64,
    /// The binding event of the root recording would select at the head, or
    /// `None` when it would select none there.
    pub current_binding: Option<ObjectId>,
    /// Whether the binding selected at the cut differs from the one at the
    /// head. Recording an evaluation at a cut whose binding moved since is
    /// refused before the sighting is looked at.
    pub binding_changed: bool,
    pub root: NamedRootAtCut,
}

/// The named root recording selects at the cut: the current claim's newest
/// binding after its latest release, when that binding names a root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum NamedRootAtCut {
    /// No root is bound at the cut; recording checks no sighting. An empty
    /// struct variant, so an unknown field beside the tag is refused.
    None {},
    Bound {
        workspace_id: String,
        generation: i64,
        /// The binding event's record id.
        binding_event: ObjectId,
        /// The binding event's run-feed position.
        binding_position: i64,
        sighting: InitialSighting,
    },
}

/// The root's newest qualifying source record at or before the cut, as
/// recording looks for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum InitialSighting {
    /// No qualifying record sights the root at or before the cut: recording
    /// an evaluation there would be refused for want of one.
    Absent {},
    Present {
        /// The sighting record's id.
        record: ObjectId,
        /// Its run-feed position.
        position: i64,
        /// The source revision it sighted.
        revision: String,
    },
}

/// Why a read was refused. A refusal is never a finding about the root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedRootSightingReadRefusal {
    /// The work reference names no item of the routed project.
    InvalidWorkRef,
    /// The run is unknown, or is not a run of the item.
    WrongRun,
    /// The cut is negative or past the run-feed head.
    InvalidCut,
    /// The complete result does not fit its byte bound.
    ResponseTooLarge,
}

impl NamedRootSightingReadRefusal {
    /// The host error code this refusal answers with.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidWorkRef => "named_root_sighting_read_invalid_work_ref",
            Self::WrongRun => "named_root_sighting_read_wrong_run",
            Self::InvalidCut => "named_root_sighting_read_invalid_cut",
            Self::ResponseTooLarge => "named_root_sighting_read_response_too_large",
        }
    }
}
