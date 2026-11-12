//! One validator for observed Git custody against actual SQL ledger records.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use super::{Ledger, StoreError, Transition};
use crate::task_sync::{ledger::require, retention};

#[derive(Debug)]
pub(crate) struct Observed {
    root: std::path::PathBuf,
    inventory: retention::RetainedInventory,
    publications: BTreeMap<uuid::Uuid, retention::PublicationWitness>,
    audits: retention::audit::AuditInventory,
    abandoned: BTreeMap<uuid::Uuid, crate::task_sync::ledger::RetainedCommit>,
    resolution: Option<crate::task_sync::exchange::InspectedDeltaResolution>,
    evidence: Option<crate::task_sync::ledger::EvidenceImportRecipe>,
    successor: Option<retention::successor::RetainedSuccessor>,
    retired: BTreeMap<
        uuid::Uuid,
        (
            crate::ObjectId,
            super::audit::retained::ObservedConstruction,
        ),
    >,
    retirement: Option<(
        crate::task_sync::delta::CustodyAuditRequest,
        super::audit::retained::ObservedConstruction,
    )>,
}

impl Observed {
    pub(crate) fn capture(root: &Path) -> Result<Self, StoreError> {
        let inventory =
            retention::inventory(root).map_err(|e| super::super::invalid(e.to_string()))?;
        let mut publications = BTreeMap::new();
        for operation in &inventory.pushes {
            let witness = retention::read_publication_witness(root, *operation)
                .map_err(|e| super::super::invalid(e.to_string()))?
                .ok_or_else(|| {
                    super::super::invalid("publication witness disappeared; preserve uncertainty")
                })?;
            publications.insert(*operation, witness);
        }
        let audits = retention::audit::inventory(root)
            .map_err(|error| super::super::invalid(error.to_string()))?;
        let mut abandoned = BTreeMap::new();
        let mut retired = BTreeMap::new();
        for audit in audits.events.values() {
            if let Transition::CustodyCheckpoint { checkpoint } = &audit.event.transition
                && checkpoint.artifacts.iter().any(|artifact| {
                    artifact.operation == checkpoint.operation
                        && super::audit::retained::artifact_family(artifact).is_some()
                })
            {
                let construction = super::audit::retained::ObservedConstruction::capture(
                    root,
                    &inventory,
                    checkpoint.operation,
                )?;
                require(
                    retired
                        .insert(checkpoint.operation, (audit.event_id.clone(), construction))
                        .is_none(),
                    "construction has competing retained retirement owners",
                )?;
            }
            if let Transition::CustodyCheckpoint { checkpoint } = &audit.event.transition
                && let Some(artifact) = checkpoint.abandoned_commit()
            {
                let commit = retention::commit_selection(root, artifact.operation)
                    .map_err(|error| super::super::invalid(error.to_string()))?
                    .ok_or_else(|| super::super::invalid("abandoned carrier selection missing"))?;
                super::audit::orphan::git_pin(root, &commit)?;
                require(
                    abandoned.insert(artifact.operation, commit).is_none(),
                    "abandoned carrier has competing retained checkpoints",
                )?;
            }
        }
        Ok(Self {
            root: root.to_owned(),
            inventory,
            publications,
            audits,
            abandoned,
            resolution: None,
            evidence: None,
            successor: None,
            retired,
            retirement: None,
        })
    }

    pub(crate) fn admit_retirement(
        &mut self,
        connection: &rusqlite::Connection,
        request: &crate::task_sync::delta::CustodyAuditRequest,
    ) -> Result<bool, StoreError> {
        use super::view::read;
        require(
            connection.is_autocommit(),
            "observe construction custody before the SQL writer",
        )?;
        let mut budget = read::ReadBudget::default();
        let binding = read::binding_on(connection, &request.binding, &mut budget)?;
        if binding.repository.git_directory != self.root {
            return Ok(false);
        }
        if let Some((id, construction)) = self.retired.get(&request.operation) {
            construction.matches_request(request)?;
            let retained = &self.audits.events[id];
            let Transition::CustodyCheckpoint { checkpoint } = &retained.event.transition else {
                return Err(super::super::invalid("retirement checkpoint missing"));
            };
            require(
                retained.event.binding == request.binding
                    && checkpoint.request_fingerprint == request.fingerprint()?,
                "retirement replay differs from its retained audit request",
            )?;
            return Ok(true);
        }
        let construction = super::audit::retained::ObservedConstruction::capture(
            &self.root,
            &self.inventory,
            request.operation,
        )?;
        construction.matches_request(request)?;
        let bindings = self.bindings_on(connection)?;
        construction.prove_on(connection, &bindings, &request.binding, None)?;
        self.retirement = Some((request.clone(), construction));
        Ok(true)
    }

