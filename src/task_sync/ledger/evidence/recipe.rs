//! Identity-stable construction bytes, prepared before file or SQL mutation.

use super::{
    Binding, EvidenceIntent, EvidenceReceipt, EvidenceStop, ObjectId, StoreError, limits, require,
};
use crate::task_sync::ledger::{Event, Transition};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Exact preparation and the fixed envelope of one terminal event. The planned
/// applied transition may be replaced by a known refusal under the same id.
/// Retention owns these bytes before the writer can use them. This recipe
/// grants no admission, publication, applicability or execution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceImportRecipe {
    pub prepared_id: ObjectId,
    pub prepared: Event,
    pub outcome_id: ObjectId,
    pub outcome: Event,
}

impl EvidenceImportRecipe {
    /// Construct a refusal using the original terminal identity and envelope.
    ///
    /// # Errors
    /// Refuses invalid terminal metadata.
    pub fn stopped_event(&self, outcome: EvidenceStop) -> Result<Event, StoreError> {
        outcome.validate()?;
        let mut event = self.outcome.clone();
        event.transition = Transition::EvidenceStopped {
            prepared: self.prepared_id.clone(),
            outcome,
        };
        Ok(event)
    }

    /// Compare actual canonical terminal metadata with the retained template.
    ///
    /// # Errors
    /// Refuses invalid refusal metadata, changed envelopes or another family.
    pub fn matches_outcome(&self, event: &Event) -> Result<bool, StoreError> {
        match &event.transition {
            Transition::EvidenceImportApplied { .. } => Ok(event == &self.outcome),
            Transition::EvidenceStopped { prepared, outcome } if prepared == &self.prepared_id => {
                Ok(event == &self.stopped_event(outcome.clone())?)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn recover(
        prepared_id: ObjectId,
        prepared: Event,
        outcome_id: ObjectId,
        actual: &Event,
    ) -> Result<Self, StoreError> {
        let Transition::EvidencePrepared { intent } = &prepared.transition else {
            return Err(crate::task_sync::invalid(
                "evidence recipe preparation family differs",
            ));
        };
        let mut outcome = actual.clone();
        outcome.transition = Transition::EvidenceImportApplied {
            prepared: prepared_id.clone(),
            receipt: EvidenceReceipt {
                admissions: intent
                    .packs
                    .iter()
                    .map(|(id, selected)| (id.clone(), selected.admission.clone()))
                    .collect(),
            },
        };
        let recipe = Self {
            prepared_id,
            prepared,
            outcome_id,
            outcome,
        };
        require(
            recipe.matches_outcome(actual)?,
            "evidence terminal differs from retained construction",
        )?;
        Ok(recipe)
    }

    /// Select event ids and timestamps once, before retaining any payload.
    /// An unfinished retry must load the original recipe instead of calling
    /// this constructor again. The SQL owner rechecks the selected chain head.
    ///
    /// # Errors
    /// Refuses an invalid selection, exhausted sequence or oversized recipe.
    pub fn prepare(
        binding_id: &ObjectId,
        binding: &Binding,
        intent: EvidenceIntent,
        predecessor: &ObjectId,
        sequence: u64,
        recorded_at: DateTime<Utc>,
    ) -> Result<Self, StoreError> {
        let prepared_id = ObjectId::mint();
        let outcome_id = ObjectId::mint();
        let receipt = EvidenceReceipt {
            admissions: intent
                .packs
                .iter()
                .map(|(id, selected)| (id.clone(), selected.admission.clone()))
                .collect(),
        };
        let prepared = Event {
            schema_version: 1,
            binding: binding_id.clone(),
            sequence: sequence
                .checked_add(1)
                .ok_or_else(|| crate::task_sync::invalid("evidence sequence exhausted"))?,
            predecessor: Some(predecessor.clone()),
            actor: intent.actor.clone(),
            recorded_at,
            transition: Transition::EvidencePrepared {
                intent: Box::new(intent),
            },
        };
        let outcome = Event {
            schema_version: 1,
            binding: binding_id.clone(),
            sequence: prepared
                .sequence
                .checked_add(1)
                .ok_or_else(|| crate::task_sync::invalid("evidence sequence exhausted"))?,
            predecessor: Some(prepared_id.clone()),
            actor: prepared.actor.clone(),
            recorded_at,
            transition: Transition::EvidenceImportApplied {
                prepared: prepared_id.clone(),
                receipt,
            },
        };
        let recipe = Self {
            prepared_id,
            prepared,
            outcome_id,
            outcome,
        };
        recipe.validate(binding_id, binding)?;
        Ok(recipe)
    }

    /// Validate retained construction bytes without minting replacements.
    ///
    /// # Errors
    /// Refuses another family, changed attribution, reused identities, a broken
    /// event chain, a different receipt or complete construction metadata bounds.
    pub fn validate(&self, binding_id: &ObjectId, binding: &Binding) -> Result<(), StoreError> {
        let Transition::EvidencePrepared { intent } = &self.prepared.transition else {
            return Err(crate::task_sync::invalid(
                "evidence recipe preparation family differs",
            ));
        };
        intent.validate(binding_id, binding)?;
        let Transition::EvidenceImportApplied { prepared, receipt } = &self.outcome.transition
        else {
            return Err(crate::task_sync::invalid(
                "evidence recipe outcome family differs",
            ));
        };
        receipt.validate(intent)?;
        let mut ids = std::collections::BTreeSet::new();
        for id in [&self.prepared_id, &self.outcome_id].into_iter().chain(
            intent
                .packs
                .iter()
                .flat_map(|(id, selected)| [id, &selected.admission]),
        ) {
            require(
                id.as_str().len() == 32 && ids.insert(id),
                "evidence recipe identities are missing or reused",
            )?;
        }
        require(
            self.prepared.schema_version == 1
                && self.outcome.schema_version == 1
                && self.prepared.binding == *binding_id
                && self.outcome.binding == *binding_id
                && self.prepared.sequence > 1
                && self.prepared.sequence.checked_add(1) == Some(self.outcome.sequence)
                && self
                    .prepared
                    .predecessor
                    .as_ref()
                    .is_some_and(|id| !ids.contains(id))
                && prepared == &self.prepared_id
                && self.outcome.predecessor.as_ref() == Some(&self.prepared_id)
                && self.prepared.actor == intent.actor
                && self.outcome.actor == intent.actor
                && self.prepared.recorded_at == self.outcome.recorded_at,
            "evidence recipe event chain or attribution differs",
        )?;
        require(
            crate::canonical::canonical_bytes(self)?.len() <= limits::MAX_PLAN_BYTES,
            "evidence recipe exceeds construction metadata bound",
        )
    }

    /// The original request in this construction recipe.
    ///
    /// # Errors
    /// Refuses a deserialized recipe containing another preparation family.
    pub fn intent(&self) -> Result<&EvidenceIntent, StoreError> {
        match &self.prepared.transition {
            Transition::EvidencePrepared { intent } => Ok(intent),
            _ => Err(crate::task_sync::invalid(
                "evidence recipe preparation family differs",
            )),
        }
    }
}
