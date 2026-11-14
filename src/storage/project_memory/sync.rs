//! Project-memory text exchange through the ordinary local write invariants.

use std::collections::BTreeMap;

use super::*;
use crate::{
    DevelopmentNoopRedactor,
    task_sync::memories::{
        self, ExportOutcome, Head, ImportOutcome, ImportProvenance, ImportRequest, KeyHistory,
        Revision, Source, WithheldKey,
    },
};

fn invalid(text: &str) -> StoreError {
    StoreError::InvalidProjectMemory(text.into())
}

fn revision(
    entry: &StoredProjectMemory,
    project: &crate::ProjectId,
    key: &str,
    previous: Option<ObjectId>,
) -> Result<Revision, StoreError> {
    if entry.version.sensitivity != Sensitivity::Internal {
        return Err(invalid(
            "unsupported memory sensitivity: only Internal history is exchangeable",
        ));
    }
    let provenance = if let Some(tag) = ImportProvenance::from_actor(&entry.version.actor)? {
        tag.validate_capture(project, Some(key), &entry.version.body)?;
        if tag.retiring_target != entry.version.retiring_target {
            return Err(invalid("stored import target differs from source tag"));
        }
        tag
    } else {
        ImportProvenance {
            format: memories::FORMAT.into(),
            project_id: project.clone(),
            key: key.into(),
            revision_id: entry.version_id.clone(),
            previous,
            body_sha256: memories::body_sha256(&entry.version.body),
            retiring_target: entry.version.retiring_target.clone(),
            source_clear: entry.version.retiring_target_cleared,
            source: Source::from_actor(&entry.version.actor, entry.version.created_at),
        }
    };
    provenance.marked()?;
    Ok(Revision {
        provenance,
        body: entry.version.body.clone(),
    })
}

fn same_value(local: &Revision, shared: &Revision) -> bool {
    local.body == shared.body
        && local.provenance.source == shared.provenance.source
        && local.provenance.retiring_target == shared.provenance.retiring_target
        && local.provenance.source_clear == shared.provenance.source_clear
}

fn shared_local(local: &Revision, shared: &Revision, imported: bool) -> bool {
    same_value(local, shared) && (!imported || local.provenance == shared.provenance)
}

