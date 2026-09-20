//! Host-evaluated, core-enforced acceptance before completion.
//!
//! The evaluator submits an immutable per-criterion record; Engram validates
//! its structure and provenance, binds it to the exact criteria and run state,
//! and refuses completion without a fresh passing record. Engram never runs a
//! model, and every identity here keeps its actual asserted assurance.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ObjectId;

use super::{ActorContext, FeedPosition, ProjectId, SessionId, WorkId, WorkRunId};

/// Who evaluates, relative to the completing session. These are alternatives
/// selected by policy compatibility, not an ordinal security ladder.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceEvaluationMode {
    /// The completing session evaluates its own work as an explicit continuation.
    SameSession,
    /// An evaluator spawned under the completing session with a distinct
    /// execution identity and a host-attested parent relationship.
    SubAgent,
    /// A separate session, possibly another model or provider.
    IndependentSession,
}

impl AcceptanceEvaluationMode {
    /// Stable lower-case word used in receipts and command arguments.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::SameSession => "same_session",
            Self::SubAgent => "sub_agent",
            Self::IndependentSession => "independent_session",
        }
    }

    /// Parses the stable word; case-insensitive, hyphens accepted.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "same_session" => Some(Self::SameSession),
            "sub_agent" => Some(Self::SubAgent),
            "independent_session" => Some(Self::IndependentSession),
            _ => None,
        }
    }
}

/// Result recorded for one criterion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceVerdict {
    Pass,
    Fail,
    InsufficientEvidence,
    NeedsHuman,
}

impl AcceptanceVerdict {
    /// Stable lower-case word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::NeedsHuman => "needs_human",
        }
    }

    /// Parses the stable word; case-insensitive, hyphens accepted.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "insufficient_evidence" | "insufficient" => Some(Self::InsufficientEvidence),
            "needs_human" | "human" => Some(Self::NeedsHuman),
            _ => None,
        }
    }
}

/// What a verdict rests on. Engram validates that the cited objects are what
/// the basis claims; relevance stays the evaluator's judgment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceBasis {
    /// Host-minted verification evidence with a passed result.
    Observed,
    /// Agent-recorded gate evidence with no failure labels.
    Asserted,
    /// The evaluator's own judgment over cited run evidence and rationale.
    Judgment,
    /// A human decision is required; never a pass.
    HumanRequired,
}

impl AcceptanceBasis {
    /// Stable lower-case word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Asserted => "asserted",
            Self::Judgment => "judgment",
            Self::HumanRequired => "human_required",
        }
    }

    /// Parses the stable word; case-insensitive, hyphens accepted.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "observed" => Some(Self::Observed),
            "asserted" => Some(Self::Asserted),
            "judgment" | "judgement" => Some(Self::Judgment),
            "human_required" => Some(Self::HumanRequired),
            _ => None,
        }
    }
}

/// Minimum evidence class for a pass on a mechanical (check-backed) basis.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MechanicalBasis {
    /// Agent-recorded gate evidence is enough for an `asserted` pass.
    #[default]
    Asserted,
    /// Only host-minted verification evidence may back a pass; `asserted`
    /// passes are refused at record time.
    Observed,
}

impl MechanicalBasis {
    /// Stable lower-case word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Asserted => "asserted",
            Self::Observed => "observed",
        }
    }

    /// Parses the stable word.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "asserted" => Some(Self::Asserted),
            "observed" => Some(Self::Observed),
            _ => None,
        }
    }
}

/// Per-project acceptance-evaluation policy, carried by the immutable control
/// policy. Empty `allowed_modes` is the legacy self-asserted path.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceEvaluationPolicy {
    /// Modes an evaluation may use; a set, not a floor.
    #[serde(default)]
    pub allowed_modes: Vec<AcceptanceEvaluationMode>,
    /// Minimum evidence class for a mechanical pass.
    #[serde(default)]
    pub mechanical_basis: MechanicalBasis,
    /// Completion must present a source fingerprint equal to the evaluated one.
    #[serde(default)]
    pub require_source_freshness: bool,
}

