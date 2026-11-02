use super::{
    BTreeSet, Connection, EVENT_KIND, Event, HashSet, ImportOrigin, MAX_RECORDS, ORIGIN_KIND,
    ObjectId, OptionalExtension, ProjectId, Records, SHARED_VERSION_KIND, SharedImportVersion,
    SqliteStore, StoreError, Transition, WorkId, family_on, invalid, params, prepared_on, query,
    require, validate_work_basis_links,
};
use crate::task_sync::ledger::{Binding, Intent};
use crate::task_sync::reconciliation::{ModelAImportReceipt, ReconciliationBase};

pub(super) fn stored_head_on(
    connection: &Connection,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    let id: Option<String> = connection
        .query_row(
            "SELECT head_id FROM work_task_sync_import_heads WHERE project_id=?1 AND work_id=?2",
            params![&project.0, work.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if id.is_none() {
        let item = query::load_work_item(connection, work)?;
        require(
            &item.project_id == project,
            "imported head item scope differs",
        )?;
        require(
            item.created_by.source_tool.as_deref() != Some(super::super::IMPORT_TOOL),
            "origin-created task has no durable imported head",
        )?;
    }
    id.map(|id| ObjectId::from_stored(id).ok_or_else(|| invalid("invalid imported head id")))
        .transpose()
}

pub(in crate::storage) fn imported_head_on(
    connection: &Connection,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    let id = stored_head_on(connection, project, work)?;
    if let Some(id) = &id {
        baseline_on(connection, id, project, work)?;
    }
    Ok(id)
}

/// Resolve only admitted imported baseline facts. Native item edits remain in
/// the ordinary work projection; this never replaces their creation origin.
pub(super) fn baseline_on(
    connection: &Connection,
    id: &ObjectId,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<crate::task_sync::TaskDocument>, StoreError> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT object_kind FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if kind.as_deref() == Some(super::super::folder::IMPORT_PLANNING_KIND) {
        if let Some(entry) =
            super::super::baseline::entries(connection, project, [work])?.remove(&work)
            && entry.record == *id
        {
            // The current keyed view has already checked this row's subject,
            // leaf and admitted feed position. Do not decode the whole delta.
            return Ok(entry.document);
        }
        // Historical readsets keep resolving their immutable named record.
        return super::heads::folder_baseline_on(connection, id, project, work);
    }
    if let Some(origin) = family_on::<ImportOrigin>(connection, id, ORIGIN_KIND)? {
        require(
            &origin.project_id == project && origin.task.id == work,
            "imported head origin scope differs",
        )?;
        super::super::origin::validate_record(connection, &origin)?;
        let record: crate::RestoredRecord = SqliteStore::get_typed_object_on(
            connection,
            &origin.record_id,
            "work_restored_record",
        )?
        .ok_or_else(|| invalid("imported head original restored record is missing"))?;
        require(
            record.project_id == *project && record.item.work_id == work,
            "imported head original record scope differs",
        )?;
        return Ok(Some(origin.task));
    }
    let version: SharedImportVersion =
        SqliteStore::get_typed_object_on(connection, id, SHARED_VERSION_KIND)?
            .ok_or_else(|| invalid("imported head is not an origin or shared version"))?;
    require(
        &version.project_id == project && version.task.id == work,
        "imported shared head scope differs",
    )?;
    validate_current_version(connection, id, &version)?;
    Ok(Some(version.task))
}

fn validate_current_version(
    connection: &Connection,
    id: &ObjectId,
    version: &SharedImportVersion,
) -> Result<(), StoreError> {
    require(
        version.schema_version == 1 && version.task.id == version.local_before.work_id,
        "invalid shared version schema or local task",
    )?;
    require(
        version.predecessor == version.local_before.imported_head,
        "shared version does not extend its captured imported head",
    )?;
    let item = query::load_work_item(connection, version.task.id)?;
    let original = if item.created_by.source_tool.as_deref() == Some(super::super::IMPORT_TOOL) {
        Some(super::super::origin_reference(&item.created_by)?)
    } else {
        None
    };
    require(
        version.original_import_origin == original,
        "shared version changes original creation provenance",
    )?;
    crate::task_sync::ledger::attribution(&version.actor)?;
    let (_, prepared) = prepared_on(
        connection,
        &version.binding,
        &version.prepared,
        &version.project_id,
    )?;
    require(
        prepared.actor == version.actor && prepared.recorded_at <= version.imported_at,
        "shared version attribution or time differs",
    )?;
    let receipt: Event =
        SqliteStore::get_typed_object_on(connection, &version.receipt, EVENT_KIND)?
            .ok_or_else(|| invalid("shared version receipt is missing"))?;
    let Transition::ReconciledImportApplied {
        prepared,
        receipt: result,
    } = &receipt.transition
    else {
        return Err(invalid("shared version requires reconciled receipt"));
    };
    require(
        prepared == &version.prepared
            && receipt.binding == version.binding
            && receipt.actor == version.actor
            && receipt.recorded_at == version.imported_at
            && result.base == version.base,
        "shared version receipt binding differs",
    )?;
    let mut tasks = result
        .tasks
        .iter()
        .filter(|task| task.task == version.task.id);
    let task = tasks
        .next()
        .ok_or_else(|| invalid("shared version is absent from its receipt"))?;
    require(
        tasks.next().is_none()
            && &task.shared_version == id
            && task.previous_head == version.predecessor,
        "shared version receipt task differs or is repeated",
    )?;
    validate_shared_records(connection, version, &task.local_record_ids)?;
    require(
        version.original_import_origin.is_some() == version.original_restored_record.is_some(),
        "partial original import provenance",
    )?;
    if let Some(origin_id) = &version.original_import_origin {
        let origin: ImportOrigin =
            SqliteStore::get_typed_object_on(connection, origin_id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("shared original import origin missing"))?;
        require(
            origin.project_id == version.project_id
                && origin.task.id == version.task.id
                && Some(&origin.record_id) == version.original_restored_record.as_ref(),
            "shared original import link differs",
        )?;
        super::super::origin::validate_record(connection, &origin)?;
    }
    Ok(())
}

fn validate_shared_records(
    connection: &Connection,
    version: &SharedImportVersion,
    local_records: &[ObjectId],
) -> Result<(), StoreError> {
    require(
        version.shared_records.len() <= MAX_RECORDS && local_records.len() <= MAX_RECORDS,
        "shared version local records exceed supported bound",
    )?;
    let shared: HashSet<_> = version.shared_records.iter().collect();
    let local: HashSet<_> = local_records.iter().collect();
    require(
        shared.len() == version.shared_records.len()
            && local.len() == local_records.len()
            && shared == local,
        "shared version records differ from receipt or are repeated",
    )?;
    for id in &version.shared_records {
        if let Some(event) = family_on::<crate::WorkEvent>(connection, id, "work_event")? {
            require(
                event.project_id == version.project_id
                    && event.work_id == version.task.id
                    && event.revision > version.local_before.revision
                    && event.created_at == version.imported_at,
                "shared version local work record scope differs",
            )?;
        } else {
            let record: crate::RestoredRecord = family_on(connection, id, "work_restored_record")?
                .ok_or_else(|| invalid("shared version local record kind or existence differs"))?;
            require(
                record.project_id == version.project_id
                    && record.item.work_id == version.task.id
                    && version.original_restored_record.as_ref() == Some(id),
                "shared version local restored record scope differs",
            )?;
        }
    }
    Ok(())
}

/// Historical readsets and doctor audit predecessor facts in full. Current
/// heads check their own immutable links without traversing prior versions.
pub(super) fn validate_version(
    connection: &Connection,
    id: &ObjectId,
    version: &SharedImportVersion,
) -> Result<(), StoreError> {
    validate_current_version(connection, id, version)?;
    validate_work_basis_links(connection, &version.project_id, &version.local_before)?;
    let mut current = version.predecessor.clone();
    let mut visited = BTreeSet::from([id.clone()]);
    while let Some(previous) = current {
        require(
            visited.insert(previous.clone()) && visited.len() <= MAX_RECORDS,
            "shared import predecessor cycle or bound exceeded",
        )?;
        if let Some(old) =
            family_on::<SharedImportVersion>(connection, &previous, SHARED_VERSION_KIND)?
        {
            require(
                old.project_id == version.project_id
                    && old.task.id == version.task.id
                    && old.original_import_origin == version.original_import_origin
                    && old.original_restored_record == version.original_restored_record,
                "shared predecessor changes original provenance",
            )?;
            current = old.predecessor;
        } else if family_on::<serde_json::Value>(
            connection,
            &previous,
            super::super::folder::IMPORT_PLANNING_KIND,
        )?
        .is_some()
        {
            super::heads::folder_baseline_on(
                connection,
                &previous,
                &version.project_id,
                version.task.id,
            )?;
            current = None;
        } else {
            require(
                version.original_import_origin.as_ref() == Some(&previous),
                "shared predecessor is not its original origin",
            )?;
            let origin: ImportOrigin =
                SqliteStore::get_typed_object_on(connection, &previous, ORIGIN_KIND)?
                    .ok_or_else(|| invalid("original import origin is missing"))?;
            require(
                origin.project_id == version.project_id
                    && origin.task.id == version.task.id
                    && Some(&origin.record_id) == version.original_restored_record.as_ref(),
                "shared version original provenance differs",
            )?;
            current = None;
        }
    }
    Ok(())
}

pub(super) fn verify_heads(
    connection: &Connection,
    records: &Records,
    checked: &mut usize,
    failures: &mut Vec<String>,
) -> Result<(), StoreError> {
    let successors: HashSet<_> = records
        .versions
        .values()
        .filter_map(|version| {
            version
                .predecessor
                .as_ref()
                .map(|previous| (version.project_id.0.as_str(), version.task.id, previous))
        })
        .collect();
    let mut predecessors = BTreeSet::new();
    for (id, version) in &records.versions {
        if !predecessors.insert((
            version.project_id.0.clone(),
            version.task.id.0,
            version.predecessor.clone(),
        )) {
            failures.push(format!(
                "shared import version {id} forks an imported baseline"
            ));
        }
        if !successors.contains(&(version.project_id.0.as_str(), version.task.id, id)) {
            let head: Option<String> = connection.query_row("SELECT head_id FROM work_task_sync_import_heads WHERE project_id=?1 AND work_id=?2",params![&version.project_id.0,version.task.id.0.to_string()],|row|row.get(0)).optional()?;
            if head.is_none() {
                failures.push(format!(
                    "shared import version {id} has no current durable head"
                ));
            }
        }
    }
    let mut statement =
        connection.prepare("SELECT project_id,work_id,head_id FROM work_task_sync_import_heads")?;
    let mut expected_heads = std::collections::HashMap::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (project, work, id) = row?;
        *checked += 1;
        let mut check = || {
            let work = WorkId(
                uuid::Uuid::parse_str(&work).map_err(|_| invalid("invalid imported head task"))?,
            );
            let id = ObjectId::from_stored(id.clone())
                .ok_or_else(|| invalid("invalid imported head identity"))?;
            let project = ProjectId(project.clone());
            require(
                query::load_work_item(connection, work)?.project_id == project,
                "imported head item scope differs",
            )?;
            baseline_on(connection, &id, &project, work)?;
            let expected = expected_heads.entry(project.0.clone()).or_insert_with(|| {
                super::heads::derived_on(connection, &project).map_err(|error| error.to_string())
            });
            let expected = expected.as_ref().map_err(|error| invalid(error.clone()))?;
            require(
                expected.get(&work) == Some(&id),
                "imported head is not the latest admitted shared baseline",
            )?;
            require(
                // A folder delta can be the baseline of many tasks. A shared
                // successor advances only its own project and task's head.
                !successors.contains(&(project.0.as_str(), work, &id)),
                "imported head points behind a shared successor",
            )?;
            Ok::<_, StoreError>(())
        };
        if let Err(error) = check() {
            failures.push(format!("imported head {work}: {error}"));
        }
    }
    let mut origins = connection.prepare("SELECT object_id FROM objects WHERE object_kind=?1")?;
    for row in origins.query_map([ORIGIN_KIND], |row| row.get::<_, String>(0))? {
        let raw = row?;
        *checked += 1;
        let check = || {
            let id = ObjectId::from_stored(raw.clone())
                .ok_or_else(|| invalid("invalid import origin id"))?;
            let origin: ImportOrigin =
                SqliteStore::get_typed_object_on(connection, &id, ORIGIN_KIND)?
                    .ok_or_else(|| invalid("import origin missing"))?;
            let item = query::load_work_item(connection, origin.task.id)?;
            if item.created_by.source_tool.as_deref() == Some(super::super::IMPORT_TOOL)
                && super::super::origin_reference(&item.created_by)? == id
            {
                imported_head_on(connection, &origin.project_id, item.work_id)?;
            }
            Ok::<_, StoreError>(())
        };
        if let Err(error) = check() {
            failures.push(format!(
                "task_sync_import_origin {raw} imported head: {error}"
            ));
        }
    }
    Ok(())
}

pub(in crate::storage::work::task_sync) fn validate_receipt_on(
    connection: &Connection,
    event_id: &ObjectId,
    binding: &Binding,
    intent: &Intent,
    receipt: &ModelAImportReceipt,
) -> Result<(), StoreError> {
    let resolved = super::super::folder::retained_planning_on(
        connection,
        &receipt.planning,
        &binding.project,
    )?;
    require(
        receipt.operation == intent.operation
            && receipt.operation_key == intent.key
            && receipt.candidate_fingerprint == intent.candidate.documents_fingerprint
            && receipt.resolved == intent.commit,
        "reconciled receipt differs from prepared intent",
    )?;
    require(
        resolved.inventory_root == receipt.after_basis.as_str()
            && resolved.profile.include_notes == binding.profile.include_notes,
        "reconciled receipt graph or notes profile differs from retained resolved input",
    )?;
    match (&receipt.base, &intent.common_commit) {
        (ReconciliationBase::KnownBase { commit }, Some(expected)) => {
            require(commit == expected, "reconciled common base differs")?;
        }
        (
            ReconciliationBase::ExplicitUnbasedResolution {
                local_basis,
                incoming_basis,
            },
            None,
        ) => require(
            local_basis == &intent.local_planning_basis
                && incoming_basis == &intent.candidate.planning_fingerprint,
            "unbased resolution does not retain both inputs",
        )?,
        _ => return Err(invalid("reconciled base mode differs")),
    }
    require(
        receipt.tasks.len() <= MAX_RECORDS
            && receipt.before.len() <= MAX_RECORDS
            && receipt.holds.len() <= MAX_RECORDS
            && receipt.terminal_evidence.len() <= MAX_RECORDS,
        "reconciled receipt exceeds supported bound",
    )?;
    let outcome: Event = SqliteStore::get_typed_object_on(connection, event_id, EVENT_KIND)?
        .ok_or_else(|| invalid("reconciled receipt event missing"))?;
    let Transition::ReconciledImportApplied { prepared, .. } = &outcome.transition else {
        return Err(invalid("reconciled receipt event kind differs"));
    };
    let (_, prepared_event) =
        prepared_on(connection, &outcome.binding, prepared, &binding.project)?;
    require(
        matches!(&prepared_event.transition, Transition::Prepared { intent: actual } if actual.as_ref() == intent),
        "receipt prepared intent differs",
    )?;
    let mut before = HashSet::new();
    for basis in &receipt.before {
        require(
            before.insert(basis.work_id),
            "duplicate receipt before task",
        )?;
        validate_work_basis_links(connection, &binding.project, basis)?;
    }
    let mut holds = HashSet::new();
    for id in &receipt.holds {
        require(holds.insert(id), "duplicate receipt hold")?;
        let hold: super::ReconciliationHold =
            SqliteStore::get_typed_object_on(connection, id, super::HOLD_KIND)?
                .ok_or_else(|| invalid("receipt hold missing"))?;
        super::holds::validate_hold(connection, id, &hold)?;
        require(
            hold.project_id == binding.project
                && hold.binding == outcome.binding
                && hold.prepared == *prepared
                && hold.candidate_fingerprint == receipt.candidate_fingerprint
                && hold
                    .affected
                    .iter()
                    .all(|basis| receipt.before.contains(basis)),
            "receipt hold candidate or readset differs",
        )?;
    }
    let mut terminal = HashSet::new();
    for id in &receipt.terminal_evidence {
        require(terminal.insert(id), "duplicate receipt terminal evidence")?;
        let event: crate::WorkEvent =
            SqliteStore::get_typed_object_on(connection, id, "work_event")?
                .ok_or_else(|| invalid("receipt terminal evidence missing"))?;
        require(
            event.project_id == binding.project
                && matches!(&event.transition, crate::WorkTransition::SupersededByImport { hold, prepared: selected, receipt: selected_receipt, .. }
                if holds.contains(hold) && selected == prepared && selected_receipt == event_id),
            "receipt terminal evidence candidate differs",
        )?;
        super::validate_retirement_on(connection, id, &event)?;
    }
    let mut tasks = HashSet::new();
    for task in &receipt.tasks {
        require(tasks.insert(task.task), "duplicate reconciled task")?;
        let version: SharedImportVersion = SqliteStore::get_typed_object_on(
            connection,
            &task.shared_version,
            SHARED_VERSION_KIND,
        )?
        .ok_or_else(|| invalid("receipt shared version missing"))?;
        require(
            version.project_id == binding.project
                && &version.receipt == event_id
                && version.binding == outcome.binding
                && &version.prepared == prepared
                && version.task.id == task.task
                && version.predecessor == task.previous_head
                && receipt
                    .before
                    .iter()
                    .any(|basis| basis == &version.local_before),
            "receipt shared version scope differs",
        )?;
        require(
            resolved.changes.get(&task.task) == Some(&Some(version.task.clone())),
            "shared version differs from resolved graph",
        )?;
        validate_version(connection, &task.shared_version, &version)?;
        validate_shared_records(connection, &version, &task.local_record_ids)?;
    }
    Ok(())
}

/// The writer checks freshly admitted candidate content; historical readers
/// compare stored roots and links without re-encoding old documents.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "The atomic Model-A writer consumes this check in the next slice"
    )
)]
pub(in crate::storage::work::task_sync) fn verify_receipt_content_on(
    connection: &Connection,
    binding: &Binding,
    intent: &Intent,
    receipt: &ModelAImportReceipt,
    candidate: &crate::task_sync::TaskTree,
) -> Result<(), StoreError> {
    let planning = super::super::folder::retained_planning_on(
        connection,
        &receipt.planning,
        &binding.project,
    )?;
    require(
        candidate.project.project_id == binding.project
            && candidate.project.notes_included == binding.profile.include_notes
            && crate::task_sync::progress::documents_fingerprint(&candidate.documents()?)
                == intent.candidate.planning_fingerprint.as_str()
            && super::super::baseline::tree_root(candidate)? == planning.inventory_root
            && planning.inventory_root == receipt.after_basis.as_str(),
        "reconciled writer graph differs from retained resolved candidate",
    )
}