    fn bindings_on(
        &self,
        connection: &rusqlite::Connection,
    ) -> Result<BTreeMap<crate::ObjectId, super::Binding>, StoreError> {
        use super::view::{read, select};
        let mut budget = read::ReadBudget::default();
        select::bindings(connection, select::Bindings::Root(&self.root))?
            .into_iter()
            .map(|id| read::binding_on(connection, &id, &mut budget).map(|binding| (id, binding)))
            .collect()
    }

    fn retirements_on(
        &self,
        connection: &rusqlite::Connection,
        bindings: &BTreeMap<crate::ObjectId, super::Binding>,
        operation: Option<uuid::Uuid>,
        request: Option<&crate::ObjectId>,
        pending: Option<&crate::task_sync::origin::private::PendingAudit>,
    ) -> Result<BTreeSet<uuid::Uuid>, StoreError> {
        use super::view::read;
        let mut result = BTreeSet::new();
        for (operation, (id, construction)) in &self.retired {
            let retained = &self.audits.events[id];
            let Transition::CustodyCheckpoint { checkpoint } = &retained.event.transition else {
                return Err(super::super::invalid("retirement checkpoint missing"));
            };
            let pending_owner = pending
                .map(|pending| pending.matches_retained(retained))
                .transpose()?
                .unwrap_or(false);
            if !pending_owner {
                let event: super::Event = read::object(
                    connection,
                    id,
                    super::EVENT_KIND,
                    &mut read::ReadBudget::default(),
                )?;
                require(
                    event == retained.event && self.audits.acknowledgements.contains_key(id),
                    "retirement has no exact canonical acknowledgement",
                )?;
            }
            let expected = construction.artifacts()?;
            require(
                construction.key()? == checkpoint.key
                    && checkpoint
                        .artifacts
                        .iter()
                        .filter(|artifact| artifact.operation == *operation)
                        .cloned()
                        .collect::<Vec<_>>()
                        == expected,
                "retirement checkpoint differs from exact retained construction artifacts",
            )?;
            construction.prove_on(connection, bindings, &retained.event.binding, Some(id))?;
            construction.recheck_files(&self.root)?;
            result.insert(*operation);
        }
        if let Some((selected, construction)) = &self.retirement {
            require(
                operation == Some(selected.operation) && request == Some(&selected.fingerprint()?),
                "retirement differs from admitted exact audit",
            )?;
            construction.matches_request(selected)?;
            construction.prove_on(connection, bindings, &selected.binding, None)?;
            construction.recheck_files(&self.root)?;
            require(
                result.insert(selected.operation),
                "construction has competing retirement contexts",
            )?;
        }
        Ok(result)
    }

    fn retirements_ledger(&self, ledger: &Ledger) -> Result<BTreeSet<uuid::Uuid>, StoreError> {
        let mut result = BTreeSet::new();
        for (operation, (id, construction)) in &self.retired {
            let retained = &self.audits.events[id];
            let event = ledger.events.get(id).ok_or_else(|| {
                super::super::invalid("retired construction has no canonical checkpoint")
            })?;
            require(
                event == &retained.event && self.audits.acknowledgements.contains_key(id),
                "retired construction acknowledgement differs",
            )?;
            let Transition::CustodyCheckpoint { checkpoint } = &event.transition else {
                return Err(super::super::invalid("retirement checkpoint missing"));
            };
            require(
                checkpoint.key == construction.key()?
                    && checkpoint
                        .artifacts
                        .iter()
                        .filter(|artifact| artifact.operation == *operation)
                        .cloned()
                        .collect::<Vec<_>>()
                        == construction.artifacts()?,
                "retired construction artifacts differ",
            )?;
            construction.prove_ledger(ledger, &event.binding, Some(id))?;
            construction.recheck_files(&self.root)?;
            result.insert(*operation);
        }
        Ok(result)
    }

