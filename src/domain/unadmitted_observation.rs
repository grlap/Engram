//! Execution a host observed without admission: a turn it noticed only after
//! it had started, a workspace change between turns, or a check inside such a
//! turn. The record is attributed history, never authority. It carries no
//! grant, no begin and no turn result; every check in it is uncredited; and
//! its cause is unknown unless the host asserts one, which stays an assertion.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    ActorContext, ControlWorkBinding, ExecutionSourceBasis, FeedPosition, NamedRootState,
    ProjectId, ProjectPolicyEpoch, SessionId, VerificationKind, VerificationResult,
    validate_session_id_length,
};
use crate::ObjectId;

mod strict;

/// Schema version of a stored unadmitted execution observation.
pub const UNADMITTED_EXECUTION_OBSERVATION_SCHEMA_VERSION: u16 = 1;
/// Checks one unadmitted turn may report.
pub const MAX_OBSERVED_CHECKS: usize = 16;
/// Bytes of an idempotency key, host turn reference or host check id.
pub const MAX_OBSERVATION_LABEL_BYTES: usize = 128;
/// Bytes of an opaque host evidence reference on a check.
pub const MAX_OBSERVED_EVIDENCE_REF_BYTES: usize = 2_048;
/// Bytes of the basis a host gives for an asserted cause.
pub const MAX_OBSERVATION_CAUSAL_BASIS_BYTES: usize = 4_096;
/// Bytes of a workspace id or source revision.
pub const MAX_OBSERVED_SOURCE_BYTES: usize = 512;
/// Bytes of a whole `execution_observe` request frame, checked before decoding.
pub const MAX_EXECUTION_OBSERVE_REQUEST_BYTES: usize = 64 * 1_024;
/// Bytes of the complete receipt, checked before the record commits.
pub const MAX_EXECUTION_OBSERVE_RESULT_BYTES: usize = 16 * 1_024;

/// The claim's named-root lifecycle at the run-feed cut where the host took
/// its capture, as `named_root_read` reports it for that run and claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRootBasis {
    pub capture_run_cut: i64,
    /// The claim's newest recorded root event at the cut, bound or ended;
    /// `None` only when the claim never had one.
    pub latest_event: Option<ObjectId>,
    #[serde(deserialize_with = "strict_root_state")]
    pub state: NamedRootState,
}

/// A root state decoded strictly: each variant takes exactly its own fields,
/// so a field the state does not name is refused, never dropped.
fn strict_root_state<'de, D>(deserializer: D) -> Result<NamedRootState, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = strict::StrictValue::deserialize(deserializer)?.0;
    let object = value
        .as_object()
        .ok_or_else(|| D::Error::custom("root_basis.state must be an object"))?;
    let known: &[&str] = match object.get("state").and_then(serde_json::Value::as_str) {
        Some("bound") => &["state", "workspace_id", "generation", "named_at"],
        Some("unbound_by_release") => &["state", "last_generation", "released_at_position"],
        // `none`, or a state the decoder below refuses by name.
        _ => &["state"],
    };
    if let Some(field) = object.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(D::Error::unknown_field(field, known));
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// The window the host actually observed. It need not reach back to the
/// true start of what it saw.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedInterval {
    pub from: DateTime<Utc>,
    pub through: DateTime<Utc>,
}

/// An earlier measured revision of the workspace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredBaseline {
    pub workspace_id: String,
    pub source_revision: String,
    pub observed_at: DateTime<Utc>,
}

/// The measured revision the host saw at the end of the change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredSighting {
    pub source_basis: ExecutionSourceBasis,
    pub observed_at: DateTime<Utc>,
}

