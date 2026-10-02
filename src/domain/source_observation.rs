//! A host's source record as source-change accounting reads it: an admitted
//! turn's execution observation, or an unadmitted observation that was
//! accounted. Accounting compares workspaces, revisions, root lifecycles and
//! times; it never needs a grant, an effect or a producer, so one view serves
//! both kinds. Verification producers stay admitted observations only.

use chrono::{DateTime, Utc};

use super::{
    ControlWorkBinding, ExecutionObservation, ExecutionSourceBasis, ObservationAccounting,
    ObservationPolicyBasis, ObservedSourceChange, ProjectId, SessionId, SourceChangeDetection,
    UnadmittedExecutionObservation,
};
use crate::ObjectId;

/// One source record on a run, of either kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceObservation {
    pub record: ObjectId,
    /// Whether a turn admitted it; an unadmitted record was accounted.
    pub admitted: bool,
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    /// The session that reported it.
    pub reporting_session: SessionId,
    /// How a person names it: the host's observation id for an admitted
    /// record, `unadmitted:<record id>` for an unadmitted one.
    pub label: String,
    /// Whether it counts as a change, as accounting read it: an admitted
    /// record's `source_changed`, or an unadmitted record accounted as a new
    /// change. A repeat is a sighting that is not a change.
    pub source_changed: bool,
    pub reported_source_change: Option<SourceChangeDetection>,
    /// The measured source; `None` when no closing revision was taken.
    pub source_basis: Option<ExecutionSourceBasis>,
    pub observed_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    /// The rule set it opened obligations under.
    pub obligation_rule_set: ObjectId,
}

impl SourceObservation {
    /// An admitted turn's observation.
    #[must_use]
    pub fn admitted(record: ObjectId, observation: &ExecutionObservation) -> Self {
        Self {
            record,
            admitted: true,
            project_id: observation.project_id.clone(),
            binding: observation.binding.clone(),
            reporting_session: observation.session_id.clone(),
            label: observation.observation_id.clone(),
            source_changed: observation.source_changed,
            reported_source_change: observation.reported_source_change,
            source_basis: observation.source_basis.clone(),
            observed_at: observation.observed_at,
            recorded_at: observation.recorded_at,
            obligation_rule_set: observation.obligation_rule_set.clone(),
        }
    }

    /// An unadmitted record, when accounting read it as a source record: a
    /// new change or a repeat of one. `None` for an audit-only record, one
    /// that reported no change, or one recorded under no accounting basis.
    #[must_use]
    pub fn unadmitted(
        record: ObjectId,
        observation: &UnadmittedExecutionObservation,
    ) -> Option<Self> {
        let source_changed = match &observation.accounting {
            ObservationAccounting::SourceChange { .. } => true,
            ObservationAccounting::Repeat { .. } => false,
            ObservationAccounting::NoSourceChange {} | ObservationAccounting::AuditOnly { .. } => {
                return None;
            }
        };
        let ObservationPolicyBasis::AccountIfEligible {
            obligation_rule_set,
            ..
        } = &observation.policy_basis
        else {
            return None;
        };
        let change = observation.occurrence.source_change()?;
        let (source_basis, observed_at, detection) = match change {
            ObservedSourceChange::ContentComparison { sighting, .. } => (
                Some(sighting.source_basis.clone()),
                sighting.observed_at,
                SourceChangeDetection::ContentComparison,
            ),
            ObservedSourceChange::AssumedMissingBaseline { sighting, .. } => (
                Some(sighting.source_basis.clone()),
                sighting.observed_at,
                SourceChangeDetection::AssumedMissingBaseline,
            ),
            ObservedSourceChange::WatcherOnly { observed_at, .. } => {
                (None, *observed_at, SourceChangeDetection::WatcherOnly)
            }
        };
        Some(Self {
            label: format!("unadmitted:{record}"),
            record,
            admitted: false,
            project_id: observation.project_id.clone(),
            binding: observation.binding.clone(),
            reporting_session: observation.observing_session.clone(),
            source_changed,
            reported_source_change: Some(detection),
            source_basis,
            observed_at: Some(observed_at),
            recorded_at: observation.recorded_at,
            obligation_rule_set: obligation_rule_set.clone(),
        })
    }
}