    pub(super) fn abandoned_pin(
        &self,
        operation: uuid::Uuid,
        commit: &crate::task_sync::ledger::RetainedCommit,
    ) -> Result<(), StoreError> {
        require(
            self.abandoned.get(&operation) == Some(commit),
            "abandoned carrier differs from its pre-writer Git observation",
        )
    }

    /// Exception for one complete inspected recipe, never candidate-only text.
    /// Ordinary captures have no exception. This context is memory-only and
    /// loses all admission power when its exact private request differs.
    pub(crate) fn admit_delta_resolution(
        &mut self,
        input: &crate::task_sync::exchange::InspectedDeltaResolution,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        input
            .recheck_files()
            .map_err(|error| super::super::invalid(error.to_string()))?;
        require(
            input
                .request_fingerprint()
                .map_err(|error| super::super::invalid(error.to_string()))?
                == *request,
            "delta resolution custody belongs to another exact recipe",
        )?;
        self.resolution = Some(input.clone());
        Ok(())
    }

    pub(crate) fn admit_evidence_recipe(
        &mut self,
        binding: &super::Binding,
        recipe: &crate::task_sync::ledger::EvidenceImportRecipe,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        recipe.validate(&recipe.prepared.binding, binding)?;
        let operation = recipe.intent()?.operation;
        require(
            self.inventory.evidence_imports.get(&operation) == Some(recipe)
                && !self.inventory.candidates.contains(&operation)
                && !self.inventory.delta_candidates.contains_key(&operation)
                && !self.inventory.imports.contains(&operation)
                && !self.inventory.pushes.contains(&operation),
            "evidence construction aliases another retained family",
        )?;
        require(
            retention::evidence::request_fingerprint(recipe)
                .map_err(|error| super::super::invalid(error.to_string()))?
                == *request,
            "evidence custody request fingerprint differs",
        )?;
        require(
            retention::evidence::read_recipe(&recipe.prepared.binding, binding, recipe.intent()?)
                .map_err(|error| super::super::invalid(error.to_string()))?
                .as_ref()
                == Some(recipe),
            "evidence custody differs from retained recipe",
        )?;
        self.evidence = Some(recipe.clone());
        Ok(())
    }

    pub(crate) fn admit_successor(
        &mut self,
        binding: &super::Binding,
        retained: &retention::successor::RetainedSuccessor,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        retained
            .prepared_event(binding)
            .map_err(|error| super::super::invalid(error.to_string()))?;
        let operation = retained.recipe.request.intent.operation;
        require(
            self.inventory.planning_successors.get(&operation) == Some(retained)
                && retained
                    .request_fingerprint()
                    .map_err(|error| super::super::invalid(error.to_string()))?
                    == *request
                && retention::successor::read(&retained.binding, binding, &retained.recipe.request)
                    .map_err(|error| super::super::invalid(error.to_string()))?
                    .as_ref()
                    == Some(retained),
            "successor custody differs from its exact retained construction",
        )?;
        self.successor = Some(retained.clone());
        Ok(())
    }

    fn admits_successor(
        &self,
        retained: &retention::successor::RetainedSuccessor,
        operation: Option<uuid::Uuid>,
        request: Option<&crate::ObjectId>,
    ) -> Result<bool, StoreError> {
        Ok(self.successor.as_ref() == Some(retained)
            && operation == Some(retained.recipe.request.intent.operation)
            && request
                == Some(
                    &retained
                        .request_fingerprint()
                        .map_err(|error| super::super::invalid(error.to_string()))?,
                ))
    }

    fn admits_evidence(
        &self,
        recipe: &crate::task_sync::ledger::EvidenceImportRecipe,
        operation: Option<uuid::Uuid>,
        request: Option<&crate::ObjectId>,
    ) -> Result<bool, StoreError> {
        Ok(self.evidence.as_ref() == Some(recipe)
            && operation == Some(recipe.intent()?.operation)
            && request
                == Some(
                    &retention::evidence::request_fingerprint(recipe)
                        .map_err(|error| super::super::invalid(error.to_string()))?,
                ))
    }

    fn admits_delta(
        &self,
        retained: &retention::delta::RetainedRequest,
        operation: Option<uuid::Uuid>,
    ) -> bool {
        self.resolution.as_ref().is_some_and(|input| {
            input.operation().ok() == operation && input.original.binding_id == retained.binding
                && matches!(&input.recipe, crate::task_sync::ledger::OutgoingReconciliation::DeltaResolved {resolved,..}
                    if resolved.as_ref() == &retained.intent)
        })
    }
}

