//! Origin-qualified historical context, separate from native destination feeds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{FeedId, ObjectId, ProjectId};

use super::{EvidenceError, RecordKind, limits};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OriginFeed {
    pub origin: ObjectId,
    pub project_id: ProjectId,
    pub feed: FeedId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedMember {
    pub position: i64,
    pub object_kind: String,
    pub object_id: ObjectId,
}

/// The caller supplies a previously admitted boundary from its origin-context view.
/// Its segment id is a record link, not a hash of the range or membership.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedBoundary {
    pub source: OriginFeed,
    pub through: i64,
    pub segment: Option<ObjectId>,
    pub endpoint: Option<FeedMember>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OriginFeedSegment {
    pub source: OriginFeed,
    pub from_exclusive: i64,
    pub through: i64,
    pub predecessor: Option<ObjectId>,
    pub anchor: Option<FeedMember>,
    pub entries: Vec<FeedMember>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OriginFeedCut {
    pub source: OriginFeed,
    pub through: i64,
    pub endpoint: Option<FeedMember>,
    pub segments: Vec<ObjectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WitnessProblem {
    SourceScopeDiffers,
    SourceFeedGap { position: i64 },
    SourceFeedConflict { position: i64 },
    CutUnavailable,
    ContinuityDiffers,
    GoverningSnapshotDiffers,
}

/// An evaluation's hash is a content comparison, never an object-closure link.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoverningBasis {
    pub evaluation: ObjectId,
    pub project_id: ProjectId,
    pub work_id: crate::WorkId,
    pub work_revision: i64,
    pub content_fingerprint: ObjectId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotAvailability {
    Exact {
        snapshot: Box<crate::WorkItem>,
        source_snapshot: Option<ObjectId>,
    },
    GoverningSnapshotUnavailable,
}

/// The containing canonical envelope supplies this witness's separately minted id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoverningSnapshotWitness {
    pub basis: GoverningBasis,
    pub snapshot: SnapshotAvailability,
}

impl GoverningSnapshotWitness {
    /// Compare the full retained governing snapshot with an original evaluation.
    /// Missing historical input is explicit and never reconstructed from criteria.
    ///
    /// # Errors
    /// Refuses oversized witnesses and mismatched evaluation or governing content.
    pub fn validate(
        &self,
        expected: &GoverningBasis,
    ) -> Result<Option<&crate::WorkItem>, EvidenceError> {
        validate_size(self)?;
        if &self.basis != expected {
            return Err(refusal(WitnessProblem::GoverningSnapshotDiffers));
        }
        let SnapshotAvailability::Exact {
            snapshot,
            source_snapshot,
        } = &self.snapshot
        else {
            return Ok(None);
        };
        let bytes = serde_json_canonicalizer::to_vec(snapshot)?;
        if source_snapshot != &snapshot.source_snapshot_id
            || snapshot.project_id != self.basis.project_id
            || snapshot.work_id != self.basis.work_id
            || snapshot.revision != self.basis.work_revision
            || ObjectId::from_canonical_bytes(&bytes) != self.basis.content_fingerprint
        {
            return Err(refusal(WitnessProblem::GoverningSnapshotDiffers));
        }
        Ok(Some(snapshot))
    }
}

fn refusal(problem: WitnessProblem) -> EvidenceError {
    EvidenceError::Witness(problem)
}

fn validate_source(source: &OriginFeed) -> Result<(), EvidenceError> {
    if source.project_id.0.trim().is_empty()
        || matches!(&source.feed, FeedId::Project(project) if project != &source.project_id)
    {
        return Err(refusal(WitnessProblem::SourceScopeDiffers));
    }
    Ok(())
}

fn validate_size(body: &impl Serialize) -> Result<(), EvidenceError> {
    let bytes = serde_json_canonicalizer::to_vec(body)?;
    limits::enforce(
        limits::LimitKind::RecordTooLarge,
        None,
        limits::Dimension::Bytes,
        bytes.len(),
        limits::MAX_RECORD_BYTES,
    )?;
    Ok(())
}

impl OriginFeedSegment {
    /// Check one dense retained interval against its previously admitted boundary.
    /// No claim, path selection, destination cursor or feed head is modified.
    ///
    /// # Errors
    /// Refuses oversized bodies, missing interior entries or mismatched continuity.
    pub fn validate(&self, previous: &FeedBoundary) -> Result<(), EvidenceError> {
        validate_size(self)?;
        validate_source(&self.source)?;
        if self.source != previous.source {
            return Err(refusal(WitnessProblem::SourceScopeDiffers));
        }
        if self.from_exclusive < 0
            || self.through <= self.from_exclusive
            || self.from_exclusive != previous.through
            || self.predecessor != previous.segment
            || self.anchor != previous.endpoint
        {
            return Err(refusal(WitnessProblem::ContinuityDiffers));
        }
        if previous.through == 0 {
            if previous.segment.is_some() || previous.endpoint.is_some() {
                return Err(refusal(WitnessProblem::ContinuityDiffers));
            }
        } else if previous.segment.is_none()
            || previous
                .endpoint
                .as_ref()
                .is_none_or(|entry| entry.position != previous.through)
        {
            return Err(refusal(WitnessProblem::ContinuityDiffers));
        }
        let mut position = self.from_exclusive;
        for entry in &self.entries {
            let expected = position
                .checked_add(1)
                .ok_or_else(|| refusal(WitnessProblem::CutUnavailable))?;
            if entry.position != expected || entry.position > self.through {
                return Err(refusal(WitnessProblem::SourceFeedGap {
                    position: expected,
                }));
            }
            RecordKind::parse(&entry.object_kind)?;
            position = expected;
        }
        if position != self.through {
            return Err(refusal(WitnessProblem::SourceFeedGap {
                position: position + 1,
            }));
        }
        Ok(())
    }
}

impl OriginFeedCut {
    /// Resolve an exact retained segment chain and compare any already known overlap.
    /// The returned membership is only origin context; storage still resolves bodies
    /// and proves enrollment/lineage before admitting a component.
    ///
    /// # Errors
    /// Refuses missing segments, gaps, collisions, beyond-cut entries or broken links.
    pub fn validate_chain(
        &self,
        prefix: &FeedBoundary,
        segments: &BTreeMap<ObjectId, OriginFeedSegment>,
        known: &BTreeMap<i64, FeedMember>,
    ) -> Result<BTreeMap<i64, FeedMember>, EvidenceError> {
        validate_size(self)?;
        validate_source(&self.source)?;
        if self.source != prefix.source {
            return Err(refusal(WitnessProblem::SourceScopeDiffers));
        }
        if self.through < prefix.through || prefix.through < 0 {
            return Err(refusal(WitnessProblem::CutUnavailable));
        }
        if prefix.through == 0 {
            if prefix.segment.is_some() || prefix.endpoint.is_some() {
                return Err(refusal(WitnessProblem::ContinuityDiffers));
            }
        } else if prefix.segment.is_none()
            || prefix
                .endpoint
                .as_ref()
                .is_none_or(|entry| entry.position != prefix.through)
        {
            return Err(refusal(WitnessProblem::ContinuityDiffers));
        }
        if let Some(endpoint) = &prefix.endpoint
            && known
                .get(&endpoint.position)
                .is_some_and(|entry| entry != endpoint)
        {
            return Err(refusal(WitnessProblem::SourceFeedConflict {
                position: endpoint.position,
            }));
        }
        let mut boundary = prefix.clone();
        let mut membership = BTreeMap::new();
        for id in &self.segments {
            let segment = segments
                .get(id)
                .ok_or_else(|| refusal(WitnessProblem::CutUnavailable))?;
            segment.validate(&boundary)?;
            if segment.through > self.through {
                return Err(refusal(WitnessProblem::CutUnavailable));
            }
            for entry in &segment.entries {
                if known
                    .get(&entry.position)
                    .is_some_and(|prior| prior != entry)
                {
                    return Err(refusal(WitnessProblem::SourceFeedConflict {
                        position: entry.position,
                    }));
                }
                membership.insert(entry.position, entry.clone());
            }
            boundary = FeedBoundary {
                source: segment.source.clone(),
                through: segment.through,
                segment: Some(id.clone()),
                endpoint: segment.entries.last().cloned(),
            };
        }
        if boundary.through != self.through || boundary.endpoint != self.endpoint {
            return Err(refusal(WitnessProblem::CutUnavailable));
        }
        Ok(membership)
    }
}

#[cfg(test)]
mod tests;
