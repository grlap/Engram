//! Explicit cold checkpoint admission. Files are retained before one canonical
//! writer; the existing origin owner acknowledges only its exact committed tail.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use super::super::{invalid, producing_origin};
use super::{
    Binding, CanonicalObject, EVENT_KIND, Event, Ledger, ObjectId, StoreError, Transition, require,
    view,
};
use crate::{
    ActorContext,
    storage::SqliteStore,
    task_sync::{
        self, TaskTree,
        delta::{
            BootstrapMember, BootstrapMembers, CheckpointSeed, CustodyArtifactKind,
            CustodyArtifactRef, CustodyAuditReceipt, CustodyAuditRequest, CustodySeedSelection,
            ManagedCustodyCheckpoint, MembershipSnapshot, OutputMembership,
        },
        inventory::InventoryRoot,
        origin::{continuity::PreparedContinuity, private},
        progress::ExportCut,
        retention::{
            self,
            audit::{AuditReservation, RetainedAudit},
        },
    },
};

pub(super) mod data;
pub(super) mod orphan;
pub(super) mod retained;

fn recorded(
    connection: &Connection,
    request: &CustodyAuditRequest,
) -> Result<Option<RetainedAudit>, StoreError> {
    let Some(id) = view::read::request_identity_on(
        connection,
        &request.binding,
        request.operation,
        &request.key,
    )?
    else {
        return Ok(None);
    };
    let event: Event = view::read::object(
        connection,
        &id,
        EVENT_KIND,
        &mut view::read::ReadBudget::default(),
    )?;
    let Transition::CustodyCheckpoint { checkpoint } = &event.transition else {
        return Err(invalid(
            "custody audit request collides with an ordinary exchange",
        ));
    };
    require(
        event.binding == request.binding
            && checkpoint.operation == request.operation
            && checkpoint.key == request.key
            && checkpoint.request_fingerprint == request.fingerprint()?,
        "custody audit request differs from its original checkpoint",
    )?;
    Ok(Some(RetainedAudit {
        event_id: id,
        event,
    }))
}

fn receipt(
    audit: &RetainedAudit,
    repeated: bool,
    acknowledged: bool,
) -> Result<CustodyAuditReceipt, StoreError> {
    let Transition::CustodyCheckpoint { checkpoint } = &audit.event.transition else {
        return Err(invalid("custody audit result is not a checkpoint"));
    };
    Ok(CustodyAuditReceipt {
        checkpoint: audit.event_id.clone(),
        operation: checkpoint.operation,
        request_fingerprint: checkpoint.request_fingerprint.clone(),
        repeated,
        acknowledged,
    })
}

pub(super) fn restore_basis(binding: &Binding) -> Result<(), StoreError> {
    use crate::backup::restore::{RestoreRecords, RestoreState, read_restore_record};
    let home = crate::project_home_of(&binding.store.database, &binding.project)
        .ok_or_else(|| invalid("custody audit requires its project-local store"))?;
    let observed = match read_restore_record(&home, &binding.project) {
        RestoreRecords::None => None,
        RestoreRecords::Recorded(record) if record.state == RestoreState::Completed => {
            Some(record.occurrence_id)
        }
        RestoreRecords::Recorded(_) => {
            return Err(invalid(
                "restore occurrence remains pending; finish or abandon it through the restore owner before custody audit",
            ));
        }
        RestoreRecords::Unreadable { .. } => {
            return Err(invalid(
                "restore occurrence is unreadable; custody audit admission is unavailable",
            ));
        }
    };
    require(
        observed == binding.store.restore_occurrence,
        "custody audit restore occurrence differs from its binding",
    )
}

fn cut(request: &CustodyAuditRequest) -> ExportCut {
    ExportCut {
        position: request.expected_local_cut.position.position,
        anchor: request.expected_local_cut.anchor.clone(),
    }
}

fn active(
    ledger: &Ledger,
    request: &CustodyAuditRequest,
    binding: &Binding,
) -> Result<(), StoreError> {
    require(
        ledger.bindings.get(&request.binding) == Some(binding)
            && !ledger
                .bindings
                .values()
                .any(|next| next.supersedes.as_ref() == Some(&request.binding)),
        "custody audit binding changed or was superseded",
    )
}

