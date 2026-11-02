//! One derivation for admitted shared heads in the current writer and doctor.

use super::{
    Connection, ImportOrigin, ObjectId, ProjectId, SHARED_VERSION_KIND, SharedImportVersion,
    SqliteStore, StoreError, WorkId, invalid, params, query, require,
};

pub(in crate::storage) fn head_for_imported_task_on(
    connection: &Connection,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    let item = query::load_work_item(connection, work)?;
    require(
        item.project_id == *project,
        "baseline derivation project differs",
    )?;
    let Some(current) = super::versions::imported_head_on(connection, project, work)? else {
        // Export membership alone never gives native work an imported origin.
        return Ok(None);
    };
    let mut entries = super::super::baseline::entries(connection, project, [work])?;
    let Some(entry) = entries.remove(&work) else {
        require(
            super::family_on::<ImportOrigin>(connection, &current, super::ORIGIN_KIND)?.is_some(),
            "shared imported head has no baseline entry",
        )?;
        return Ok(Some(current));
    };
    let current_document = super::versions::baseline_on(connection, &current, project, work)?;
    if current == entry.record {
        return Ok(Some(current));
    }
    if super::family_on::<ImportOrigin>(connection, &current, super::ORIGIN_KIND)?.is_none() {
        let (count, position): (i64, Option<i64>) = connection.query_row(
            "SELECT COUNT(*),MAX(position) FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1 AND object_id=?2 AND object_kind IN ('task_sync_import_planning','task_sync_shared_import_version')",
            params![&project.0, current.as_str()], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        require(count == 1, "imported head lacks unique admitted provenance")?;
        if position.is_some_and(|position| position > entry.position) {
            require(
                super::family_on::<SharedImportVersion>(connection, &current, SHARED_VERSION_KIND)?
                    .is_some()
                    && current_document == entry.document,
                "imported folder head is ahead of its keyed baseline",
            )?;
            return Ok(Some(current));
        }
    }
    // Initial enrollment and an unchanged sibling retain their original head.
    Ok(Some(if current_document == entry.document {
        current
    } else {
        entry.record
    }))
}

struct SharedHead {
    id: ObjectId,
    document: Option<crate::task_sync::TaskDocument>,
    member: bool,
}

pub(super) fn derived_on(
    connection: &Connection,
    project: &ProjectId,
) -> Result<std::collections::HashMap<WorkId, ObjectId>, StoreError> {
    let mut heads = std::collections::HashMap::new();
    let mut statement = connection
        .prepare("SELECT work_id FROM work_items WHERE project_id=?1 ORDER BY work_id")?;
    let works = statement
        .query_map([&project.0], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for work in works {
        let work =
            WorkId(uuid::Uuid::parse_str(&work).map_err(|_| invalid("invalid baseline task id"))?);
        let item = query::load_work_item(connection, work)?;
        if item.created_by.source_tool.as_deref() == Some(super::super::IMPORT_TOOL) {
            let id = super::super::origin_reference(&item.created_by)?;
            let origin: ImportOrigin =
                SqliteStore::get_typed_object_on(connection, &id, super::ORIGIN_KIND)?
                    .ok_or_else(|| invalid("baseline derivation origin missing"))?;
            super::super::origin::validate_record(connection, &origin)?;
            require(
                origin.project_id == *project && origin.task.id == work,
                "baseline derivation origin scope differs",
            )?;
            heads.insert(
                work,
                SharedHead {
                    id,
                    document: Some(origin.task),
                    member: false,
                },
            );
        }
    }
    let mut statement = connection.prepare("SELECT object_id,object_kind FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1 AND object_kind IN ('task_sync_import_planning','task_sync_shared_import_version') ORDER BY position")?;
    let mut rows = statement.query([&project.0])?;
    let mut seen = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        let id = ObjectId::from_stored(row.get(0)?)
            .ok_or_else(|| invalid("invalid admitted baseline id"))?;
        require(
            seen.insert(id.clone()) && seen.len() <= super::MAX_RECORDS,
            "ambiguous or excessive admitted baseline history",
        )?;
        let kind: String = row.get(1)?;
        if kind == SHARED_VERSION_KIND {
            let version: SharedImportVersion =
                SqliteStore::get_typed_object_on(connection, &id, SHARED_VERSION_KIND)?
                    .ok_or_else(|| invalid("admitted shared version missing"))?;
            require(
                version.project_id == *project,
                "admitted shared version project differs",
            )?;
            super::versions::validate_version(connection, &id, &version)?;
            heads.insert(
                version.task.id,
                SharedHead {
                    id,
                    document: Some(version.task),
                    member: true,
                },
            );
        } else {
            let planning = super::super::folder::retained_planning_on(connection, &id, project)?;
            for (work, next) in planning.changes {
                let Some(head) = heads.get_mut(&work) else {
                    continue;
                };
                head.member |= next.is_some();
                if head.member && next != head.document {
                    head.id = id.clone();
                    head.document = next;
                }
            }
        }
    }
    Ok(heads
        .into_iter()
        .map(|(work, head)| (work, head.id))
        .collect())
}

/// Validate only the tasks selected by the caller. Full history belongs to doctor.
pub(in crate::storage::work::task_sync) fn validate_current_on(
    connection: &Connection,
    project: &ProjectId,
    ids: impl IntoIterator<Item = WorkId>,
) -> Result<(), StoreError> {
    for work in ids {
        require(
            super::versions::imported_head_on(connection, project, work)?
                == head_for_imported_task_on(connection, project, work)?,
            "current imported head differs from its admitted baseline",
        )?;
    }
    Ok(())
}

pub(in crate::storage::work::task_sync) fn advance_on(
    connection: &Connection,
    project: &ProjectId,
    ids: impl IntoIterator<Item = WorkId>,
    previous: &std::collections::BTreeMap<WorkId, super::super::baseline::BaselineEntry>,
) -> Result<(), StoreError> {
    for work in ids {
        // Admission checked these heads before append. Read their identities
        // here without resolving the now-historical previous delta again.
        let Some(current) = super::versions::stored_head_on(connection, project, work)? else {
            continue;
        };
        let entry = super::super::baseline::entries(connection, project, [work])?
            .remove(&work)
            .ok_or_else(|| invalid("advanced imported task has no baseline entry"))?;
        if current == entry.record
            || previous
                .get(&work)
                .is_some_and(|old| old.document == entry.document)
        {
            continue;
        }
        if let Some(origin) =
            super::family_on::<ImportOrigin>(connection, &current, super::ORIGIN_KIND)?
        {
            require(
                origin.project_id == *project && origin.task.id == work,
                "advanced imported origin scope differs",
            )?;
            // Initial enrollment leaves creation provenance as the head until
            // the task's admitted shared document actually changes.
            if entry.document.as_ref() == Some(&origin.task) {
                continue;
            }
        }
        super::holds::refuse_held_on(connection, work)?;
        require(connection.execute("UPDATE work_task_sync_import_heads SET head_id=?3 WHERE project_id=?1 AND work_id=?2 AND head_id=?4", params![&project.0,work.0.to_string(),entry.record.as_str(),current.as_str()])?==1,"imported head moved before advancement")?;
    }
    Ok(())
}

pub(super) fn folder_baseline_on(
    connection: &Connection,
    id: &ObjectId,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<crate::task_sync::TaskDocument>, StoreError> {
    super::super::folder::retained_planning_on(connection, id, project)?
        .changes
        .remove(&work)
        .ok_or_else(|| invalid("folder head does not name this task"))
}