/// How the host established a source change it reports, with what it
/// measured. Every value is the host's report; the core computes no
/// fingerprint of its own.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "detection", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedSourceChange {
    /// Two measured revisions of one workspace differed.
    ContentComparison {
        workspace_id: String,
        baseline: MeasuredBaseline,
        sighting: MeasuredSighting,
    },
    /// No earlier measurement exists, so a change is assumed.
    AssumedMissingBaseline {
        workspace_id: String,
        sighting: MeasuredSighting,
    },
    /// No closing revision could be taken; only file notifications decided.
    /// It carries no revision and no source basis.
    WatcherOnly {
        workspace_id: String,
        observed_at: DateTime<Utc>,
    },
}

impl ObservedSourceChange {
    /// The workspace the change was seen in.
    #[must_use]
    pub fn workspace_id(&self) -> &str {
        match self {
            Self::ContentComparison { workspace_id, .. }
            | Self::AssumedMissingBaseline { workspace_id, .. }
            | Self::WatcherOnly { workspace_id, .. } => workspace_id,
        }
    }

    /// The detection as the host protocol spells it.
    #[must_use]
    pub const fn detection(&self) -> &'static str {
        match self {
            Self::ContentComparison { .. } => "content_comparison",
            Self::AssumedMissingBaseline { .. } => "assumed_missing_baseline",
            Self::WatcherOnly { .. } => "watcher_only",
        }
    }

    /// The measured closing revision, when one was taken.
    #[must_use]
    pub fn sighting(&self) -> Option<&MeasuredSighting> {
        match self {
            Self::ContentComparison { sighting, .. }
            | Self::AssumedMissingBaseline { sighting, .. } => Some(sighting),
            Self::WatcherOnly { .. } => None,
        }
    }
}

/// One check the host saw run inside an unadmitted turn: a bounded fact,
/// never verification evidence and never a command log.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedCheck {
    /// The host's stable id for the check, scoped to the observing session.
    pub host_check_id: String,
    pub check_kind: VerificationKind,
    pub observed_result: VerificationResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    /// The source the check ran on, as the host asserts it; absent when
    /// unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<ExecutionSourceBasis>,
    /// An opaque host artifact or log reference, never fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_evidence_ref: Option<String>,
}

/// What a host observed, as it sends it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedOccurrence {
    /// Execution seen without an admitted begin; not a successful or
    /// completed turn. `source_change: null` means only that no change is
    /// reported, never that the source is unchanged.
    UnadmittedTurn {
        host_turn_ref: String,
        source_change: Option<ObservedSourceChange>,
        observed_checks: Vec<ObservedCheck>,
    },
    /// A workspace change seen between turns.
    InterTurnChange { source_change: ObservedSourceChange },
    /// One check seen inside an unadmitted turn, reported on its own.
    ObservedCheck {
        host_turn_ref: String,
        check: ObservedCheck,
    },
}

/// The credit an observed check carries: always none.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedCheckCredit {
    Uncredited,
}

/// An observed check as it is stored, with its server-set credit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedObservedCheck {
    pub check: ObservedCheck,
    pub credit: ObservedCheckCredit,
}

/// What a host observed, as it is stored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedOccurrence {
    UnadmittedTurn {
        host_turn_ref: String,
        source_change: Option<ObservedSourceChange>,
        observed_checks: Vec<RecordedObservedCheck>,
    },
    InterTurnChange {
        source_change: ObservedSourceChange,
    },
    ObservedCheck {
        host_turn_ref: String,
        check: RecordedObservedCheck,
    },
}

impl RecordedOccurrence {
    /// The stored form of `occurrence`, every check uncredited.
    #[must_use]
    pub fn record(occurrence: ObservedOccurrence) -> Self {
        let recorded = |check| RecordedObservedCheck {
            check,
            credit: ObservedCheckCredit::Uncredited,
        };
        match occurrence {
            ObservedOccurrence::UnadmittedTurn {
                host_turn_ref,
                source_change,
                observed_checks,
            } => Self::UnadmittedTurn {
                host_turn_ref,
                source_change,
                observed_checks: observed_checks.into_iter().map(recorded).collect(),
            },
            ObservedOccurrence::InterTurnChange { source_change } => {
                Self::InterTurnChange { source_change }
            }
            ObservedOccurrence::ObservedCheck {
                host_turn_ref,
                check,
            } => Self::ObservedCheck {
                host_turn_ref,
                check: recorded(check),
            },
        }
    }

