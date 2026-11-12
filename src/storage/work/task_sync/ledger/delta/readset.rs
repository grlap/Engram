//! Historical cuts remain fixed; only the selected native readset must be quiet.

use super::{Connection, StoreError, producing_origin, require};
use crate::{ProjectId, WorkId, task_sync::delta::OriginQualifiedCut};
use rusqlite::params;

pub(super) fn historical_on(
    connection: &Connection,
    project: &ProjectId,
    cut: &OriginQualifiedCut,
) -> Result<(), StoreError> {
    producing_origin::check_cut_on(
        connection,
        project,
        &crate::task_sync::progress::ExportCut {
            position: cut.position.position,
            anchor: cut.anchor.clone(),
        },
    )
}

/// Call with the complete freshly captured connected scope, including missing
/// endpoints. Native claims, generations and planning all emit work events;
/// keyed lookups avoid reading unrelated project history or foreign originals.
pub(super) fn unchanged_on(
    connection: &Connection,
    project: &ProjectId,
    cut: &OriginQualifiedCut,
    works: impl IntoIterator<Item = WorkId>,
) -> Result<(), StoreError> {
    historical_on(connection, project, cut)?;
    let mut statement = connection.prepare(
        "SELECT EXISTS(SELECT 1 FROM work_feed_entries INDEXED BY work_feed_entries_work_event_item
         WHERE feed_kind='project' AND work_id=?1 AND work_id IS NOT NULL
           AND object_kind='work_event' AND feed_id=?2 AND position>?3)",
    )?;
    for work in works {
        let changed: bool = statement.query_row(
            params![work.0.to_string(), project.0, cut.position.position],
            |row| row.get(0),
        )?;
        require(
            !changed,
            "retained exchange native readset changed since its local cut",
        )?;
    }
    Ok(())
}