impl AcceptanceEvaluationPolicy {
    /// Whether the project keeps the legacy self-asserted completion path.
    #[must_use]
    pub fn is_legacy(&self) -> bool {
        self.allowed_modes.is_empty()
    }

    /// Whether the project requires an evaluation before completion.
    #[must_use]
    pub fn is_evaluated(&self) -> bool {
        !self.is_legacy()
    }

    /// Whether the policy admits this mode.
    #[must_use]
    pub fn allows(&self, mode: AcceptanceEvaluationMode) -> bool {
        self.allowed_modes.contains(&mode)
    }

    /// Canonical form: deduplicated modes in declaration order. An empty mode
    /// list is the legacy policy whatever the other fields say, so it is the
    /// default value: the canonical policy bytes omit it, and the requested,
    /// stored, and read policies agree.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let mut allowed_modes = Vec::new();
        for mode in [
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
            AcceptanceEvaluationMode::IndependentSession,
        ] {
            if self.allowed_modes.contains(&mode) {
                allowed_modes.push(mode);
            }
        }
        if allowed_modes.is_empty() {
            return Self::default();
        }
        Self {
            allowed_modes,
            mechanical_basis: self.mechanical_basis,
            require_source_freshness: self.require_source_freshness,
        }
    }
}

/// Maximum bytes of one evaluator model segment (provider, model, version).
pub const MAX_EVALUATOR_MODEL_SEGMENT_BYTES: usize = 128;

/// Maximum bytes of a sub-agent execution identity: an asserted identifier,
/// never prose, bounded like the other asserted identifiers on the record.
pub const MAX_EXECUTION_IDENTITY_BYTES: usize = 256;

/// Structured, optional, asserted metadata about the evaluating model.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorModel {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl EvaluatorModel {
    /// The documented segment bounds, checked wherever a model enters a
    /// record: every segment is non-blank, at most
    /// [`MAX_EVALUATOR_MODEL_SEGMENT_BYTES`], and free of control characters.
    ///
    /// # Errors
    ///
    /// Returns the bound that a segment violates.
    pub fn validate(&self) -> Result<(), String> {
        for segment in [
            Some(&self.provider),
            Some(&self.model),
            self.version.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if segment.trim().is_empty()
                || segment.len() > MAX_EVALUATOR_MODEL_SEGMENT_BYTES
                || segment.chars().any(char::is_control)
            {
                return Err(format!(
                    "each evaluator model segment must be non-blank, at most {MAX_EVALUATOR_MODEL_SEGMENT_BYTES} bytes, and free of control characters"
                ));
            }
        }
        Ok(())
    }
}

/// Host-measured source identity at evaluation time. Asserted context unless
/// a host channel mediates it; equality with a later measurement is asserted
/// freshness, not independent verification.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceSourceBasis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub fingerprint: String,
}

/// One criterion's recorded result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriterionVerdict {
    pub criterion: String,
    pub verdict: AcceptanceVerdict,
    pub basis: AcceptanceBasis,
    pub rationale: String,
    /// Run evidence the verdict rests on: note/gate/verification/environment
    /// objects on the evaluated run.
    pub evidence: Vec<ObjectId>,
}

/// Immutable acceptance evaluation appended to the run execution feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceEvaluation {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub root_id: WorkId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub work_revision: i64,
    /// Canonical identity of the exact work revision whose criteria were judged.
    pub work_revision_hash: ObjectId,
    /// The criteria evaluated, copied verbatim at record time.
    pub criteria: Vec<String>,
    /// The run-feed position the evaluator read through, exactly as the
    /// submission supplied it (R3b); the head at submission time is never
    /// substituted, and a host-observed change after it refuses the record.
    pub evaluated_cut: FeedPosition,
    /// Every run evidence object on the feed at or before `evaluated_cut`:
    /// the selection the evaluator could have read.
    pub evidence_basis: Vec<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<AcceptanceSourceBasis>,
    pub mode: AcceptanceEvaluationMode,
    pub evaluator: ActorContext,
    /// Sub-agent mode: the distinct evaluator execution identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity: Option<String>,
    /// Sub-agent mode: the attested parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_model: Option<EvaluatorModel>,
    pub verdicts: Vec<CriterionVerdict>,
    /// Explicit or content-derived attempt identity.
    pub attempt_key: String,
    pub created_at: DateTime<Utc>,
}

