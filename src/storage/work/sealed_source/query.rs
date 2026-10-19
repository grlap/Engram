//! Run-local SQL selection. Counts and independent selectors decode only the
//! bounded requested rows; no whole-root replay or workspace hashing occurs.

use super::{CompletionSeal, ObjectId, SealedSourceDiagnostic, SealedSourceFact, StoreError};
use crate::storage::work::feeds::source_basis_sql;
use rusqlite::{Connection, params_from_iter, types::Value};

pub(super) struct SourceQuery {
    cte: String,
    values: Vec<Value>,
}

impl SourceQuery {
    pub(super) fn new(seal: &CompletionSeal, diagnostic: &SealedSourceDiagnostic) -> Self {
        let binding = |field| format!("json_extract(object.canonical_json, '$.binding.{field}')");
        let revision = source_basis_sql("source_revision");
        let workspace = source_basis_sql("workspace_id");
        let generation = source_basis_sql("source_root_generation");
        let state = source_basis_sql("source_root_state");
        let scope = format!(
            "json_extract(object.canonical_json, '$.project_id') = ?4
             AND {} = ?5 AND {} = ?1 AND {} = ?6 AND {} = ?7 AND {} = ?8
             AND {workspace} = ?9 AND {generation} = ?10 AND {state} = 'named'",
            binding("work_id"),
            binding("run_id"),
            binding("root_execution_id"),
            binding("claim_id"),
            binding("claim_fence"),
        );
        let cte = format!(
            "WITH facts AS (
                SELECT entry.position, entry.object_id,
                  CASE WHEN ?11 IS NULL THEN 'judged_basis_unavailable'
                       WHEN NOT COALESCE(({scope}), 0) THEN 'unrelated_or_missing_basis'
                       WHEN {revision} IS NULL THEN 'closing_sighting_unavailable'
                       WHEN {revision} = ?11 THEN 'agreement'
                       ELSE 'differs' END AS comparison
                FROM work_feed_entries entry
                JOIN objects object ON object.object_id = entry.object_id
                WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                  AND entry.position > ?2 AND entry.position <= ?3
                  AND entry.object_kind IN
                    ('execution_observation', 'unadmitted_execution_observation')
             )"
        );
        let judged = diagnostic.judged.as_ref();
        let values = vec![
            Value::Text(seal.run_id.0.to_string()),
            Value::Integer(diagnostic.completion_cut),
            Value::Integer(diagnostic.horizon),
            Value::Text(seal.root_execution.project_id.0.clone()),
            Value::Text(seal.work_id.0.to_string()),
            Value::Text(seal.root_execution_id.0.to_string()),
            Value::Text(seal.claim_id.0.to_string()),
            Value::Integer(seal.claim_fence),
            judged.map_or(Value::Null, |b| Value::Text(b.workspace_id.clone())),
            judged
                .and_then(|b| b.source_root_generation)
                .map_or(Value::Null, Value::Integer),
            judged.map_or(Value::Null, |b| Value::Text(b.source_revision.clone())),
        ];
        Self { cte, values }
    }

    pub(super) fn count(
        &self,
        connection: &Connection,
        out: &mut SealedSourceDiagnostic,
    ) -> Result<(), StoreError> {
        let mut statement = connection.prepare(&format!(
            "{} SELECT comparison, COUNT(*) FROM facts GROUP BY comparison",
            self.cte,
        ))?;
        let counts = statement.query_map(params_from_iter(&self.values), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for count in counts {
            let (kind, count) = count?;
            let count = usize::try_from(count)
                .map_err(|_| StoreError::InvalidWorkProjection("invalid source count".into()))?;
            out.total += count;
            match kind.as_str() {
                "differs" => out.differences += count,
                "agreement" => out.agreements += count,
                _ => out.incomparable += count,
            }
        }
        Ok(())
    }

    pub(super) fn select(
        &self,
        connection: &Connection,
        before: Option<i64>,
        kind: Option<&str>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<SealedSourceFact>, StoreError> {
        let filter = match kind {
            Some("differs") => "comparison = 'differs'",
            Some("endpoint") => "comparison IN ('differs', 'agreement')",
            _ => "1 = 1",
        };
        let order = if descending { "DESC" } else { "ASC" };
        let mut values = self.values.clone();
        values.push(Value::Integer(before.unwrap_or(i64::MAX)));
        values.push(Value::Integer(i64::try_from(limit).map_err(|_| {
            StoreError::InvalidWorkProjection("source page limit exceeds integer range".into())
        })?));
        let mut statement = connection.prepare(&format!(
            "{} SELECT position, object_id, comparison FROM facts
             WHERE position < ?12 AND {filter} ORDER BY position {order} LIMIT ?13",
            self.cte,
        ))?;
        let rows = statement.query_map(params_from_iter(&values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (position, id, comparison) = row?;
            Ok(SealedSourceFact {
                position,
                observation: ObjectId::from_stored(id.clone())
                    .ok_or(StoreError::InvalidStoredKey(id))?,
                comparison,
                source_basis: None,
            })
        })
        .collect()
    }

    pub(super) fn newer(
        &self,
        connection: &Connection,
        boundary: i64,
    ) -> Result<usize, StoreError> {
        let mut values = self.values.clone();
        values.push(Value::Integer(boundary));
        let count: i64 = connection.query_row(
            &format!(
                "{} SELECT COUNT(*) FROM facts WHERE position >= ?12",
                self.cte,
            ),
            params_from_iter(&values),
            |row| row.get(0),
        )?;
        usize::try_from(count)
            .map_err(|_| StoreError::InvalidWorkProjection("invalid source count".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_source_scope_checks_each_original_binding_component() {
        let fixture = crate::storage::sealed_source_transport_fixture("difference");
        let store = crate::SqliteStore::open(&fixture.database).unwrap();
        let seal = store.completion_seal_at(&fixture.seal).unwrap();
        let original = store
            .sealed_source_page(&fixture.seal, None, None, 0)
            .unwrap()
            .diagnostic;
        for component in [
            "project",
            "work",
            "root",
            "claim",
            "fence",
            "workspace",
            "generation",
            "judged",
        ] {
            let mut seal = seal.clone();
            let mut diagnostic = original.clone();
            diagnostic.total = 0;
            diagnostic.differences = 0;
            diagnostic.agreements = 0;
            diagnostic.incomparable = 0;
            match component {
                "project" => seal.root_execution.project_id.0.push_str("-other"),
                "work" => seal.work_id = crate::WorkId(uuid::Uuid::new_v4()),
                "root" => seal.root_execution_id = crate::RootExecutionId(uuid::Uuid::new_v4()),
                "claim" => seal.claim_id = crate::WorkClaimId(uuid::Uuid::new_v4()),
                "fence" => seal.claim_fence += 1,
                "workspace" => diagnostic
                    .judged
                    .as_mut()
                    .unwrap()
                    .workspace_id
                    .push_str("-other"),
                "generation" => diagnostic.judged.as_mut().unwrap().source_root_generation = None,
                "judged" => diagnostic.judged = None,
                _ => unreachable!(),
            }
            let query = SourceQuery::new(&seal, &diagnostic);
            query.count(&store.connection, &mut diagnostic).unwrap();
            assert_eq!(diagnostic.total, 40, "{component}");
            assert_eq!(diagnostic.agreements, 0, "{component}");
            assert_eq!(diagnostic.differences, 0, "{component}");
            assert_eq!(diagnostic.incomparable, 40, "{component}");
        }
    }
}
