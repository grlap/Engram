//! A new planning request keeps the original deferred source selection.

use super::{DeltaBasis, DeltaIntent};
use crate::{
    ObjectId,
    storage::StoreError,
    task_sync::ledger::{Binding, Direction, fingerprint, require},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    RootExecutionId, WorkId, WorkRunId, task_sync::reconciliation::ReconciliationWorkBasis,
};

/// An explicit successor, never an instruction to retry or rewrite the old
/// operation. Storage must resolve these original record identities and check
/// the fresh destination under its reconciliation writer before admitting it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningSuccessorRequest {
    pub deferred: ObjectId,
    pub hold: ObjectId,
    pub original_prepared: ObjectId,
    pub original_candidate: ObjectId,
    pub intent: DeltaIntent,
}

/// One original local run and the new event planned to account for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedRunRetirement {
    pub run: WorkRunId,
    pub event: ObjectId,
}

/// The global exchange predecessor and event envelope selected once before
/// retaining a new successor request. This is not an applied planning frontier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningSuccessorChain {
    pub predecessor: ObjectId,
    pub prepared_sequence: u64,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

/// A native obligation definition selected before the successor is retained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedSuccessorObligation {
    pub obligation_id: crate::WorkObligationId,
    pub record: ObjectId,
}

/// A local waiver event for an existing native obligation, without rewriting it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedSuccessorResolution {
    pub obligation_id: crate::WorkObligationId,
    pub record: ObjectId,
}

/// Local construction identities frozen once for a genuinely new operation.
/// Original foreign or deferred record identities are never reminted here.
/// These planned identities grant no write or hold-resolution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningSuccessorRecipe {
    pub request: PlanningSuccessorRequest,
    pub before: Vec<ReconciliationWorkBasis>,
    pub prepared: ObjectId,
    pub outcome: ObjectId,
    pub baseline: ObjectId,
    pub planning: ObjectId,
    /// Ordered local events for each selected task. Reopen, revision and
    /// relation transitions retain separate native event identities.
    pub task_events: BTreeMap<WorkId, Vec<ObjectId>>,
    /// Fresh, unclaimed generations selected by the native reopen owner.
    pub new_runs: BTreeMap<WorkId, WorkRunId>,
    /// Fresh aggregates keyed by their root, including a runless restored root
    /// whose selected child is reopening. Existing aggregates are reused.
    pub new_root_executions: BTreeMap<WorkId, RootExecutionId>,
    /// The original shared blocker id selects a new local canonical record;
    /// incoming blocker identities and attribution are preserved separately.
    pub blocker_records: BTreeMap<WorkId, BTreeMap<String, ObjectId>>,
    pub obligation_definitions: BTreeMap<WorkId, Vec<PlannedSuccessorObligation>>,
    pub obligation_resolutions: BTreeMap<WorkId, Vec<PlannedSuccessorResolution>>,
    pub chain: Option<PlanningSuccessorChain>,
    pub retirements: Vec<PlannedRunRetirement>,
}

impl PlanningSuccessorRecipe {
    /// Build the preparation from the retained identities and event envelope.
    /// The storage owner resolves the original deferred facts and admits the
    /// current destination; this constructor grants no write permission.
    ///
    /// # Errors
    /// Refuses an unbound envelope or a payload outside the selected intent.
    pub fn prepared_event(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        payload: super::DeltaCandidatePayload,
    ) -> Result<crate::task_sync::ledger::Event, StoreError> {
        self.validate()?;
        payload.validate_successor(binding_id, binding, &self.request.intent)?;
        let chain = self.chain.as_ref().ok_or_else(|| {
            crate::task_sync::invalid("planning successor event envelope is not retained")
        })?;
        let event = crate::task_sync::ledger::Event {
            schema_version: 1,
            binding: binding_id.clone(),
            sequence: chain.prepared_sequence,
            predecessor: Some(chain.predecessor.clone()),
            actor: self.request.intent.actor.clone(),
            recorded_at: chain.recorded_at,
            transition: crate::task_sync::ledger::Transition::PlanningSuccessorPrepared {
                recipe: Box::new(self.clone()),
                payload: Box::new(payload),
            },
        };
        self.validate_historical_prepared(&event, binding)?;
        Ok(event)
    }

