//! Attributed reconciliation of one outgoing operation. The original intent and
//! publication witness remain immutable; a resolved candidate has its own
//! retention operation, without becoming a second pending ledger operation.

use super::{Deserialize, Intent, ObjectId, Serialize, Transition};
use crate::task_sync::delta::{DeltaCandidatePayload, DeltaIntent};

mod delta;
pub(crate) use delta::DeltaResolutionRecipe;

/// A known failure allows reconciliation. Transport uncertainty is deliberately
/// absent: it remains a `PublicationObservation::Unknown` on the original run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutgoingFailure {
    AncestryChanged {
        fetched_tip: String,
    },
    NonFastForward {
        push_started: ObjectId,
        fetched_tip: String,
    },
}

impl OutgoingFailure {
    pub(crate) fn fetched_tip(&self) -> &str {
        match self {
            Self::AncestryChanged { fetched_tip } | Self::NonFastForward { fetched_tip, .. } => {
                fetched_tip
            }
        }
    }
}

/// The only retry is selected once, started once, and either confirmed by the
/// ordinary publication observation or stopped explicitly. Replaying a stored
/// step returns that event; it never authorizes repeating its external effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutgoingReconciliation {
    Resolved {
        failure: OutgoingFailure,
        /// A fresh retained candidate/commit recipe. Its operation UUID owns
        /// artifacts only; the ledger operation stays the original prepared id.
        resolved: Box<Intent>,
    },
    DeltaResolved {
        failure: OutgoingFailure,
        /// Changed bytes against the original native preparation. The merged
        /// Git membership does not replace that preparation's native cut.
        resolved: Box<DeltaIntent>,
        payload: Box<DeltaCandidatePayload>,
        commit: Box<super::RetainedCommit>,
    },
    PushStarted {
        reconciliation: ObjectId,
    },
    Stopped {
        failure: OutgoingFailure,
    },
}

impl Transition {
    pub(crate) fn retained_delta(&self) -> Option<(&DeltaIntent, &DeltaCandidatePayload)> {
        match self {
            Self::DeltaPrepared { intent, payload } => Some((intent, payload)),
            Self::OutgoingReconciled { reconciliation, .. } => match reconciliation.as_ref() {
                OutgoingReconciliation::DeltaResolved {
                    resolved, payload, ..
                } => Some((resolved, payload)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Artifact recipes include the original preparation and its one resolved
    /// retry. This does not make the latter a second pending ledger request.
    pub(crate) fn retained_intent(&self) -> Option<&Intent> {
        match self {
            Self::Prepared { intent } => Some(intent),
            Self::OutgoingReconciled { reconciliation, .. } => match reconciliation.as_ref() {
                OutgoingReconciliation::Resolved { resolved, .. } => Some(resolved),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn retention_operation(&self) -> Option<uuid::Uuid> {
        if let Self::PlanningSuccessorPrepared { recipe, .. } = self {
            return Some(recipe.request.intent.operation);
        }
        if let Self::EvidencePrepared { intent } = self {
            return Some(intent.operation);
        }
        self.retained_delta()
            .map(|(intent, _)| intent.operation)
            .or_else(|| self.retained_intent().map(|intent| intent.operation))
    }

    pub(crate) fn retention_key(&self) -> Option<&str> {
        if let Self::PlanningSuccessorPrepared { recipe, .. } = self {
            return Some(&recipe.request.intent.key);
        }
        if let Self::EvidencePrepared { intent } = self {
            return Some(&intent.key);
        }
        self.retained_delta()
            .map(|(intent, _)| intent.key.as_str())
            .or_else(|| self.retained_intent().map(|intent| intent.key.as_str()))
    }

    /// The canonical recipe owning one publication witness. A retry names its
    /// resolved recipe, preserving the original preparation and witness too.
    pub(crate) fn publication_request(&self) -> Option<&ObjectId> {
        match self {
            Self::PushStarted { prepared } => Some(prepared),
            Self::OutgoingReconciled { reconciliation, .. } => match reconciliation.as_ref() {
                OutgoingReconciliation::PushStarted { reconciliation } => Some(reconciliation),
                _ => None,
            },
            _ => None,
        }
    }
}