type OperationKey = (crate::ObjectId, crate::ObjectId);
type Operations = BTreeMap<OperationKey, super::view::read::CheckedOperation>;

fn check_facts(
    observed: &Observed,
    admitted_operation: Option<uuid::Uuid>,
    is_known: impl Fn(&uuid::Uuid) -> bool,
    publication_matches: impl Fn(&retention::PublicationWitness) -> bool,
    required: &BTreeSet<uuid::Uuid>,
) -> Result<(), StoreError> {
    let inventory = &observed.inventory;
    require(
        inventory
            .candidates
            .union(&inventory.commits)
            .all(|operation| admitted_operation == Some(*operation) || is_known(operation)),
        "retained operations are ahead of this ledger; preserve both sides for reconciliation",
    )?;
    require(
        inventory.imports.iter().all(is_known),
        "retained import plans are ahead of this ledger; preserve both sides for reconciliation",
    )?;
    for operation in &inventory.pushes {
        let witness = observed.publications.get(operation).ok_or_else(|| {
            super::super::invalid("publication witness disappeared; preserve uncertainty")
        })?;
        require(
            publication_matches(witness),
            "publication witness is ahead of this ledger; reconcile without retrying transport",
        )?;
    }
    require(
        required
            .iter()
            .all(|operation| inventory.pushes.contains(operation)),
        "publication witness is missing; preserve uncertainty",
    )
}

