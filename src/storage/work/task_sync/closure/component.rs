//! Snapshot-scoped connected reads through the forward and reverse indexes.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::{
    IMPORT_TOOL, ORIGIN_KIND, baseline, folder::IMPORT_PLANNING_KIND, origin, query,
};
use super::graph::document_edges;
use crate::{
    ObjectId, ProjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{TaskDocument, TaskGraphComponent, TaskSyncGraphView, TaskSyncRefusal},
};

#[cfg(test)]
thread_local! {
    pub(in crate::storage::work::task_sync) static NODE_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(in crate::storage::work::task_sync) static MEMBER_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(in crate::storage::work::task_sync) static EDGE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl TaskSyncGraphView {
    fn kind(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Baseline => "baseline",
        }
    }
}

struct StoredNode {
    project: String,
    record: String,
    root: Option<String>,
    notes: bool,
    member_project: Option<String>,
    path: Option<String>,
    bytes: Option<Vec<u8>>,
    kind: Option<String>,
}

pub(super) struct Scope<'a> {
    pub project: &'a ProjectId,
    pub kind: &'static str,
    pub id: &'a str,
    pub view: Option<TaskSyncGraphView>,
    pub baseline: Option<ObjectId>,
    pub notes: bool,
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!(
        "task-sync graph {message}; repair projections explicitly"
    ))
}

pub(super) fn check_capacity(count: usize) -> Result<(), StoreError> {
    if count > 200_000 {
        return Err(
            TaskSyncRefusal::InvalidGraph.invalid("connected task scope exceeds supported bound")
        );
    }
    Ok(())
}

pub(super) fn work_id(text: &str) -> Result<WorkId, StoreError> {
    let id = uuid::Uuid::parse_str(text).map_err(|_| invalid("contains an invalid task id"))?;
    if id.to_string() != text {
        return Err(invalid("contains a noncanonical task id"));
    }
    Ok(WorkId(id))
}

impl SqliteStore {
    /// Capture complete connected shared components, traversing parent,
    /// prerequisite and replacement relations in both directions. Native work
    /// and immutable origins are never substituted for exchange membership.
    ///
    /// The result has not been admitted. It does not validate a delta, grant
    /// import authority or establish an admitted source-commit basis.
    /// Unchanged data relies on the admitted projection's coherence; doctor
    /// checks its exhaustive correspondence with canonical records.
    ///
    /// # Errors
    /// Refuses unavailable baseline heads, foreign-project selections and
    /// missing or inconsistent selected projections. No full scan is used.
    pub fn capture_task_sync_graph_component(
        &self,
        project: &ProjectId,
        view: TaskSyncGraphView,
        seeds: impl IntoIterator<Item = WorkId>,
    ) -> Result<TaskGraphComponent, StoreError> {
        if project.0.trim().is_empty() {
            return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
        }
        let seeds = seeds.into_iter().collect::<BTreeSet<_>>();
        self.work_read_snapshot(|store| {
            origin::with_origin_scope(&store.connection, || {
                capture(&store.connection, project, view, seeds)
            })
        })
    }
}

fn baseline_head(
    connection: &Connection,
    project: &ProjectId,
) -> Result<(ObjectId, bool), StoreError> {
    let head = baseline::head(connection, project)?.ok_or_else(|| {
        TaskSyncRefusal::IncrementalBasisUnavailable
            .invalid("no admitted baseline; bootstrap or rebuild explicitly")
    })?;
    let graph: Option<(String, bool)> = connection
        .query_row(
            "SELECT record_id,notes_included FROM work_task_sync_graph_heads WHERE scope_id=?1",
            [&project.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if graph != Some((head.record.as_str().into(), head.include_notes)) {
        return Err(invalid("baseline head is missing or stale"));
    }
    let admitted: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_feed_entries entry
         JOIN objects object ON object.object_id=entry.object_id
         WHERE entry.feed_kind='project' AND entry.feed_id=?1 AND entry.position=?2
           AND entry.object_id=?3 AND entry.object_kind=?4 AND object.object_kind=?4)",
        params![
            project.0,
            head.position,
            head.record.as_str(),
            IMPORT_PLANNING_KIND
        ],
        |row| row.get(0),
    )?;
    if !admitted {
        return Err(invalid("baseline head provenance differs"));
    }
    Ok((head.record, head.include_notes))
}