fn export_key(
    project: &crate::ProjectId,
    key: &str,
    history: &[StoredProjectMemory],
    selected: Option<&KeyHistory>,
) -> Result<KeyHistory, StoreError> {
    memories::validate_key(key)?;
    let head = history
        .last()
        .ok_or_else(|| invalid("memory history is empty"))?;
    if let Some(selected) = selected {
        selected.validate()?;
        if &selected.project_id != project || selected.key != key {
            return Err(invalid("selected memory belongs to another project/key"));
        }
    }
    if history
        .iter()
        .any(|entry| entry.version.sensitivity != Sensitivity::Internal)
    {
        return Err(invalid(
            "unsupported memory sensitivity: whole key withheld",
        ));
    }
    if head.assertion.status == MemoryStatus::Tombstoned {
        if let Some(selected) = selected
            && matches!(selected.head, Head::Tombstone { .. })
        {
            return Ok(selected.clone());
        }
        let source = retirement_source(&head.assertion.actor)?.unwrap_or_else(|| {
            Source::from_actor(&head.assertion.actor, head.assertion.created_at)
        });
        return Ok(KeyHistory {
            format: memories::FORMAT.into(),
            project_id: project.clone(),
            key: key.into(),
            head: Head::Tombstone { source },
            revisions: BTreeMap::new(),
        });
    }
    if selected.is_some_and(|s| matches!(s.head, Head::Tombstone { .. })) {
        return Err(invalid(
            "cannot export active memory over a selected permanent tombstone",
        ));
    }
    let mut result = selected.cloned().unwrap_or_else(|| KeyHistory {
        format: memories::FORMAT.into(),
        project_id: project.clone(),
        key: key.into(),
        head: Head::Active { chain: Vec::new() },
        revisions: BTreeMap::new(),
    });
    let mut native = Vec::new();
    let mut previous = None;
    for entry in history {
        let value = revision(entry, project, key, previous)?;
        previous = Some(value.provenance.revision_id.clone());
        native.push(value);
    }
    let Head::Active { chain } = &mut result.head else {
        unreachable!()
    };
    let mut anchor = None;
    for (index, value) in native.iter().enumerate() {
        if let Some(position) = chain
            .iter()
            .position(|id| id == &value.provenance.revision_id)
        {
            let shared = result
                .revisions
                .get(&chain[position])
                .ok_or_else(|| invalid("shared anchor file missing"))?;
            if !shared_local(
                value,
                shared,
                ImportProvenance::from_actor(&history[index].version.actor)?.is_some(),
            ) {
                return Err(invalid(
                    "shared anchor identity has different immutable content",
                ));
            }
            anchor = Some((index, position));
        }
    }
    let start = if let Some((index, position)) = anchor {
        if position + 1 != chain.len() {
            return Err(invalid(
                "selected shared suffix must be imported before export",
            ));
        }
        index + 1
    } else if chain.is_empty() {
        0
    } else {
        let prefix = native
            .iter()
            .zip(chain.iter())
            .take_while(|(local, id)| {
                result
                    .revisions
                    .get(*id)
                    .is_some_and(|shared| same_value(local, shared))
            })
            .count();
        if prefix != chain.len() {
            return Err(invalid(
                "memory histories diverge; explicit agent merge required",
            ));
        }
        prefix
    };
    for (index, mut value) in native.into_iter().enumerate().skip(start) {
        if ImportProvenance::from_actor(&history[index].version.actor)?.is_some() {
            return Err(invalid(
                "imported suffix does not belong to selected shared chain",
            ));
        }
        value.provenance.previous = chain.last().cloned();
        chain.push(value.provenance.revision_id.clone());
        result
            .revisions
            .insert(value.provenance.revision_id.clone(), value);
    }
    result.validate()?;
    Ok(result)
}

fn export_authored_merge(
    project: &crate::ProjectId,
    key: &str,
    history: &[StoredProjectMemory],
    inputs: &memories::MergeInputs,
    current_selection: Option<&KeyHistory>,
) -> Result<KeyHistory, StoreError> {
    let (head, earlier) = history
        .split_last()
        .ok_or_else(|| invalid("authored memory merge has no local result"))?;
    if head.assertion.status != MemoryStatus::Active
        || ImportProvenance::from_actor(&head.version.actor)?.is_some()
    {
        return Err(invalid(
            "memory merge result must be a newly authored active local revision",
        ));
    }
    let compared = export_key(project, key, earlier, inputs.common.as_ref())?;
    if compared != inputs.local {
        return Err(invalid(
            "authored memory merge local compared input is stale",
        ));
    }
    let mut selected = inputs.common.clone().unwrap_or_else(|| KeyHistory {
        format: memories::FORMAT.into(),
        project_id: project.clone(),
        key: key.into(),
        head: Head::Active { chain: Vec::new() },
        revisions: BTreeMap::new(),
    });
    let Head::Active { chain } = &mut selected.head else {
        return Err(invalid("authored memory merge cannot replace retirement"));
    };
    let result = revision(head, project, key, chain.last().cloned())?;
    chain.push(result.provenance.revision_id.clone());
    selected
        .revisions
        .insert(result.provenance.revision_id.clone(), result);
    inputs.validate_selection(&selected)?;
    if !current_selection.is_some_and(|current| {
        current == &inputs.local || current == &inputs.incoming || current == &selected
    }) {
        return Err(invalid("authored memory merge selected input changed"));
    }
    Ok(selected)
}

