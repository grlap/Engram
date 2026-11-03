//! Catalog, scalar, canonical-row and protocol-attempt checks.

use super::{
    CanonicalObject, CompletionSeal, Connection, DeserializeOwned, HashSet, ObjectId, Serialize,
    StoreError, WorkClaim, WorkHandoffOffer, WorkItem, WorkRun, decode_failure_label,
    decode_projection_bytes, load_typed_work_object, normalize_work_catalog_key,
    preserves_projection_representation, validate_work_protocol_result_binding,
    work_catalog_search_text,
};

pub(in crate::storage::work) fn verify_work_catalog_projections(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    // The catalog's stored search text is read once, every row kept, rather
    // than looked up per item through its unindexed work id.
    let content = crate::storage::fts_verification::fts_content(
        connection,
        "SELECT work_id, search_text FROM work_catalog_fts",
        |row| crate::storage::fts_verification::text(row, 1),
    )?;
    let mut work_ids = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT work_id, item_json, assigned_to_key, search_text_key
         FROM work_items ORDER BY work_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (work_id, item_json, assigned_to_key, search_text_key) = row?;
        *checked += 1;
        // Every item owns its catalog row, one that does not decode included.
        work_ids.insert(work_id.clone());
        let item = match serde_json::from_slice::<WorkItem>(&item_json) {
            Ok(item) => item,
            Err(error) => {
                invalid.push(decode_failure_label(
                    format!("work_catalog:{work_id}:item_decode"),
                    &error.into(),
                ));
                continue;
            }
        };
        let expected_assigned = item.assigned_to.as_deref().map(normalize_work_catalog_key);
        let expected_search = work_catalog_search_text(connection, &item)?;
        let mut expected_labels = item
            .labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<Vec<_>>();
        expected_labels.sort();
        expected_labels.dedup();
        let mut label_statement = connection.prepare(
            "SELECT label_key FROM work_item_labels
             WHERE work_id = ?1 ORDER BY label_key",
        )?;
        let actual_labels = label_statement
            .query_map([work_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        // An unreadable catalog is reported once below, never as every
        // item's row gone missing.
        let fts_bound = content.as_ref().map_or(true, |content| {
            content.get(&Some(work_id.clone())).map(Vec::as_slice)
                == Some(std::slice::from_ref(&Some(expected_search.clone())))
        });
        if item.work_id.0.to_string() != work_id
            || assigned_to_key != expected_assigned
            || search_text_key != expected_search
            || actual_labels != expected_labels
            || !fts_bound
        {
            invalid.push(format!("work_catalog:{work_id}:projection_binding"));
        }
    }
    drop(statement);
    match &content {
        Ok(content) => {
            if content
                .keys()
                .any(|work_id| work_id.as_ref().is_none_or(|id| !work_ids.contains(id)))
            {
                invalid.push("work_catalog:orphaned_fts_rows".into());
            }
        }
        Err(detail) => invalid.push(crate::storage::fts_verification::bounded_finding(
            "work_catalog:fts_content",
            detail,
        )),
    }
    // SQLite's FTS5 xIntegrity checks every posting against the table content.
    // A single read-only pass also covers malformed segments and rows with no
    // searchable trigram, which a per-item MATCH query cannot exercise.
    if let Some(finding) = crate::storage::fts_verification::fts_index_finding(
        connection,
        "work_catalog_fts",
        "work_catalog:fts_index",
    )? {
        invalid.push(finding);
    }
    Ok(())
}

pub(in crate::storage::work) fn verify_work_scalar_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let checks = [
        (
            "work_item",
            "SELECT item.work_id FROM work_items item WHERE
             work_id != json_extract(item_json, '$.work_id') OR
             project_id != json_extract(item_json, '$.project_id') OR
             short_ref != json_extract(item_json, '$.short_ref') OR
             root_id != json_extract(item_json, '$.root_id') OR
             COALESCE(parent_id, '') != COALESCE(json_extract(item_json, '$.parent_id'), '') OR
             child_requirement != json_extract(item_json, '$.child_requirement') OR
             lifecycle != json_extract(item_json, '$.lifecycle') OR
             priority != json_extract(item_json, '$.priority') OR
             COALESCE(assigned_to, '') != COALESCE(json_extract(item_json, '$.assigned_to'), '') OR
             revision != json_extract(item_json, '$.revision') OR
             COALESCE(active_run_id, '') != COALESCE(json_extract(item_json, '$.active_run_id'), '') OR
             COALESCE(source_snapshot_id, '') != COALESCE(json_extract(item_json, '$.source_snapshot_id'), '')",
        ),
        (
            "work_run",
            "SELECT run_id FROM work_runs WHERE
             run_id != json_extract(run_json, '$.run_id') OR
             root_execution_id != json_extract(run_json, '$.root_execution_id') OR
             work_id != json_extract(run_json, '$.work_id') OR
             generation != json_extract(run_json, '$.generation') OR
             COALESCE(executor_session_id, '') != COALESCE(json_extract(run_json, '$.executor'), '') OR
             state != json_extract(run_json, '$.state') OR
             revision != json_extract(run_json, '$.revision') OR
             COALESCE(last_checkpoint_id, '') != COALESCE(json_extract(run_json, '$.last_checkpoint'), '') OR
             COALESCE(completion_seal_id, '') != COALESCE(json_extract(run_json, '$.completion_seal'), '')",
        ),
        (
            "work_root_execution",
            "SELECT root_execution_id FROM work_root_executions WHERE
             root_execution_id != json_extract(header_json, '$.root_execution_id') OR
             project_id != json_extract(header_json, '$.project_id') OR
             root_id != json_extract(header_json, '$.root_id') OR
             generation != json_extract(header_json, '$.generation') OR
             state != json_extract(header_json, '$.state') OR
             revision != json_extract(header_json, '$.revision')",
        ),
        (
            "work_claim",
            "SELECT run_id FROM work_claims WHERE
             run_id != json_extract(claim_json, '$.run_id') OR
             work_id != json_extract(claim_json, '$.work_id') OR
             claim_id != json_extract(claim_json, '$.claim_id') OR
             holder_session_id != json_extract(claim_json, '$.holder') OR
             state != json_extract(claim_json, '$.state') OR
             revision != json_extract(claim_json, '$.revision') OR
             fence != json_extract(claim_json, '$.fence')",
        ),
        (
            "work_handoff_offer",
            "SELECT offer_id FROM work_handoff_offers WHERE
             offer_object_id IS NULL OR
             offer_id != json_extract(offer_json, '$.offer_id') OR
             run_id != json_extract(offer_json, '$.run_id') OR
             work_id != json_extract(offer_json, '$.work_id') OR
             state != json_extract(offer_json, '$.state')",
        ),
        (
            "work_blocker",
            "SELECT blocker_id FROM work_blockers WHERE
             blocker_id != json_extract(blocker_json, '$.blocker_id') OR
             work_id != json_extract(blocker_json, '$.work_id')",
        ),
    ];
    for (kind, sql) in checks {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            *checked += 1;
            invalid.push(format!("{kind}:{}:scalar_binding", row?));
        }
    }

    let mut statement = connection.prepare(
        "SELECT item.work_id FROM work_items item WHERE
         COALESCE(item.latest_event_id, '') != COALESCE((
             SELECT entry.object_id FROM work_feed_entries entry
             WHERE entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = item.work_id
             ORDER BY entry.position DESC LIMIT 1
         ), '')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        *checked += 1;
        invalid.push(format!("work_item:{}:latest_event_id", row?));
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT work_id, deferred_until_ms, superseded_by, created_at_ms, updated_at_ms, item_json
         FROM work_items ORDER BY work_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, deferred, superseded_by, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkItem>(&bytes).is_ok_and(|item| {
            deferred == item.deferred_until.map(|value| value.timestamp_millis())
                && superseded_by == item.superseded_by.map(|value| value.0.to_string())
                && created_at == item.created_at.timestamp_millis()
                && updated_at == item.updated_at.timestamp_millis()
        });
        if !valid {
            invalid.push(format!("work_item:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT run.run_id, run.claim_fence_head, claim.fence,
                run.created_at_ms, run.updated_at_ms, run.run_json
         FROM work_runs run
         LEFT JOIN work_claims claim ON claim.run_id = run.run_id
         ORDER BY run.run_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, fence_head, claim_fence, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkRun>(&bytes).is_ok_and(|run| {
            fence_head == claim_fence.unwrap_or(0)
                && created_at == run.created_at.timestamp_millis()
                && updated_at == run.updated_at.timestamp_millis()
        });
        if !valid {
            invalid.push(format!("work_run:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT root_execution_id, created_at_ms, updated_at_ms, header_json
         FROM work_root_executions ORDER BY root_execution_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    for row in rows {
        let (id, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<crate::domain::RootExecutionHeader>(&bytes).is_ok_and(
            |execution| {
                created_at == execution.created_at.timestamp_millis()
                    && updated_at == execution.updated_at.timestamp_millis()
            },
        );
        if !valid {
            invalid.push(format!("work_root_execution:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection
        .prepare("SELECT run_id, expires_at_ms, claim_json FROM work_claims ORDER BY run_id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, expires_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkClaim>(&bytes)
            .is_ok_and(|claim| expires_at == claim.expires_at.timestamp_millis());
        if !valid {
            invalid.push(format!("work_claim:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT offer_id, expires_at_ms, offer_json
         FROM work_handoff_offers ORDER BY offer_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, expires_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkHandoffOffer>(&bytes)
            .is_ok_and(|offer| expires_at == offer.expires_at.timestamp_millis());
        if !valid {
            invalid.push(format!("work_handoff_offer:{id}:extended_scalar_binding"));
        }
    }
    Ok(())
}

pub(in crate::storage::work) fn verify_canonical_work_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let projections = [
        (
            "completion_seal",
            "SELECT projection.seal_id, projection.seal_json,
                    object.object_kind, object.canonical_json
             FROM work_completion_seals projection
             LEFT JOIN objects object ON object.object_id = projection.seal_id
             ORDER BY projection.seal_id",
            typed_projection_bytes_equal::<CompletionSeal> as fn(&[u8], &[u8]) -> bool,
            canonical_side_finding::<CompletionSeal> as fn(&[u8], &[u8]) -> Option<bool>,
        ),
        (
            "work_handoff_offer",
            "SELECT projection.offer_object_id, projection.offer_json,
                    object.object_kind, object.canonical_json
             FROM work_handoff_offers projection
             LEFT JOIN objects object ON object.object_id = projection.offer_object_id
             ORDER BY projection.offer_id",
            typed_projection_bytes_equal::<WorkHandoffOffer> as fn(&[u8], &[u8]) -> bool,
            canonical_side_finding::<WorkHandoffOffer> as fn(&[u8], &[u8]) -> Option<bool>,
        ),
    ];
    for (kind, sql, equivalent, canonical_side) in projections {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?;
        for row in rows {
            let (stored_hash, projection, object_kind, canonical) = row?;
            *checked += 1;
            let valid = match (
                ObjectId::from_stored(stored_hash.clone()),
                canonical.as_ref(),
            ) {
                (Some(hash), Some(bytes)) => {
                    CanonicalObject::stored(&hash, bytes.clone()).is_ok()
                        && object_kind.as_deref() == Some(kind)
                        && equivalent(&projection, bytes)
                }
                _ => false,
            };
            if !valid {
                // A canonical side that does not survive its own type is named
                // apart; a projection that fails its own guard or differs
                // from what that side decodes to is still reported beside it.
                let finding = canonical
                    .as_deref()
                    .filter(|_| object_kind.as_deref() == Some(kind))
                    .and_then(|bytes| canonical_side(&projection, bytes));
                if finding.is_some() {
                    invalid.push(format!("{kind}:{stored_hash}:canonical_representation"));
                }
                if finding != Some(false) {
                    invalid.push(format!("{kind}:{stored_hash}"));
                }
            }
        }
    }
    Ok(())
}

/// For stored canonical bytes whose representation is lost, whether the
/// projection bytes also fail: they fail their own guard or differ from
/// what the canonical bytes decode to. `None` when the canonical
/// representation is not lost.
fn canonical_side_finding<T: DeserializeOwned + Serialize + PartialEq>(
    projection: &[u8],
    canonical: &[u8],
) -> Option<bool> {
    let decoded = serde_json::from_slice::<T>(canonical).ok()?;
    let stored = serde_json::from_slice::<serde_json::Value>(canonical).ok()?;
    if preserves_projection_representation(&stored, &decoded) {
        return None;
    }
    Some(decode_projection_bytes::<T>(projection).as_ref() != Some(&decoded))
}

// Malformed input never equals malformed input, even when both fail decoding.
fn typed_projection_bytes_equal<T: DeserializeOwned + Serialize + PartialEq>(
    projected: &[u8],
    canonical: &[u8],
) -> bool {
    matches!((decode_projection_bytes::<T>(projected), decode_projection_bytes::<T>(canonical)),
        (Some(projected), Some(canonical)) if projected == canonical)
}

pub(in crate::storage::work) fn verify_work_protocol_attempts(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT project_id, session_id, operation, idempotency_key,
                request_hash, basis_hash, basis_json, result_id, result_json
         FROM work_protocol_attempts
         ORDER BY project_id, session_id, operation, idempotency_key",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<Vec<u8>>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<Vec<u8>>>(8)?,
        ))
    })?;
    for row in rows {
        let (
            project_id,
            session_id,
            operation,
            key,
            request_hash,
            basis_hash,
            basis_json,
            result_id,
            result_json,
        ) = row?;
        *checked += 1;
        let label = format!("work_protocol_attempt:{project_id}:{session_id}:{operation}:{key}");
        let request_valid = ObjectId::from_stored(request_hash).is_some();
        let basis_valid = match (&basis_hash, &basis_json, &result_id, &result_json) {
            (Some(stored_hash), Some(bytes), _, _) => ObjectId::from_stored(stored_hash.clone())
                .is_some_and(|hash| CanonicalObject::stored(&hash, bytes.clone()).is_ok()),
            (stored_hash, None, Some(_), Some(_)) => stored_hash
                .as_ref()
                .is_none_or(|hash| ObjectId::from_stored(hash.clone()).is_some()),
            _ => false,
        };
        let result_valid = match (result_id, &result_json) {
            (None, None) => true,
            (Some(stored_hash), Some(bytes)) => ObjectId::from_stored(stored_hash)
                .and_then(|hash| {
                    load_typed_work_object::<serde_json::Value>(
                        connection,
                        &hash,
                        "work_protocol_result",
                    )
                    .ok()
                    .map(|value| (hash, value))
                })
                .is_some_and(|(_, value)| {
                    serde_json_canonicalizer::to_vec(&value).is_ok_and(|canonical| {
                        &canonical == bytes
                            && validate_work_protocol_result_binding(
                                connection,
                                &project_id,
                                &operation,
                                &value,
                            )
                            .is_ok()
                    })
                }),
            _ => false,
        };
        // The decoder's reason is kept even when an earlier check also
        // fails: a result without its receipt fails the binding check too.
        match crate::storage::work::receipts::protocol_result_problem(
            &operation,
            result_json.as_deref(),
        ) {
            Some(reason) => invalid.push(format!("{label}:{reason}")),
            None if !request_valid || !basis_valid || !result_valid => invalid.push(label),
            None => {}
        }
    }
    Ok(())
}
