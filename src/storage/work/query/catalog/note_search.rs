//! Literal public-note matching from the incrementally maintained projection.

use super::{
    DateTime, SqliteStore, StoreError, Utc, WorkCatalogQuery, normalize_work_catalog_key,
    parse_work_id, work_catalog_sql,
};
use crate::storage::work::record_windows::WorkRecordFamily;
use crate::storage::work::search_budget;
use std::collections::BTreeMap;

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
    /// Caller holds the snapshot shared by matching, membership and page reads.
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
        #[cfg(test)]
        search_budget::pause(&self.connection)?;
        // Complete eligible set, independent of page or seek suffix.
        let mut eligible = query.clone();
        eligible.search = None;
        eligible.after = None;
        eligible.after_priority = None;
        eligible.limit = 0;
        let (sql, mut parameters) = work_catalog_sql(project, now, &eligible, false, None)?;
        let selected = select_eligible_ids(&sql)?;
        // MATCH parsing is NUL-terminated; short/NUL literals use the same exact
        // normalized projection fields instead. No canonical scan survives.
        let candidates = if super::catalog_uses_fts(&search) {
            parameters.push(rusqlite::types::Value::Text(
                super::catalog_literal_fts_query(&search),
            ));
            format!("work_note_fts MATCH ?{} AND", parameters.len())
        } else {
            String::new()
        };
        let sql = format!("SELECT work_id, locator, family, fields_json FROM work_note_fts
            WHERE {candidates} work_id IN ({selected})
            ORDER BY CAST(layer AS INTEGER), CAST(generation AS INTEGER), CAST(position AS INTEGER)");
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(parameters.iter()))?;
        while let Some(row) = rows.next()? {
            search_budget::record_row(&self.connection, row)?;
            let id = parse_work_id(&row.get::<_, String>(0)?)?;
            if matches.contains_key(&id.0) {
                continue;
            }
            let fields: Vec<String> = serde_json::from_str(&row.get::<_, String>(3)?)?;
            let mut found = false;
            for field in fields {
                if search_budget::contains_normalized(&self.connection, &field, &search)? {
                    found = true;
                    break;
                }
            }
            if found {
                let family = match row.get::<_, String>(2)?.as_str() {
                    "notes" => WorkRecordFamily::Notes,
                    "observations" => WorkRecordFamily::Observations,
                    "gates" => WorkRecordFamily::Gates,
                    _ => {
                        return Err(StoreError::InvalidWorkProjection(
                            "unknown note search family".into(),
                        ));
                    }
                };
                search_budget::record(&self.connection)?;
                matches.insert(
                    id.0,
                    WorkNoteSearchMatch {
                        locator: row.get(1)?,
                        family,
                    },
                );
            }
        }
        drop(rows);
        #[cfg(test)]
        crate::storage::work::cost::sql("note_search_candidates", &statement);
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
        assert!(
            matches!(select_eligible_ids("SELECT total FROM classified"),
            Err(StoreError::InvalidWorkProjection(message)) if message == "note search query lost its classified count selection")
        );
    }
}