fn retirement_source(actor: &ActorContext) -> Result<Option<Source>, StoreError> {
    let mut source = None;
    for link in &actor.provenance_chain {
        if link.source != memories::RETIREMENT_MARKER {
            continue;
        }
        if source.is_some() || link.relation != crate::domain::ProvenanceRelation::DerivedFrom {
            return Err(invalid("invalid retirement source tag"));
        }
        source = Some(serde_json::from_str(
            link.reference
                .as_deref()
                .ok_or_else(|| invalid("retirement source missing"))?,
        )?);
    }
    Ok(source)
}

fn retirement_actor(actor: &ActorContext, source: &Source) -> Result<ActorContext, StoreError> {
    let mut actor = actor.clone();
    actor.provenance_chain.push(crate::domain::ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: memories::RETIREMENT_MARKER.into(),
        reference: Some(serde_json::to_string(source)?),
    });
    validate_project_memory_actor_shape(&actor)?;
    Ok(actor)
}

fn apply_revision(
    transaction: &rusqlite::Transaction<'_>,
    request: &ImportRequest<'_>,
    revision: &Revision,
    current: u64,
) -> Result<(), StoreError> {
    use crate::domain::{
        ProjectMemoryRetiringTarget as Target, ProjectMemoryRetiringTargetChange as Change,
        ProjectMemoryRetiringTargetInput as Input,
    };
    let project = &request.selected.project_id;
    let key = &request.selected.key;
    let history = lookup_project_memory_history_on(transaction, project, key)?;
    let retiring_target = match &revision.provenance.retiring_target {
        Some(Target::Local { work_ref, .. }) => Change::Set {
            target: Input::Local {
                work_ref: work_ref.clone(),
            },
        },
        Some(Target::External { project, reference }) => Change::Set {
            target: Input::External {
                project: project.clone(),
                reference: reference.clone(),
            },
        },
        None if history::has_clearable_retiring_target(&history) => Change::Clear,
        None => Change::Keep,
    };
    let mut actor = request.actor.clone();
    revision.provenance.attach(&mut actor)?;
    SqliteStore::remember_project_memory_on(
        transaction,
        &RememberProjectMemoryRequest {
            project_id: project.clone(),
            session_id: request.session_id.clone(),
            key: Some(key.clone()),
            revise: current != 0,
            expected_revision: (current != 0).then_some(current),
            body: revision.body.clone(),
            retiring_target,
            actor,
            created_at: request.now,
        },
        &crate::domain::ProjectMemoryEdit::Whole,
        &DevelopmentNoopRedactor,
        crate::work_service::ensure_project_memory_full_is_admissible,
    )?;
    Ok(())
}

impl SqliteStore {
    /// Reads the immutable local head used as the per-key import fence.
    ///
    /// # Errors
    /// Refuses an invalid canonical memory history.
    pub fn project_memory_sync_head(
        &self,
        project: &crate::ProjectId,
        key: &str,
    ) -> Result<Option<ObjectId>, StoreError> {
        Ok(project_memory_history_on(&self.connection, project, key)?
            .last()
            .map(|entry| entry.version_id.clone()))
    }
    /// Captures whole memory histories from one read transaction. A failure to
    /// disclose any local revision withholds its whole key with a visible reason.
    ///
    /// # Errors
    /// Refuses a broken store/query; individual key refusals are reported.
    pub fn export_project_memory_sync(
        &self,
        project: &crate::ProjectId,
        selected: &BTreeMap<String, KeyHistory>,
    ) -> Result<ExportOutcome, StoreError> {
        self.export_project_memory_sync_with_resolutions(project, selected, &BTreeMap::new())
    }