fn membership(
    tree: &TaskTree,
    profile: task_sync::progress::ExportProfile,
) -> Result<OutputMembership, StoreError> {
    let mut inventory_root = InventoryRoot::ZERO;
    for task in &tree.tasks {
        inventory_root = inventory_root.including(task)?;
    }
    Ok(OutputMembership {
        inventory_root,
        profile,
    })
}

fn embedded(tree: TaskTree) -> BootstrapMembers {
    BootstrapMembers::Captured {
        members: tree
            .tasks
            .into_iter()
            .map(|task| {
                (
                    task.id,
                    BootstrapMember::Document {
                        task: Box::new(task),
                    },
                )
            })
            .collect(),
    }
}

fn seed_on(
    connection: &Connection,
    ledger: &Ledger,
    request: &CustodyAuditRequest,
    binding: &Binding,
) -> Result<Option<CheckpointSeed>, StoreError> {
    let seed = match &request.seed {
        CustodySeedSelection::CustodyOnly
        | CustodySeedSelection::AbandonFetchedCommit { .. }
        | CustodySeedSelection::AbandonRetainedConstruction { .. } => {
            return Ok(None);
        }
        CustodySeedSelection::InitialImport { outcome } => {
            super::super::closure::source::initial_seed_on(
                connection,
                &request.binding,
                binding,
                outcome,
            )?
        }
        CustodySeedSelection::LocalExport => {
            let tree = super::super::super::query::on_one_snapshot(connection, |connection| {
                require(
                    producing_origin::cut_on(connection, &binding.project)? == cut(request),
                    "local export audit cut changed before capture",
                )?;
                Ok(super::super::export_on_with_profile(
                    connection,
                    &binding.project,
                    &binding.profile,
                )?
                .tree)
            })?;
            CheckpointSeed::PreparedExport {
                membership: membership(&tree, binding.profile.clone())?,
                members: embedded(tree),
            }
        }
        CustodySeedSelection::IncomingCommit { commit, prepared } => {
            // A complete cold read supplies task bytes, never an invented pin.
            // Select the existing canonical retention owner for this exact oid.
            let intent = ledger.events.get(prepared).filter(|event| event.binding == request.binding).and_then(|event| match &event.transition {
                Transition::Prepared { intent } if &intent.commit.oid == commit => Some(intent.as_ref()),
                _ => None,
            }).ok_or_else(|| task_sync::TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
                "incoming audit commit has no canonical retained selection; retain it through the existing exchange owner first"))?;
            let selected =
                retention::commit_selection(&binding.repository.git_directory, intent.operation)
                    .map_err(|error| invalid(error.to_string()))?;
            require(
                selected.as_ref() == Some(&intent.commit),
                "incoming audit commit retention differs or is missing",
            )?;
            let source = task_sync::git::read_task_snapshot(
                &binding.repository.main_checkout,
                &binding.project,
                commit,
            )
            .map_err(|error| invalid(error.to_string()))?;
            require(
                source.commit.oid == intent.commit.oid
                    && source.commit.tree == intent.commit.tree
                    && source.commit.parents == intent.commit.parents
                    && source.commit.raw_bytes_fingerprint == intent.commit.raw_bytes_fingerprint,
                "incoming audit commit differs from its exact retained bytes",
            )?;
            let reference = task_sync::git::query(
                &binding.repository.main_checkout,
                &[
                    "show-ref",
                    "--hash",
                    "--verify",
                    &intent.commit.retention_ref,
                ],
                false,
                None,
                crate::operation_time::MINIMUM,
            )
            .map_err(|error| invalid(error.to_string()))?;
            require(
                reference == format!("{commit}\n").as_bytes(),
                "incoming audit commit retention reference differs",
            )?;
            CheckpointSeed::Incoming {
                snapshot: Box::new(MembershipSnapshot {
                    commit: intent.commit.clone(),
                    membership: membership(&source.tree, source.envelope.profile)?,
                }),
                members: embedded(source.tree),
            }
        }
        CustodySeedSelection::IncomingDataCommit { commit } => {
            data::seed(binding, request.operation, commit)?
        }
    };
    Ok(Some(seed))
}