pub(in crate::storage::work::task_sync) fn capture(
    connection: &Connection,
    project: &ProjectId,
    view: TaskSyncGraphView,
    pending: BTreeSet<WorkId>,
) -> Result<TaskGraphComponent, StoreError> {
    let (baseline, notes) = match view {
        TaskSyncGraphView::Origin => (None, false),
        TaskSyncGraphView::Baseline => {
            let (record, notes) = baseline_head(connection, project)?;
            (Some(record), notes)
        }
    };
    capture_scope(
        connection,
        Scope {
            project,
            kind: view.kind(),
            id: &project.0,
            view: Some(view),
            baseline,
            notes,
        },
        pending,
    )
}

pub(super) fn capture_scope(
    connection: &Connection,
    scope: Scope<'_>,
    mut pending: BTreeSet<WorkId>,
) -> Result<TaskGraphComponent, StoreError> {
    check_capacity(pending.len())?;
    let project = scope.project;
    let mut result = TaskGraphComponent {
        documents: BTreeMap::new(),
        missing: BTreeSet::new(),
        members: BTreeMap::new(),
        expected_roots: BTreeMap::new(),
        notes_included: scope.notes,
        baseline: scope.baseline,
    };
    let mut seen = BTreeSet::new();
    // Seed ids select a component, not arbitrary native creation histories.
    // Native fallback is justified only by outgoing references of documents
    // actually reached in the immutable origin view.
    let mut native_references = BTreeSet::new();
    while let Some(id) = pending.pop_first() {
        if !seen.insert(id) {
            continue;
        }
        check_capacity(seen.len())?;
        #[cfg(test)]
        NODE_LOOKUPS.with(|count| count.set(count.get() + 1));
        let node = connection.query_row("SELECT node.project_id,node.record_id,node.expected_root_id,node.notes_included,
            member.project_id,member.member_path,member.document_json,object.object_kind
            FROM work_task_sync_graph_nodes node
            LEFT JOIN work_task_sync_document_members member ON member.record_id=node.record_id AND member.work_id=node.work_id
            LEFT JOIN objects object ON object.object_id=node.record_id
            WHERE node.view_kind=?1 AND node.scope_id=?2 AND node.work_id=?3",
            params![scope.kind,scope.id,id.0.to_string()], |row| Ok(StoredNode {
                project:row.get(0)?,record:row.get(1)?,root:row.get(2)?,notes:row.get(3)?,member_project:row.get(4)?,path:row.get(5)?,bytes:row.get(6)?,kind:row.get(7)?,
            })).optional()?;
        let Some(node) = node else {
            if scope.view == Some(TaskSyncGraphView::Origin)
                && native_references.contains(&id)
                && let Some(endpoint) = origin::native_endpoint(connection, project, id)?
            {
                for (_, target) in document_edges(&endpoint.task) {
                    native_references.insert(target);
                    if result.missing.remove(&target) {
                        seen.remove(&target);
                    }
                    pending.insert(target);
                }
                incoming(connection, scope.kind, scope.id, id, &mut pending)?;
                result.expected_roots.insert(id, endpoint.root);
                result.members.insert(id, endpoint.record);
                result.documents.insert(id, endpoint.task);
                continue;
            }
            if let Some(view) = scope.view {
                missing_node(connection, project, view, id)?;
            }
            incoming(connection, scope.kind, scope.id, id, &mut pending)?;
            result.missing.insert(id);
            continue;
        };
        if node.project != project.0
            || node.member_project.as_deref() != Some(project.0.as_str())
            || !member_provenance(&node, scope.view, id)
        {
            return Err(invalid("selected member provenance differs"));
        }
        if match scope.view {
            Some(TaskSyncGraphView::Origin) => node.root.is_none(),
            Some(TaskSyncGraphView::Baseline) | None => {
                node.root.is_some() || node.notes != result.notes_included
            }
        } {
            return Err(invalid("selected node profile or root binding differs"));
        }
        let bytes = node
            .bytes
            .ok_or_else(|| invalid("selected member body is missing"))?;
        #[cfg(test)]
        MEMBER_DECODES.with(|count| count.set(count.get() + 1));
        let document: TaskDocument = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("selected member body is invalid"))?;
        if document.id != id {
            return Err(invalid("selected member task differs"));
        }
        let mut forward = BTreeSet::new();
        for edge in connection.prepare("SELECT relation,target_id FROM work_task_sync_graph_edges WHERE view_kind=?1 AND scope_id=?2 AND owner_id=?3")?.query_map(params![scope.kind,scope.id,id.0.to_string()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))? {
            #[cfg(test)]
            EDGE_READS.with(|count| count.set(count.get() + 1));
            let (relation,target)=edge?; let target=work_id(&target)?;
            forward.insert((relation,target)); pending.insert(target);
            if scope.view == Some(TaskSyncGraphView::Origin) {
                native_references.insert(target);
                if result.missing.remove(&target) { seen.remove(&target); }
            }
        }
        let expected = document_edges(&document)
            .into_iter()
            .map(|(relation, target)| (relation.to_owned(), target))
            .collect::<BTreeSet<_>>();
        if forward != expected {
            return Err(invalid("selected member adjacency differs"));
        }
        incoming(connection, scope.kind, scope.id, id, &mut pending)?;
        if let Some(root) = node.root {
            result.expected_roots.insert(id, work_id(&root)?);
        }
        result.notes_included |= node.notes;
        result.members.insert(
            id,
            ObjectId::from_stored(node.record)
                .ok_or_else(|| invalid("member record id is invalid"))?,
        );
        result.documents.insert(id, document);
    }
    Ok(result)
}

