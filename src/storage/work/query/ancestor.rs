use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::super::completion::{AncestorExecutionState, first_blocking_ancestor};
use super::{
    StoreError, WorkItem, WorkLifecycle, WorkReadinessReason, catalog, encode_state, parse_work_id,
};

pub(super) fn parent_execution_guidance(
    connection: &Connection,
    item: &WorkItem,
    ancestor: Option<&crate::domain::WorkBlockingAncestor>,
    now: DateTime<Utc>,
    independently_unblocked: bool,
    reasons: &mut Vec<WorkReadinessReason>,
    why: &mut Vec<String>,
) -> Result<(), StoreError> {
    if let Some(ancestor) = ancestor {
        let lifecycle = ancestor.lifecycle;
        why.push(format!(
            "execution blocked by ancestor {} ({})",
            ancestor.short_ref,
            encode_state(lifecycle)?
        ));
        if matches!(
            lifecycle,
            WorkLifecycle::Completed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded
        ) && independently_unblocked
            && catalog::projected_detach_admitted(connection, item, now)?
        {
            reasons.push(WorkReadinessReason::DetachAvailable);
        }
        Ok(())
    } else {
        why.push("the ancestor or root-execution generation does not admit execution".into());
        Ok(())
    }
}

pub(super) fn projected_blocking_ancestor(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<crate::domain::WorkBlockingAncestor>, StoreError> {
    first_blocking_ancestor(item, |parent| {
        let row: Option<(String, String, Option<String>, String, String)> = connection
            .query_row(
                "SELECT project_id, root_id, parent_id, lifecycle, short_ref
                 FROM work_items WHERE work_id = ?1",
                [parent.0.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let (project_id, root_id, parent_id, lifecycle, short_ref) = row.ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("work ancestor {parent:?} is missing"))
        })?;
        Ok(AncestorExecutionState {
            project_id: crate::domain::ProjectId(project_id),
            root_id: parse_work_id(&root_id)?,
            parent_id: parent_id.map(|value| parse_work_id(&value)).transpose()?,
            ancestor: crate::domain::WorkBlockingAncestor {
                work_id: parent,
                short_ref,
                lifecycle: serde_json::from_value(serde_json::Value::String(lifecycle))?,
            },
        })
    })
}