    /// The occurrence as the host sent it.
    #[must_use]
    pub fn reported(&self) -> ObservedOccurrence {
        match self {
            Self::UnadmittedTurn {
                host_turn_ref,
                source_change,
                observed_checks,
            } => ObservedOccurrence::UnadmittedTurn {
                host_turn_ref: host_turn_ref.clone(),
                source_change: source_change.clone(),
                observed_checks: observed_checks
                    .iter()
                    .map(|check| check.check.clone())
                    .collect(),
            },
            Self::InterTurnChange { source_change } => ObservedOccurrence::InterTurnChange {
                source_change: source_change.clone(),
            },
            Self::ObservedCheck {
                host_turn_ref,
                check,
            } => ObservedOccurrence::ObservedCheck {
                host_turn_ref: host_turn_ref.clone(),
                check: check.check.clone(),
            },
        }
    }

    /// The occurrence kind as the host protocol spells it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UnadmittedTurn { .. } => "unadmitted_turn",
            Self::InterTurnChange { .. } => "inter_turn_change",
            Self::ObservedCheck { .. } => "observed_check",
        }
    }

    /// The reported source change, if any.
    #[must_use]
    pub fn source_change(&self) -> Option<&ObservedSourceChange> {
        match self {
            Self::UnadmittedTurn { source_change, .. } => source_change.as_ref(),
            Self::InterTurnChange { source_change } => Some(source_change),
            Self::ObservedCheck { .. } => None,
        }
    }

    /// Every stored check, in the order the host sent them.
    #[must_use]
    pub fn checks(&self) -> Vec<&RecordedObservedCheck> {
        match self {
            Self::UnadmittedTurn {
                observed_checks, ..
            } => observed_checks.iter().collect(),
            Self::InterTurnChange { .. } => Vec::new(),
            Self::ObservedCheck { check, .. } => vec![check],
        }
    }

    /// The host's turn reference, when the occurrence is inside a turn.
    #[must_use]
    pub fn host_turn_ref(&self) -> Option<&str> {
        match self {
            Self::UnadmittedTurn { host_turn_ref, .. }
            | Self::ObservedCheck { host_turn_ref, .. } => Some(host_turn_ref),
            Self::InterTurnChange { .. } => None,
        }
    }
}

/// Who caused what the host observed, as far as anyone knows.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationCausality {
    /// A struct variant, so a field beside `kind` is refused, not dropped:
    /// serde skips extra keys on an internally tagged unit variant.
    Unknown {},
    /// The host's assertion, never upgraded into verified authorship.
    HostAssertion {
        #[serde(deserialize_with = "strict_claimed_actor")]
        claimed_actor: Box<ActorContext>,
        basis: String,
    },
}

/// Bytes of each text field of an asserted cause's actor.
pub const MAX_CLAIMED_ACTOR_TEXT_BYTES: usize = 512;
/// Provenance links an asserted cause's actor may carry.
pub const MAX_CLAIMED_ACTOR_PROVENANCE_LINKS: usize = 8;

