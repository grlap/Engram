//! Current graph owners, separate from historical document copies.

use super::Node;
use crate::{WorkId, storage::StoreError, task_sync::TaskDocument};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Row {
    kind: String,
    scope: String,
    work: String,
    family: String,
    machine: String,
    value: String,
}

pub(super) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS work_task_sync_graph_identity_members (
        view_kind TEXT NOT NULL CHECK(view_kind IN ('origin','baseline','source','export')),
        scope_id TEXT NOT NULL,
        work_id TEXT NOT NULL,
        family TEXT NOT NULL CHECK(family IN ('note','activity')),
        machine TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY(view_kind,scope_id,work_id,family,machine,value),
        FOREIGN KEY(view_kind,scope_id,work_id) REFERENCES work_task_sync_graph_nodes(view_kind,scope_id,work_id) ON DELETE CASCADE
    ) STRICT;
    CREATE INDEX IF NOT EXISTS work_task_sync_graph_identity_lookup ON work_task_sync_graph_identity_members
        (view_kind,scope_id,family,machine,value,work_id);
    CREATE INDEX IF NOT EXISTS work_task_sync_graph_identity_original ON work_task_sync_graph_identity_members
        (family,machine,value,scope_id,work_id) WHERE view_kind='origin';")?;
    Ok(())
}

pub(super) fn rows(node: &Node, task: &TaskDocument) -> BTreeSet<Row> {
    super::super::documents::document_identities(task)
        .into_iter()
        .map(|identity| Row {
            kind: node.kind.clone(),
            scope: node.scope.clone(),
            work: node.work.clone(),
            family: identity.family.into(),
            machine: identity.machine,
            value: identity.value,
        })
        .collect()
}

pub(super) fn write(connection: &Connection, rows: &BTreeSet<Row>) -> Result<(), StoreError> {
    for row in rows {
        connection.execute(
            "INSERT INTO work_task_sync_graph_identity_members
            (view_kind,scope_id,work_id,family,machine,value) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                row.kind,
                row.scope,
                row.work,
                row.family,
                row.machine,
                row.value
            ],
        )?;
    }
    Ok(())
}

pub(super) fn actual(connection: &Connection) -> Result<BTreeSet<Row>, StoreError> {
    Ok(connection.prepare("SELECT view_kind,scope_id,work_id,family,machine,value FROM work_task_sync_graph_identity_members")?
        .query_map([], |row| Ok(Row { kind: row.get(0)?, scope: row.get(1)?, work: row.get(2)?, family: row.get(3)?, machine: row.get(4)?, value: row.get(5)? }))?
        .collect::<Result<_, _>>()?)
}

pub(in crate::storage::work::task_sync) fn original_held(
    connection: &Connection,
    family: &str,
    machine: &str,
    value: &str,
) -> Result<bool, StoreError> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM work_task_sync_graph_identity_members
        INDEXED BY work_task_sync_graph_identity_original WHERE view_kind='origin' AND family=?1 AND machine=?2 AND value=?3)",
        params![family,machine,value], |row| row.get(0))?)
}

pub(in crate::storage::work::task_sync) fn current_owners(
    connection: &Connection,
    kind: &str,
    scope: &str,
    family: &str,
    machine: &str,
    value: &str,
) -> Result<BTreeSet<WorkId>, StoreError> {
    connection.prepare("SELECT work_id FROM work_task_sync_graph_identity_members
        INDEXED BY work_task_sync_graph_identity_lookup WHERE view_kind=?1 AND scope_id=?2 AND family=?3 AND machine=?4 AND value=?5")?
        .query_map(params![kind,scope,family,machine,value], |row| row.get::<_, String>(0))?
        .map(|value| super::super::component::work_id(&value?)).collect()
}

pub(super) fn validate_replacements(
    connection: &Connection,
    binding: &str,
    delta: &super::super::source::delta::MembershipDelta,
) -> Result<(), StoreError> {
    for next in delta.changes.values().flatten() {
        let (_, task) = next;
        for identity in super::super::documents::document_identities(task) {
            if current_owners(
                connection,
                delta.role,
                binding,
                identity.family,
                &identity.machine,
                &identity.value,
            )?
            .into_iter()
            .any(|owner| owner != task.id && !delta.changes.contains_key(&owner))
            {
                return Err(crate::task_sync::TaskSyncRefusal::InvalidGraph
                    .invalid("shared identity belongs to a retained source/export member"));
            }
        }
    }
    Ok(())
}