/// Existing full-admission writers already own this ledger. Preserve their
/// filesystem-alias comparison while sharing all custody obligations below.
pub(super) fn check(
    ledger: &Ledger,
    root: &Path,
    observed: &Observed,
    admitted_operation: Option<uuid::Uuid>,
    same_root: impl Fn(&Path, &Path) -> bool,
) -> Result<(), StoreError> {
    let bootstrap = super::audit::data::known(ledger, root)?;
    let retired = observed.retirements_ledger(ledger)?;
    let abandoned = super::audit::orphan::known_captured(ledger, root, observed)?;
    for (operation, artifact) in &abandoned {
        require(observed.audits.events.iter().any(|(id, retained)| {
            ledger.events.get(id) == Some(&retained.event)
                && matches!(&retained.event.transition, Transition::CustodyCheckpoint { checkpoint }
                    if checkpoint.operation == *operation && checkpoint.abandoned_commit() == Some(artifact))
                && (observed.audits.acknowledgements.contains_key(id) || admitted_operation == Some(*operation))
        }), "abandoned carrier has no retained custody checkpoint")?;
    }
    for (operation, commit) in &bootstrap {
        require(
            observed.audits.events.iter().any(|(id, retained)| {
                ledger.events.get(id) == Some(&retained.event)
                    && matches!(&retained.event.transition, Transition::CustodyCheckpoint { checkpoint }
                        if checkpoint.operation == *operation && checkpoint.data_commit() == Some(commit))
                    && (observed.audits.acknowledgements.contains_key(id)
                        || admitted_operation == Some(*operation))
            }),
            "data bootstrap has no retained custody checkpoint",
        )?;
        super::audit::data::validate_pin(root, &observed.inventory, *operation, commit)?;
    }
    for recipe in observed.inventory.evidence_imports.values() {
        if retired.contains(&recipe.intent()?.operation) {
            continue;
        }
        let binding = ledger
            .bindings
            .get(&recipe.prepared.binding)
            .ok_or_else(|| super::super::invalid("retained evidence binding is unavailable"))?;
        recipe.validate(&recipe.prepared.binding, binding)?;
        require(
            same_root(&binding.repository.git_directory, root)
                && ledger.events.get(&recipe.prepared_id) == Some(&recipe.prepared)
                && ledger
                    .events
                    .get(&recipe.outcome_id)
                    .map(|event| recipe.matches_outcome(event))
                    .transpose()?
                    .unwrap_or(false),
            "retained evidence recipe is ahead of canonical progress; preserve exact construction",
        )?;
    }
    for retained in observed.inventory.planning_successors.values() {
        if retired.contains(&retained.recipe.request.intent.operation) {
            continue;
        }
        let binding = ledger
            .bindings
            .get(&retained.binding)
            .ok_or_else(|| super::super::invalid("retained successor binding is unavailable"))?;
        let prepared = retained
            .prepared_event(binding)
            .map_err(|error| super::super::invalid(error.to_string()))?;
        require(
            same_root(&binding.repository.git_directory, root)
                && ledger.events.get(&retained.recipe.prepared) == Some(&prepared)
                && ledger
                    .events
                    .get(&retained.recipe.outcome)
                    .map(|event| retained.matches_outcome(binding, event))
                    .transpose()
                    .map_err(|error| super::super::invalid(error.to_string()))?
                    .unwrap_or(false),
            "retained successor is ahead of canonical progress; preserve exact construction",
        )?;
    }
    require(
        observed.audits.pending_acknowledgements.is_empty(),
        "custody audit acknowledgement remains pending; exact recovery required",
    )?;
    for (id, audit) in &observed.audits.events {
        require(
            ledger.events.get(id) == Some(&audit.event)
                && observed.audits.acknowledgements.contains_key(id)
                && ledger
                    .bindings
                    .get(&audit.event.binding)
                    .is_some_and(|binding| same_root(&binding.repository.git_directory, root)),
            "retained custody audit is ahead of this ledger; preserve exact pending intent",
        )?;
    }
    let mut known = ledger
        .events
        .values()
        .filter_map(|event| {
            let binding = ledger.bindings.get(&event.binding)?;
            if !same_root(&binding.repository.git_directory, root) {
                return None;
            }
            event.transition.retention_operation()
        })
        .collect::<BTreeSet<_>>();
    require(
        bootstrap.keys().all(|operation| !known.contains(operation)),
        "data bootstrap operation aliases an ordinary exchange",
    )?;
    known.extend(bootstrap.keys().copied());
    require(
        abandoned.keys().all(|operation| !known.contains(operation)),
        "abandoned carrier aliases an ordinary exchange",
    )?;
    known.extend(abandoned.keys().copied());
    require(
        retired.iter().all(|operation| !known.contains(operation)),
        "retired construction aliases canonical custody",
    )?;
    known.extend(retired.iter().copied());
    for retained in observed.inventory.delta_candidates.values() {
        if retired.contains(&retained.intent.operation) {
            continue;
        }
        let binding = ledger
            .bindings
            .get(&retained.binding)
            .ok_or_else(|| super::super::invalid("retained delta binding is unavailable"))?;
        retained.intent.validate(&retained.binding, binding)?;
        require(
            same_root(&binding.repository.git_directory, root)
                && (observed.admits_delta(retained, admitted_operation)
                    || ledger.events.values().any(|event| {
                        event.binding == retained.binding
                            && event
                                .transition
                                .retained_delta()
                                .is_some_and(|(intent, _)| intent == &retained.intent)
                    })),
            "retained delta differs from canonical preparation; preserve exact request",
        )?;
    }
    // Unknown legacy artifacts retain check_facts' existing ahead-of-ledger
    // diagnosis. A known preparation must still belong to the legacy family.
    require(
        observed
            .inventory
            .candidates
            .iter()
            .chain(&observed.inventory.imports)
            .all(|operation| {
                !known.contains(operation)
                    || ledger.events.values().any(|event| {
                        ledger.bindings.get(&event.binding).is_some_and(|binding| {
                            same_root(&binding.repository.git_directory, root)
                        }) && event
                            .transition
                            .retained_intent()
                            .is_some_and(|intent| intent.operation == *operation)
                    })
            }),
        "legacy retained artifact has no matching legacy preparation",
    )?;
    let mut required = BTreeSet::new();
    for event in ledger.events.values() {
        if let Some(request) = event.transition.publication_request() {
            let Some(binding) = ledger.bindings.get(&event.binding) else {
                continue;
            };
            if !same_root(&binding.repository.git_directory, root) {
                continue;
            }
            let operation = ledger
                .events
                .get(request)
                .and_then(|event| event.transition.retention_operation())
                .ok_or_else(|| super::super::invalid("publication intent missing"))?;
            required.insert(operation);
        }
    }
    check_facts(
        observed,
        admitted_operation,
        |operation| known.contains(operation),
        |witness| {
            ledger.events.get(&witness.event_id) == Some(&witness.event)
                && witness
                    .event
                    .transition
                    .publication_request()
                    .and_then(|request| ledger.events.get(request))
                    .is_some_and(|request| {
                        request.binding == witness.event.binding
                            && request.transition.retention_operation() == Some(witness.operation)
                    })
                && ledger
                    .bindings
                    .get(&witness.event.binding)
                    .is_some_and(|binding| same_root(&binding.repository.git_directory, root))
                && ledger.events.values().any(|event| {
                    event.binding == witness.event.binding
                        && event.transition.retention_operation() == Some(witness.operation)
                })
        },
        &required,
    )
}

