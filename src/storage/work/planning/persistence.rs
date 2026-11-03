use serde::Serialize;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

#[cfg(test)]
use super::WORK_CATALOG_FTS_DELETE_STATEMENTS;
use super::{
    Connection, ObjectId, RootExecution, StoreError, Transaction, WorkBlocker, WorkClaim, WorkItem,
    WorkRun, catalog_fts, params,
};

pub(in crate::storage) fn persist_operation_result<T: Serialize>(
    transaction: &Transaction<'_>,
    operation: &str,
    key: &str,
    request_hash: &ObjectId,
    result: &T,
) -> Result<(), StoreError> {
    crate::storage::work::receipts::admit_core_receipt(operation)?;
    let receipt = serde_json::to_vec(result)?;
    #[cfg(test)]
    crate::storage::work::receipts::assert_core_receipt_decodes(operation, &receipt);
    transaction.execute(
        "INSERT INTO work_operation_results (
             operation, idempotency_key, request_hash, result_json
         ) VALUES (?1, ?2, ?3, ?4)",
        params![operation, key, request_hash.as_str(), receipt],
    )?;
    Ok(())
}

pub(in crate::storage) fn encode_state<T: Serialize>(value: T) -> Result<String, StoreError> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(str::to_owned).ok_or_else(|| {
        StoreError::InvalidWorkProjection("enum did not serialize as a string".into())
    })
}

pub(in crate::storage::work) fn normalize_work_catalog_key(value: &str) -> String {
    let nfc = value.trim().nfc().collect::<String>();
    nfc.as_str().case_fold().collect::<String>().nfc().collect()
}

pub(in crate::storage::work) fn work_catalog_search_text(
    connection: &Connection,
    item: &WorkItem,
) -> Result<String, StoreError> {
    let mut parts = vec![
        item.short_ref.clone(),
        item.title.clone(),
        item.outcome.clone(),
    ];
    parts.extend(item.labels.iter().cloned());
    parts.extend(item.external_ref.iter().cloned());
    let mut statement = connection.prepare(
        "SELECT blocker_json FROM work_blockers
         WHERE work_id = ?1 AND state = 'active' ORDER BY blocker_id",
    )?;
    let blocker_details = statement
        .query_map([item.work_id.0.to_string()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| {
            serde_json::from_slice::<WorkBlocker>(&row?)
                .map(|blocker| blocker.detail)
                .map_err(StoreError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    parts.extend(blocker_details);
    Ok(normalize_work_catalog_key(&parts.join("\n")))
}

pub(in crate::storage) fn refresh_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    write_work_catalog_projection(connection, item, CatalogProjectionMode::Refresh)
}

pub(super) fn initialize_new_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    // Creation has just minted this UUID and inserted its work item.
    write_work_catalog_projection(connection, item, CatalogProjectionMode::Create)
}

#[derive(Clone, Copy)]
enum CatalogProjectionMode {
    Create,
    Refresh,
    Restore,
}

fn write_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
    mode: CatalogProjectionMode,
) -> Result<(), StoreError> {
    let assigned_to_key = item.assigned_to.as_deref().map(normalize_work_catalog_key);
    let search_text_key = work_catalog_search_text(connection, item)?;
    let changed = connection.execute(
        "UPDATE work_items
         SET assigned_to_key = ?2, search_text_key = ?3
         WHERE work_id = ?1",
        params![item.work_id.0.to_string(), assigned_to_key, search_text_key],
    )?;
    if changed != 1 {
        let action = if matches!(mode, CatalogProjectionMode::Restore) {
            "initialization"
        } else {
            "refresh"
        };
        return Err(StoreError::InvalidWorkProjection(format!(
            "work catalog {action} lost item {:?}",
            item.work_id
        )));
    }
    // Restore must not silently clear unexpected destination projection rows.
    if !matches!(mode, CatalogProjectionMode::Restore) {
        connection.execute(
            "DELETE FROM work_item_labels WHERE work_id = ?1",
            [item.work_id.0.to_string()],
        )?;
    }
    let mut label_keys = item
        .labels
        .iter()
        .map(|label| normalize_work_catalog_key(label))
        .collect::<Vec<_>>();
    label_keys.sort();
    label_keys.dedup();
    for label_key in label_keys {
        connection.execute(
            "INSERT INTO work_item_labels (work_id, label_key) VALUES (?1, ?2)",
            params![item.work_id.0.to_string(), label_key],
        )?;
    }
    if matches!(mode, CatalogProjectionMode::Refresh) {
        #[cfg(test)]
        WORK_CATALOG_FTS_DELETE_STATEMENTS.with(|count| count.set(count.get().saturating_add(1)));
        catalog_fts::remove_prior_rows(connection, item)?;
    }
    connection.execute(
        "INSERT INTO work_catalog_fts (work_id, search_text) VALUES (?1, ?2)",
        params![item.work_id.0.to_string(), search_text_key],
    )?;
    Ok(())
}

/// Initializes the catalog for a newly restored item. Snapshot loading has
/// already established an empty destination and checked incoming ids against
/// existing FTS rows, so replacing an FTS row here would perform an
/// unnecessary full scan of the unindexed `work_id` column.
pub(in crate::storage) fn initialize_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    write_work_catalog_projection(connection, item, CatalogProjectionMode::Restore)
}

