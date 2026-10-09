//! On-demand canonical note matching; no durable search projection.

use std::collections::BTreeMap;

use super::{
    DateTime, SqliteStore, StoreError, Utc, WorkCatalogQuery, normalize_work_catalog_key,
    parse_work_id, work_catalog_sql,
};
use crate::storage::work::record_windows::{WorkRecordContent, WorkRecordFamily, WorkRecordKind};

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct WorkNoteSearchMatch {
    pub locator: String,
    pub family: WorkRecordFamily,
}

pub(super) fn matching_ids(
    matches: &BTreeMap<uuid::Uuid, WorkNoteSearchMatch>,
) -> Result<Option<String>, StoreError> {
    if matches.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(
        &matches.keys().map(ToString::to_string).collect::<Vec<_>>(),
    )?))
}

fn select_eligible_ids(sql: &str) -> Result<String, StoreError> {
    const COUNT_SELECTION: &str = "SELECT COUNT(*) FROM classified";
    if !sql.contains(COUNT_SELECTION) {
        return Err(StoreError::InvalidWorkProjection(
            "note search query lost its classified count selection".into(),
        ));
    }
    Ok(sql.replace(COUNT_SELECTION, "SELECT work_id FROM classified"))
}

impl SqliteStore {
    /// Caller holds the same read snapshot used by membership and page reads.
    pub(super) fn catalog_note_matches(
        &self,
        project: &crate::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<BTreeMap<uuid::Uuid, WorkNoteSearchMatch>, StoreError> {
        let mut matches = BTreeMap::new();
        let Some(search) = query
            .search
            .as_deref()
            .map(normalize_work_catalog_key)
            .filter(|text| !text.is_empty())
        else {
            return Ok(matches);
        };
        // Discover the entire eligible set, never just this page or seek suffix.
        let mut eligible = query.clone();
        eligible.search = None;
        eligible.after = None;
        eligible.after_priority = None;
        eligible.limit = 0;
        let (sql, parameters) = work_catalog_sql(project, now, &eligible, false, None)?;
        let selected = select_eligible_ids(&sql)?;
        // A metadata-only item has no note bytes to search. Avoid loading its
        // canonical item/events merely to establish an empty record window.
        let sql = format!("SELECT work_id FROM ({selected}) eligible WHERE
            EXISTS (SELECT 1 FROM work_run_evidence note WHERE note.work_id = eligible.work_id)
            OR EXISTS (SELECT 1 FROM work_observations note WHERE note.work_id = eligible.work_id)
            OR EXISTS (SELECT 1 FROM work_restored_evidence note WHERE note.work_id = eligible.work_id)
            OR EXISTS (SELECT 1 FROM work_restored_records record WHERE record.work_id = eligible.work_id)
            ORDER BY work_id");
        let mut statement = self.connection.prepare(&sql)?;
        let ids = statement
            .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
                row.get::<_, String>(0)
            })?
            .map(|row| parse_work_id(&row?))
            .collect::<Result<Vec<_>, StoreError>>()?;
        #[cfg(test)]
        crate::storage::work::cost::sql("note_search_eligible", &statement);
        for id in ids {
            for entry in self.work_record_index(project, id, WorkRecordKind::NotesWithGates)? {
                let WorkRecordContent::Note(note) =
                    self.work_record_content_for_search(project, id, &entry)?
                else {
                    continue;
                };
                // Match public note text only, not JSON keys, identity, raw
                // environment evidence, private scratch or history-only events.
                let contains = |text: &str| normalize_work_catalog_key(text).contains(&search);
                if contains(&note.summary)
                    || note.refs.iter().any(|text| contains(text))
                    || note.gate.as_ref().is_some_and(|gate| {
                        contains(&gate.name) || gate.failed.iter().any(|text| contains(text))
                    })
                {
                    matches.insert(
                        id.0,
                        WorkNoteSearchMatch {
                            locator: entry.locator,
                            family: entry.record_family,
                        },
                    );
                    break;
                }
            }
        }
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_search_selection_preserves_filters_and_refuses_a_lost_shape() {
        let sql = "WITH classified AS (SELECT work_id FROM work_items WHERE project_id = ?1) SELECT COUNT(*) FROM classified WHERE priority = ?2";
        assert_eq!(
            select_eligible_ids(sql).unwrap(),
            "WITH classified AS (SELECT work_id FROM work_items WHERE project_id = ?1) SELECT work_id FROM classified WHERE priority = ?2"
        );
        assert!(matches!(
            select_eligible_ids("SELECT total FROM classified"),
            Err(StoreError::InvalidWorkProjection(message))
                if message == "note search query lost its classified count selection"
        ));
    }
}
