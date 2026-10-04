//! Atomic derivation of the optional stranded-child advisory group.

use super::{DateTime, LocalWorkService, SqliteStore, StoreError, Utc, compact_text_to};
use crate::work_service::views::WorkStrandedChild;

impl LocalWorkService {
    pub(super) fn stranded_children_advice(
        &self,
        store: &SqliteStore,
        now: DateTime<Utc>,
    ) -> Result<(Vec<WorkStrandedChild>, usize), StoreError> {
        let page = store.stranded_work_children(&self.project_id, &self.session_id)?;
        let mut rows = Vec::with_capacity(page.items.len());
        for (child, parent) in page.items {
            let (blocked_reason, remedy) =
                match store.check_work_detach_admission(child.work_id, now) {
                    Ok(()) => (
                        format!(
                            "parent {} is completed; continue as independent work",
                            parent.short_ref
                        ),
                        format!(
                            "engram work update {} --detach \"Continue as independent work\"",
                            child.short_ref
                        ),
                    ),
                    Err(StoreError::WorkDetachRefused { reason, remedy, .. }) => (
                        format!("parent {} is completed; {reason}", parent.short_ref),
                        remedy,
                    ),
                    Err(error) => return Err(error),
                };
            rows.push(WorkStrandedChild {
                work_ref: child.short_ref,
                parent_ref: parent.short_ref,
                child_requirement: child.child_requirement,
                title: compact_text_to(&child.title, 192),
                blocked_reason,
                remedy,
            });
        }
        Ok((rows, page.omitted))
    }
}
#[cfg(test)]
mod tests {
    use crate::work_service::test_support::*;
    use crate::work_service::*;

    #[test]
    fn stranded_query_failure_is_isolated_but_store_open_failure_is_fatal() {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("work.db");
        let service = LocalWorkService::new(
            database.clone(),
            ProjectId("query-boundary".into()),
            "reader".into(),
            SessionId("reader".into()),
            None,
        );
        service
            .work_propose(root_input("Healthy ready work", "healthy"), at(0))
            .unwrap();
        let store = SqliteStore::open(&database).unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        let index_sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'work_items_ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute_batch("DROP INDEX work_items_ready")
            .unwrap();
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Ready, WorkNextSection::Participated],
            ..Default::default()
        };
        // Invoke the advisory on an already-open store: its forced candidate
        // index is missing, whereas independent ready/discovery reads work.
        let advisory = service
            .next_advisory(&store, 20, &query, at(1), None, &mut Vec::new())
            .unwrap();
        assert_eq!(advisory.ready.as_ref().unwrap().len(), 1);
        assert!(advisory.discovery.stranded_children_unavailable);
        assert_eq!(
            advisory.discovery.stranded_children_error_class,
            Some("sqlite_error")
        );
        assert_eq!(advisory.discovery.stranded_children.len(), 0);
        assert_eq!(advisory.discovery.stranded_children_omitted, 0);
        assert_eq!(advisory.discovery.stranded_children_next, None);
        // Opening the same damaged schema is outside that boundary and still
        // fails before either ordinary next or peek can return orientation.
        assert!(
            service
                .work_next_peek_for_agent(20, 20, false, query, at(1), |_| true)
                .is_err()
        );
        connection.execute_batch(&index_sql).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        connection.close().unwrap();
    }
}