fn cached<'a>(
    connection: &rusqlite::Connection,
    bindings: &BTreeMap<crate::ObjectId, super::Binding>,
    operations: &'a mut Operations,
    binding: &crate::ObjectId,
    prepared: &crate::ObjectId,
    budget: &mut super::view::read::ReadBudget,
) -> Result<&'a super::view::read::CheckedOperation, StoreError> {
    use std::collections::btree_map::Entry;
    match operations.entry((binding.clone(), prepared.clone())) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let actual = bindings.get(binding).ok_or_else(|| {
                super::super::invalid("publication binding does not own observed repository")
            })?;
            let operation =
                super::view::read::operation_on(connection, binding, actual, prepared, budget)?;
            Ok(entry.insert(operation))
        }
    }
}

pub(super) fn check_on(
    connection: &rusqlite::Connection,
    root: &Path,
    observed: &Observed,
    admitted_operation: Option<uuid::Uuid>,
    admitted_request: Option<&crate::ObjectId>,
    audit: Option<&crate::task_sync::origin::private::PendingAudit>,
    budget: &mut super::view::read::ReadBudget,
) -> Result<(), StoreError> {
    use super::view::{read, select};
    let mut bindings = BTreeMap::new();
    for id in select::bindings(connection, select::Bindings::Root(root))? {
        let binding = read::binding_on(connection, &id, budget)?;
        require(
            crate::storage::store_path_text(&binding.repository.git_directory)
                == crate::storage::store_path_text(root),
            "origin Git custody repository differs",
        )?;
        bindings.insert(id, binding);
    }
    let inventory = &observed.inventory;
    let retired = observed.retirements_on(
        connection,
        &bindings,
        admitted_operation,
        admitted_request,
        audit,
    )?;
    for (id, acknowledgement) in &observed.audits.pending_acknowledgements {
        require(
            audit.is_some_and(|audit| {
                &audit.event_id == id
                    && audit.binding == acknowledgement.binding
                    && audit.event_fingerprint == acknowledgement.event_fingerprint
                    && admitted_operation == Some(audit.operation)
            }),
            "custody audit acknowledgement remains pending; exact recovery required",
        )?;
        let committed: super::Event = read::object(connection, id, super::EVENT_KIND, budget)?;
        require(
            observed
                .audits
                .events
                .get(id)
                .is_some_and(|retained| retained.event == committed),
            "pending audit acknowledgement has no exact committed event",
        )?;
    }
    for (id, retained) in &observed.audits.events {
        if let Some(audit) = audit
            && admitted_operation == Some(audit.operation)
            && bindings.contains_key(&audit.binding)
            && audit.matches_retained(retained)?
        {
            continue;
        }
        let event: super::Event = read::object(connection, id, super::EVENT_KIND, budget)?;
        require(
            event == retained.event
                && bindings.contains_key(&event.binding)
                && observed.audits.acknowledgements.contains_key(id),
            "retained custody audit differs from this ledger; preserve exact pending intent",
        )?;
    }
    for recipe in inventory.evidence_imports.values() {
        if retired.contains(&recipe.intent()?.operation) {
            continue;
        }
        let binding = bindings
            .get(&recipe.prepared.binding)
            .ok_or_else(|| super::super::invalid("retained evidence binding is unavailable"))?;
        recipe.validate(&recipe.prepared.binding, binding)?;
        if observed.admits_evidence(recipe, admitted_operation, admitted_request)? {
            continue;
        }
        let prepared: super::Event =
            read::object(connection, &recipe.prepared_id, super::EVENT_KIND, budget)?;
        let outcome: super::Event =
            read::object(connection, &recipe.outcome_id, super::EVENT_KIND, budget)?;
        require(
            prepared == recipe.prepared && recipe.matches_outcome(&outcome)?,
            "retained evidence recipe differs from canonical progress; preserve exact construction",
        )?;
    }
    for retained in inventory.planning_successors.values() {
        if retired.contains(&retained.recipe.request.intent.operation) {
            continue;
        }
        let binding = bindings
            .get(&retained.binding)
            .ok_or_else(|| super::super::invalid("retained successor binding is unavailable"))?;
        let expected = retained
            .prepared_event(binding)
            .map_err(|error| super::super::invalid(error.to_string()))?;
        if observed.admits_successor(retained, admitted_operation, admitted_request)? {
            continue;
        }
        let prepared: super::Event = read::object(
            connection,
            &retained.recipe.prepared,
            super::EVENT_KIND,
            budget,
        )?;
        let outcome: super::Event = read::object(
            connection,
            &retained.recipe.outcome,
            super::EVENT_KIND,
            budget,
        )?;
        require(
            prepared == expected
                && retained
                    .matches_outcome(binding, &outcome)
                    .map_err(|error| super::super::invalid(error.to_string()))?,
            "retained successor differs from canonical progress; preserve exact construction",
        )?;
    }
    let selected = inventory
        .candidates
        .iter()
        .chain(&inventory.commits)
        .chain(&inventory.imports)
        .chain(&inventory.pushes)
        .chain(inventory.delta_candidates.keys())
        .chain(inventory.evidence_imports.keys())
        .chain(inventory.planning_successors.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut operations = Operations::new();
    let mut known = BTreeMap::new();
    let mut bootstrap = BTreeSet::new();
    for operation in selected {
        if retired.contains(&operation) {
            bootstrap.insert(operation);
            known.insert(operation, Vec::new());
            continue;
        }
        let mut matches = Vec::new();
        for id in bindings.keys() {
            let original = read::prepared_for_operation(connection, id, operation)?;
            let resolved = read::reconciliation_parent_for_operation(connection, id, operation)?;
            require(
                original.is_none() || resolved.is_none(),
                "retention operation aliases an original and resolved request",
            )?;
            let prepared = original.or(resolved);
            if let Some(prepared) = prepared {
                let event: super::Event =
                    read::object(connection, &prepared, super::EVENT_KIND, budget)?;
                if let Transition::CustodyCheckpoint { checkpoint } = &event.transition
                    && let Some(artifact) = checkpoint.abandoned_commit()
                {
                    checkpoint.validate(id, &bindings[id])?;
                    require(
                        checkpoint.operation == operation
                            && bootstrap.insert(operation)
                            && observed
                                .audits
                                .events
                                .get(&prepared)
                                .is_some_and(|retained| retained.event == event),
                        "abandoned carrier has competing owners",
                    )?;
                    let commit = super::audit::orphan::validate_accounted(
                        &bindings, id, root, inventory, artifact,
                    )?;
                    observed.abandoned_pin(operation, &commit)?;
                    continue;
                }
                if let Transition::CustodyCheckpoint { checkpoint } = &event.transition
                    && let Some(commit) = checkpoint.data_commit()
                {
                    checkpoint.validate(id, &bindings[id])?;
                    require(
                        checkpoint.operation == operation
                            && bootstrap.insert(operation)
                            && observed
                                .audits
                                .events
                                .get(&prepared)
                                .is_some_and(|retained| retained.event == event),
                        "data bootstrap operation has competing owners",
                    )?;
                    super::audit::data::validate_pin(root, inventory, operation, commit)?;
                    continue;
                }
                let actual = cached(
                    connection,
                    &bindings,
                    &mut operations,
                    id,
                    &prepared,
                    budget,
                )?;
                require(
                    actual.intent.operation() == operation
                        || actual.events.iter().any(|(_, event)| {
                            event.transition.retention_operation() == Some(operation)
                        }),
                    "publication intent operation differs",
                )?;
                matches.push((id.clone(), prepared));
            }
        }
        require(
            !bootstrap.contains(&operation) || matches.is_empty(),
            "data bootstrap operation aliases an ordinary exchange",
        )?;
        known.insert(operation, matches);
    }
    for (operation, recipe) in &inventory.evidence_imports {
        let matches = &known[operation];
        require(
            matches.is_empty()
                || (matches.len() == 1
                    && matches[0] == (recipe.prepared.binding.clone(), recipe.prepared_id.clone())),
            "evidence retention operation aliases another canonical request",
        )?;
    }
    for (operation, retained) in &inventory.planning_successors {
        let matches = &known[operation];
        require(
            matches.is_empty()
                || (matches.len() == 1
                    && matches[0] == (retained.binding.clone(), retained.recipe.prepared.clone())),
            "successor retention operation aliases another canonical request",
        )?;
    }
    for (operation, retained) in &inventory.delta_candidates {
        if retired.contains(operation) {
            continue;
        }
        let binding = bindings.get(&retained.binding).ok_or_else(|| {
            super::super::invalid("retained delta binding does not own observed repository")
        })?;
        retained.intent.validate(&retained.binding, binding)?;
        let matches = known
            .get(operation)
            .ok_or_else(|| super::super::invalid("retained delta operation was not selected"))?;
        let exact = if matches.is_empty() {
            (observed.admits_delta(retained, admitted_operation)
                && admitted_request
                    == observed
                        .resolution
                        .as_ref()
                        .map(crate::task_sync::exchange::InspectedDeltaResolution::request_fingerprint)
                        .transpose()
                        .map_err(|error| super::super::invalid(error.to_string()))?
                        .as_ref())
                || (admitted_operation == Some(*operation)
                    && admitted_request
                        == Some(
                            &retention::delta::request_fingerprint(
                                &retained.binding,
                                &retained.intent,
                            )
                            .map_err(|error| super::super::invalid(error.to_string()))?,
                        ))
        } else {
            matches.len() == 1
                && matches.iter().any(|key| {
                    key.0 == retained.binding
                        && operations.get(key).is_some_and(|actual| {
                            actual.events.iter().any(|(_, event)| {
                                event
                                    .transition
                                    .retained_delta()
                                    .is_some_and(|(intent, _)| intent == &retained.intent)
                            })
                        })
                })
        };
        require(
            exact,
            "retained delta differs from canonical or admitted exact request; preserve uncertainty",
        )?;
    }
    require(
        inventory
            .candidates
            .iter()
            .chain(&inventory.imports)
            .all(|operation| {
                known.get(operation).is_some_and(|matches| {
                    matches.is_empty()
                        || matches.iter().any(|key| {
                            operations.get(key).is_some_and(|actual| {
                                actual.events.iter().any(|(_, event)| {
                                    event
                                        .transition
                                        .retained_intent()
                                        .is_some_and(|intent| intent.operation == *operation)
                                })
                            })
                        })
                })
            }),
        "legacy retained artifact has no matching legacy preparation",
    )?;
    let mut required = BTreeSet::new();
    for binding in bindings.keys() {
        for id in select::pushes(connection, binding)? {
            let event: super::Event = read::object(connection, &id, super::EVENT_KIND, budget)?;
            let (Transition::PushStarted { prepared }
            | Transition::OutgoingReconciled { prepared, .. }) = &event.transition
            else {
                return Err(super::super::invalid(
                    "historical publication event differs",
                ));
            };
            require(
                &event.binding == binding,
                "historical publication binding differs",
            )?;
            let actual = cached(
                connection,
                &bindings,
                &mut operations,
                binding,
                prepared,
                budget,
            )?;
            require(
                actual
                    .events
                    .iter()
                    .any(|(actual_id, actual_event)| actual_id == &id && actual_event == &event),
                "historical publication operation differs",
            )?;
            let operation = event
                .transition
                .publication_request()
                .and_then(|request| actual.events.iter().find(|(id, _)| id == request))
                .and_then(|(_, event)| event.transition.retention_operation())
                .ok_or_else(|| super::super::invalid("publication recipe is missing"))?;
            required.insert(operation);
        }
    }
    check_facts(
        observed,
        admitted_operation,
        |operation| {
            bootstrap.contains(operation)
                || known
                    .get(operation)
                    .is_some_and(|matches| !matches.is_empty())
        },
        |witness| {
            known.get(&witness.operation).is_some_and(|matches| {
                matches.iter().any(|key| {
                    key.0 == witness.event.binding
                        && operations.get(key).is_some_and(|actual| {
                            actual.events.iter().any(|(id, event)| {
                                id == &witness.event_id && event == &witness.event
                            }) && witness
                                .event
                                .transition
                                .publication_request()
                                .and_then(|request| {
                                    actual.events.iter().find(|(id, _)| id == request)
                                })
                                .is_some_and(|(_, request)| {
                                    request.binding == witness.event.binding
                                        && request.transition.retention_operation()
                                            == Some(witness.operation)
                                })
                        })
                })
            })
        },
        &required,
    )
}