fn artifact<T: serde::Serialize>(
    operation: uuid::Uuid,
    kind: CustodyArtifactKind,
    record: ObjectId,
    value: &T,
    total: &mut usize,
) -> Result<CustodyArtifactRef, StoreError> {
    let object = CanonicalObject::freeze(value)?;
    *total = total
        .checked_add(object.bytes().len())
        .ok_or_else(|| invalid("cold custody artifact byte bound exceeded"))?;
    require(
        *total <= super::MAX_BYTES,
        "cold custody artifact byte bound exceeded",
    )?;
    Ok(CustodyArtifactRef {
        operation,
        kind,
        content_fingerprint: object.key().clone(),
        record: Some(record),
    })
}

pub(super) fn artifacts(
    connection: &Connection,
    ledger: &Ledger,
    roots: &BTreeSet<PathBuf>,
    bootstrap: Option<(&Binding, &CustodyAuditRequest)>,
) -> Result<Vec<CustodyArtifactRef>, StoreError> {
    let mut result = Vec::new();
    let mut total = 0;
    for root in roots {
        let mut data_commits = data::known(ledger, root)?;
        let abandoned = orphan::for_audit(ledger, root, bootstrap)?;
        let retired = retained::for_audit(ledger, root, bootstrap)?;
        for construction in retired.values() {
            result.extend(construction.artifacts()?);
        }
        if let Some((binding, request)) = bootstrap
            && let Some(commit) = data::selection(request)
            && crate::storage::store_path_text(&binding.repository.git_directory)
                == crate::storage::store_path_text(root)
        {
            require(
                data_commits
                    .get(&request.operation)
                    .is_none_or(|old| old == commit),
                "data bootstrap pin differs from its checkpoint",
            )?;
            data_commits.insert(request.operation, commit.clone());
        }
        let mut known = BTreeMap::new();
        let mut deltas = BTreeMap::new();
        for (id, event) in &ledger.events {
            if let Some((intent, _)) = event.transition.retained_delta()
                && ledger.bindings.get(&event.binding).is_some_and(|binding| {
                    crate::storage::store_path_text(&binding.repository.git_directory)
                        == crate::storage::store_path_text(root)
                })
            {
                require(
                    deltas
                        .insert(intent.operation, (id, event, intent))
                        .is_none(),
                    "ambiguous retained delta custody operation",
                )?;
            }
            if let Some(intent) = event.transition.retained_intent()
                && ledger.bindings.get(&event.binding).is_some_and(|binding| {
                    crate::storage::store_path_text(&binding.repository.git_directory)
                        == crate::storage::store_path_text(root)
                })
            {
                require(
                    known.insert(intent.operation, (id, intent)).is_none(),
                    "ambiguous retained custody operation",
                )?;
            }
        }
        require(
            deltas
                .keys()
                .all(|operation| !known.contains_key(operation)),
            "ambiguous legacy and delta custody operation",
        )?;
        let inventory = retention::inventory(root).map_err(|error| invalid(error.to_string()))?;
        for (operation, recipe) in &inventory.evidence_imports {
            if retired.contains_key(operation) {
                continue;
            }
            require(
                !known.contains_key(operation)
                    && !deltas.contains_key(operation)
                    && !data_commits.contains_key(operation)
                    && !abandoned.contains_key(operation)
                    && !inventory.planning_successors.contains_key(operation),
                "evidence custody operation aliases another family",
            )?;
            let binding = ledger
                .bindings
                .get(&recipe.prepared.binding)
                .ok_or_else(|| invalid("retained evidence binding missing during audit"))?;
            recipe.validate(&recipe.prepared.binding, binding)?;
            require(
                ledger.events.get(&recipe.prepared_id) == Some(&recipe.prepared)
                    && ledger
                        .events
                        .get(&recipe.outcome_id)
                        .map(|event| recipe.matches_outcome(event))
                        .transpose()?
                        .unwrap_or(false),
                "retained evidence recipe is ahead of canonical progress",
            )?;
            result.push(artifact(
                *operation,
                CustodyArtifactKind::EvidenceRecipe,
                recipe.prepared_id.clone(),
                recipe,
                &mut total,
            )?);
        }
        for (operation, retained) in &inventory.planning_successors {
            if retired.contains_key(operation) {
                continue;
            }
            let binding = ledger
                .bindings
                .get(&retained.binding)
                .ok_or_else(|| invalid("retained successor binding missing during audit"))?;
            let prepared = retained
                .prepared_event(binding)
                .map_err(|error| invalid(error.to_string()))?;
            require(
                ledger.events.get(&retained.recipe.prepared) == Some(&prepared)
                    && ledger
                        .events
                        .get(&retained.recipe.outcome)
                        .map(|event| retained.matches_outcome(binding, event))
                        .transpose()
                        .map_err(|error| invalid(error.to_string()))?
                        .unwrap_or(false),
                "retained successor is ahead of canonical progress",
            )?;
            result.push(artifact(
                *operation,
                CustodyArtifactKind::PlanningSuccessorRecipe,
                retained.recipe.prepared.clone(),
                retained,
                &mut total,
            )?);
        }
        for (operation, retained) in &inventory.delta_candidates {
            if retired.contains_key(operation) {
                continue;
            }
            let (record, event, intent) = deltas
                .get(operation)
                .ok_or_else(|| invalid("retained delta custody is ahead of this ledger"))?;
            require(
                retained.binding == event.binding && &retained.intent == *intent,
                "retained delta differs from its canonical intent",
            )?;
            let binding = ledger
                .bindings
                .get(&event.binding)
                .ok_or_else(|| invalid("retained delta binding missing"))?;
            let payload = retention::delta::read_candidate(&event.binding, binding, intent)
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| invalid("retained delta disappeared during audit"))?;
            result.push(artifact(
                *operation,
                CustodyArtifactKind::Candidate,
                (*record).clone(),
                &payload,
                &mut total,
            )?);
        }
        for (kind, operations) in [
            (CustodyArtifactKind::Candidate, &inventory.candidates),
            (CustodyArtifactKind::Commit, &inventory.commits),
            (CustodyArtifactKind::ImportPlan, &inventory.imports),
            (CustodyArtifactKind::PublicationWitness, &inventory.pushes),
        ] {
            for operation in operations {
                if retired.contains_key(operation) {
                    require(
                        kind == CustodyArtifactKind::Commit,
                        "retired construction aliases an unexpected artifact",
                    )?;
                    continue;
                }
                if let Some(artifact) = abandoned.get(operation) {
                    result.push(orphan::retained_artifact(
                        root,
                        artifact,
                        kind,
                        known.contains_key(operation)
                            || deltas.contains_key(operation)
                            || data_commits.contains_key(operation),
                        &mut total,
                    )?);
                    continue;
                }
                if let Some(recipe) = inventory.evidence_imports.get(operation) {
                    let selected = retention::commit_selection(root, *operation)
                        .map_err(|error| invalid(error.to_string()))?
                        .ok_or_else(|| {
                            invalid("retained evidence carrier disappeared during audit")
                        })?;
                    require(
                        kind == CustodyArtifactKind::Commit && selected == recipe.intent()?.source,
                        "retained evidence carrier differs from canonical provenance",
                    )?;
                    result.push(artifact(
                        *operation,
                        kind,
                        recipe.prepared_id.clone(),
                        &selected,
                        &mut total,
                    )?);
                    continue;
                }
                if let Some(commit) = data_commits.get(operation) {
                    require(
                        kind == CustodyArtifactKind::Commit
                            && !known.contains_key(operation)
                            && !deltas.contains_key(operation)
                            && retention::commit_selection(root, *operation)
                                .map_err(|error| invalid(error.to_string()))?
                                .as_ref()
                                == Some(commit),
                        "data bootstrap aliases another artifact or commit selection",
                    )?;
                    result.push(data::artifact(*operation, commit, &mut total)?);
                    continue;
                }
                if let Some((record, event, intent)) = deltas.get(operation) {
                    let entry = match kind {
                        CustodyArtifactKind::Commit => {
                            let selected = retention::commit_selection(root, *operation)
                                .map_err(|error| invalid(error.to_string()))?
                                .ok_or_else(|| {
                                    invalid("retained delta commit disappeared during audit")
                                })?;
                            let matches = if let Transition::OutgoingReconciled {
                                reconciliation,
                                ..
                            } = &event.transition
                            {
                                matches!(reconciliation.as_ref(), task_sync::ledger::OutgoingReconciliation::DeltaResolved { commit, .. }
                                    if commit.as_ref() == &selected)
                            } else {
                                match &intent.basis {
                                task_sync::delta::DeltaBasis::Incoming { source_after, .. } => source_after.commit == selected,
                                task_sync::delta::DeltaBasis::Outgoing { .. } => ledger.events.values().any(|candidate| {
                                    candidate.binding == event.binding
                                        && matches!(&candidate.transition, Transition::DeltaLocalCommitRecorded { prepared, commit }
                                            if prepared == *record && commit == &selected)
                                }),
                            }
                            };
                            require(
                                matches,
                                "retained delta commit differs from canonical provenance",
                            )?;
                            artifact(*operation, kind, (*record).clone(), &selected, &mut total)?
                        }
                        CustodyArtifactKind::PublicationWitness => {
                            let witness = retention::read_publication_witness(root, *operation)
                                .map_err(|error| invalid(error.to_string()))?
                                .ok_or_else(|| {
                                    invalid("delta publication witness disappeared during audit")
                                })?;
                            artifact(
                                *operation,
                                kind,
                                witness.event_id.clone(),
                                &witness,
                                &mut total,
                            )?
                        }
                        _ => {
                            return Err(invalid(
                                "legacy artifact cannot belong to a delta preparation",
                            ));
                        }
                    };
                    result.push(entry);
                    continue;
                }
                let (record, intent) = known
                    .get(operation)
                    .ok_or_else(|| invalid("retained custody operation is ahead of this ledger"))?;
                let entry = match kind {
                    CustodyArtifactKind::Candidate => artifact(
                        *operation,
                        kind,
                        (*record).clone(),
                        &retention::read_retained_documents(root, *operation, &intent.candidate)
                            .map_err(|error| invalid(error.to_string()))?,
                        &mut total,
                    )?,
                    CustodyArtifactKind::Commit => {
                        let selected = retention::commit_selection(root, *operation)
                            .map_err(|error| invalid(error.to_string()))?
                            .ok_or_else(|| invalid("retained commit disappeared during audit"))?;
                        require(
                            selected == intent.commit,
                            "retained commit selection differs from its canonical intent",
                        )?;
                        artifact(*operation, kind, (*record).clone(), &selected, &mut total)?
                    }
                    CustodyArtifactKind::ImportPlan => {
                        let retained = retention::read_import_plan(root, *operation)
                            .map_err(|error| invalid(error.to_string()))?
                            .ok_or_else(|| {
                                invalid("retained import plan disappeared during audit")
                            })?;
                        let event = ledger
                            .events
                            .get(*record)
                            .ok_or_else(|| invalid("retained import preparation missing"))?;
                        let binding = ledger
                            .bindings
                            .get(&event.binding)
                            .ok_or_else(|| invalid("retained import binding missing"))?;
                        let documents =
                            retention::read_retained_documents(root, *operation, &intent.candidate)
                                .map_err(|error| invalid(error.to_string()))?;
                        let input = task_sync::exchange::InspectedExchange {
                            binding_id: event.binding.clone(),
                            binding: binding.clone(),
                            intent: (**intent).clone(),
                            tree: TaskTree::from_stored_documents(&documents)?,
                        };
                        super::operations::verify_historical_plan(
                            connection, ledger, &input, &retained,
                        )?;
                        artifact(*operation, kind, (*record).clone(), &retained, &mut total)?
                    }
                    CustodyArtifactKind::PublicationWitness => {
                        let witness = retention::read_publication_witness(root, *operation)
                            .map_err(|error| invalid(error.to_string()))?
                            .ok_or_else(|| {
                                invalid("publication witness disappeared during audit")
                            })?;
                        artifact(
                            *operation,
                            kind,
                            witness.event_id.clone(),
                            &witness,
                            &mut total,
                        )?
                    }
                    CustodyArtifactKind::EvidenceRecipe
                    | CustodyArtifactKind::PlanningSuccessorRecipe
                    | CustodyArtifactKind::IncomingDataCommit
                    | CustodyArtifactKind::AbandonedFetchedCommit
                    | CustodyArtifactKind::AbandonedDeltaConstruction
                    | CustodyArtifactKind::AbandonedEvidenceConstruction
                    | CustodyArtifactKind::AbandonedSuccessorConstruction => {
                        return Err(invalid("evidence recipe is not a legacy artifact"));
                    }
                };
                result.push(entry);
            }
        }
    }
    result.sort_by_key(|entry| (entry.operation, entry.kind));
    require(
        result
            .windows(2)
            .all(|pair| (pair[0].operation, pair[0].kind) < (pair[1].operation, pair[1].kind)),
        "ambiguous custody artifact identity across repositories",
    )?;
    Ok(result)
}