fn incoming(
    connection: &Connection,
    kind: &str,
    scope: &str,
    id: WorkId,
    pending: &mut BTreeSet<WorkId>,
) -> Result<(), StoreError> {
    for owner in connection.prepare("SELECT owner_id FROM work_task_sync_graph_edges INDEXED BY work_task_sync_graph_edges_target WHERE view_kind=?1 AND scope_id=?2 AND target_id=?3")?.query_map(params![kind,scope,id.0.to_string()], |row| row.get::<_,String>(0))? {
        #[cfg(test)]
        EDGE_READS.with(|count| count.set(count.get() + 1));
        pending.insert(work_id(&owner?)?);
    }
    Ok(())
}

fn member_provenance(node: &StoredNode, view: Option<TaskSyncGraphView>, id: WorkId) -> bool {
    let (kind, path) = match view {
        Some(TaskSyncGraphView::Origin) => (ORIGIN_KIND, "/task".into()),
        Some(TaskSyncGraphView::Baseline) => (IMPORT_PLANNING_KIND, format!("/changes/{}", id.0)),
        None => match node.kind.as_deref() {
            Some(ORIGIN_KIND | crate::task_sync::reconciliation::SHARED_VERSION_KIND) => {
                return node.path.as_deref() == Some("/task");
            }
            Some(IMPORT_PLANNING_KIND) => (IMPORT_PLANNING_KIND, format!("/changes/{}", id.0)),
            Some(crate::task_sync::ledger::EVENT_KIND) => {
                return node.path.as_deref()
                    == Some(&format!(
                        "/transition/checkpoint/seed/members/members/{}/task",
                        id.0
                    ))
                    || node.path.as_deref()
                        == Some(&format!("/transition/payload/documents/{}", id.0));
            }
            _ => return false,
        },
    };
    node.kind.as_deref() == Some(kind) && node.path.as_deref() == Some(path.as_str())
}

fn missing_node(
    connection: &Connection,
    project: &ProjectId,
    view: TaskSyncGraphView,
    id: WorkId,
) -> Result<(), StoreError> {
    let actual_project: Option<String> = connection
        .query_row(
            "SELECT project_id FROM work_items WHERE work_id=?1",
            [id.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if actual_project
        .as_ref()
        .is_some_and(|actual| actual != &project.0)
    {
        return Err(
            TaskSyncRefusal::OriginMismatch.invalid("selected task belongs to another project")
        );
    }
    let expected = match view {
        TaskSyncGraphView::Baseline => baseline::entries(connection, project, [id])?
            .get(&id)
            .is_some_and(|entry| entry.document.is_some()),
        TaskSyncGraphView::Origin if actual_project.is_some() => {
            query::load_work_item(connection, id)?
                .created_by
                .source_tool
                .as_deref()
                == Some(IMPORT_TOOL)
        }
        TaskSyncGraphView::Origin => false,
    };
    if expected {
        return Err(invalid("selected node is missing"));
    }
    Ok(())
}
