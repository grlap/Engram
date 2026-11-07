//! Evidence projections and restored-evidence integrity checks.

use super::{
    CanonicalObject, Connection, EnvironmentEvidence, EvidenceProjectionRow, HashMap, HashSet,
    ObjectId, RestoredWorkEvidence, SCHEMA_VERSION, StoreError, VerificationEvidence, WorkId,
    WorkItem, encode_state, load_typed_work_object, normalize_gate_evidence_input, params,
    run_feed_position_for_object_on,
};

pub(in crate::storage::work) fn expected_verification_projection(
    connection: &Connection,
    evidence_id: &ObjectId,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<VerificationEvidence>(
        connection,
        evidence_id,
        "verification_evidence",
    )?;
    let basis = &evidence.source_basis;
    let shared = basis.source_id().is_some();
    let producer = crate::storage::work::load_verification_producer_on(connection, &evidence)?;
    if shared
        && (evidence.bound_from.is_some()
            || evidence.environment.is_some()
            || evidence.applicability.is_some()
            || producer.registered_source_id_basis(&evidence).is_none())
    {
        return Err(StoreError::InvalidWorkProjection(
            "shared-input verification requires its native registered producer".into(),
        ));
    }
    // The check ran under the original's binding and basis. A bound record
    // carries the original's facts unchanged and its own run, generation,
    // binder and binding time; it is checked against its original here.
    let (check_binding, check_basis, original_matches) = match &evidence.bound_from {
        None => (evidence.binding.clone(), basis.clone(), true),
        Some(source) => {
            let original = load_typed_work_object::<VerificationEvidence>(
                connection,
                &source.verification,
                "verification_evidence",
            )?;
            let original_position =
                run_feed_position_for_object_on(connection, source.run_id, &source.verification)
                    .map(|position| position.position)
                    .ok();
            let original_basis = original.source_basis.legacy_basis().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "bound verification changed source representation".into(),
                )
            })?;
            let basis = basis.legacy_basis().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "bound verification requires a legacy source basis".into(),
                )
            })?;
            let matches = original.bound_from.is_none()
                && original.result == crate::domain::VerificationResult::Passed
                && original.project_id == evidence.project_id
                && original.binding.work_id == source.work_id
                && original.binding.run_id == source.run_id
                && original_position == Some(source.original_position)
                && original.producer_observation == evidence.producer_observation
                && original.check_kind == evidence.check_kind
                && original.applicability == evidence.applicability
                && original.check_fingerprint == evidence.check_fingerprint
                && original.result == evidence.result
                && original.completed_at == evidence.completed_at
                && original.environment == evidence.environment
                && original_basis.workspace_id == basis.workspace_id
                && original_basis.source_revision == basis.source_revision
                && original_basis.source_root_state == Some(crate::domain::SourceRootState::Named)
                && basis.source_root_generation.is_some()
                && basis.source_root_state == Some(crate::domain::SourceRootState::Named)
                && source.measurement.workspace_id == basis.workspace_id
                && source.measurement.source_revision == basis.source_revision
                && original.summary == evidence.summary
                && original.refs == evidence.refs
                && source.criteria.first().is_none_or(|first| *first >= 1)
                && source.criteria.windows(2).all(|pair| pair[0] < pair[1])
                && (source.work_id, source.run_id)
                    != (evidence.binding.work_id, evidence.binding.run_id)
                && bound_sighting_matches(connection, evidence_id, &evidence, source)?;
            (original.binding, original.source_basis, matches)
        }
    };
    let environment_matches = if let Some(environment_hash) = &evidence.environment {
        expected_environment_projection(connection, environment_hash)?;
        let environment = load_typed_work_object::<EnvironmentEvidence>(
            connection,
            environment_hash,
            "environment_evidence",
        )?;
        environment.project_id == evidence.project_id
            && environment.binding.root_execution_id == check_binding.root_execution_id
            && environment.binding.work_id == check_binding.work_id
            && environment.binding.run_id == check_binding.run_id
            && Some(environment.source_basis.source_revision.as_str())
                == check_basis.legacy_revision()
    } else {
        true
    };
    let run_id = evidence.binding.run_id.0.to_string();
    let result_matches = matches!(
        (producer.outcome, evidence.result),
        (
            crate::domain::ExecutionOutcome::Succeeded,
            crate::domain::VerificationResult::Passed
        ) | (
            crate::domain::ExecutionOutcome::Failed,
            crate::domain::VerificationResult::Failed
        ) | (
            crate::domain::ExecutionOutcome::Unknown,
            crate::domain::VerificationResult::Indeterminate
        )
    );
    let producer_session_matches =
        evidence.bound_from.is_some() || producer.session_id == evidence.session_id;
    let bound = evidence.schema_version == SCHEMA_VERSION
        && original_matches
        && crate::control::check_verification_applicability(
            evidence.applicability.as_ref(),
            check_basis.legacy_basis().as_ref(),
        )
        .is_ok()
        && producer.project_id == evidence.project_id
        && producer.binding == check_binding
        && producer_session_matches
        && producer.source_basis.as_ref() == Some(&check_basis)
        && producer.observed_at == Some(evidence.completed_at)
        && producer.action_fingerprint == evidence.check_fingerprint
        && result_matches
        && (producer.run_outcome.is_some() || evidence.completed_at <= evidence.recorded_at)
        && producer.recorded_at <= evidence.recorded_at
        && evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(run_id.as_str())
        && environment_matches;
    if !bound {
        return Err(StoreError::InvalidWorkProjection(format!(
            "verification evidence {evidence_id} is not bound to its producer observation"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "verification".into(),
        workspace_id: Some(basis.workspace_id.clone()),
        source_revision: basis.legacy_revision().map(str::to_owned),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_id: Some(evidence.producer_observation.to_string()),
        check_fingerprint: Some(evidence.check_fingerprint.to_string()),
        verification_result: Some(encode_state(evidence.result)?),
        observed_at_ms: Some(evidence.completed_at.timestamp_millis()),
        environment_fingerprint: None,
        environment_evidence_id: evidence.environment.map(|hash| hash.to_string()),
        components_json: None,
    })
}

/// Whether a bound record stood, when it was written, on what the bind
/// requires on the target's run: the claim's named root in the record's
/// workspace and generation, whose newest sighting just before the record is
/// the one it names, at its revision, with no change of unknown place after
/// that sighting.
fn bound_sighting_matches(
    connection: &Connection,
    evidence_id: &ObjectId,
    evidence: &VerificationEvidence,
    source: &crate::domain::BoundVerificationSource,
) -> Result<bool, StoreError> {
    let Ok(position) =
        run_feed_position_for_object_on(connection, evidence.binding.run_id, evidence_id)
    else {
        return Ok(false);
    };
    let Some(root) = crate::storage::work::completion::named_root_at_cut_on(
        connection,
        evidence.binding.run_id,
        evidence.binding.claim_id,
        position.position - 1,
    )?
    else {
        return Ok(false);
    };
    let Some(basis) = evidence.source_basis.legacy_basis() else {
        return Ok(false);
    };
    let Some((sighting_position, sighting)) = &root.latest_sighting else {
        return Ok(false);
    };
    Ok(root.workspace_id == basis.workspace_id
        && Some(root.generation) == basis.source_root_generation
        && sighting.record == source.sighting
        && sighting.source_basis.as_ref().is_some_and(|sighted| {
            sighted.workspace_id == basis.workspace_id
                && sighted.source_root_generation == basis.source_root_generation
                && sighted.source_root_state == Some(crate::domain::SourceRootState::Named)
                && sighted.source_revision == basis.source_revision
                && sighted.git_tree == basis.git_tree
                && crate::control::check_verification_applicability(
                    evidence.applicability.as_ref(),
                    Some(sighted),
                )
                .is_ok()
        })
        && root
            .unknown_change_position
            .is_none_or(|unknown| unknown <= *sighting_position))
}

pub(in crate::storage::work) fn expected_environment_projection(
    connection: &Connection,
    evidence_id: &ObjectId,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<EnvironmentEvidence>(
        connection,
        evidence_id,
        "environment_evidence",
    )?;
    let run_id = evidence.binding.run_id.0.to_string();
    let source_text_is_valid = |value: &str| {
        let trimmed = value.trim();
        !trimmed.is_empty() && trimmed == value && value.len() <= 512
    };
    let source_basis_matches_contract = source_text_is_valid(&evidence.source_basis.workspace_id)
        && source_text_is_valid(&evidence.source_basis.source_revision);
    let components_match = if let Some(components) = &evidence.components {
        let text_is_valid = |value: &str| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed == value && value.len() <= 256
        };
        text_is_valid(&components.toolchain)
            && text_is_valid(&components.workspace_id)
            && components.sandbox.as_deref().is_none_or(text_is_valid)
            && components.workspace_id == evidence.source_basis.workspace_id
            && components.capability_map_revision > 0
            && CanonicalObject::freeze(components)?.key() == &evidence.environment_fingerprint
    } else {
        true
    };
    let bound = evidence.schema_version == SCHEMA_VERSION
        && source_basis_matches_contract
        && evidence.observed_at <= evidence.recorded_at
        && evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(run_id.as_str())
        && components_match;
    if !bound {
        return Err(StoreError::InvalidWorkProjection(format!(
            "environment evidence {evidence_id} has an invalid run/session binding"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "environment".into(),
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_id: None,
        check_fingerprint: None,
        verification_result: None,
        observed_at_ms: Some(evidence.observed_at.timestamp_millis()),
        environment_fingerprint: Some(evidence.environment_fingerprint.to_string()),
        environment_evidence_id: None,
        components_json: evidence
            .components
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?,
    })
}

pub(in crate::storage::work) fn verify_evidence_rows(
    connection: &Connection,
    expected: &HashMap<String, EvidenceProjectionRow>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT evidence_id, work_id, run_id, evidence_kind,
                workspace_id, source_revision, producer_session_id,
                producer_observation_id, check_fingerprint,
                verification_result, observed_at_ms, environment_fingerprint,
                environment_evidence_id, components_json
         FROM work_run_evidence ORDER BY evidence_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            EvidenceProjectionRow {
                work_id: row.get(1)?,
                run_id: row.get(2)?,
                evidence_kind: row.get(3)?,
                workspace_id: row.get(4)?,
                source_revision: row.get(5)?,
                producer_session_id: row.get(6)?,
                producer_observation_id: row.get(7)?,
                check_fingerprint: row.get(8)?,
                verification_result: row.get(9)?,
                observed_at_ms: row.get(10)?,
                environment_fingerprint: row.get(11)?,
                environment_evidence_id: row.get(12)?,
                components_json: row.get(13)?,
            },
        ))
    })?;
    for row in rows {
        let (evidence_id, projected) = row?;
        *checked += 1;
        seen.insert(evidence_id.clone());
        if expected.get(&evidence_id) != Some(&projected) {
            invalid.push(format!("work_evidence:{evidence_id}:run_binding"));
        }
    }
    for evidence_id in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("work_evidence:{evidence_id}:missing"));
    }
    Ok(())
}