pub(in crate::storage::work) fn persist_work_item(
    transaction: &Transaction<'_>,
    item: &WorkItem,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_items SET
             lifecycle = ?2, priority = ?3, assigned_to = ?4,
             deferred_until_ms = ?5, revision = ?6, active_run_id = ?7,
             superseded_by = ?8, updated_at_ms = ?9, item_json = ?10
         WHERE work_id = ?1",
        params![
            item.work_id.0.to_string(),
            encode_state(item.lifecycle)?,
            item.priority,
            item.assigned_to,
            item.deferred_until.map(|value| value.timestamp_millis()),
            item.revision,
            item.active_run_id.map(|value| value.0.to_string()),
            item.superseded_by.map(|value| value.0.to_string()),
            item.updated_at.timestamp_millis(),
            serde_json::to_vec(item)?
        ],
    )?;
    refresh_work_catalog_projection(transaction, item)
}

pub(in crate::storage::work) fn persist_work_run(
    transaction: &Transaction<'_>,
    run: &WorkRun,
    claim_fence_head: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_runs SET
             executor_session_id = ?2, state = ?3, revision = ?4,
             claim_fence_head = ?5, last_checkpoint_id = ?6,
             completion_seal_id = ?7, updated_at_ms = ?8, run_json = ?9
         WHERE run_id = ?1",
        params![
            run.run_id.0.to_string(),
            run.executor.as_ref().map(|value| value.0.as_str()),
            encode_state(run.state)?,
            run.revision,
            claim_fence_head,
            run.last_checkpoint.as_ref().map(ObjectId::as_str),
            run.completion_seal.as_ref().map(ObjectId::as_str),
            run.updated_at.timestamp_millis(),
            serde_json::to_vec(run)?
        ],
    )?;
    Ok(())
}

pub(in crate::storage::work) fn persist_claim(
    transaction: &Transaction<'_>,
    claim: &WorkClaim,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO work_claims (
             run_id, work_id, claim_id, holder_session_id, state,
             expires_at_ms, revision, fence, claim_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(run_id) DO UPDATE SET
             claim_id = excluded.claim_id,
             holder_session_id = excluded.holder_session_id,
             state = excluded.state,
             expires_at_ms = excluded.expires_at_ms,
             revision = excluded.revision,
             fence = excluded.fence,
             claim_json = excluded.claim_json",
        params![
            claim.run_id.0.to_string(),
            claim.work_id.0.to_string(),
            claim.claim_id.0.to_string(),
            claim.holder.0,
            encode_state(claim.state)?,
            claim.expires_at.timestamp_millis(),
            claim.revision,
            claim.fence,
            serde_json::to_vec(claim)?
        ],
    )?;
    Ok(())
}

pub(in crate::storage::work) fn persist_root_execution(
    transaction: &Transaction<'_>,
    execution: &RootExecution,
) -> Result<(), StoreError> {
    crate::storage::work::root_state::persist(transaction, execution)
}