impl SqliteStore {
    pub(crate) fn has_task_sync_custody_audit(
        &self,
        request: &CustodyAuditRequest,
    ) -> Result<bool, StoreError> {
        self.work_read_snapshot(|store| Ok(recorded(&store.connection, request)?.is_some()))
    }

    /// Run an explicitly selected cold custody audit. It appends no planning
    /// mutation, publishes no task text, and does not enable managed writers.
    ///
    /// # Errors
    /// Refuses changed requests, unresolved effects, missing continuity, stale
    /// bases, unavailable source seeds, capacity and partial retained state.
    pub fn audit_task_sync_custody(
        &mut self,
        request: &CustodyAuditRequest,
        actor: &ActorContext,
        now: DateTime<Utc>,
    ) -> Result<CustodyAuditReceipt, StoreError> {
        crate::storage::admit_live_actor_session(actor)?;
        task_sync::ledger::attribution(actor)?;
        let crate::FeedId::Project(project) = &request.expected_local_cut.position.feed else {
            return Err(invalid("custody audit requires a project feed"));
        };
        let database = PathBuf::from(
            self.connection
                .path()
                .ok_or_else(|| invalid("custody audit requires a file-backed store"))?,
        );
        let fingerprint = request.fingerprint()?;
        if let Some(original) = recorded(&self.connection, request)?
            && private::read(&database, project)?
                .audit
                .as_ref()
                .is_none_or(|pending| pending.request != fingerprint)
        {
            let binding: Binding = view::read::binding_on(
                &self.connection,
                &request.binding,
                &mut view::read::ReadBudget::default(),
            )?;
            return receipt(
                &original,
                true,
                retention::audit::acknowledged(&binding.repository.git_directory, &original)
                    .map_err(|error| invalid(error.to_string()))?,
            );
        }
        let binding = view::read::binding_on(
            &self.connection,
            &request.binding,
            &mut view::read::ReadBudget::default(),
        )?;
        require(
            binding.project == *project,
            "custody audit project binding differs",
        )?;
        request.validate(&binding)?;
        // Compare the existing filesystem's canonical spelling, as binding
        // activation does. SQLite may report another equivalent Windows path
        // spelling; text normalization alone does not resolve those aliases.
        // This does not prove which file the live SQLite handle holds.
        let opened = crate::storage::canonical_store_path(&database)
            .map_err(|_| invalid("custody audit connection store path cannot be resolved"))?;
        let declared = crate::storage::canonical_store_path(&binding.store.database)
            .map_err(|_| invalid("custody audit binding store path cannot be resolved"))?;
        require(opened == declared, "custody audit store binding differs")?;
        restore_basis(&binding)?;
        let origin = producing_origin::load_on(&self.connection, project)?
            .ok_or_else(|| invalid("custody audit requires explicit origin enrollment"))?;
        require(
            binding.store.origin.as_ref() == Some(&origin.origin_id),
            "custody audit binding does not name the current origin",
        )?;
        if let Some(commit) = data::selection(request) {
            data::seed(&binding, request.operation, commit)?;
        }
        if orphan::selection(request).is_some() {
            let ledger =
                super::super::super::query::on_one_snapshot(&self.connection, Ledger::load)?;
            orphan::admit(&ledger, request)?;
        }
        let (audit, repeated) = producing_origin::with_custody_audit(
            self,
            project,
            &fingerprint,
            (data::selection(request).is_some()
                || orphan::selection(request).is_some()
                || matches!(
                    request.seed,
                    CustodySeedSelection::AbandonRetainedConstruction { .. }
                ))
            .then_some((
                binding.repository.git_directory.as_path(),
                request.operation,
            )),
            matches!(
                request.seed,
                CustodySeedSelection::AbandonRetainedConstruction { .. }
            )
            .then_some(request),
            |store, guard, reservations| {
                store.audit_custody_checked(request, &binding, actor, now, guard, reservations)
            },
        )?;
        receipt(&audit, repeated, true)
    }