/// An asserted actor decoded as strictly as the rest of the request: a field
/// `ActorContext` does not name, here or in a provenance link, is refused
/// rather than dropped, so it can neither vanish from the record nor change
/// silently under a committed key.
fn strict_claimed_actor<'de, D>(deserializer: D) -> Result<Box<ActorContext>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    const ACTOR_FIELDS: &[&str] = &[
        "actor_id",
        "actor_kind",
        "assurance",
        "run_id",
        "session_id",
        "source_tool",
        "source_skill",
        "provenance_chain",
        "reason",
    ];
    const LINK_FIELDS: &[&str] = &["relation", "source", "reference"];
    let value = strict::StrictValue::deserialize(deserializer)?.0;
    let unknown = |object: &serde_json::Map<String, serde_json::Value>, known: &[&str]| {
        object
            .keys()
            .find(|key| !known.contains(&key.as_str()))
            .cloned()
    };
    let object = value
        .as_object()
        .ok_or_else(|| D::Error::custom("claimed_actor must be an object"))?;
    if let Some(field) = unknown(object, ACTOR_FIELDS) {
        return Err(D::Error::unknown_field(&field, ACTOR_FIELDS));
    }
    if let Some(links) = object
        .get("provenance_chain")
        .and_then(serde_json::Value::as_array)
    {
        for link in links {
            if let Some(field) = link.as_object().and_then(|link| unknown(link, LINK_FIELDS)) {
                return Err(D::Error::unknown_field(&field, LINK_FIELDS));
            }
        }
    }
    serde_json::from_value(value)
        .map(Box::new)
        .map_err(D::Error::custom)
}

/// The asserted actor is only ever asserted, and bounded.
fn validate_claimed_actor(actor: &ActorContext) -> Result<(), String> {
    if actor.assurance != super::AssuranceLevel::Asserted {
        return Err(
            "causality.claimed_actor must carry assurance asserted; a host cannot assert a stronger one"
                .into(),
        );
    }
    actor
        .validate_attribution_context()
        .map_err(|error| format!("causality.claimed_actor: {error}"))?;
    bounded_text(
        &actor.actor_id,
        MAX_CLAIMED_ACTOR_TEXT_BYTES,
        "causality.claimed_actor.actor_id",
    )?;
    for (value, label) in [
        (Some(actor.actor_kind.as_str()), "actor_kind"),
        (Some(actor.reason.as_str()), "reason"),
        (actor.run_id.as_deref(), "run_id"),
        (actor.source_tool.as_deref(), "source_tool"),
        (actor.source_skill.as_deref(), "source_skill"),
    ] {
        if value.is_some_and(|value| value.len() > MAX_CLAIMED_ACTOR_TEXT_BYTES) {
            return Err(format!(
                "causality.claimed_actor.{label} exceeds {MAX_CLAIMED_ACTOR_TEXT_BYTES} bytes"
            ));
        }
    }
    if actor.provenance_chain.len() > MAX_CLAIMED_ACTOR_PROVENANCE_LINKS
        || actor.provenance_chain.iter().any(|link| {
            link.source.len() > MAX_CLAIMED_ACTOR_TEXT_BYTES
                || link
                    .reference
                    .as_ref()
                    .is_some_and(|reference| reference.len() > MAX_CLAIMED_ACTOR_TEXT_BYTES)
        })
    {
        return Err(format!(
            "causality.claimed_actor carries at most {MAX_CLAIMED_ACTOR_PROVENANCE_LINKS} provenance links of at most {MAX_CLAIMED_ACTOR_TEXT_BYTES} bytes each"
        ));
    }
    if let Some(session) = &actor.session_id {
        validate_session_id_length(&session.0).map_err(|_| {
            "causality.claimed_actor session id exceeds the live session-id bound".to_owned()
        })?;
    }
    Ok(())
}

/// Whether the host asks for the record to enter source-change accounting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationPolicyBasis {
    /// Keep the fact; account for nothing. A struct variant, so a field
    /// beside `mode` is refused, not dropped.
    AuditOnly {},
    /// Account under the project's current policy, named exactly.
    AccountIfEligible {
        project_policy_epoch: ProjectPolicyEpoch,
        policy: ObjectId,
        obligation_rule_set: ObjectId,
    },
}

/// The admission an observation records: always none.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationAdmission {
    Unadmitted,
}

/// Why a recorded observation stays out of source-change accounting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationAuditReason {
    ExplicitAudit,
    FinishedRun,
    HistoricalBinding,
    RootBasisMoved,
}

