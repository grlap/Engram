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
    pub(super) fn new(
        seal: &CompletionSeal,
        diagnostic: &SealedSourceDiagnostic,
        judged: Option<&crate::domain::VerificationSourceBasis>,
    ) -> Result<Self, StoreError> {
        let binding = |field| format!("json_extract(object.canonical_json, '$.binding.{field}')");
        let revision = source_basis_sql("source_revision");
        let source_id = source_basis_sql("source_id");
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
                  CASE WHEN ?11 IS NULL AND ?12 IS NULL THEN 'judged_basis_unavailable'
                       WHEN NOT COALESCE(({scope}), 0) THEN 'unrelated_or_missing_basis'
                       WHEN {revision} IS NULL AND {source_id} IS NULL THEN 'closing_sighting_unavailable'
                       WHEN (?11 IS NOT NULL AND {revision} = ?11 AND {source_id} IS NULL)
                         OR (?12 IS NOT NULL AND {source_id} = ?12 AND {revision} IS NULL) THEN 'agreement'
                       ELSE 'differs' END AS comparison
                FROM work_feed_entries entry
                JOIN objects object ON object.object_id = entry.object_id
                WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                  AND entry.position > ?2 AND entry.position <= ?3
                  AND entry.object_kind IN
                    ('execution_observation', 'unadmitted_execution_observation')
             )"
        );
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
            judged
                .and_then(|b| b.legacy_revision())
                .map_or(Value::Null, |v| Value::Text(v.to_owned())),
            judged
                .and_then(|b| b.source_id())
                .map(crate::canonical::canonical_bytes)
                .transpose()?
                .map_or(Value::Null, |bytes| {
                    Value::Text(String::from_utf8_lossy(&bytes).into_owned())
                }),
        ];
        Ok(Self { cte, values })
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
             WHERE position < ?13 AND {filter} ORDER BY position {order} LIMIT ?14",
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
                "{} SELECT COUNT(*) FROM facts WHERE position >= ?13",
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
    fn shared_sealed_source_counts_compare_the_complete_canonical_identity() {
        let mut fixture = crate::storage::sealed_source_transport_fixture("missing");
        let source = crate::source::SourceId {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            submodules: vec![],
            changes: vec![crate::source::Change {
                path: "source-\"quoted\".rs".into(),
                status: " M".into(),
                origin: None,
                blob: Some("c".repeat(40)),
                mode: Some(crate::source::Mode::Regular),
            }],
        };
        fixture.append_shared_sighting(source.clone(), "shared-equal");
        let mut different = source.clone();
        different.changes[0].blob = Some("d".repeat(40));
        fixture.append_shared_sighting(different, "shared-different");
        let store = crate::SqliteStore::open(&fixture.database).unwrap();
        let seal = store.completion_seal_at(&fixture.seal).unwrap();
        let mut diagnostic = store
            .sealed_source_page(&fixture.seal, None, None, 0)
            .unwrap()
            .diagnostic;
        let judged = crate::domain::VerificationSourceBasis {
            workspace_id: "workspace-B".into(),
            identity: crate::domain::VerificationSourceIdentity::SourceId { source_id: source },
            source_root_generation: Some(9),
            source_root_state: Some(crate::domain::SourceRootState::Named),
        };
        diagnostic.judged = Some(super::super::SealedSourceBasis::from(&judged));
        diagnostic.total = 0;
        diagnostic.differences = 0;
        diagnostic.agreements = 0;
        diagnostic.incomparable = 0;
        let query = SourceQuery::new(&seal, &diagnostic, Some(&judged)).unwrap();
        query.count(&store.connection, &mut diagnostic).unwrap();
        assert_eq!(diagnostic.total, 2);
        assert_eq!(diagnostic.agreements, 1);
        assert_eq!(diagnostic.differences, 1);
        assert_eq!(diagnostic.incomparable, 0);
        assert_eq!(
            query
                .select(&store.connection, None, Some("differs"), false, 1)
                .unwrap()
                .len(),
            1
        );
        let projected = serde_json::to_string(&diagnostic).unwrap();
        assert!(!projected.contains("quoted"));
        assert!(projected.contains("source_id_summary"));
    }

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
            let judged =
                diagnostic
                    .judged
                    .as_ref()
                    .map(|basis| crate::domain::VerificationSourceBasis {
                        workspace_id: basis.workspace_id.clone(),
                        identity: crate::domain::VerificationSourceIdentity::Legacy {
                            source_revision: basis.source_revision.clone().unwrap(),
                            git_tree: None,
                        },
                        source_root_generation: basis.source_root_generation,
                        source_root_state: basis.source_root_state,
                    });
            let query = SourceQuery::new(&seal, &diagnostic, judged.as_ref()).unwrap();
            query.count(&store.connection, &mut diagnostic).unwrap();
            assert_eq!(diagnostic.total, 40, "{component}");
            assert_eq!(diagnostic.agreements, 0, "{component}");
            assert_eq!(diagnostic.differences, 0, "{component}");
            assert_eq!(diagnostic.incomparable, 40, "{component}");
        }
    }
}
