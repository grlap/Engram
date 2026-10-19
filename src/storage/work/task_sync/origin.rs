//! Read and integrity validation for the new import origin only.

use super::{
    Connection, FORMAT, ImportOrigin, ORIGIN_KIND, ObjectId, ProjectDocument, SqliteStore,
    StoreError, TaskSyncRefusal, TaskTree, WorkItem, history, invalid,
};
use crate::RestoredRecord;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

// Refusals that a task's import origin binding does not hold. The shared
// validators return them as plain invalid input to every caller; task
// export and import classify them as origin mismatches.
pub(super) const ITEM_BINDING_DIFFERS: &str = "task import origin item binding differs";
pub(super) const RECORD_BINDING_DIFFERS: &str = "task import origin record binding differs";
pub(super) const PROJECTION_DIFFERS: &str = "task projection differs from canonical import origin";
pub(super) const ORIGIN_ID_INVALID: &str = "invalid import origin id";
pub(super) const ORIGIN_MISSING: &str = "task import origin missing";
pub(super) const RECORD_MISSING: &str = "import origin restored record missing";
pub(super) const BINDING_INVALID: &str = "invalid task import origin binding";
pub(super) const TASK_MISSING: &str = "import origin task missing";
pub(super) const REFERENCE_MISSING: &str = "task import origin reference missing";
pub(super) const REFERENCE_INVALID: &str = "invalid task import origin reference";
const BINDING_REFUSALS: [&str; 10] = [
    ITEM_BINDING_DIFFERS,
    RECORD_BINDING_DIFFERS,
    PROJECTION_DIFFERS,
    ORIGIN_ID_INVALID,
    ORIGIN_MISSING,
    RECORD_MISSING,
    BINDING_INVALID,
    TASK_MISSING,
    REFERENCE_MISSING,
    REFERENCE_INVALID,
];

/// Classify one of the origin binding refusals above as an origin mismatch
/// at a task export or import boundary. Any other error, including the
/// scope errors below, passes through unchanged.
pub(super) fn classify_binding(error: StoreError) -> StoreError {
    match &error {
        StoreError::InvalidWork(message) if BINDING_REFUSALS.contains(&message.as_str()) => {
            TaskSyncRefusal::OriginMismatch.classify(error)
        }
        _ => error,
    }
}

struct OriginScope {
    owner: std::rc::Rc<()>,
    connection: usize,
    changes: u64,
    origins: HashMap<ObjectId, ImportOrigin>,
}

#[derive(Debug)]
pub(crate) struct OriginScopeGuard {
    owner: Option<std::rc::Rc<()>>,
}

impl Drop for OriginScopeGuard {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            ORIGIN_SCOPES.with(|scopes| {
                let mut scopes = scopes.borrow_mut();
                if let Some(depth) = scopes
                    .iter()
                    .position(|s| std::rc::Rc::ptr_eq(&s.owner, owner))
                {
                    // A misordered owner drop discards dependent scopes too;
                    // their later drops cannot remove a new transaction's scope.
                    scopes.truncate(depth);
                }
            });
        }
    }
}