impl AcceptanceEvaluation {
    /// Whether every criterion passed.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        self.verdicts
            .iter()
            .all(|verdict| verdict.verdict == AcceptanceVerdict::Pass)
    }

    /// First non-passing verdict in precedence order: fail, then insufficient
    /// evidence, then needs-human; within one verdict class, list order.
    #[must_use]
    pub fn first_blocking(&self) -> Option<&CriterionVerdict> {
        for verdict in [
            AcceptanceVerdict::Fail,
            AcceptanceVerdict::InsufficientEvidence,
            AcceptanceVerdict::NeedsHuman,
        ] {
            if let Some(found) = self.verdicts.iter().find(|entry| entry.verdict == verdict) {
                return Some(found);
            }
        }
        None
    }
}

/// Why the newest evaluation no longer counts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStaleReason {
    /// The work revision (criteria or other planning fields) changed.
    Revision,
    /// The evaluated run is not the completing run.
    Run,
    /// A host-observed mutation followed the evaluated cut.
    Mutation,
    /// The source fingerprint presented at completion differs or is missing.
    Source,
    /// The policy or task no longer admits the evaluation: its mode is not
    /// allowed or pinned differently, or a requirement grew stricter than a
    /// pass basis the record relies on.
    Policy,
    /// A check the evaluation relied on has a newer record after the cut.
    Evidence,
    /// An `independent_session` evaluator has since held or executed the
    /// run, so the record no longer describes an independent judgment.
    Identity,
}

impl AcceptanceStaleReason {
    /// Stable lower-case word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Revision => "revision",
            Self::Run => "run",
            Self::Mutation => "mutation",
            Self::Source => "source",
            Self::Policy => "policy",
            Self::Evidence => "evidence",
            Self::Identity => "identity",
        }
    }
}

/// One criterion input, addressed by its one-based position in the acceptance
/// list read at `expected_work_revision`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriterionVerdictInput {
    pub criterion: usize,
    pub verdict: AcceptanceVerdict,
    pub basis: AcceptanceBasis,
    pub rationale: String,
    #[serde(default)]
    pub evidence: Vec<ObjectId>,
}

/// Request to record one acceptance evaluation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordAcceptanceEvaluationRequest {
    pub project_id: ProjectId,
    pub work_id: WorkId,
    /// The acceptance basis the evaluator read; a changed revision refuses.
    pub expected_work_revision: i64,
    /// The run-feed position the evaluator read through (the evidence
    /// basis printed by `show`). A host-observed change after it, or a
    /// citation beyond it, refuses: the record binds what was evaluated,
    /// never the feed head sampled at submission.
    pub evaluated_through: i64,
    pub mode: AcceptanceEvaluationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_model: Option<EvaluatorModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<AcceptanceSourceBasis>,
    pub verdicts: Vec<CriterionVerdictInput>,
    pub evaluator: ActorContext,
    /// Explicit attempt key; omitted keys are content-derived by storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_key: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// Maximum citations admitted per verdict.
pub const MAX_ACCEPTANCE_VERDICT_CITATIONS: usize = 64;

/// Maximum canonical bytes of one evaluation object (1 MiB). Rationales share
/// the note-text bound individually and the criteria count is not capped, so
/// this bounds the whole record without making a large item unevaluable: an
/// admitted evaluation is always finite to project and to read back in full.
pub const MAX_ACCEPTANCE_EVALUATION_BYTES: usize = 1024 * 1024;

/// Maximum bytes of a source fingerprint or workspace id carried by an
/// evaluation; these are identifiers, never prose.
pub const MAX_ACCEPTANCE_SOURCE_BASIS_BYTES: usize = 256;
