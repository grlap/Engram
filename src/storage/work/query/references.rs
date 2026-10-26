//! Project-scoped work-reference resolution and bounded ambiguity.
use super::{
    Connection, StoreError, WorkItem, ambiguous_work_reference_error, load_work_item, params,
    parse_work_id,
};

pub(super) const MAX_AMBIGUOUS_WORK_CANDIDATES: usize = 8;

pub(in crate::storage) fn resolve_work_ref_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    work_ref: &str,
) -> Result<WorkItem, StoreError> {
    let work_ref = work_ref.trim();
    if work_ref.is_empty() {
        return Err(StoreError::InvalidWork(
            "work reference must not be empty".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT work_id, COUNT(*) OVER()
         FROM work_items
         WHERE project_id = ?1 AND (short_ref = ?2 OR work_id = ?2)
         ORDER BY work_id LIMIT ?3",
    )?;
    let matches = statement
        .query_map(
            params![
                project_id.0,
                work_ref,
                i64::try_from(MAX_AMBIGUOUS_WORK_CANDIDATES).map_err(|_| {
                    StoreError::InvalidWorkProjection(
                        "ambiguous-work candidate limit overflowed SQLite".into(),
                    )
                })?
            ],
            |row| {
                super::super::search_budget::record_row(connection, row)?;
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    match matches.as_slice() {
        [(work_id, _)] => load_work_item(connection, parse_work_id(work_id)?),
        [] => Err(StoreError::InvalidWork(format!(
            "work reference {work_ref:?} does not exist in project {:?}",
            project_id.0
        ))),
        _ => {
            let total = usize::try_from(matches[0].1).map_err(|_| {
                StoreError::InvalidWorkProjection(
                    "ambiguous-work candidate count overflowed usize".into(),
                )
            })?;
            let candidates = matches
                .iter()
                .map(|(work_id, _)| load_work_item(connection, parse_work_id(work_id)?))
                .collect::<Result<Vec<_>, StoreError>>()?;
            let more = total.saturating_sub(candidates.len());
            Err(ambiguous_work_reference_error(work_ref, candidates, more))
        }
    }
}