thread_local! {
    static ORIGIN_SCOPES: RefCell<Vec<OriginScope>> = const { RefCell::new(Vec::new()) };
    #[cfg(test)]
    pub(super) static ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    pub(super) static DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A transient decoded-object cache lives within one transaction's read call.
/// Nested reads of that connection share it; the guard drops it before return,
/// including errors and unwinds, and before an owned read transaction ends.
/// Own writes invalidate it within that call; the next call always starts empty
/// so commits from other connections are observed in its new snapshot.
pub(in crate::storage) fn with_origin_scope<T>(
    connection: &Connection,
    read: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let _scope = enter_origin_scope(connection)?;
    read()
}

/// The transaction owner drops this guard before committing or rolling back.
pub(in crate::storage) fn enter_origin_scope(
    connection: &Connection,
) -> Result<OriginScopeGuard, StoreError> {
    if connection.is_autocommit() {
        return Err(invalid("task origin scope requires a transaction"));
    }
    let key = std::ptr::from_ref(connection) as usize;
    let nested =
        ORIGIN_SCOPES.with(|scopes| scopes.borrow().last().is_some_and(|s| s.connection == key));
    if nested {
        return Ok(OriginScopeGuard { owner: None });
    }
    let owner = std::rc::Rc::new(());
    ORIGIN_SCOPES.with(|scopes| {
        scopes.borrow_mut().push(OriginScope {
            owner: std::rc::Rc::clone(&owner),
            connection: key,
            changes: connection.total_changes(),
            origins: HashMap::new(),
        });
    });
    Ok(OriginScopeGuard { owner: Some(owner) })
}

fn lookup(connection: &Connection, id: &ObjectId) -> Result<ImportOrigin, StoreError> {
    ORIGIN_SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        let scope = scopes
            .last_mut()
            .ok_or_else(|| invalid("task origin read outside snapshot scope"))?;
        if scope.connection != std::ptr::from_ref(connection) as usize {
            return Err(invalid("task origin read on a different connection"));
        }
        let changes = connection.total_changes();
        if scope.changes != changes {
            scope.origins.clear();
            scope.changes = changes;
        }
        if let Some(origin) = scope.origins.get(id) {
            return Ok(origin.clone());
        }
        let origin = read_origin(connection, id.as_str())?;
        scope.origins.insert(id.clone(), origin.clone());
        Ok(origin)
    })
}

