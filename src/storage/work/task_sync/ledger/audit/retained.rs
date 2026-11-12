//! Exact custody descriptions for constructions that never became exchanges.

use std::{collections::BTreeMap, path::Path};

use crate::{
    CanonicalObject, ObjectId,
    storage::StoreError,
    task_sync::{
        delta::{
            CustodyArtifactKind, CustodyArtifactRef, CustodyAuditRequest, CustodySeedSelection,
            DeltaBasis, DeltaCandidatePayload, RetainedConstructionKind,
        },
        ledger::{Binding, EvidenceImportRecipe, RetainedCommit, require},
        retention::{self, RetainedInventory, successor::RetainedSuccessor},
    },
};

use super::invalid;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Construction {
    Delta(
        Box<retention::delta::RetainedRequest>,
        Box<DeltaCandidatePayload>,
    ),
    Evidence(Box<EvidenceImportRecipe>),
    Successor(Box<RetainedSuccessor>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObservedConstruction {
    pub(crate) construction: Construction,
    pub(crate) carrier: Option<RetainedCommit>,
}

fn retained_error(error: impl std::fmt::Display) -> StoreError {
    invalid(error.to_string())
}

impl ObservedConstruction {
    /// Read through the owning codecs and verify any carrier before the writer.
    pub(crate) fn capture(
        root: &Path,
        inventory: &RetainedInventory,
        operation: uuid::Uuid,
    ) -> Result<Self, StoreError> {
        let observed = Self::from_files(root, inventory, operation)?;
        if let Some(carrier) = &observed.carrier {
            super::orphan::git_pin(root, carrier)?;
        }
        Ok(observed)
    }

    pub(crate) fn from_files(
        root: &Path,
        inventory: &RetainedInventory,
        operation: uuid::Uuid,
    ) -> Result<Self, StoreError> {
        require(
            !inventory.candidates.contains(&operation)
                && !inventory.imports.contains(&operation)
                && !inventory.pushes.contains(&operation),
            "construction aliases legacy, imported or published custody",
        )?;
        let families = usize::from(inventory.delta_candidates.contains_key(&operation))
            + usize::from(inventory.evidence_imports.contains_key(&operation))
            + usize::from(inventory.planning_successors.contains_key(&operation));
        require(families == 1, "construction family is missing or ambiguous")?;
        let construction = if let Some(request) = inventory.delta_candidates.get(&operation) {
            let (actual, payload) =
                retention::delta::observe_construction(root, operation).map_err(retained_error)?;
            require(
                &actual == request,
                "retained delta changed during observation",
            )?;
            require(
                matches!(actual.intent.basis, DeltaBasis::Incoming { .. }),
                "outgoing construction requires its publication recovery owner",
            )?;
            Construction::Delta(Box::new(actual), Box::new(payload))
        } else if let Some(recipe) = inventory.evidence_imports.get(&operation) {
            Construction::Evidence(Box::new(recipe.clone()))
        } else {
            Construction::Successor(Box::new(inventory.planning_successors[&operation].clone()))
        };
        let expected = match &construction {
            Construction::Delta(request, _) => match &request.intent.basis {
                DeltaBasis::Incoming { source_after, .. } => Some(&source_after.commit),
                DeltaBasis::Outgoing { .. } => None,
            },
            Construction::Evidence(recipe) => Some(&recipe.intent()?.source),
            Construction::Successor(_) => None,
        };
        let carrier = retention::commit_selection(root, operation).map_err(retained_error)?;
        require(
            carrier.as_ref() == expected
                && inventory.commits.contains(&operation) == expected.is_some(),
            "construction carrier differs from exact retained provenance",
        )?;
        // The exact selected recipe and its carrier were checked together
        // above. The commit-only bootstrap/orphan validator deliberately
        // rejects accompanying recipes and cannot validate this family.
        Ok(Self {
            construction,
            carrier,
        })
    }

    pub(crate) fn binding(&self) -> &ObjectId {
        match &self.construction {
            Construction::Delta(request, _) => &request.binding,
            Construction::Evidence(recipe) => &recipe.prepared.binding,
            Construction::Successor(retained) => &retained.binding,
        }
    }

    pub(crate) fn operation(&self) -> Result<uuid::Uuid, StoreError> {
        Ok(match &self.construction {
            Construction::Delta(request, _) => request.intent.operation,
            Construction::Evidence(recipe) => recipe.intent()?.operation,
            Construction::Successor(retained) => retained.recipe.request.intent.operation,
        })
    }

    pub(crate) fn key(&self) -> Result<&str, StoreError> {
        Ok(match &self.construction {
            Construction::Delta(request, _) => &request.intent.key,
            Construction::Evidence(recipe) => &recipe.intent()?.key,
            Construction::Successor(retained) => &retained.recipe.request.intent.key,
        })
    }

    pub(crate) fn family(&self) -> RetainedConstructionKind {
        match &self.construction {
            Construction::Delta(..) => RetainedConstructionKind::Delta,
            Construction::Evidence(_) => RetainedConstructionKind::Evidence,
            Construction::Successor(_) => RetainedConstructionKind::Successor,
        }
    }

    pub(crate) fn fingerprint(&self) -> Result<ObjectId, StoreError> {
        match &self.construction {
            Construction::Delta(request, payload) => {
                Ok(
                    CanonicalObject::freeze(&("retained_delta", request, payload))?
                        .key()
                        .clone(),
                )
            }
            Construction::Evidence(recipe) => {
                retention::evidence::request_fingerprint(recipe).map_err(retained_error)
            }
            Construction::Successor(retained) => {
                retained.request_fingerprint().map_err(retained_error)
            }
        }
    }

    pub(crate) fn validate(&self, binding: &Binding) -> Result<(), StoreError> {
        match &self.construction {
            Construction::Delta(request, payload) => {
                request.intent.validate(&request.binding, binding)?;
                payload.validate(&request.binding, binding, &request.intent)
            }
            Construction::Evidence(recipe) => recipe.validate(&recipe.prepared.binding, binding),
            Construction::Successor(retained) => retained
                .prepared_event(binding)
                .map(|_| ())
                .map_err(retained_error),
        }
    }

    pub(crate) fn matches_request(&self, request: &CustodyAuditRequest) -> Result<(), StoreError> {
        let CustodySeedSelection::AbandonRetainedConstruction {
            binding,
            family,
            construction_fingerprint,
        } = &request.seed
        else {
            return Err(invalid("audit does not select a retained construction"));
        };
        require(
            binding == self.binding()
                && *family == self.family()
                && construction_fingerprint == &self.fingerprint()?
                && request.operation == self.operation()?
                && request.key == self.key()?,
            "audit differs from the exact retained construction identity",
        )
    }

    pub(crate) fn artifacts(&self) -> Result<Vec<CustodyArtifactRef>, StoreError> {
        let kind = match self.family() {
            RetainedConstructionKind::Delta => CustodyArtifactKind::AbandonedDeltaConstruction,
            RetainedConstructionKind::Evidence => {
                CustodyArtifactKind::AbandonedEvidenceConstruction
            }
            RetainedConstructionKind::Successor => {
                CustodyArtifactKind::AbandonedSuccessorConstruction
            }
        };
        let operation = self.operation()?;
        let mut artifacts = vec![CustodyArtifactRef {
            operation,
            kind,
            content_fingerprint: self.fingerprint()?,
            record: Some(self.binding().clone()),
        }];
        if let Some(carrier) = &self.carrier {
            artifacts.push(CustodyArtifactRef {
                operation,
                kind: CustodyArtifactKind::Commit,
                content_fingerprint: CanonicalObject::freeze(carrier)?.key().clone(),
                record: Some(self.binding().clone()),
            });
        }
        artifacts.sort_by_key(|artifact| (artifact.operation, artifact.kind));
        Ok(artifacts)
    }
}

/// Resolve only indexed operation/key candidates across this repository's
/// bindings; retirement is canonical and survives private-file loss or repair.
pub(crate) fn refuse_retired_on(
    connection: &Connection,
    binding_id: &ObjectId,
    operation: uuid::Uuid,
    key: &str,
) -> Result<(), StoreError> {
    use super::super::{
        EVENT_KIND, Event, Transition,
        view::{read, select},
    };
    // Ordinary receipt lookup keeps its compact decode budget. Discover only
    // matching retirement checkpoints through existing indexed metadata first;
    // an ordinary preparation or outcome must not enter the decoder here.
    let root: Option<String> = connection
        .query_row(
            "SELECT CASE WHEN json_valid(canonical_json)
                 THEN json_extract(canonical_json,'$.repository.git_directory') END
             FROM objects WHERE object_id=?1 AND object_kind='task_sync_binding'",
            [binding_id.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let root = root.ok_or_else(|| invalid("exchange binding root is missing"))?;
    let binding_ids = select::bindings(connection, select::Bindings::Root(Path::new(&root)))?;
    let operation_text = operation.to_string();
    let mut candidates = Vec::new();
    for id in &binding_ids {
        for (index, field, value) in [
            (
                "objects_task_sync_exchange_operation",
                "operation",
                operation_text.as_str(),
            ),
            ("objects_task_sync_exchange_key", "key", key),
        ] {
            let mut statement = connection.prepare(&format!(
                "SELECT object_id FROM objects INDEXED BY {index}
                 WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
                   AND json_extract(canonical_json,'$.binding')=?1
                   AND COALESCE(json_extract(canonical_json,'$.transition.intent.{field}'),
                                json_extract(canonical_json,'$.transition.checkpoint.{field}'),
                                json_extract(canonical_json,'$.transition.recipe.request.intent.{field}'))=?2
                   AND json_extract(canonical_json,'$.transition.transition')='custody_checkpoint'
                   AND EXISTS(SELECT 1 FROM json_each(canonical_json,'$.transition.checkpoint.artifacts') artifact
                       WHERE json_extract(artifact.value,'$.operation')=json_extract(objects.canonical_json,'$.transition.checkpoint.operation')
                         AND json_extract(artifact.value,'$.kind') IN
                             ('abandoned_delta_construction','abandoned_evidence_construction','abandoned_successor_construction'))
                 LIMIT 2"
            ))?;
            candidates.extend(
                statement
                    .query_map(params![id.as_str(), value], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            require(
                candidates.len() <= super::super::MAX_RECORDS,
                "retirement selection exceeds supported bound",
            )?;
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    candidates.sort();
    candidates.dedup();
    let mut budget = read::ReadBudget::default();
    let mut bindings = BTreeMap::new();
    for id in binding_ids {
        bindings.insert(id.clone(), read::binding_on(connection, &id, &mut budget)?);
    }
    let binding = bindings
        .get(binding_id)
        .ok_or_else(|| invalid("exchange binding is missing"))?;
    for candidate in candidates {
        let candidate = ObjectId::from_stored(candidate)
            .ok_or_else(|| invalid("invalid retirement identity"))?;
        let event: Event = read::object(connection, &candidate, EVENT_KIND, &mut budget)?;
        let selected = bindings
            .get(&event.binding)
            .ok_or_else(|| invalid("retirement binding is missing"))?;
        if selected.project != binding.project || selected.store.origin != binding.store.origin {
            continue;
        }
        let Transition::CustodyCheckpoint { checkpoint } = &event.transition else {
            continue;
        };
        let Some(artifact) = checkpoint.artifacts.iter().find(|artifact| {
            artifact.operation == checkpoint.operation && artifact_family(artifact).is_some()
        }) else {
            continue;
        };
        let original = artifact
            .record
            .as_ref()
            .ok_or_else(|| invalid("retired construction binding missing"))?;
        lineage(&bindings, &event.binding, original)?;
        if lineage(&bindings, binding_id, original).is_err()
            && lineage(&bindings, original, binding_id).is_err()
        {
            continue;
        }
        // Related binding epochs cannot recycle an accounted operation or
        // key; unrelated bindings keep their existing request namespaces.
        require(
            checkpoint.operation != operation && checkpoint.key != key,
            "exchange construction was explicitly retired by its custody audit",
        )?;
    }
    Ok(())
}

pub(crate) fn artifact_family(artifact: &CustodyArtifactRef) -> Option<RetainedConstructionKind> {
    match artifact.kind {
        CustodyArtifactKind::AbandonedDeltaConstruction => Some(RetainedConstructionKind::Delta),
        CustodyArtifactKind::AbandonedEvidenceConstruction => {
            Some(RetainedConstructionKind::Evidence)
        }
        CustodyArtifactKind::AbandonedSuccessorConstruction => {
            Some(RetainedConstructionKind::Successor)
        }
        _ => None,
    }
}

pub(crate) fn lineage(
    bindings: &BTreeMap<ObjectId, Binding>,
    current: &ObjectId,
    original: &ObjectId,
) -> Result<(), StoreError> {
    super::orphan::lineage(bindings, current, original)
}

pub(super) fn for_audit(
    ledger: &super::super::Ledger,
    root: &Path,
    selection: Option<(&Binding, &CustodyAuditRequest)>,
) -> Result<BTreeMap<uuid::Uuid, ObservedConstruction>, StoreError> {
    use crate::task_sync::ledger::Transition;
    let inventory = retention::inventory(root).map_err(retained_error)?;
    let mut result = BTreeMap::new();
    for (id, event) in &ledger.events {
        let Transition::CustodyCheckpoint { checkpoint } = &event.transition else {
            continue;
        };
        if ledger.bindings[&event.binding].repository.git_directory != root {
            continue;
        }
        if !checkpoint.artifacts.iter().any(|artifact| {
            artifact.operation == checkpoint.operation && artifact_family(artifact).is_some()
        }) {
            continue;
        }
        let construction = ObservedConstruction::capture(root, &inventory, checkpoint.operation)?;
        require(
            checkpoint.key == construction.key()?
                && checkpoint
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.operation == checkpoint.operation)
                    .cloned()
                    .collect::<Vec<_>>()
                    == construction.artifacts()?,
            "retired construction checkpoint artifacts differ",
        )?;
        construction.prove_ledger(ledger, &event.binding, Some(id))?;
        require(
            result.insert(checkpoint.operation, construction).is_none(),
            "construction has competing canonical retirements",
        )?;
    }
    if let Some((binding, request)) = selection
        && binding.repository.git_directory == root
        && matches!(
            request.seed,
            CustodySeedSelection::AbandonRetainedConstruction { .. }
        )
    {
        if let Some(construction) = result.get(&request.operation) {
            construction.matches_request(request)?;
        } else {
            let construction = ObservedConstruction::capture(root, &inventory, request.operation)?;
            construction.matches_request(request)?;
            construction.prove_ledger(ledger, &request.binding, None)?;
            result.insert(request.operation, construction);
        }
    }
    Ok(result)
}

/// Check canonical facts only. This is also usable during import without Git
/// custody. A checkpoint may replay its own identity; no other request may use it.
pub(crate) fn canonical_absence(
    connection: &Connection,
    bindings: &BTreeMap<ObjectId, Binding>,
    current: &ObjectId,
    original: &ObjectId,
    operation: uuid::Uuid,
    key: &str,
    owner: Option<&ObjectId>,
) -> Result<(), StoreError> {
    lineage(bindings, current, original)?;
    let selected = &bindings[current];
    for (id, binding) in bindings {
        if binding.project != selected.project
            || binding.repository != selected.repository
            || binding.store.origin != selected.store.origin
        {
            continue;
        }
        let by_operation =
            super::super::view::read::prepared_for_operation(connection, id, operation)?;
        let mut statement = connection.prepare(
            "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_key
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.binding')=?1
               AND COALESCE(json_extract(canonical_json,'$.transition.intent.key'),
                            json_extract(canonical_json,'$.transition.checkpoint.key'),
                            json_extract(canonical_json,'$.transition.recipe.request.intent.key'))=?2 LIMIT 2",
        )?;
        let by_key = statement
            .query_map(params![id.as_str(), key], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        require(
            by_operation
                .as_ref()
                .is_none_or(|found| Some(found) == owner)
                && by_key
                    .iter()
                    .all(|found| owner.is_some_and(|owner| owner.as_str() == found))
                && super::super::view::read::reconciliation_parent_for_operation(
                    connection, id, operation,
                )?
                .is_none(),
            "construction already has canonical progress or a competing request",
        )?;
        let alias: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_task_sync_exchange_reconciliation_key
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
               AND json_extract(canonical_json,'$.transition.reconciliation.step') IN ('resolved','delta_resolved')
               AND json_extract(canonical_json,'$.binding')=?1
               AND json_extract(canonical_json,'$.transition.reconciliation.resolved.key')=?2)",
            params![id.as_str(), key], |row| row.get(0),
        )?;
        require(!alias, "construction aliases a canonical reconciliation")?;
    }
    Ok(())
}

/// Validate durable retirement links without private files. Import and repair
/// use the same canonical proof as admission, without requiring a repository.
pub(crate) fn validate_checkpoint_on(
    connection: &Connection,
    id: &ObjectId,
    event: &crate::task_sync::ledger::Event,
) -> Result<(), StoreError> {
    use super::super::{
        Transition,
        view::{read, select},
    };
    let Transition::CustodyCheckpoint { checkpoint } = &event.transition else {
        return Ok(());
    };
    let owners = checkpoint
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.operation == checkpoint.operation && artifact_family(artifact).is_some()
        })
        .collect::<Vec<_>>();
    if owners.is_empty() {
        return Ok(());
    }
    require(
        owners.len() == 1 && checkpoint.seed.is_none(),
        "retirement cannot seed membership or mix construction families",
    )?;
    let owner = owners[0];
    let original = owner
        .record
        .as_ref()
        .ok_or_else(|| invalid("retired construction binding missing"))?;
    let mut budget = read::ReadBudget::default();
    let binding = read::binding_on(connection, &event.binding, &mut budget)?;
    let mut bindings = BTreeMap::new();
    for id in select::bindings(
        connection,
        select::Bindings::Root(&binding.repository.git_directory),
    )? {
        bindings.insert(id.clone(), read::binding_on(connection, &id, &mut budget)?);
    }
    let family = artifact_family(owner).ok_or_else(|| invalid("retirement family missing"))?;
    let selected = checkpoint
        .artifacts
        .iter()
        .filter(|artifact| artifact.operation == checkpoint.operation)
        .collect::<Vec<_>>();
    require(
        selected.len()
            == if family == RetainedConstructionKind::Successor {
                1
            } else {
                2
            }
            && selected.iter().all(|artifact| {
                artifact.record.as_ref() == Some(original)
                    && (artifact.kind == owner.kind || artifact.kind == CustodyArtifactKind::Commit)
            }),
        "retirement has incomplete or foreign accompanying artifacts",
    )?;
    canonical_absence(
        connection,
        &bindings,
        &event.binding,
        original,
        checkpoint.operation,
        &checkpoint.key,
        Some(id),
    )
}

impl ObservedConstruction {
    pub(crate) fn prove_on(
        &self,
        connection: &Connection,
        bindings: &BTreeMap<ObjectId, Binding>,
        current: &ObjectId,
        owner: Option<&ObjectId>,
    ) -> Result<(), StoreError> {
        canonical_absence(
            connection,
            bindings,
            current,
            self.binding(),
            self.operation()?,
            self.key()?,
            owner,
        )?;
        self.validate(&bindings[self.binding()])?;
        let ids = match &self.construction {
            Construction::Delta(..) => Vec::new(),
            Construction::Evidence(recipe) => vec![&recipe.prepared_id, &recipe.outcome_id],
            Construction::Successor(retained) => {
                vec![&retained.recipe.prepared, &retained.recipe.outcome]
            }
        };
        for id in ids {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id=?1)",
                [id.as_str()],
                |row| row.get(0),
            )?;
            require(
                !exists,
                "retained construction identity already has a canonical record",
            )?;
        }
        Ok(())
    }

    pub(crate) fn recheck_files(&self, root: &Path) -> Result<(), StoreError> {
        let inventory = retention::inventory(root).map_err(retained_error)?;
        require(
            Self::from_files(root, &inventory, self.operation()?)? == *self,
            "construction bytes or accompanying artifacts changed after observation",
        )
    }

    pub(crate) fn prove_ledger(
        &self,
        ledger: &super::super::Ledger,
        current: &ObjectId,
        owner: Option<&ObjectId>,
    ) -> Result<(), StoreError> {
        use crate::task_sync::ledger::{OutgoingReconciliation, Transition};
        lineage(&ledger.bindings, current, self.binding())?;
        self.validate(&ledger.bindings[self.binding()])?;
        let operation = self.operation()?;
        let key = self.key()?;
        for (id, event) in &ledger.events {
            if Some(id) == owner {
                continue;
            }
            let binding = &ledger.bindings[&event.binding];
            let selected = &ledger.bindings[current];
            if binding.project != selected.project
                || binding.repository != selected.repository
                || binding.store.origin != selected.store.origin
            {
                continue;
            }
            let identity = match &event.transition {
                Transition::Prepared { intent } => Some((intent.operation, intent.key.as_str())),
                Transition::DeltaPrepared { intent, .. } => {
                    Some((intent.operation, intent.key.as_str()))
                }
                Transition::EvidencePrepared { intent } => {
                    Some((intent.operation, intent.key.as_str()))
                }
                Transition::PlanningSuccessorPrepared { recipe, .. } => Some((
                    recipe.request.intent.operation,
                    recipe.request.intent.key.as_str(),
                )),
                Transition::CustodyCheckpoint { checkpoint } => {
                    Some((checkpoint.operation, checkpoint.key.as_str()))
                }
                Transition::OutgoingReconciled { reconciliation, .. } => {
                    match reconciliation.as_ref() {
                        OutgoingReconciliation::Resolved { resolved, .. } => {
                            Some((resolved.operation, resolved.key.as_str()))
                        }
                        OutgoingReconciliation::DeltaResolved { resolved, .. } => {
                            Some((resolved.operation, resolved.key.as_str()))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            require(
                event.transition.retention_operation() != Some(operation)
                    && identity.is_none_or(|(observed, selected)| {
                        observed != operation && selected != key
                    }),
                "construction has canonical progress or a competing request",
            )?;
        }
        Ok(())
    }
}