    /// Compare the retained envelope directly without rebuilding or admitting
    /// its historical payload. New construction uses this same structural check.
    pub(crate) fn validate_historical_prepared(
        &self,
        event: &crate::task_sync::ledger::Event,
        binding: &Binding,
    ) -> Result<(), StoreError> {
        self.validate()?;
        let crate::task_sync::ledger::Transition::PlanningSuccessorPrepared { recipe, payload } =
            &event.transition
        else {
            return Err(crate::task_sync::invalid(
                "successor preparation family differs",
            ));
        };
        payload.validate_historical_successor(&event.binding, binding, &self.request.intent)?;
        let chain = self.chain.as_ref().ok_or_else(|| {
            crate::task_sync::invalid("planning successor event envelope is not retained")
        })?;
        require(
            recipe.as_ref() == self
                && event.schema_version == 1
                && event.sequence == chain.prepared_sequence
                && event.predecessor.as_ref() == Some(&chain.predecessor)
                && event.actor == self.request.intent.actor
                && event.recorded_at == chain.recorded_at,
            "successor preparation differs from its retained event envelope",
        )
    }

    /// Keep the original terminal identity/envelope around the actual atomic
    /// writer's receipt. The old operation's deferred event remains unchanged.
    ///
    /// # Errors
    /// Refuses another basis, hold or planning record.
    pub fn applied_event(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        receipt: super::DeltaImportReceipt,
    ) -> Result<crate::task_sync::ledger::Event, StoreError> {
        self.validate()?;
        receipt.validate_successor(binding_id, binding, &self.request.intent)?;
        require(
            receipt.before == self.before
                && receipt.planning == self.planning
                && receipt.holds == [self.request.hold.clone()]
                && receipt.terminal_evidence.is_empty(),
            "planning successor receipt differs from its retained construction",
        )?;
        let chain = self.chain.as_ref().ok_or_else(|| {
            crate::task_sync::invalid("planning successor event envelope is not retained")
        })?;
        Ok(crate::task_sync::ledger::Event {
            schema_version: 1,
            binding: binding_id.clone(),
            sequence: chain.prepared_sequence + 1,
            predecessor: Some(self.prepared.clone()),
            actor: self.request.intent.actor.clone(),
            recorded_at: chain.recorded_at,
            transition: crate::task_sync::ledger::Transition::PlanningSuccessorApplied {
                prepared: self.prepared.clone(),
                receipt: Box::new(receipt),
            },
        })
    }

    /// Allocate local identities for one new request. Retrying an unfinished
    /// request must reload this recipe instead of calling this constructor.
    /// The caller still retains the exact recipe before any external effect.
    ///
    /// # Errors
    /// Refuses repeated or malformed captured work/run membership and retirement
    /// outside that readset. No claim absence or host settlement is inferred.
    pub fn new(
        request: PlanningSuccessorRequest,
        before: Vec<ReconciliationWorkBasis>,
        retire: &[WorkRunId],
    ) -> Result<Self, StoreError> {
        let mut retirements = retire
            .iter()
            .map(|&run| PlannedRunRetirement {
                run,
                event: ObjectId::mint(),
            })
            .collect::<Vec<_>>();
        retirements.sort_by_key(|planned| planned.run.0);
        let value = Self {
            task_events: request
                .intent
                .candidate
                .changed
                .iter()
                .map(|&work| (work, vec![ObjectId::mint()]))
                .collect(),
            retirements,
            new_runs: BTreeMap::new(),
            new_root_executions: BTreeMap::new(),
            blocker_records: BTreeMap::new(),
            obligation_definitions: BTreeMap::new(),
            obligation_resolutions: BTreeMap::new(),
            chain: None,
            request,
            before,
            prepared: ObjectId::mint(),
            outcome: ObjectId::mint(),
            baseline: ObjectId::mint(),
            planning: ObjectId::mint(),
        };
        value.validate()?;
        Ok(value)
    }

