//! Original and current shared graph views. Native adjacency remains in work SQL.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::{
    ImportOrigin, ORIGIN_KIND, baseline::TaskSyncImportPlanning, folder::IMPORT_PLANNING_KIND,
    origin,
};
use crate::{CanonicalObject, ObjectId, WorkId, storage::StoreError, task_sync::TaskDocument};

pub(in crate::storage::work::task_sync) mod identities;

#[cfg(test)]
thread_local! {
    pub(super) static DELTA_NODE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static DELTA_MEMBER_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Node {
    kind: String,
    scope: String,
    project: String,
    work: String,
    record: String,
    root: Option<String>,
    notes: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Edge {
    kind: String,
    scope: String,
    owner: String,
    relation: String,
    target: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Head {
    scope: String,
    record: String,
    notes: bool,
}

#[derive(Default)]
struct View {
    nodes: BTreeMap<(String, String, String), Node>,
    edges: BTreeMap<(String, String, String), BTreeSet<Edge>>,
    heads: BTreeMap<String, Head>,
    identities: BTreeMap<(String, String, String), BTreeSet<identities::Row>>,
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_graph_nodes (
             view_kind TEXT NOT NULL CHECK(view_kind IN ('origin','baseline','source','export')),
             scope_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             work_id TEXT NOT NULL,
             record_id TEXT NOT NULL,
             expected_root_id TEXT,
             notes_included INTEGER NOT NULL CHECK(notes_included IN (0,1)),
             PRIMARY KEY(view_kind,scope_id,work_id),
             FOREIGN KEY(record_id,work_id)
                 REFERENCES work_task_sync_document_members(record_id,work_id) ON DELETE CASCADE
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_graph_edges (
             view_kind TEXT NOT NULL CHECK(view_kind IN ('origin','baseline','source','export')),
             scope_id TEXT NOT NULL,
             owner_id TEXT NOT NULL,
             relation TEXT NOT NULL CHECK(relation IN ('parent','prerequisite','replacement')),
             target_id TEXT NOT NULL,
             PRIMARY KEY(view_kind,scope_id,owner_id,relation,target_id),
             FOREIGN KEY(view_kind,scope_id,owner_id)
                 REFERENCES work_task_sync_graph_nodes(view_kind,scope_id,work_id) ON DELETE CASCADE
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_graph_edges_target
             ON work_task_sync_graph_edges(view_kind,scope_id,target_id,relation,owner_id);
         CREATE TABLE IF NOT EXISTS work_task_sync_graph_heads (
             scope_id TEXT PRIMARY KEY,
             record_id TEXT NOT NULL REFERENCES objects(object_id) ON DELETE CASCADE,
             notes_included INTEGER NOT NULL CHECK(notes_included IN (0,1))
         ) STRICT;",
    )?;
    identities::create_schema(connection)?;
    super::source::create_schema(connection)
}

fn edges(kind: &str, scope: &str, task: &TaskDocument) -> BTreeSet<Edge> {
    document_edges(task)
        .into_iter()
        .map(|(relation, target)| Edge {
            kind: kind.into(),
            scope: scope.into(),
            owner: task.id.0.to_string(),
            relation: relation.into(),
            target: target.0.to_string(),
        })
        .collect()
}

pub(in crate::storage::work::task_sync) fn document_edges(
    task: &TaskDocument,
) -> BTreeSet<(&'static str, WorkId)> {
    task.parent_id
        .into_iter()
        .map(|id| ("parent", id))
        .chain(
            task.prerequisites
                .iter()
                .copied()
                .map(|id| ("prerequisite", id)),
        )
        .chain(task.superseded_by.map(|id| ("replacement", id)))
        .collect()
}

fn node(
    kind: &str,
    project: &str,
    record: &ObjectId,
    task: &TaskDocument,
    root: Option<WorkId>,
    notes: bool,
) -> Node {
    Node {
        kind: kind.into(),
        scope: project.into(),
        project: project.into(),
        work: task.id.0.to_string(),
        record: record.as_str().into(),
        root: root.map(|id| id.0.to_string()),
        notes,
    }
}

fn write_node(
    connection: &Connection,
    node: &Node,
    edges: &BTreeSet<Edge>,
    identities: &BTreeSet<identities::Row>,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO work_task_sync_graph_nodes
        (view_kind,scope_id,project_id,work_id,record_id,expected_root_id,notes_included)
        VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            node.kind,
            node.scope,
            node.project,
            node.work,
            node.record,
            node.root,
            node.notes
        ],
    )?;
    identities::write(connection, identities)?;
    for edge in edges {
        connection.execute(
            "INSERT INTO work_task_sync_graph_edges
            (view_kind,scope_id,owner_id,relation,target_id) VALUES (?1,?2,?3,?4,?5)",
            params![
                edge.kind,
                edge.scope,
                edge.owner,
                edge.relation,
                edge.target
            ],
        )?;
    }
    Ok(())
}

fn write_head(connection: &Connection, head: &Head) -> Result<(), StoreError> {
    connection.execute("INSERT INTO work_task_sync_graph_heads(scope_id,record_id,notes_included)
        VALUES (?1,?2,?3) ON CONFLICT(scope_id) DO UPDATE SET record_id=excluded.record_id,notes_included=excluded.notes_included",
        params![head.scope,head.record,head.notes])?;
    Ok(())
}

/// Reads only this origin's bound original record, never a project inventory.
pub(super) fn record_on(
    connection: &Connection,
    kind: &str,
    object: &CanonicalObject,
) -> Result<(), StoreError> {
    if kind == ORIGIN_KIND {
        let original: ImportOrigin = object.decode()?;
        let root = origin::validate_record(connection, &original)?;
        let node = node(
            "origin",
            &original.project_id.0,
            object.key(),
            &original.task,
            Some(root),
            original.notes_included,
        );
        write_node(
            connection,
            &node,
            &edges("origin", &original.project_id.0, &original.task),
            &identities::rows(&node, &original.task),
        )?;
    } else if kind == IMPORT_PLANNING_KIND {
        let planning: TaskSyncImportPlanning = object.decode()?;
        for (work, document) in &planning.changes {
            connection.execute("DELETE FROM work_task_sync_graph_nodes WHERE view_kind='baseline' AND scope_id=?1 AND work_id=?2", params![planning.project_id.0,work.0.to_string()])?;
            if let Some(document) = document {
                let node = node(
                    "baseline",
                    &planning.project_id.0,
                    object.key(),
                    document,
                    None,
                    planning.profile.include_notes,
                );
                write_node(
                    connection,
                    &node,
                    &edges("baseline", &planning.project_id.0, document),
                    &identities::rows(&node, document),
                )?;
            }
        }
        write_head(
            connection,
            &Head {
                scope: planning.project_id.0,
                record: object.key().as_str().into(),
                notes: planning.profile.include_notes,
            },
        )?;
    } else if kind == crate::task_sync::ledger::EVENT_KIND {
        let event: crate::task_sync::ledger::Event = object.decode()?;
        if let crate::task_sync::ledger::Transition::CustodyCheckpoint { checkpoint } =
            &event.transition
        {
            let mut budget = super::super::ledger::view::read::ReadBudget::default();
            let binding = super::super::ledger::view::read::binding_on(
                connection,
                &event.binding,
                &mut budget,
            )?;
            checkpoint.validate(&event.binding, &binding)?;
            if let Some(resolved) =
                super::source::resolve_seed_on(connection, object.key(), &binding, checkpoint)?
            {
                connection.execute(
                    "DELETE FROM work_task_sync_graph_nodes WHERE view_kind=?1 AND scope_id=?2",
                    params![resolved.role, event.binding.as_str()],
                )?;
                for (owner, task) in resolved.members.values() {
                    let mut node = node(
                        resolved.role,
                        &binding.project.0,
                        owner,
                        task,
                        None,
                        resolved.output.profile.include_notes,
                    );
                    node.scope = event.binding.to_string();
                    write_node(
                        connection,
                        &node,
                        &edges(resolved.role, event.binding.as_str(), task),
                        &identities::rows(&node, task),
                    )?;
                }
                super::source::record_on(connection, object.key(), checkpoint, &resolved)?;
            }
        } else if matches!(&event.transition,
            crate::task_sync::ledger::Transition::DeltaPrepared { intent, .. }
                if intent.direction == crate::task_sync::ledger::Direction::Outgoing)
            || matches!(
                &event.transition,
                crate::task_sync::ledger::Transition::DeltaImportApplied { .. }
                    | crate::task_sync::ledger::Transition::PlanningSuccessorApplied { .. }
            )
        {
            let binding = super::super::ledger::view::read::binding_on(
                connection,
                &event.binding,
                &mut super::super::ledger::view::read::ReadBudget::default(),
            )?;
            if let Some(delta) =
                super::source::delta::resolve_on(connection, object.key(), &event, &binding, false)?
            {
                record_delta_on(
                    connection,
                    object.key(),
                    &event.binding,
                    &binding.project,
                    &delta,
                )?;
            }
        }
    }
    Ok(())
}

fn record_delta_on(
    connection: &Connection,
    event_id: &ObjectId,
    binding: &ObjectId,
    project: &crate::ProjectId,
    delta: &super::source::delta::MembershipDelta,
) -> Result<(), StoreError> {
    let prior: Option<(String, String, Vec<u8>)> = connection.query_row(
        "SELECT checkpoint_id,inventory_root,profile_json FROM work_task_sync_source_heads WHERE binding_id=?1 AND role=?2",
        params![binding.as_str(), delta.role], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let (head, root, profile) =
        prior.ok_or_else(|| invalid("delta membership has no admitted bootstrap"))?;
    let head = parse_id(head)?;
    let before = crate::task_sync::delta::OutputMembership {
        inventory_root: crate::task_sync::inventory::InventoryRoot::parse(&root)
            .ok_or_else(|| invalid("delta membership root is invalid"))?,
        profile: serde_json::from_slice(&profile)?,
    };
    delta.require_before(&head, &before)?;
    let mut selected = BTreeMap::new();
    for work in delta.changes.keys() {
        #[cfg(test)]
        DELTA_NODE_READS.with(|count| count.set(count.get() + 1));
        let old: Option<(String, String, String, Option<Vec<u8>>)> = connection
            .query_row(
                "SELECT node.project_id,node.record_id,member.project_id,member.document_json
             FROM work_task_sync_graph_nodes node LEFT JOIN work_task_sync_document_members member
                ON member.record_id=node.record_id AND member.work_id=node.work_id
             WHERE node.view_kind=?1 AND node.scope_id=?2 AND node.work_id=?3",
                params![delta.role, binding.as_str(), work.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((node_project, owner, member_project, bytes)) = old {
            if node_project != project.0 || member_project != project.0 {
                return Err(invalid("delta old member project differs"));
            }
            #[cfg(test)]
            DELTA_MEMBER_DECODES.with(|count| count.set(count.get() + 1));
            let task: TaskDocument = serde_json::from_slice(
                &bytes.ok_or_else(|| invalid("delta old member is missing"))?,
            )?;
            if task.id != *work {
                return Err(invalid("delta old member task differs"));
            }
            selected.insert(*work, (parse_id(owner)?, task));
        }
    }
    delta.apply(&mut selected, &before)?;
    identities::validate_replacements(connection, binding.as_str(), delta)?;
    for (work, next) in &delta.changes {
        connection.execute("DELETE FROM work_task_sync_graph_nodes WHERE view_kind=?1 AND scope_id=?2 AND work_id=?3",
            params![delta.role, binding.as_str(), work.0.to_string()])?;
        if let Some((owner, document)) = next {
            let mut changed = node(
                delta.role,
                &project.0,
                owner,
                document,
                None,
                delta.output.profile.include_notes,
            );
            changed.scope = binding.to_string();
            write_node(
                connection,
                &changed,
                &edges(delta.role, binding.as_str(), document),
                &identities::rows(&changed, document),
            )?;
        }
    }
    super::source::record_head_on(
        connection,
        event_id,
        binding,
        &super::source::ResolvedBootstrap {
            project: project.clone(),
            role: delta.role,
            output: delta.output.clone(),
            members: BTreeMap::new(),
        },
    )
}

fn expected(connection: &Connection) -> Result<View, StoreError> {
    let mut view = View::default();
    let mut statement = connection.prepare(
        "SELECT object_id,canonical_json FROM objects WHERE object_kind=?1 ORDER BY object_id",
    )?;
    let mut rows = statement.query([ORIGIN_KIND])?;
    while let Some(row) = rows.next()? {
        let id = parse_id(row.get(0)?)?;
        let original: ImportOrigin = CanonicalObject::stored(&id, row.get(1)?)?.decode()?;
        let root = origin::validate_record(connection, &original)?;
        let node = node(
            "origin",
            &original.project_id.0,
            &id,
            &original.task,
            Some(root),
            original.notes_included,
        );
        let key = (node.kind.clone(), node.scope.clone(), node.work.clone());
        view.identities
            .insert(key.clone(), identities::rows(&node, &original.task));
        if view.nodes.insert(key.clone(), node).is_some() {
            return Err(invalid("duplicate immutable origin graph owner"));
        }
        view.edges
            .insert(key, edges("origin", &original.project_id.0, &original.task));
    }
    let mut statement = connection.prepare(
        "SELECT object.object_id,object.canonical_json
        FROM work_feed_entries entry JOIN objects object ON object.object_id=entry.object_id
        WHERE entry.feed_kind='project' AND entry.object_kind='task_sync_import_planning'
        ORDER BY entry.feed_id,entry.position",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id = parse_id(row.get(0)?)?;
        let planning: TaskSyncImportPlanning =
            CanonicalObject::stored(&id, row.get(1)?)?.decode()?;
        planning.validate(&planning.project_id)?;
        for (work, document) in &planning.changes {
            let key = (
                "baseline".into(),
                planning.project_id.0.clone(),
                work.0.to_string(),
            );
            view.nodes.remove(&key);
            view.edges.remove(&key);
            view.identities.remove(&key);
            if let Some(document) = document {
                let node = node(
                    "baseline",
                    &planning.project_id.0,
                    &id,
                    document,
                    None,
                    planning.profile.include_notes,
                );
                view.identities
                    .insert(key.clone(), identities::rows(&node, document));
                view.nodes.insert(key.clone(), node);
                view.edges
                    .insert(key, edges("baseline", &planning.project_id.0, document));
            }
        }
        view.heads.insert(
            planning.project_id.0.clone(),
            Head {
                scope: planning.project_id.0,
                record: id.as_str().into(),
                notes: planning.profile.include_notes,
            },
        );
    }
    for ((binding, _role), (_checkpoint, resolved)) in super::source::expected(connection)? {
        for (owner, task) in resolved.members.values() {
            let mut node = node(
                resolved.role,
                &resolved.project.0,
                owner,
                task,
                None,
                resolved.output.profile.include_notes,
            );
            node.scope.clone_from(&binding);
            let key = (node.kind.clone(), node.scope.clone(), node.work.clone());
            view.identities
                .insert(key.clone(), identities::rows(&node, task));
            view.nodes.insert(key.clone(), node);
            view.edges.insert(key, edges(resolved.role, &binding, task));
        }
    }
    Ok(view)
}

fn parse_id(value: String) -> Result<ObjectId, StoreError> {
    ObjectId::from_stored(value).ok_or_else(|| invalid("invalid graph provenance id"))
}
fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}

pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    let view = match expected(connection) {
        Ok(view) => view,
        Err(StoreError::Sqlite(error)) => return Err(error.into()),
        Err(error) => {
            return Ok(vec![crate::storage::decode_failure_label(
                "work_task_sync_graph:underivable".into(),
                &error,
            )]);
        }
    };
    connection.execute("DELETE FROM work_task_sync_graph_edges", [])?;
    connection.execute("DELETE FROM work_task_sync_graph_identity_members", [])?;
    connection.execute("DELETE FROM work_task_sync_graph_nodes", [])?;
    connection.execute("DELETE FROM work_task_sync_graph_heads", [])?;
    for (key, node) in &view.nodes {
        write_node(
            connection,
            node,
            &view.edges.get(key).cloned().unwrap_or_default(),
            &view.identities.get(key).cloned().unwrap_or_default(),
        )?;
    }
    for head in view.heads.values() {
        write_head(connection, head)?;
    }
    Ok(Vec::new())
}

pub(in crate::storage::work) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid_rows: &mut Vec<String>,
) -> Result<(), StoreError> {
    let view = match expected(connection) {
        Ok(view) => view,
        Err(StoreError::Sqlite(error)) => return Err(error.into()),
        Err(error) => {
            invalid_rows.push(crate::storage::decode_failure_label(
                "work_task_sync_graph:underivable".into(),
                &error,
            ));
            return Ok(());
        }
    };
    let actual_nodes = connection.prepare("SELECT view_kind,scope_id,project_id,work_id,record_id,expected_root_id,notes_included FROM work_task_sync_graph_nodes")?.query_map([], |row| Ok(Node {
        kind: row.get(0)?,scope: row.get(1)?,project: row.get(2)?,work: row.get(3)?,record: row.get(4)?,root: row.get(5)?,notes: row.get(6)?,
    }))?.collect::<Result<BTreeSet<_>,_>>()?;
    let actual_edges = connection
        .prepare(
            "SELECT view_kind,scope_id,owner_id,relation,target_id FROM work_task_sync_graph_edges",
        )?
        .query_map([], |row| {
            Ok(Edge {
                kind: row.get(0)?,
                scope: row.get(1)?,
                owner: row.get(2)?,
                relation: row.get(3)?,
                target: row.get(4)?,
            })
        })?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let actual_heads = connection
        .prepare("SELECT scope_id,record_id,notes_included FROM work_task_sync_graph_heads")?
        .query_map([], |row| {
            Ok(Head {
                scope: row.get(0)?,
                record: row.get(1)?,
                notes: row.get(2)?,
            })
        })?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let actual_identities = identities::actual(connection)?;
    *checked += view.nodes.len()
        + view.edges.values().map(BTreeSet::len).sum::<usize>()
        + view.heads.len()
        + actual_nodes.len()
        + actual_edges.len()
        + actual_heads.len()
        + view.identities.values().map(BTreeSet::len).sum::<usize>()
        + actual_identities.len();
    if view.nodes.into_values().collect::<BTreeSet<_>>() != actual_nodes {
        invalid_rows.push("work_task_sync_graph_nodes:content".into());
    }
    if view.edges.into_values().flatten().collect::<BTreeSet<_>>() != actual_edges {
        invalid_rows.push("work_task_sync_graph_edges:content".into());
    }
    if view.heads.into_values().collect::<BTreeSet<_>>() != actual_heads {
        invalid_rows.push("work_task_sync_graph_heads:content".into());
    }
    if view
        .identities
        .into_values()
        .flatten()
        .collect::<BTreeSet<_>>()
        != actual_identities
    {
        invalid_rows.push("work_task_sync_graph_identity_members:content".into());
    }
    Ok(())
}