impl SqliteStore {
    /// Read the admitted baseline, preserving explicit removed membership.
    ///
    /// # Errors
    /// Refuses missing heads and invalid historical baseline provenance.
    pub fn task_sync_imported_baseline_state(
        &self,
        project: &ProjectId,
        work: WorkId,
    ) -> Result<Option<crate::task_sync::reconciliation::ImportedBaseline>, StoreError> {
        use crate::task_sync::reconciliation::ImportedBaseline;
        self.work_read_snapshot(|store| {
            let head = imported_head_on(&store.connection, project, work)?;
            require(
                head == super::head_for_imported_task_on(&store.connection, project, work)?,
                "imported head is not the latest admitted shared baseline",
            )?;
            head.map(|head| {
                Ok(
                    match baseline_on(&store.connection, &head, project, work)? {
                        Some(task) => ImportedBaseline::Present {
                            head,
                            task: Box::new(task),
                        },
                        None => ImportedBaseline::Removed { head },
                    },
                )
            })
            .transpose()
        })
    }

    /// Read the admitted shared baseline separately from current native edits.
    ///
    /// # Errors
    /// Refuses dangling, cross-project and malformed provenance links.
    pub fn task_sync_imported_baseline(
        &self,
        project: &ProjectId,
        work: WorkId,
    ) -> Result<Option<crate::task_sync::TaskDocument>, StoreError> {
        Ok(
            match self.task_sync_imported_baseline_state(project, work)? {
                Some(crate::task_sync::reconciliation::ImportedBaseline::Present {
                    task, ..
                }) => Some(*task),
                Some(crate::task_sync::reconciliation::ImportedBaseline::Removed { .. }) | None => {
                    None
                }
            },
        )
    }
}