    /// Select a new native generation while constructing the retained recipe,
    /// before any external effect. Recovery reloads these ids unchanged.
    /// This selects identities only; the native owner still admits reopening.
    ///
    /// # Errors
    /// Refuses another task or repeated generation selection.
    pub fn plan_reopen(&mut self, work: WorkId, root: bool) -> Result<(), StoreError> {
        require(
            self.task_events.contains_key(&work) && !self.new_runs.contains_key(&work),
            "planning successor reopening repeats or leaves its selected tasks",
        )?;
        self.new_runs.insert(work, WorkRunId::new());
        if root {
            self.new_root_executions
                .insert(work, RootExecutionId::new());
        }
        self.validate()
    }

    /// Freeze the event envelope before retaining or admitting this request.
    /// An unfinished retry reloads the original envelope instead.
    ///
    /// # Errors
    /// Refuses a repeated envelope, invalid predecessor or exhausted sequence.
    pub fn bind_chain(
        &mut self,
        predecessor: ObjectId,
        sequence: u64,
        recorded_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError> {
        require(
            self.chain.is_none(),
            "planning successor envelope is already selected",
        )?;
        require(
            predecessor.as_str().len() == 32 && sequence > 0 && sequence <= u64::MAX - 2,
            "planning successor predecessor or sequence is invalid",
        )?;
        self.chain = Some(PlanningSuccessorChain {
            predecessor,
            prepared_sequence: sequence + 1,
            recorded_at,
        });
        self.validate()
    }

    /// Validate a retained construction plan, without reallocation or admission.
    ///
    /// # Errors
    /// Refuses overlapping identities, changed task selection, repeated readset
    /// members or a retirement outside the exact captured generation inventory.
    pub fn validate(&self) -> Result<(), StoreError> {
        require(
            self.request.intent.direction == Direction::Incoming,
            "planning successor requires an incoming planning request",
        )?;
        require(
            crate::canonical::canonical_bytes(self)?.len()
                <= crate::task_sync::retention::MAX_IMPORT_PLAN,
            "planning successor construction metadata exceeds its byte bound",
        )?;
        require(
            self.task_events.keys().copied().collect::<BTreeSet<_>>()
                == self.request.intent.candidate.changed,
            "planning successor recipe task selection differs",
        )?;
        let mut work = BTreeSet::new();
        let mut runs = BTreeSet::new();
        for basis in &self.before {
            require(
                work.insert(basis.work_id) && basis.revision > 0,
                "planning successor recipe repeats or malforms a work basis",
            )?;
            let mut item_runs = BTreeSet::new();
            for run in &basis.runs {
                require(
                    runs.insert(run.run_id.0)
                        && item_runs.insert(run.run_id.0)
                        && run.generation > 0
                        && run.revision > 0
                        && run.claim_fence >= 0,
                    "planning successor recipe repeats or malforms a run basis",
                )?;
            }
            require(
                basis
                    .active_run_id
                    .is_none_or(|run| item_runs.contains(&run.0)),
                "planning successor active run is outside its captured work",
            )?;
        }
        require(
            self.retirements
                .iter()
                .all(|planned| runs.contains(&planned.run.0))
                && self
                    .retirements
                    .windows(2)
                    .all(|pair| pair[0].run.0 < pair[1].run.0),
            "planning successor retirement is outside its captured generations",
        )?;
        let mut native_ids = BTreeSet::new();
        for (work, run) in &self.new_runs {
            require(
                self.task_events.contains_key(work)
                    && !runs.contains(&run.0)
                    && !run.0.is_nil()
                    && native_ids.insert(run.0),
                "planning successor new run repeats an original or unselected identity",
            )?;
        }
        for (work, root) in &self.new_root_executions {
            require(
                (self.new_runs.contains_key(work)
                    || self.before.iter().any(|basis| basis.work_id == *work))
                    && !root.0.is_nil()
                    && native_ids.insert(root.0),
                "planning successor new root leaves its captured scope or repeats an identity",
            )?;
        }
        for (work, definitions) in &self.obligation_definitions {
            require(
                self.task_events.contains_key(work),
                "successor obligations leave the selected tasks",
            )?;
            for definition in definitions {
                require(
                    !definition.obligation_id.0.is_nil()
                        && native_ids.insert(definition.obligation_id.0),
                    "successor obligation definition identities overlap",
                )?;
            }
        }
        let mut resolved = BTreeSet::new();
        for (work, resolutions) in &self.obligation_resolutions {
            require(
                self.task_events.contains_key(work),
                "successor obligation resolutions leave the selected tasks",
            )?;
            for resolution in resolutions {
                require(
                    !resolution.obligation_id.0.is_nil()
                        && !native_ids.contains(&resolution.obligation_id.0)
                        && resolved.insert(resolution.obligation_id.0),
                    "successor obligation resolution selection overlaps",
                )?;
            }
        }
        require(
            self.blocker_records.iter().all(|(work, blockers)| {
                self.task_events.contains_key(work)
                    && blockers
                        .keys()
                        .all(|id| !id.trim().is_empty() && id.len() <= 4096)
            }),
            "planning successor blocker construction leaves its selected tasks",
        )?;
        let mut originals = BTreeSet::from([
            &self.request.deferred,
            &self.request.hold,
            &self.request.original_prepared,
            &self.request.original_candidate,
        ]);
        for basis in &self.before {
            originals.extend(basis.head.iter());
            originals.extend(basis.imported_head.iter());
        }
        let mut selected = BTreeSet::new();
        for id in [
            &self.prepared,
            &self.outcome,
            &self.baseline,
            &self.planning,
        ]
        .into_iter()
        .chain(self.task_events.values().flatten())
        .chain(
            self.blocker_records
                .values()
                .flat_map(|blockers| blockers.values()),
        )
        .chain(self.retirements.iter().map(|planned| &planned.event))
        .chain(
            self.obligation_definitions
                .values()
                .flatten()
                .map(|planned| &planned.record),
        )
        .chain(
            self.obligation_resolutions
                .values()
                .flatten()
                .map(|planned| &planned.record),
        ) {
            require(
                id.as_str().len() == 32
                    && selected.insert(id)
                    && !originals.contains(id)
                    && uuid::Uuid::parse_str(id.as_str()).is_ok_and(|value| {
                        !value.is_nil()
                            && !native_ids.contains(&value)
                            && !resolved.contains(&value)
                    }),
                "planning successor construction identities overlap",
            )?;
        }
        if let Some(chain) = &self.chain {
            require(
                chain.predecessor.as_str().len() == 32
                    && !selected.contains(&chain.predecessor)
                    && chain.prepared_sequence > 1
                    && chain.prepared_sequence < u64::MAX,
                "planning successor retained event envelope differs",
            )?;
        }
        Ok(())
    }
}

impl PlanningSuccessorRequest {
    /// Check the new intent against the original selected source. A fresh local
    /// cut, destination baseline and local output are allowed; changing the
    /// original source requires a different reconciliation candidate.
    ///
    /// This shape check supplies no claim, hold resolution or host settlement.
    ///
    /// # Errors
    /// Refuses reused operation identities, an outgoing request, changed source
    /// selection, producer or binding, and a mismatched original comparison.
    pub fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        original: &DeltaIntent,
    ) -> Result<(), StoreError> {
        original.validate(binding_id, binding)?;
        self.intent.validate_successor(binding_id, binding)?;
        fingerprint(&self.original_candidate)?;
        require(
            self.deferred != self.hold
                && self.deferred != self.original_prepared
                && self.hold != self.original_prepared,
            "planning successor original record identities overlap",
        )?;
        require(
            self.intent.operation != original.operation
                && self.intent.key != original.key
                && self.intent.producer == original.producer
                && self.original_candidate == original.candidate.content_fingerprint,
            "planning successor reuses an operation or changes its original candidate",
        )?;
        let (
            DeltaBasis::Incoming {
                source_before: original_before,
                source_after: original_after,
                source_changed: original_changed,
                ..
            },
            DeltaBasis::Incoming {
                source_before,
                source_after,
                source_changed,
                ..
            },
        ) = (&original.basis, &self.intent.basis)
        else {
            return Err(crate::task_sync::invalid(
                "planning successor requires original and new incoming intents",
            ));
        };
        require(
            original.direction == Direction::Incoming
                && self.intent.direction == Direction::Incoming
                && source_before == original_before
                && source_after == original_after
                && source_changed == original_changed
                && self.intent.common_commit == original.common_commit,
            "planning successor changes its retained source selection",
        )
    }
}