    pub(crate) fn export_project_memory_sync_with_resolutions(
        &self,
        project: &crate::ProjectId,
        selected: &BTreeMap<String, KeyHistory>,
        resolutions: &BTreeMap<String, memories::ImportOptions>,
    ) -> Result<ExportOutcome, StoreError> {
        if self.connection.is_autocommit() {
            let transaction = self.connection.unchecked_transaction()?;
            let result =
                self.export_project_memory_sync_with_resolutions(project, selected, resolutions)?;
            transaction.commit()?;
            return Ok(result);
        }
        let transaction = &self.connection;
        let keys = transaction.prepare("SELECT DISTINCT json_extract(canonical_json, '$.project_key') FROM objects INDEXED BY objects_project_memory_key WHERE object_kind = 'memory_version' AND json_extract(canonical_json, '$.scope.kind') = 'project' AND json_extract(canonical_json, '$.scope.project') = ?1 AND json_type(canonical_json, '$.project_key') = 'text'")?
            .query_map([&project.0], |row| row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
        let mut result = ExportOutcome::default();
        for key in keys {
            let captured =
                project_memory_history_on(transaction, project, &key).and_then(|history| {
                    match resolutions.get(&key) {
                        Some(options) if options.seed.is_some() || options.mark_untagged_seed => {
                            Err(invalid("seed resolution is import-only"))
                        }
                        Some(memories::ImportOptions {
                            merge: Some(inputs),
                            ..
                        }) => export_authored_merge(
                            project,
                            &key,
                            &history,
                            inputs,
                            selected.get(&key),
                        ),
                        _ => export_key(project, &key, &history, selected.get(&key)),
                    }
                });
            match captured {
                Ok(history) => {
                    result.keys.insert(key, history);
                }
                Err(error) => result.withheld.push(WithheldKey {
                    key,
                    reason: error.to_string(),
                }),
            }
        }
        if resolutions.keys().any(|key| {
            !result.keys.contains_key(key) && !result.withheld.iter().any(|held| &held.key == key)
        }) {
            return Err(invalid("memory resolution names an absent local key"));
        }
        Ok(result)
    }

    /// Applies one complete selected memory key under an exact local-head fence.
    /// Targets are checked before any write; dry runs roll back the same path.
    ///
    /// # Errors
    /// Refuses stale local heads, divergence without compared merge inputs,
    /// missing historical targets, terminal resurrection and malformed text.
    pub fn import_project_memory_sync(
        &mut self,
        request: &ImportRequest<'_>,
    ) -> Result<ImportOutcome, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = Self::import_project_memory_sync_on(&transaction, request)?;
        if request.dry_run {
            transaction.rollback()?;
        } else {
            transaction.commit()?;
        }
        Ok(outcome)
    }