/// How a recorded observation entered source-change accounting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationAccounting {
    /// A new change, the anchor later repeats point at. The receipt names
    /// the record itself; the stored record leaves the id out, since a record
    /// cannot carry its own id, and its absence means "this record".
    SourceChange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_change: Option<ObjectId>,
    },
    /// The same revision as the anchor, with no other revision between.
    Repeat {
        source_change: ObjectId,
    },
    /// No change was reported; nothing is cleared and freshness stays.
    NoSourceChange {},
    AuditOnly {
        reason: ObservationAuditReason,
    },
}

/// The body of one `execution_observe` request, past its routing token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObserveInput {
    pub idempotency_key: String,
    pub binding: ControlWorkBinding,
    pub root_basis: ObservationRootBasis,
    pub observed_interval: ObservedInterval,
    pub occurrence: ObservedOccurrence,
    pub causality: ObservationCausality,
    pub policy_basis: ObservationPolicyBasis,
}

/// One immutable record of execution observed without admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnadmittedExecutionObservation {
    pub schema_version: u16,
    pub project_id: ProjectId,
    /// The control session that recorded it, separate from any cause.
    pub observing_session: SessionId,
    pub observer: ActorContext,
    pub binding: ControlWorkBinding,
    /// The canonical work event that records the bound claim epoch at or
    /// before the capture cut.
    pub claim_epoch_event: ObjectId,
    pub root_basis: ObservationRootBasis,
    pub observed_interval: ObservedInterval,
    pub occurrence: RecordedOccurrence,
    pub causality: ObservationCausality,
    pub policy_basis: ObservationPolicyBasis,
    pub admission: ObservationAdmission,
    pub accounting: ObservationAccounting,
    pub recorded_at: DateTime<Utc>,
}

/// One check's audit summary on a receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedCheckSummary {
    pub host_check_id: String,
    pub credit: ObservedCheckCredit,
}

/// What `execution_observe` returns: the fact was recorded, and nothing more.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionObservationDecision {
    Recorded,
}

/// Host-private receipt for one recorded unadmitted observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationReceipt {
    pub decision: ExecutionObservationDecision,
    pub observation: ObjectId,
    /// The record's position on the bound run's feed.
    pub position: FeedPosition,
    pub observing_session: SessionId,
    pub binding: ControlWorkBinding,
    pub admission: ObservationAdmission,
    pub causality: ObservationCausality,
    pub policy_basis: ObservationPolicyBasis,
    pub accounting: ObservationAccounting,
    pub opened_obligations: Vec<ObjectId>,
    pub observed_checks: Vec<ObservedCheckSummary>,
    pub recorded_at: DateTime<Utc>,
}

impl ExecutionObserveInput {
    /// Checks the request's own shape against `recorded_at`, before any store
    /// read: bounds, required relations between its facts, and times that
    /// lie inside the observed window and not after recording. Nothing is
    /// truncated; any violation refuses the whole request.
    ///
    /// # Errors
    ///
    /// Returns the first violation, described for the host.
    pub fn validate_shape(&self, recorded_at: DateTime<Utc>) -> Result<(), String> {
        bounded_label(&self.idempotency_key, "idempotency_key")?;
        validate_observed_facts(
            &ObservedFacts {
                binding: &self.binding,
                root_basis: &self.root_basis,
                observed_interval: self.observed_interval,
                occurrence: &self.occurrence,
                causality: &self.causality,
                policy_basis: &self.policy_basis,
            },
            recorded_at,
        )
    }
}

impl UnadmittedExecutionObservation {
    /// Checks a stored record's own shape: the same rules its request
    /// passed, against its record time, plus the constants it must carry.
    ///
    /// # Errors
    ///
    /// Returns the first violation.
    pub fn validate_recorded_shape(&self) -> Result<(), String> {
        if self.schema_version != UNADMITTED_EXECUTION_OBSERVATION_SCHEMA_VERSION {
            return Err("unsupported unadmitted observation schema version".into());
        }
        if self.observer.session_id.as_ref() != Some(&self.observing_session)
            || self.observer.run_id.as_deref() != Some(self.binding.run_id.0.to_string().as_str())
        {
            return Err("the observer is not bound to its session and run".into());
        }
        let occurrence = self.occurrence.reported();
        validate_observed_facts(
            &ObservedFacts {
                binding: &self.binding,
                root_basis: &self.root_basis,
                observed_interval: self.observed_interval,
                occurrence: &occurrence,
                causality: &self.causality,
                policy_basis: &self.policy_basis,
            },
            self.recorded_at,
        )
    }
}