    fn audit_custody_checked(
        &mut self,
        request: &CustodyAuditRequest,
        binding: &Binding,
        actor: &ActorContext,
        now: DateTime<Utc>,
        guard: &mut PreparedContinuity,
        reservations: &BTreeMap<PathBuf, AuditReservation>,
    ) -> Result<(RetainedAudit, bool), StoreError> {
        let roots = reservations.keys().cloned().collect::<BTreeSet<_>>();
        let reservation = reservations
            .get(&binding.repository.git_directory)
            .ok_or_else(|| invalid("audit repository reservation missing"))?;
        let pending = private::read(guard.database(), &binding.project)?.audit;
        let original = recorded(&self.connection, request)?;
        if let Some(original) = &original
            && pending.is_none()
        {
            require(
                retention::audit::acknowledged(&binding.repository.git_directory, original)
                    .map_err(|error| invalid(error.to_string()))?,
                "committed audit acknowledgement missing without its pending descriptor",
            )?;
            return Ok((original.clone(), true));
        }
        let mut ledger =
            super::super::super::query::on_one_snapshot(&self.connection, Ledger::load)?;
        active(&ledger, request, binding)?;
        let (audit, pending) = if let Some(pending) = pending {
            require(
                pending.request == request.fingerprint()?
                    && pending.binding == request.binding
                    && pending.operation == request.operation,
                "custody audit differs from the exact pending request",
            )?;
            let audit = reservation
                .read(&pending.event_id)
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| {
                    invalid(
                        "retained custody audit event missing; preserve exact pending descriptor",
                    )
                })?;
            require(
                pending.matches_retained(&audit)?,
                "pending custody audit event differs",
            )?;
            if let Some(original) = &original {
                require(
                    original == &audit,
                    "committed custody audit differs from retention",
                )?;
            }
            (audit, pending)
        } else {
            let repository_head = reservation
                .inventory()
                .map_err(|error| invalid(error.to_string()))?
                .head;
            require(
                repository_head
                    .as_ref()
                    .filter(|head| head.binding == request.binding)
                    .map(|head| &head.checkpoint)
                    == request.previous_checkpoint.as_ref(),
                "custody audit repository head differs from the named predecessor; preserve custody for exact recovery",
            )?;
            require(
                producing_origin::cut_on(&self.connection, &binding.project)? == cut(request),
                "custody audit local cut changed before capture",
            )?;
            let (tail_id, tail) = ledger
                .events
                .iter()
                .filter(|(_, event)| event.binding == request.binding)
                .max_by_key(|(_, event)| event.sequence)
                .ok_or_else(|| invalid("custody audit activation missing"))?;
            require(
                tail_id == &request.expected_tail && super::settled_tail(tail),
                "custody audit expected settled tail differs",
            )?;
            let checkpoint = ManagedCustodyCheckpoint {
                operation: request.operation,
                key: request.key.clone(),
                request_fingerprint: request.fingerprint()?,
                previous_checkpoint: request.previous_checkpoint.clone(),
                local_cut: request.expected_local_cut.clone(),
                reason: request.reason,
                seed: seed_on(&self.connection, &ledger, request, binding)?,
                artifacts: artifacts(&self.connection, &ledger, &roots, Some((binding, request)))?,
            };
            checkpoint.validate(&request.binding, binding)?;
            let event = Event {
                schema_version: 1,
                binding: request.binding.clone(),
                sequence: tail
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid("custody audit sequence exhausted"))?,
                predecessor: Some(tail_id.clone()),
                actor: actor.clone(),
                recorded_at: now,
                transition: Transition::CustodyCheckpoint {
                    checkpoint: Box::new(checkpoint),
                },
            };
            let object = CanonicalObject::mint(&event)?;
            ledger.capacity(&[&object])?;
            let audit = RetainedAudit {
                event_id: object.key().clone(),
                event,
            };
            audit
                .preflight()
                .map_err(|error| invalid(error.to_string()))?;
            let pending = private::PendingAudit::new(
                &audit,
                request,
                guard
                    .state()
                    .witness
                    .clone()
                    .ok_or_else(|| invalid("origin witness missing before audit retention"))?,
                reservation
                    .inventory()
                    .map_err(|error| invalid(error.to_string()))?
                    .head,
            )?;
            // Both complete payloads are sized before the first pending slot.
            private::retain_audit(guard.database(), &pending)?;
            reservation
                .retain(&audit)
                .map_err(|error| invalid(error.to_string()))?;
            (audit, pending)
        };
        guard.refresh()?;
        guard.capture_custody(
            &self.connection,
            Some(&binding.repository.git_directory),
            Some(&pending.request),
            Some(pending.operation),
        )?;
        let Transition::CustodyCheckpoint {
            checkpoint: transition,
        } = &audit.event.transition
        else {
            return Err(invalid("retained audit transition differs"));
        };
        require(
            artifacts(&self.connection, &ledger, &roots, Some((binding, request)))?
                == transition.artifacts,
            "retained custody artifacts changed since audit capture",
        )?;
        restore_basis(binding)?;
        let transaction = self.begin_work_mutation()?;
        producing_origin::check_audit_on(&transaction, &binding.project, guard, &pending)?;
        let state = private::read(guard.database(), &binding.project)?;
        require(
            state.audit.as_ref() == Some(&pending) && state.witness == guard.state().witness,
            "custody audit private reservation changed before commit",
        )?;
        producing_origin::check_cut_on(&transaction, &binding.project, &cut(request))?;
        ledger = Ledger::load(&transaction)?;
        active(&ledger, request, binding)?;
        let existing = recorded(&transaction, request)?;
        let repeated = existing.is_some();
        if let Some(existing) = existing {
            require(
                existing == audit,
                "committed audit differs from exact retained event",
            )?;
        } else {
            orphan::admit_retained(&ledger, request)?;
            if let Some((_, commit)) = orphan::selection(request) {
                guard
                    .custody()
                    .get(&binding.repository.git_directory)
                    .ok_or_else(|| invalid("abandoned carrier observation missing"))?
                    .abandoned_pin(request.operation, commit)?;
            }
            require(
                audit.event.predecessor.as_ref() == Some(&request.expected_tail),
                "retained audit predecessor differs",
            )?;
            let object = CanonicalObject::identified(&audit.event_id, &audit.event)?;
            super::operations::store_event(&transaction, &mut ledger, &object)?;
        }
        transaction.commit()?;
        #[cfg(test)]
        failure_point("repository_ack")?;
        guard.refresh()?;
        guard.capture_custody(
            &self.connection,
            Some(&binding.repository.git_directory),
            Some(&pending.request),
            Some(pending.operation),
        )?;
        restore_basis(binding)?;
        let transaction = self.begin_work_mutation()?;
        producing_origin::acknowledge_audit_origin_on(
            &transaction,
            &binding.project,
            guard,
            &pending,
        )?;
        drop(transaction);
        guard.refresh()?;
        #[cfg(test)]
        failure_point("origin_ack")?;
        reservation
            .acknowledge(&audit, pending.repository_head.as_ref())
            .map_err(|error| invalid(error.to_string()))?;
        #[cfg(test)]
        failure_point("private_cleanup")?;
        guard.capture_custody(
            &self.connection,
            Some(&binding.repository.git_directory),
            Some(&pending.request),
            Some(pending.operation),
        )?;
        restore_basis(binding)?;
        let transaction = self.begin_work_mutation()?;
        producing_origin::check_audit_on(&transaction, &binding.project, guard, &pending)?;
        let witness = guard
            .state()
            .witness
            .as_ref()
            .ok_or_else(|| invalid("origin witness missing before audit cleanup"))?;
        private::finish_audit(guard.database(), &pending, witness)?;
        Ok((audit, repeated))
    }
}

#[cfg(test)]
thread_local! { pub(super) static FAIL_STAGE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
fn failure_point(stage: &'static str) -> Result<(), StoreError> {
    FAIL_STAGE.with(|failure| {
        if failure.get() == Some(stage) {
            failure.set(None);
            Err(invalid(format!(
                "injected custody audit failure at {stage}"
            )))
        } else {
            Ok(())
        }
    })
}