pub(in crate::storage::work) fn verify_restored_evidence_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let rows = connection
        .prepare(
            "SELECT evidence.evidence_id, evidence.work_id, evidence.record_id,
                    evidence.sequence, evidence.gate_name, evidence.created_at_ms,
                    object.object_kind, object.canonical_json,
                    item.item_json
             FROM work_restored_evidence AS evidence
             LEFT JOIN objects AS object ON object.object_id = evidence.evidence_id
             LEFT JOIN work_items AS item ON item.work_id = evidence.work_id
             ORDER BY evidence.work_id, evidence.sequence, evidence.evidence_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, Option<Vec<u8>>>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut projected = HashSet::with_capacity(rows.len());
    let mut chains = HashMap::<(WorkId, String), HashMap<ObjectId, Option<ObjectId>>>::new();
    let mut next_sequence = HashMap::<String, i64>::new();
    for (
        stored_hash,
        stored_work_id,
        stored_record,
        stored_sequence,
        stored_gate_name,
        stored_created_at,
        object_kind,
        bytes,
        item_json,
    ) in rows
    {
        *checked += 1;
        let label = format!("work_restored_evidence:{stored_hash}");
        projected.insert(stored_hash.clone());
        let Some(hash) = ObjectId::from_stored(stored_hash) else {
            invalid.push(label);
            continue;
        };
        let Some(bytes) = bytes else {
            invalid.push(label);
            continue;
        };
        let evidence = CanonicalObject::stored(&hash, bytes)
            .and_then(|object| object.decode::<RestoredWorkEvidence>());
        let item = item_json
            .as_deref()
            .map(serde_json::from_slice::<WorkItem>)
            .transpose();
        let (Ok(evidence), Ok(Some(item))) = (evidence, item) else {
            invalid.push(label);
            continue;
        };
        let record_is_bound = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_restored_records
                 WHERE work_id = ?1 AND record_id = ?2
             )",
            params![stored_work_id, stored_record],
            |row| row.get::<_, bool>(0),
        )?;
        let expected_sequence = next_sequence.entry(stored_work_id.clone()).or_insert(1);
        let mut internally_bound = object_kind.as_deref() == Some("work_restored_evidence")
            && evidence.schema_version == SCHEMA_VERSION
            && item.restored
            && evidence.work_id == item.work_id
            && evidence.work_id.0.to_string() == stored_work_id
            && evidence.restored_record.as_str() == stored_record
            && evidence.sequence == stored_sequence
            && stored_sequence == *expected_sequence
            && evidence.gate.as_ref().map(|gate| gate.name.as_str()) == stored_gate_name.as_deref()
            && evidence.created_at.timestamp_millis() == stored_created_at
            && record_is_bound;
        *expected_sequence = expected_sequence.saturating_add(1);
        if let Some(gate) = &evidence.gate {
            let evidence_ref = match evidence.refs.as_slice() {
                [] => None,
                [value] => Some(value.as_str()),
                _ => {
                    internally_bound = false;
                    None
                }
            };
            let normalized =
                normalize_gate_evidence_input(&gate.name, &gate.failed, evidence_ref).ok();
            internally_bound &= gate.schema_version == SCHEMA_VERSION
                && gate.passed == gate.failed.is_empty()
                && normalized.as_ref().is_some_and(|normalized| {
                    normalized.name == gate.name
                        && normalized.failed == gate.failed
                        && normalized.evidence_ref.as_deref() == evidence_ref
                });
            chains
                .entry((evidence.work_id, gate.name.clone()))
                .or_default()
                .insert(hash.clone(), gate.previous.clone());
        }
        if !internally_bound {
            invalid.push(format!("{label}:projection_binding"));
        }
    }
    for ((work_id, gate_name), chain) in chains {
        *checked += 1;
        if !restored_gate_chain_is_linear(&chain) {
            invalid.push(format!(
                "work_restored_evidence:{work_id:?}:{gate_name}:gate_chain"
            ));
        }
    }
    let orphaned = connection
        .prepare(
            "SELECT object_id FROM objects
             WHERE object_kind = 'work_restored_evidence'
             ORDER BY object_id",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for hash in orphaned {
        if !projected.contains(&hash) {
            *checked += 1;
            invalid.push(format!("work_restored_evidence:{hash}:missing_projection"));
        }
    }
    Ok(())
}

fn restored_gate_chain_is_linear(chain: &HashMap<ObjectId, Option<ObjectId>>) -> bool {
    let mut referenced = HashSet::with_capacity(chain.len());
    for previous in chain.values().flatten() {
        if !chain.contains_key(previous) || !referenced.insert(previous.clone()) {
            return false;
        }
    }
    let heads = chain
        .keys()
        .filter(|hash| !referenced.contains(*hash))
        .collect::<Vec<_>>();
    if heads.len() != 1 {
        return false;
    }
    let mut visited = HashSet::with_capacity(chain.len());
    let mut cursor = Some(heads[0]);
    while let Some(hash) = cursor {
        if !visited.insert(hash.clone()) {
            return false;
        }
        cursor = chain.get(hash).and_then(Option::as_ref);
    }
    visited.len() == chain.len()
}