/// The facts a request asserts, apart from its key.
struct ObservedFacts<'a> {
    binding: &'a ControlWorkBinding,
    root_basis: &'a ObservationRootBasis,
    observed_interval: ObservedInterval,
    occurrence: &'a ObservedOccurrence,
    causality: &'a ObservationCausality,
    policy_basis: &'a ObservationPolicyBasis,
}

fn validate_observed_facts(
    facts: &ObservedFacts<'_>,
    recorded_at: DateTime<Utc>,
) -> Result<(), String> {
    {
        if facts.binding.work_revision < 1 || facts.binding.claim_fence < 1 {
            return Err("binding work_revision and claim_fence must be positive".into());
        }
        if facts.root_basis.capture_run_cut < 1 {
            return Err("root_basis.capture_run_cut must be a positive run-feed position".into());
        }
        let interval = facts.observed_interval;
        if interval.from > interval.through {
            return Err("observed_interval.from is after observed_interval.through".into());
        }
        if interval.through > recorded_at {
            return Err("observed_interval.through is after the record time".into());
        }
        match facts.occurrence {
            ObservedOccurrence::UnadmittedTurn {
                host_turn_ref,
                source_change,
                observed_checks,
            } => {
                bounded_label(host_turn_ref, "host_turn_ref")?;
                if let Some(change) = source_change {
                    validate_source_change(change, interval)?;
                }
                validate_checks(observed_checks, interval)?;
            }
            ObservedOccurrence::InterTurnChange { source_change } => {
                validate_source_change(source_change, interval)?;
            }
            ObservedOccurrence::ObservedCheck {
                host_turn_ref,
                check,
            } => {
                bounded_label(host_turn_ref, "host_turn_ref")?;
                validate_checks(std::slice::from_ref(check), interval)?;
            }
        }
        if let ObservationCausality::HostAssertion {
            claimed_actor,
            basis,
        } = facts.causality
        {
            bounded_text(basis, MAX_OBSERVATION_CAUSAL_BASIS_BYTES, "causality.basis")?;
            validate_claimed_actor(claimed_actor)?;
        }
        if let ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch,
            ..
        } = facts.policy_basis
            && project_policy_epoch.0 < 1
        {
            return Err("policy_basis.project_policy_epoch must be positive".into());
        }
        Ok(())
    }
}

fn bounded_label(value: &str, label: &str) -> Result<(), String> {
    bounded_text(value, MAX_OBSERVATION_LABEL_BYTES, label)
}

fn bounded_text(value: &str, max_bytes: usize, label: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.trim() != value || value.len() > max_bytes {
        return Err(format!(
            "{label} must be trimmed, nonblank and at most {max_bytes} UTF-8 bytes"
        ));
    }
    Ok(())
}

fn within(at: DateTime<Utc>, interval: ObservedInterval, label: &str) -> Result<(), String> {
    if at < interval.from || at > interval.through {
        return Err(format!("{label} lies outside observed_interval"));
    }
    Ok(())
}

fn validate_source_basis(basis: &ExecutionSourceBasis, label: &str) -> Result<(), String> {
    bounded_text(
        &basis.workspace_id,
        MAX_OBSERVED_SOURCE_BYTES,
        &format!("{label}.workspace_id"),
    )?;
    bounded_text(
        &basis.source_revision,
        MAX_OBSERVED_SOURCE_BYTES,
        &format!("{label}.source_revision"),
    )?;
    if !matches!(
        (basis.source_root_generation, basis.source_root_state),
        (None, None) | (Some(1..), Some(_))
    ) {
        return Err(format!(
            "{label} named-root generation and state must occur together, with a positive generation"
        ));
    }
    Ok(())
}