pub(super) fn for_item(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<ImportOrigin>, StoreError> {
    if item.created_by.source_tool.as_deref() != Some(super::IMPORT_TOOL) {
        return Ok(None);
    }
    let id = super::origin_reference(&item.created_by)?;
    let origin = lookup(connection, &id)?;
    validate_record(connection, &origin)?;
    if origin.project_id != item.project_id
        || origin.task.id != item.work_id
        || item.created_by != super::linked_actor(&origin.actor, &id)
    {
        return Err(invalid(ITEM_BINDING_DIFFERS));
    }
    Ok(Some(origin))
}

pub(in crate::storage::work) fn validate_item_origin(
    connection: &Connection,
    record_id: &ObjectId,
    record: &RestoredRecord,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    if item.created_by.source_tool.as_deref() != Some(super::IMPORT_TOOL) {
        return Ok(false);
    }
    let id = super::origin_reference(&item.created_by)?;
    let origin = lookup(connection, &id)?;
    if &origin.record_id != record_id {
        return Err(invalid(RECORD_BINDING_DIFFERS));
    }
    validate_record(connection, &origin)?;
    if origin.project_id != item.project_id
        || origin.task.id != item.work_id
        || item.created_by != super::linked_actor(&origin.actor, &id)
        || item.created_at != origin.imported_at
        || item.updated_at != origin.imported_at
        || item.acceptance_bindings != origin.task.acceptance_bindings
        || origin.task.snapshot_with_root(record.item.root_id) != record.item
    {
        return Err(invalid(PROJECTION_DIFFERS));
    }
    Ok(true)
}

pub(in crate::storage) fn verify_origins(
    connection: &Connection,
    checked: &mut usize,
    invalid_records: &mut Vec<String>,
) -> Result<(), StoreError> {
    #[cfg(test)]
    ENUMERATIONS.with(|count| count.set(count.get() + 1));
    let ids = connection
        .prepare("SELECT object_id FROM objects WHERE object_kind = ?1 ORDER BY object_id")?
        .query_map([ORIGIN_KIND], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut projects = BTreeMap::<String, (bool, Vec<crate::task_sync::TaskDocument>)>::new();
    let mut records = HashSet::new();
    let mut note_ids = HashSet::new();
    let mut roots = HashMap::new();
    for id in ids {
        *checked += 1;
        match read_origin(connection, &id).and_then(|origin| {
            let root = validate_record(connection, &origin)?;
            let bytes: Vec<u8> = connection.query_row(
                "SELECT item_json FROM work_items WHERE work_id = ?1",
                [origin.task.id.0.to_string()],
                |row| row.get(0),
            )?;
            let item: WorkItem = serde_json::from_slice(&bytes)?;
            if super::origin_reference(&item.created_by)?.as_str() != id
                || item.created_by
                    != super::linked_actor(
                        &origin.actor,
                        &ObjectId::from_stored(id.clone())
                            .ok_or_else(|| invalid(ORIGIN_ID_INVALID))?,
                    )
            {
                return Err(invalid("import origin projection reference differs"));
            }
            if !records.insert(origin.record_id.clone()) {
                return Err(invalid("duplicate import record binding"));
            }
            crate::task_sync::notes::validate_note_structure(&origin.task.notes)?;
            if !origin.notes_included && !origin.task.notes.is_empty() {
                return Err(invalid("import origin notes inclusion differs"));
            }
            for note in &origin.task.notes {
                if !note_ids.insert(note.id.clone()) {
                    return Err(invalid("duplicate shared note id across import origins"));
                }
            }
            roots.insert((origin.project_id.0.clone(), origin.task.id), root);
            Ok(origin)
        }) {
            Ok(origin) => {
                let project = projects.entry(origin.project_id.0).or_default();
                project.0 |= origin.notes_included;
                project.1.push(origin.task);
            }
            Err(_) => invalid_records.push(format!("task_sync_import_origin:{id}")),
        }
    }
    for (project, (notes_included, tasks)) in projects {
        let tree = TaskTree {
            project: ProjectDocument {
                format: FORMAT.into(),
                project_id: crate::ProjectId(project.clone()),
                notes_included,
            },
            tasks,
        };
        let valid = tree.validate_stored().is_ok()
            && tree.snapshots().is_ok_and(|items| {
                items
                    .iter()
                    .all(|item| roots.get(&(project.clone(), item.work_id)) == Some(&item.root_id))
            });
        if !valid {
            invalid_records.push(format!("task_sync_import_graph:{project}"));
        }
    }
    Ok(())
}

/// Every import origin the store holds, in object-id order. Folder admission
/// judges an addition against the whole origin history this audit validates.
pub(super) fn all_origins(connection: &Connection) -> Result<Vec<ImportOrigin>, StoreError> {
    connection
        .prepare("SELECT object_id FROM objects WHERE object_kind = ?1 ORDER BY object_id")?
        .query_map([ORIGIN_KIND], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|id| read_origin(connection, &id))
        .collect()
}

fn read_origin(connection: &Connection, id: &str) -> Result<ImportOrigin, StoreError> {
    #[cfg(test)]
    DECODES.with(|count| count.set(count.get() + 1));
    let key = ObjectId::from_stored(id.to_owned()).ok_or_else(|| invalid(ORIGIN_ID_INVALID))?;
    SqliteStore::get_canonical_object_on(connection, &key, ORIGIN_KIND)?
        .ok_or_else(|| invalid(ORIGIN_MISSING))?
        .decode()
}

fn validate_record(
    connection: &Connection,
    origin: &ImportOrigin,
) -> Result<crate::WorkId, StoreError> {
    origin
        .actor
        .validate_attribution_context()
        .map_err(invalid)?;
    let record: RestoredRecord = SqliteStore::get_canonical_object_on(
        connection,
        &origin.record_id,
        "work_restored_record",
    )?
    .ok_or_else(|| invalid(RECORD_MISSING))?
    .decode()?;
    if record.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || origin
            .actor
            .provenance_chain
            .iter()
            .any(|link| link.source == ORIGIN_KIND)
        || record.generation_index != 0
        || record.project_id != origin.project_id
        || record.work_id != origin.task.id
        || record.item != origin.task.snapshot_with_root(record.item.root_id)
        || record.relations.prerequisites != origin.task.prerequisites
        || record.relations.blockers != origin.task.snapshot_blockers()
        || record.history != history(&origin.task, &origin.actor, origin.imported_at)
        || origin.actor.actor_id.trim().is_empty()
        || origin.actor.actor_id.trim() != origin.actor.actor_id
        || origin.actor.actor_kind.trim().is_empty()
        || origin.actor.actor_kind.trim() != origin.actor.actor_kind
        || origin
            .actor
            .session_id
            .as_ref()
            .is_none_or(|id| id.0.trim().is_empty())
    {
        return Err(invalid(BINDING_INVALID));
    }
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1 AND project_id = ?2)",
        [&origin.task.id.0.to_string(), &origin.project_id.0],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(invalid(TASK_MISSING));
    }
    Ok(record.item.root_id)
}