    /// The same admission/writer inside a caller-owned transaction. The caller
    /// rolls back the whole key on error, and owns final commit or dry-run rollback.
    pub(crate) fn import_project_memory_sync_on(
        transaction: &rusqlite::Transaction<'_>,
        request: &ImportRequest<'_>,
    ) -> Result<ImportOutcome, StoreError> {
        request.selected.validate()?;
        let selected = request.selected;
        validate_project_memory_authorization(request.session_id, request.actor)?;
        validated_project_memory_actor(request.actor, &DevelopmentNoopRedactor)?;
        let history =
            lookup_project_memory_history_on(transaction, &selected.project_id, &selected.key)?;
        if history.last().map(|e| &e.version_id) != request.expected_local_head {
            return Err(invalid("memory import local head changed"));
        }
        let mut outcome = ImportOutcome {
            key: selected.key.clone(),
            appended_revisions: 0,
            retired: false,
            duplicate: false,
            dry_run: request.dry_run,
        };
        if let Head::Tombstone { source } = &selected.head {
            let actor = retirement_actor(request.actor, source)?;
            if let Some(head) = history.last() {
                let receipt = Self::forget_project_memory_on(
                    transaction,
                    &ForgetProjectMemoryRequest {
                        project_id: selected.project_id.clone(),
                        session_id: request.session_id.clone(),
                        key: selected.key.clone(),
                        actor,
                        created_at: request.now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                outcome.duplicate = receipt.duplicate;
                outcome.retired = head.assertion.status != MemoryStatus::Tombstoned;
            } else {
                let before = project_memory_state_on(transaction, &selected.project_id)?;
                let ordinary = RememberProjectMemoryRequest {
                    project_id: selected.project_id.clone(),
                    session_id: request.session_id.clone(),
                    key: Some(selected.key.clone()),
                    body: "[retired project memory]".into(),
                    actor,
                    created_at: request.now,
                    revise: false,
                    expected_revision: None,
                    retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                };
                let mut prepared =
                    prepare_project_memory(&ordinary, &selected.key, None, None, false)?;
                prepared.version.source_snapshot = Some(crate::domain::SourceSnapshot {
                    source_ref: memories::TERMINAL_SOURCE.into(),
                    fingerprint: CanonicalObject::freeze(&selected.head)?.key().to_string(),
                    observed_at: request.now,
                });
                prepared.version.classification_reason = "restored project episode".into();
                prepared.version_object = CanonicalObject::mint(&prepared.version)?;
                prepared.assertion.version = prepared.version_object.key().clone();
                prepared.assertion.status = MemoryStatus::Tombstoned;
                prepared.assertion.policy_reason = "restored project-memory tombstone".into();
                prepared.assertion_object = CanonicalObject::mint(&prepared.assertion)?;
                Self::insert_project_memory_version_object(
                    transaction,
                    &prepared.version_object,
                    &selected.project_id,
                    &selected.key,
                )?;
                Self::insert_object(
                    transaction,
                    "memory_assertion_event",
                    &prepared.assertion_object,
                )?;
                Self::apply_memory_projection(
                    transaction,
                    prepared.version_object.key(),
                    prepared.assertion_object.key(),
                    &prepared.version,
                    &prepared.assertion,
                    MemoryProjectionMode::Live,
                )?;
                state::record_project_memory_retirement_on(
                    transaction,
                    &selected.project_id,
                    before,
                )?;
                outcome.retired = true;
            }
        } else {
            if history
                .last()
                .is_some_and(|e| e.assertion.status == MemoryStatus::Tombstoned)
            {
                if selected_is_local_prefix(selected, &history)? {
                    outcome.duplicate = true;
                    return Ok(outcome);
                }
                return Err(StoreError::ProjectMemoryRetired(selected.key.clone()));
            }
            validate_targets(transaction, selected)?;
            let Head::Active { chain } = &selected.head else {
                unreachable!()
            };
            let mut current = history.len() as u64;
            let start = import_start(transaction, request, &history, &mut current)?;
            for id in &chain[start..] {
                apply_revision(transaction, request, &selected.revisions[id], current)?;
                current += 1;
                outcome.appended_revisions += 1;
            }
            outcome.appended_revisions = usize::try_from(current)
                .map_err(|_| invalid("memory history exceeds addressable range"))?
                - history.len();
            outcome.duplicate = outcome.appended_revisions == 0;
        }
        Ok(outcome)
    }
}

fn validate_targets(
    transaction: &rusqlite::Transaction<'_>,
    selected: &KeyHistory,
) -> Result<(), StoreError> {
    use crate::domain::{
        ProjectMemoryRetiringTarget as Target, ProjectMemoryRetiringTargetInput as Input,
    };
    for value in selected.revisions.values() {
        let Some(target) = &value.provenance.retiring_target else {
            continue;
        };
        let input = match target {
            Target::Local { work_ref, .. } => Input::Local {
                work_ref: work_ref.clone(),
            },
            Target::External { project, reference } => Input::External {
                project: project.clone(),
                reference: reference.clone(),
            },
        };
        if resolve_retiring_target_on(transaction, &selected.project_id, &input)? != *target {
            return Err(invalid(
                "shared historical target UUID/ref identity mismatch",
            ));
        }
    }
    Ok(())
}

fn import_start(
    transaction: &rusqlite::Transaction<'_>,
    request: &ImportRequest<'_>,
    history: &[StoredProjectMemory],
    current: &mut u64,
) -> Result<usize, StoreError> {
    let selected = request.selected;
    let Head::Active { chain } = &selected.head else {
        unreachable!()
    };
    if let Some(seed) = request.seed {
        let index = history
            .iter()
            .position(|entry| entry.version_id == seed.local_version_id)
            .ok_or_else(|| invalid("recorded seed local version is absent"))?;
        let original = &history[index];
        let tag = ImportProvenance::from_actor(&original.version.actor)?;
        if let Some(tag) = tag {
            seed.validate_anchor(
                selected,
                &original.version_id,
                &original.version.body,
                Some(&tag),
            )?;
        } else if let Some(marked) = history.get(index + 1) {
            // A repeated explicit fallback proves its immutable same-body child
            // against the original recorded seed; it does not append again.
            let tag = ImportProvenance::from_actor(&marked.version.actor)?
                .ok_or_else(|| invalid("seed local head changed without an admitted rebaseline"))?;
            if marked.version.body != original.version.body
                || marked.version.parents.as_slice() != [original.version_id.clone()]
            {
                return Err(invalid("seed fallback is not the recorded same-body child"));
            }
            seed.validate_anchor(
                selected,
                &original.version_id,
                &original.version.body,
                Some(&tag),
            )?;
        } else {
            if !request.mark_untagged_seed {
                return Err(invalid(
                    "untagged seed requires explicit operator rebaseline",
                ));
            }
            let shared = selected
                .revisions
                .get(&seed.shared_revision_id)
                .ok_or_else(|| invalid("seed anchor id absent from selected chain"))?;
            let anchor = seed.validate_anchor(
                selected,
                &original.version_id,
                &original.version.body,
                Some(&shared.provenance),
            )?;
            apply_revision(transaction, request, shared, *current)?;
            *current += 1;
            return Ok(anchor + 1);
        }
    }
    let mut native = Vec::new();
    let mut previous = None;
    let mut anchor = None;
    for (index, entry) in history.iter().enumerate() {
        let value = revision(entry, &selected.project_id, &selected.key, previous)?;
        previous = Some(value.provenance.revision_id.clone());
        if let Some(position) = chain
            .iter()
            .position(|id| id == &value.provenance.revision_id)
        {
            if !shared_local(
                &value,
                &selected.revisions[&chain[position]],
                ImportProvenance::from_actor(&entry.version.actor)?.is_some(),
            ) {
                return Err(invalid("selected shared identity has different content"));
            }
            anchor = Some((index, position));
        }
        native.push(value);
    }
    if history.is_empty() {
        return Ok(0);
    }
    if let Some((local, shared)) = anchor
        && local + 1 == history.len()
    {
        return Ok(shared + 1);
    }
    let prefix = native
        .iter()
        .zip(chain)
        .take_while(|(local, id)| {
            selected
                .revisions
                .get(*id)
                .is_some_and(|shared| same_value(local, shared))
        })
        .count();
    if prefix == chain.len() {
        return Ok(chain.len());
    }
    if prefix == history.len() {
        return Ok(prefix);
    }
    let merge = request.merge.ok_or_else(|| {
        invalid("memory histories diverge; explicit compared-input agent merge required")
    })?;
    let start = merge.validate_selection(selected)?;
    let exported = export_key(
        &selected.project_id,
        &selected.key,
        history,
        merge.common.as_ref(),
    )?;
    if exported != merge.local {
        return Err(invalid("agent merge local compared input is stale"));
    }
    Ok(start)
}

fn selected_is_local_prefix(
    selected: &KeyHistory,
    history: &[StoredProjectMemory],
) -> Result<bool, StoreError> {
    let Head::Active { chain } = &selected.head else {
        return Ok(false);
    };
    if chain.len() > history.len() {
        return Ok(false);
    }
    let mut previous = None;
    for (entry, id) in history.iter().zip(chain) {
        let local = revision(entry, &selected.project_id, &selected.key, previous)?;
        previous = Some(local.provenance.revision_id.clone());
        if !shared_local(
            &local,
            &selected.revisions[id],
            ImportProvenance::from_actor(&entry.version.actor)?.is_some(),
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