fn validate_source_change(
    change: &ObservedSourceChange,
    interval: ObservedInterval,
) -> Result<(), String> {
    let workspace_id = change.workspace_id();
    bounded_text(
        workspace_id,
        MAX_OBSERVED_SOURCE_BYTES,
        "source_change.workspace_id",
    )?;
    match change {
        ObservedSourceChange::ContentComparison {
            baseline, sighting, ..
        } => {
            bounded_text(
                &baseline.workspace_id,
                MAX_OBSERVED_SOURCE_BYTES,
                "source_change.baseline.workspace_id",
            )?;
            bounded_text(
                &baseline.source_revision,
                MAX_OBSERVED_SOURCE_BYTES,
                "source_change.baseline.source_revision",
            )?;
            validate_source_basis(
                &sighting.source_basis,
                "source_change.sighting.source_basis",
            )?;
            if baseline.workspace_id != workspace_id
                || sighting.source_basis.workspace_id != workspace_id
            {
                return Err("a measured source change names one workspace throughout".into());
            }
            if baseline.source_revision == sighting.source_basis.source_revision {
                return Err(
                    "content_comparison needs two different revisions; an equal pair is no change"
                        .into(),
                );
            }
            within(baseline.observed_at, interval, "source_change.baseline")?;
            within(sighting.observed_at, interval, "source_change.sighting")?;
            if baseline.observed_at > sighting.observed_at {
                return Err("source_change.baseline was observed after its sighting".into());
            }
        }
        ObservedSourceChange::AssumedMissingBaseline { sighting, .. } => {
            validate_source_basis(
                &sighting.source_basis,
                "source_change.sighting.source_basis",
            )?;
            if sighting.source_basis.workspace_id != workspace_id {
                return Err("a measured source change names one workspace throughout".into());
            }
            within(sighting.observed_at, interval, "source_change.sighting")?;
        }
        ObservedSourceChange::WatcherOnly { observed_at, .. } => {
            within(*observed_at, interval, "source_change.observed_at")?;
        }
    }
    Ok(())
}

fn validate_checks(checks: &[ObservedCheck], interval: ObservedInterval) -> Result<(), String> {
    if checks.len() > MAX_OBSERVED_CHECKS {
        return Err(format!(
            "an unadmitted turn reports at most {MAX_OBSERVED_CHECKS} observed checks"
        ));
    }
    let mut ids = HashSet::new();
    for check in checks {
        bounded_label(&check.host_check_id, "observed check host_check_id")?;
        if !ids.insert(check.host_check_id.as_str()) {
            return Err(format!(
                "observed check id {:?} appears twice in one request",
                check.host_check_id
            ));
        }
        let label = format!("observed check {:?}", check.host_check_id);
        if let (Some(started), Some(finished)) = (check.started_at, check.finished_at)
            && started > finished
        {
            return Err(format!("{label} started after it finished"));
        }
        for (at, part) in [
            (check.started_at, "started_at"),
            (check.finished_at, "finished_at"),
            (Some(check.observed_at), "observed_at"),
        ] {
            if let Some(at) = at {
                within(at, interval, &format!("{label} {part}"))?;
            }
        }
        if matches!(
            check.observed_result,
            VerificationResult::Passed | VerificationResult::Failed
        ) && check.finished_at.is_none()
        {
            return Err(format!(
                "{label} reports a finished result without finished_at; an unfinished check is indeterminate"
            ));
        }
        if let Some(basis) = &check.source_basis {
            validate_source_basis(basis, &format!("{label} source_basis"))?;
        }
        if let Some(reference) = &check.host_evidence_ref {
            bounded_text(
                reference,
                MAX_OBSERVED_EVIDENCE_REF_BYTES,
                &format!("{label} host_evidence_ref"),
            )?;
        }
    }
    Ok(())
}
