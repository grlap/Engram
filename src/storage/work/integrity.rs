use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::super::{StoreError, decode_failure_label};
use super::EvidenceProjectionRow;
use super::completion::{
    load_work_obligation_by_id_on, obligation_rule_set_for_observation_on,
    validate_completion_seal_children_on, validate_completion_seal_environment_basis_on,
    validate_completion_seal_obligation_basis_on,
};
use super::feeds::{
    load_typed_work_object, run_feed_position_for_object_on, validate_work_protocol_result_binding,
};
use super::planning::{encode_state, normalize_work_catalog_key, work_catalog_search_text};
use super::query::parse_work_id;
use crate::{
    CanonicalObject, ObjectId, RestoredWorkEvidence,
    domain::{
        CompletionSeal, EnvironmentEvidence, ExecutionObservation, MemoryAssertionEvent,
        MemoryVersion, NamedRootBindingEvent, NamedRootBindingKind, SCHEMA_VERSION,
        VerificationEvidence, WorkCheckpoint, WorkClaim, WorkEvent, WorkEvidence, WorkHandoffOffer,
        WorkId, WorkItem, WorkObligation, WorkObligationId, WorkObligationResolutionEvent, WorkRun,
        WorkRunId, normalize_gate_evidence_input,
    },
};

mod named_root;
pub(super) use named_root::verify_named_root_history;

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static GRAPH_SCAN_METRICS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// (Full-project scans, item/prerequisite rows visited), local to this test thread.
#[cfg(test)]
pub(super) fn take_graph_scan_metrics() -> (usize, usize) {
    GRAPH_SCAN_METRICS.replace((0, 0))
}

pub(super) fn combined_graph_is_acyclic(
    connection: &Connection,
    project_id: &str,
) -> Result<bool, StoreError> {
    combined_graph_is_acyclic_with_dependency(connection, project_id, None)
}

pub(super) fn combined_graph_is_acyclic_with_dependency(
    connection: &Connection,
    project_id: &str,
    proposed_supersession: Option<(WorkId, WorkId)>,
) -> Result<bool, StoreError> {
    #[cfg(test)]
    GRAPH_SCAN_METRICS.with(|metrics| {
        let (scans, rows) = metrics.get();
        metrics.set((scans + 1, rows));
    });
    let mut graph: HashMap<WorkId, Vec<WorkId>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT work_id, parent_id, child_requirement, superseded_by
         FROM work_items WHERE project_id = ?1",
    )?;
    let rows = statement.query_map([project_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (child, parent, requirement, superseded_by) = row?;
        #[cfg(test)]
        GRAPH_SCAN_METRICS.with(|metrics| {
            let (scans, rows) = metrics.get();
            metrics.set((scans, rows + 1));
        });
        let child = parse_work_id(&child)?;
        graph.entry(child).or_default();
        if requirement == "required"
            && let Some(parent) = parent
        {
            graph
                .entry(parse_work_id(&parent)?)
                .or_default()
                .push(child);
        }
        if let Some(replacement) = superseded_by {
            graph
                .entry(child)
                .or_default()
                .push(parse_work_id(&replacement)?);
        }
    }
    let mut statement = connection.prepare(
        "SELECT p.work_id, p.prerequisite_id
         FROM work_prerequisites p
         JOIN work_items w ON w.work_id = p.work_id
         WHERE w.project_id = ?1",
    )?;
    let rows = statement.query_map([project_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (work, prerequisite) = row?;
        #[cfg(test)]
        GRAPH_SCAN_METRICS.with(|metrics| {
            let (scans, rows) = metrics.get();
            metrics.set((scans, rows + 1));
        });
        graph
            .entry(parse_work_id(&work)?)
            .or_default()
            .push(parse_work_id(&prerequisite)?);
    }
    if let Some((source, replacement)) = proposed_supersession {
        graph.entry(source).or_default().push(replacement);
        graph.entry(replacement).or_default();
    }

    let mut incoming = graph
        .keys()
        .copied()
        .map(|node| (node, 0_usize))
        .collect::<HashMap<_, _>>();
    for edges in graph.values() {
        for target in edges {
            *incoming.entry(*target).or_default() += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(*node))
        .collect::<Vec<_>>();
    let mut removed = 0_usize;
    while let Some(node) = ready.pop() {
        removed += 1;
        if let Some(edges) = graph.get(&node) {
            for target in edges {
                let count = incoming
                    .get_mut(target)
                    .expect("every graph target has an incoming count");
                *count -= 1;
                if *count == 0 {
                    ready.push(*target);
                }
            }
        }
    }
    Ok(removed == incoming.len())
}

pub(super) fn verify_json_projection<T: DeserializeOwned + Serialize + PartialEq>(
    connection: &Connection,
    kind: &str,
    sql: &str,
    expected: &HashMap<String, serde_json::Value>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (id, bytes) = row?;
        *checked += 1;
        seen.insert(id.clone());
        let Some(expected) = expected.get(&id) else {
            invalid.push(format!("{kind}:{id}"));
            continue;
        };
        let projected = decode_projection_bytes::<T>(&bytes);
        match (projected, decode_preserved_projection::<T>(expected)) {
            (Some(projected), Some(canonical)) if projected == canonical => {}
            // The canonical side itself does not survive its own type: that
            // is named apart from projection drift. The projection is still
            // compared with what the canonical side decodes to, so a
            // projection that fails its own guard or differs from it is
            // reported beside the canonical label.
            (projected, None) => match canonical_without_its_representation::<T>(expected) {
                Some(canonical) => {
                    invalid.push(format!("{kind}:{id}:canonical_representation"));
                    if projected.as_ref() != Some(&canonical) {
                        invalid.push(format!("{kind}:{id}"));
                    }
                }
                None => invalid.push(format!("{kind}:{id}")),
            },
            _ => invalid.push(format!("{kind}:{id}")),
        }
    }
    for id in expected.keys().filter(|id| !seen.contains(*id)) {
        invalid.push(format!("{kind}:{id}:missing"));
    }
    Ok(())
}

pub(super) fn verify_prerequisite_rows(
    connection: &Connection,
    expected: &HashMap<(String, String), String>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT work_id, prerequisite_id, event_id
         FROM work_prerequisites ORDER BY work_id, prerequisite_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (work_id, prerequisite_id, event_id) = row?;
        *checked += 1;
        let key = (work_id, prerequisite_id);
        seen.insert(key.clone());
        if expected.get(&key) != Some(&event_id) {
            invalid.push(format!("work_prerequisite:{}:{}", key.0, key.1));
        }
    }
    for key in expected.keys().filter(|key| !seen.contains(*key)) {
        invalid.push(format!("work_prerequisite:{}:{}:missing", key.0, key.1));
    }
    drop(statement);

    let mut statement = connection.prepare("SELECT DISTINCT project_id FROM work_items")?;
    let projects = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for project in projects {
        *checked += 1;
        if !combined_graph_is_acyclic(connection, &project)? {
            invalid.push(format!("work_graph:{project}:cycle"));
        }
    }
    drop(statement);

    Ok(())
}

pub(super) fn verify_blocker_rows(
    connection: &Connection,
    expected: &HashMap<String, (String, String, Option<String>)>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT blocker_id, state, created_event_id, cleared_event_id
         FROM work_blockers ORDER BY blocker_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (blocker_id, state, created, cleared) = row?;
        *checked += 1;
        seen.insert(blocker_id.clone());
        if expected.get(&blocker_id) != Some(&(state, created, cleared)) {
            invalid.push(format!("work_blocker:{blocker_id}:event_binding"));
        }
    }
    for blocker_id in expected.keys().filter(|id| !seen.contains(*id)) {
        invalid.push(format!("work_blocker:{blocker_id}:missing"));
    }
    Ok(())
}

pub(super) fn expected_verification_projection(
    connection: &Connection,
    evidence_id: &ObjectId,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<VerificationEvidence>(
        connection,
        evidence_id,
        "verification_evidence",
    )?;
    let producer = load_typed_work_object::<ExecutionObservation>(
        connection,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    // The check ran under the original's binding and basis. A bound record
    // carries the original's facts unchanged and its own run, generation,
    // binder and binding time; it is checked against its original here.
    let (check_binding, check_basis, original_matches) = match &evidence.bound_from {
        None => (
            evidence.binding.clone(),
            evidence.source_basis.clone(),
            true,
        ),
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
            let matches = original.bound_from.is_none()
                && original.result == crate::domain::VerificationResult::Passed
                && original.project_id == evidence.project_id
                && original.binding.work_id == source.work_id
                && original.binding.run_id == source.run_id
                && original_position == Some(source.original_position)
                && original.producer_observation == evidence.producer_observation
                && original.check_kind == evidence.check_kind
                && original.check_fingerprint == evidence.check_fingerprint
                && original.result == evidence.result
                && original.completed_at == evidence.completed_at
                && original.environment == evidence.environment
                && original.source_basis.workspace_id == evidence.source_basis.workspace_id
                && original.source_basis.source_revision == evidence.source_basis.source_revision
                && original.source_basis.source_root_state
                    == Some(crate::domain::SourceRootState::Named)
                && evidence.source_basis.source_root_generation.is_some()
                && evidence.source_basis.source_root_state
                    == Some(crate::domain::SourceRootState::Named)
                && source.measurement.workspace_id == evidence.source_basis.workspace_id
                && source.measurement.source_revision == evidence.source_basis.source_revision
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
            && environment.source_basis.source_revision == check_basis.source_revision
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
        && producer.project_id == evidence.project_id
        && producer.binding == check_binding
        && producer_session_matches
        && producer.source_basis.as_ref() == Some(&check_basis)
        && producer.observed_at == Some(evidence.completed_at)
        && producer.action_fingerprint == evidence.check_fingerprint
        && result_matches
        && evidence.completed_at <= evidence.recorded_at
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
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
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
    let Some(root) = super::completion::named_root_at_cut_on(
        connection,
        evidence.binding.run_id,
        evidence.binding.claim_id,
        position.position - 1,
    )?
    else {
        return Ok(false);
    };
    let basis = &evidence.source_basis;
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
        })
        && root
            .unknown_change_position
            .is_none_or(|unknown| unknown <= *sighting_position))
}

pub(super) fn expected_environment_projection(
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

pub(super) fn verify_evidence_rows(
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

pub(super) fn verify_restored_evidence_rows(
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

pub(super) fn verify_obligation_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let obligation_rows = connection
        .prepare(
            "SELECT obligation_id, definition_id FROM work_run_obligations
             ORDER BY obligation_id",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut projected_definitions = HashSet::new();
    let mut projected_resolutions = HashSet::new();
    for (stored_id, stored_definition) in obligation_rows {
        *checked += 1;
        let id = uuid::Uuid::parse_str(&stored_id)
            .map(WorkObligationId)
            .map_err(|error| {
                StoreError::InvalidWorkProjection(format!(
                    "obligation projection id {stored_id:?} is invalid: {error}"
                ))
            });
        if let Ok(record) = id.and_then(|id| load_work_obligation_by_id_on(connection, id)) {
            projected_definitions.insert(record.definition_id);
            if let Some(resolution) = record.resolution_id {
                projected_resolutions.insert(resolution);
            }
        } else {
            // The run's obligations load together, so one unreadable record
            // fails its siblings too. Only a record whose own definition
            // cannot be decoded carries the reason, such as a member the
            // current format has no place for.
            let label = format!("work_obligation:{stored_id}");
            let own_definition = ObjectId::from_stored(stored_definition).map(|definition| {
                load_typed_work_object::<WorkObligation>(connection, &definition, "work_obligation")
            });
            invalid.push(if let Some(Err(error)) = own_definition {
                decode_failure_label(label, &error)
            } else {
                label
            });
        }
    }
    for (kind, projected) in [
        ("work_obligation", &projected_definitions),
        ("work_obligation_resolution", &projected_resolutions),
    ] {
        let hashes = connection
            .prepare("SELECT object_id FROM objects WHERE object_kind = ?1 ORDER BY object_id")?
            .query_map([kind], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in hashes {
            *checked += 1;
            let hash = ObjectId::from_stored(stored_hash.clone());
            if hash.as_ref().is_none_or(|hash| !projected.contains(hash)) {
                invalid.push(format!("{kind}:{stored_hash}:missing_projection"));
            }
        }
    }
    let expected = connection
        .prepare(&format!(
            "SELECT entry.feed_id, entry.position, entry.object_id
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution'
               AND {} AND {}
             ORDER BY entry.feed_id, entry.position",
            super::feeds::SOURCE_RECORD_SQL,
            super::feeds::SOURCE_CHANGED_SQL
        ))?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // Each finished run's cut is read once, however many source changes it
    // holds; the rows arrive grouped by run.
    let mut cuts: HashMap<String, Option<i64>> = HashMap::new();
    for (run_id, position, stored_hash) in expected {
        *checked += 1;
        let Some(hash) = ObjectId::from_stored(stored_hash.clone()) else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(observation) = super::feeds::load_source_observation_on(connection, &hash) else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(rule_set) = obligation_rule_set_for_observation_on(connection, &observation) else {
            invalid.push(format!(
                "work_obligation_trigger:{run_id}:{position}:invalid_rule_set"
            ));
            continue;
        };
        let cut = if let Some(cut) = cuts.get(&run_id) {
            *cut
        } else {
            let cut = finished_run_cut(connection, &run_id)?;
            cuts.insert(run_id.clone(), cut);
            cut
        };
        if recorded_after_finish_without_obligations(connection, &run_id, cut, position, &hash)? {
            continue;
        }
        for (rule, _) in
            crate::control::evaluate_obligation_rules(&rule_set, observation.source_changed)
        {
            let exists = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_run_obligations
                     WHERE run_id = ?1 AND rule_id = ?2 AND rule_version = ?3
                       AND triggering_observation_id = ?4 AND trigger_position = ?5
                       AND rule_set_id = ?6
                 )",
                params![
                    run_id,
                    rule.rule_id,
                    rule.rule_version,
                    hash.as_str(),
                    position,
                    observation.obligation_rule_set.as_str(),
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                invalid.push(format!(
                    "work_obligation_trigger:{run_id}:{position}:missing_definition"
                ));
            }
        }
    }
    Ok(())
}

/// The cut at which the run named by a feed id was sealed, if it is finished.
/// A run whose seal is missing or undecodable has none, so its changes are
/// checked strictly and the damage is reported elsewhere; a SQLite failure
/// reading it is returned as itself.
fn finished_run_cut(connection: &Connection, run_id: &str) -> Result<Option<i64>, StoreError> {
    #[cfg(test)]
    super::DOCTOR_FINISHED_RUN_CUT_READS.with(|count| count.set(count.get() + 1));
    let Ok(parsed) = super::query::parse_work_run_id(run_id) else {
        return Ok(None);
    };
    super::completion::finished_run_cut_on(connection, parsed)
}

/// A source change recorded after its run was sealed at `cut` opens no
/// obligation. One an older build recorded with obligations is still checked
/// in full, and a run without a cut is checked strictly.
fn recorded_after_finish_without_obligations(
    connection: &Connection,
    run_id: &str,
    cut: Option<i64>,
    position: i64,
    observation: &ObjectId,
) -> Result<bool, StoreError> {
    let Some(cut) = cut else {
        return Ok(false);
    };
    if position <= cut {
        return Ok(false);
    }
    let opened = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_run_obligations
             WHERE run_id = ?1 AND triggering_observation_id = ?2
         )",
        params![run_id, observation.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(!opened)
}

pub(super) fn verify_completion_rows(
    connection: &Connection,
    expected: &HashMap<String, (String, String, String, serde_json::Value)>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT seal_id, work_id, run_id, root_execution_id, seal_json
         FROM work_completion_seals ORDER BY seal_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    for row in rows {
        let (seal_id, work_id, run_id, root_execution_id, bytes) = row?;
        *checked += 1;
        seen.insert(seal_id.clone());
        // A canonical seal that does not survive its own type is named apart
        // from projection damage; whatever the projection can still be checked
        // against (its own guard, its bindings, the checks below) is checked.
        // The projection is compared with what the canonical seal decodes
        // to even then, so drift is still reported beside that label.
        let canonical = expected.get(&seal_id).and_then(|expected| {
            decode_preserved_projection::<CompletionSeal>(&expected.3).or_else(|| {
                let lost = canonical_without_its_representation::<CompletionSeal>(&expected.3)?;
                invalid.push(format!(
                    "completion_seal:{seal_id}:canonical_representation"
                ));
                Some(lost)
            })
        });
        let Some(seal) = decode_projection_bytes::<CompletionSeal>(&bytes) else {
            invalid.push(format!("completion_seal:{seal_id}:projection_binding"));
            continue;
        };
        let valid = expected.get(&seal_id).is_some_and(|expected| {
            expected.0 == work_id
                && expected.1 == run_id
                && expected.2 == root_execution_id
                && canonical.as_ref() == Some(&seal)
        });
        if !valid {
            invalid.push(format!("completion_seal:{seal_id}:projection_binding"));
            continue;
        }
        if validate_completion_seal_obligation_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_id}:obligation_basis"));
            continue;
        }
        if validate_completion_seal_environment_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_id}:environment_basis"));
            continue;
        }
        if validate_completion_seal_children_on(connection, &seal, 0).is_err() {
            invalid.push(format!("completion_seal:{seal_id}:child_obligation_basis"));
        }
        if super::acceptance_evaluation::validate_completion_seal_acceptance_evaluation_on(
            connection, &seal,
        )
        .is_err()
        {
            invalid.push(format!(
                "completion_seal:{seal_id}:acceptance_evaluation_binding"
            ));
        }
    }
    for seal_id in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("completion_seal:{seal_id}:missing"));
    }
    Ok(())
}

pub(super) fn verify_work_feed_integrity(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut actual_occurrences: HashMap<String, HashSet<String>> = HashMap::new();
    let mut expected_occurrences: HashMap<String, HashSet<String>> = HashMap::new();
    let mut feed_sequences: HashMap<String, Vec<String>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT entry.feed_kind, entry.feed_id, entry.position, entry.object_kind,
                entry.object_id, entry.work_id, object.object_kind, object.canonical_json
         FROM work_feed_entries entry
         LEFT JOIN objects object ON object.object_id = entry.object_id
         ORDER BY entry.feed_kind, entry.feed_id, entry.position",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<Vec<u8>>>(7)?,
        ))
    })?;
    for row in rows {
        let (
            feed_kind,
            feed_id,
            position,
            entry_kind,
            stored_hash,
            projected_work_id,
            object_kind,
            bytes,
        ) = row?;
        *checked += 1;
        let label = format!("work_feed:{feed_kind}:{feed_id}:{position}");
        let feed_key = format!("{feed_kind}:{feed_id}");
        feed_sequences
            .entry(feed_key.clone())
            .or_default()
            .push(stored_hash.clone());
        if !actual_occurrences
            .entry(stored_hash.clone())
            .or_default()
            .insert(feed_key.clone())
        {
            invalid.push(label);
            continue;
        }
        let Some(hash) = ObjectId::from_stored(stored_hash.clone()) else {
            invalid.push(label);
            continue;
        };
        let Some(bytes) = bytes else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::stored(&hash, bytes) else {
            invalid.push(label);
            continue;
        };
        if object_kind.as_deref() != Some(entry_kind.as_str()) {
            invalid.push(label);
            continue;
        }
        if entry_kind == "work_event" {
            if object
                .decode::<WorkEvent>()
                .ok()
                .map(|event| event.work_id.0.to_string())
                != projected_work_id
            {
                invalid.push(format!("{label}:work_id_binding"));
                continue;
            }
        } else if projected_work_id.is_some() {
            invalid.push(format!("{label}:unexpected_work_id_binding"));
            continue;
        }
        let expected = match entry_kind.as_str() {
            "work_event" => object
                .decode::<WorkEvent>()
                .ok()
                .map(|event| expected_work_feeds(&event.project_id.0, event.root_id, event.run_id)),
            "work_checkpoint" => object
                .decode::<WorkCheckpoint>()
                .ok()
                .and_then(|checkpoint| {
                    expected_feeds_for_work(work_items, checkpoint.work_id, Some(checkpoint.run_id))
                }),
            "work_evidence" => object.decode::<WorkEvidence>().ok().and_then(|evidence| {
                expected_feeds_for_work(work_items, evidence.work_id, Some(evidence.run_id))
            }),
            "work_restored_evidence" => object
                .decode::<RestoredWorkEvidence>()
                .ok()
                .and_then(|evidence| expected_feeds_for_work(work_items, evidence.work_id, None)),
            "work_observation" => object
                .decode::<crate::domain::WorkObservation>()
                .ok()
                .and_then(|observation| {
                    expected_feeds_for_work(work_items, observation.work_id, None)
                }),
            "acceptance_evaluation" => object
                .decode::<crate::domain::AcceptanceEvaluation>()
                .ok()
                .and_then(|evaluation| {
                    expected_feeds_for_work(work_items, evaluation.work_id, Some(evaluation.run_id))
                }),
            "work_source_proposal" => object
                .decode::<crate::domain::WorkSourceProposal>()
                .ok()
                .and_then(|proposal| {
                    super::query::load_work_item(connection, proposal.work_id)
                        .ok()
                        .filter(|item| {
                            super::import::validate_proposal_on(connection, item, &proposal).is_ok()
                        })
                        .and_then(|_| expected_feeds_for_work(work_items, proposal.work_id, None))
                }),
            "execution_observation" => object
                .decode::<ExecutionObservation>()
                .ok()
                .map(|observation| {
                    expected_execution_observation_feeds(connection, work_items, &observation)
                })
                .transpose()?
                .flatten(),
            "named_root_binding" => object
                .decode::<NamedRootBindingEvent>()
                .ok()
                .map(|event| expected_named_root_binding_feeds(connection, work_items, &event))
                .transpose()?
                .flatten(),
            super::UNADMITTED_OBSERVATION_KIND => {
                match object.decode::<crate::domain::UnadmittedExecutionObservation>() {
                    Ok(observation)
                        if super::unadmitted_observation_is_consistent_on(
                            connection,
                            &observation,
                            &hash,
                        )? =>
                    {
                        expected_feeds_for_work(
                            work_items,
                            observation.binding.work_id,
                            Some(observation.binding.run_id),
                        )
                    }
                    _ => None,
                }
            }
            "verification_evidence" => object
                .decode::<VerificationEvidence>()
                .ok()
                .and_then(|evidence| {
                    expected_verification_projection(connection, &hash)
                        .ok()
                        .map(|_| evidence)
                })
                .and_then(|evidence| {
                    expected_feeds_for_work(
                        work_items,
                        evidence.binding.work_id,
                        Some(evidence.binding.run_id),
                    )
                }),
            "environment_evidence" => object
                .decode::<EnvironmentEvidence>()
                .ok()
                .and_then(|evidence| {
                    expected_environment_projection(connection, &hash)
                        .ok()
                        .map(|_| evidence)
                })
                .and_then(|evidence| {
                    expected_feeds_for_work(
                        work_items,
                        evidence.binding.work_id,
                        Some(evidence.binding.run_id),
                    )
                }),
            "work_obligation" => object
                .decode::<WorkObligation>()
                .ok()
                .and_then(|obligation| {
                    load_work_obligation_by_id_on(connection, obligation.obligation_id)
                        .ok()
                        .filter(|record| record.definition_id == hash)
                        .map(|_| obligation)
                })
                .map(|obligation| {
                    expected_work_feeds(
                        &obligation.project_id.0,
                        obligation.root_id,
                        Some(obligation.run_id),
                    )
                }),
            "work_obligation_resolution" => object
                .decode::<WorkObligationResolutionEvent>()
                .ok()
                .and_then(|event| {
                    load_work_obligation_by_id_on(connection, event.obligation_id)
                        .ok()
                        .filter(|record| record.resolution_id.as_ref() == Some(&hash))
                        .map(|record| record.obligation)
                })
                .map(|obligation| {
                    expected_work_feeds(
                        &obligation.project_id.0,
                        obligation.root_id,
                        Some(obligation.run_id),
                    )
                }),
            "memory_version" => object
                .decode::<MemoryVersion>()
                .ok()
                .map(|version| {
                    expected_work_memory_feeds(connection, work_items, &stored_hash, &version)
                })
                .transpose()?
                .flatten(),
            "memory_assertion_event" => object
                .decode::<MemoryAssertionEvent>()
                .ok()
                .and_then(|assertion| {
                    load_typed_work_object::<MemoryVersion>(
                        connection,
                        &assertion.version,
                        "memory_version",
                    )
                    .ok()
                    .filter(|version| version.memory_id == assertion.memory_id)
                })
                .map(|version| {
                    expected_work_memory_feeds(connection, work_items, &stored_hash, &version)
                })
                .transpose()?
                .flatten(),
            _ => None,
        };
        let Some(expected) = expected else {
            invalid.push(format!("{label}:unsupported_or_unbound_object"));
            continue;
        };
        if !expected.contains(&feed_key) {
            invalid.push(format!("{label}:wrong_membership"));
        }
        match expected_occurrences.entry(stored_hash) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(expected);
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() != &expected => {
                invalid.push(format!("{label}:inconsistent_typed_membership"));
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    drop(statement);

    for (hash, expected) in &expected_occurrences {
        *checked += 1;
        if actual_occurrences.get(hash) != Some(expected) {
            invalid.push(format!("work_feed_object:{hash}:occurrences"));
        }
    }
    verify_cross_feed_order(
        work_items,
        &expected_occurrences,
        &feed_sequences,
        checked,
        invalid,
    );

    let mut statement = connection.prepare(
        "SELECT head.feed_kind, head.feed_id, head.position,
                COUNT(entry.position), COALESCE(MIN(entry.position), 0),
                COALESCE(MAX(entry.position), 0)
         FROM work_feed_heads head
         LEFT JOIN work_feed_entries entry
           ON entry.feed_kind = head.feed_kind AND entry.feed_id = head.feed_id
         GROUP BY head.feed_kind, head.feed_id, head.position
         ORDER BY head.feed_kind, head.feed_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (feed_kind, feed_id, head, count, minimum, maximum) = row?;
        *checked += 1;
        if head <= 0 || count != head || minimum != 1 || maximum != head {
            invalid.push(format!("work_feed_head:{feed_kind}:{feed_id}"));
        }
    }
    drop(statement);

    let missing_heads = connection.query_row(
        "SELECT COUNT(*) FROM work_feed_entries entry
         LEFT JOIN work_feed_heads head
           ON head.feed_kind = entry.feed_kind AND head.feed_id = entry.feed_id
         WHERE head.feed_id IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    *checked += 1;
    if missing_heads != 0 {
        invalid.push("work_feed_entries:missing_heads".into());
    }

    let mut statement = connection.prepare(
        "SELECT object.object_kind, object.object_id FROM objects object
         LEFT JOIN work_feed_entries entry
           ON entry.object_id = object.object_id
         WHERE object.object_kind IN (
             'work_event', 'work_checkpoint', 'work_evidence', 'work_restored_evidence', 'work_observation', 'work_source_proposal',
              'verification_evidence', 'environment_evidence', 'named_root_binding',
             'work_obligation', 'work_obligation_resolution', 'unadmitted_execution_observation'
         )
           AND entry.object_id IS NULL
         ORDER BY object.object_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        *checked += 1;
        let (kind, hash) = row?;
        invalid.push(format!("{kind}:{hash}:missing_work_feeds"));
    }
    Ok(())
}

fn expected_work_memory_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    object_id: &str,
    version: &MemoryVersion,
) -> Result<Option<HashSet<String>>, StoreError> {
    let crate::domain::Scope::Work { project, work } = &version.scope else {
        return Ok(None);
    };
    let Some(item) = work_items.get(&work.0.to_string()) else {
        return Ok(None);
    };
    let Some(item_project) = item.get("project_id").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Some(root_id) = item
        .get("root_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(WorkId)
    else {
        return Ok(None);
    };
    if item_project != project.0 {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT feed_kind, feed_id FROM work_feed_entries
         WHERE object_id = ?1 ORDER BY feed_kind, feed_id",
    )?;
    let feeds = statement
        .query_map([object_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = HashSet::new();
    for (kind, id) in feeds {
        let valid = match kind.as_str() {
            "project" => id == project.0,
            "root_work" => id == root_id.0.to_string(),
            "run_execution" => connection
                .query_row(
                    "SELECT work_id FROM work_runs WHERE run_id = ?1",
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .is_some_and(|run_work| run_work == work.0.to_string()),
            _ => false,
        };
        if !valid || !expected.insert(format!("{kind}:{id}")) {
            return Ok(None);
        }
    }
    let required = HashSet::from([
        format!("project:{}", project.0),
        format!("root_work:{}", root_id.0),
    ]);
    Ok(required.is_subset(&expected).then_some(expected))
}

fn expected_work_feeds(
    project_id: &str,
    root_id: WorkId,
    run_id: Option<WorkRunId>,
) -> HashSet<String> {
    let mut feeds = HashSet::from([
        format!("project:{project_id}"),
        format!("root_work:{}", root_id.0),
    ]);
    if let Some(run_id) = run_id {
        feeds.insert(format!("run_execution:{}", run_id.0));
    }
    feeds
}

fn expected_feeds_for_work(
    work_items: &HashMap<String, serde_json::Value>,
    work_id: WorkId,
    run_id: Option<WorkRunId>,
) -> Option<HashSet<String>> {
    let item = work_items.get(&work_id.0.to_string())?;
    let project_id = item.get("project_id")?.as_str()?;
    let root_id = uuid::Uuid::parse_str(item.get("root_id")?.as_str()?)
        .ok()
        .map(WorkId)?;
    Some(expected_work_feeds(project_id, root_id, run_id))
}

fn expected_execution_observation_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    observation: &ExecutionObservation,
) -> Result<Option<HashSet<String>>, StoreError> {
    if observation.actor.session_id.as_ref() != Some(&observation.session_id)
        || observation.actor.run_id.as_deref()
            != Some(observation.binding.run_id.0.to_string().as_str())
        || observation.binding.work_revision <= 0
        || observation.binding.claim_fence <= 0
    {
        return Ok(None);
    }
    let Some(item) = work_items.get(&observation.binding.work_id.0.to_string()) else {
        return Ok(None);
    };
    if item.get("project_id").and_then(serde_json::Value::as_str)
        != Some(observation.project_id.0.as_str())
    {
        return Ok(None);
    }
    let Some(root_id) = item
        .get("root_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(WorkId)
    else {
        return Ok(None);
    };
    let relation_matches = connection
        .query_row(
            "SELECT 1 FROM work_runs run
             JOIN work_root_executions execution
               ON execution.root_execution_id = run.root_execution_id
             WHERE run.run_id = ?1 AND run.work_id = ?2
               AND run.root_execution_id = ?3 AND execution.root_id = ?4",
            params![
                observation.binding.run_id.0.to_string(),
                observation.binding.work_id.0.to_string(),
                observation.binding.root_execution_id.0.to_string(),
                root_id.0.to_string()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(relation_matches.then(|| {
        expected_work_feeds(
            &observation.project_id.0,
            root_id,
            Some(observation.binding.run_id),
        )
    }))
}

fn expected_named_root_binding_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    event: &NamedRootBindingEvent,
) -> Result<Option<HashSet<String>>, StoreError> {
    if event.actor.session_id.as_ref() != Some(&event.session_id)
        || event.actor.run_id.as_deref() != Some(event.run_id.0.to_string().as_str())
        || event.claim_fence <= 0
        || event.workspace_id.trim() != event.workspace_id
        || event.workspace_id.is_empty()
        || event.workspace_id.len() > 512
        || event.generation <= 0
        || event.named_at > event.recorded_at
        || (event.kind == NamedRootBindingKind::Bound) != event.end_reason.is_none()
    {
        return Ok(None);
    }
    let Some(item) = work_items.get(&event.work_id.0.to_string()) else {
        return Ok(None);
    };
    if item.get("project_id").and_then(serde_json::Value::as_str)
        != Some(event.project_id.0.as_str())
    {
        return Ok(None);
    }
    let relation_matches = connection
        .query_row(
            "SELECT 1 FROM work_runs run
             JOIN work_root_executions root
               ON root.root_execution_id = run.root_execution_id
             WHERE run.run_id = ?1 AND run.work_id = ?2
               AND run.root_execution_id = ?3 AND root.root_id = ?4",
            params![
                event.run_id.0.to_string(),
                event.work_id.0.to_string(),
                event.root_execution_id.0.to_string(),
                item.get("root_id").and_then(serde_json::Value::as_str)
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(relation_matches
        .then(|| expected_feeds_for_work(work_items, event.work_id, Some(event.run_id)))
        .flatten())
}

fn verify_cross_feed_order(
    work_items: &HashMap<String, serde_json::Value>,
    expected_occurrences: &HashMap<String, HashSet<String>>,
    feed_sequences: &HashMap<String, Vec<String>>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) {
    for (feed, sequence) in feed_sequences {
        let parent_feed = if let Some(root_id) = feed.strip_prefix("root_work:") {
            work_items.get(root_id).and_then(|item| {
                item.get("project_id")
                    .and_then(serde_json::Value::as_str)
                    .map(|project| format!("project:{project}"))
            })
        } else if feed.starts_with("run_execution:") {
            sequence.iter().find_map(|hash| {
                expected_occurrences.get(hash).and_then(|feeds| {
                    feeds
                        .iter()
                        .find(|candidate| candidate.starts_with("root_work:"))
                        .cloned()
                })
            })
        } else {
            None
        };
        let Some(parent_feed) = parent_feed else {
            continue;
        };
        *checked += 1;
        let parent_projection = feed_sequences
            .get(&parent_feed)
            .into_iter()
            .flatten()
            .filter(|hash| {
                expected_occurrences
                    .get(*hash)
                    .is_some_and(|feeds| feeds.contains(feed))
            })
            .collect::<Vec<_>>();
        if parent_projection != sequence.iter().collect::<Vec<_>>() {
            invalid.push(format!("work_feed_order:{feed}"));
        }
    }
}

pub(super) fn verify_work_catalog_projections(
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

pub(super) fn verify_work_scalar_bindings(
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

pub(super) fn verify_canonical_work_rows(
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

// Both sources use one guard: typed equality may materialize omitted
// defaults, but must not silently discard stored fields or normalize scalars.
fn decode_preserved_projection<T: DeserializeOwned + Serialize>(
    stored: &serde_json::Value,
) -> Option<T> {
    let decoded = T::deserialize(stored).ok()?;
    preserves_projection_representation(stored, &decoded).then_some(decoded)
}

fn preserves_projection_representation<T: Serialize>(
    stored: &serde_json::Value,
    decoded: &T,
) -> bool {
    serde_json::to_value(decoded)
        .is_ok_and(|serialized| json_members_preserved(stored, &serialized))
}

fn json_members_preserved(stored: &serde_json::Value, serialized: &serde_json::Value) -> bool {
    match (stored, serialized) {
        (serde_json::Value::Object(stored), serde_json::Value::Object(serialized)) => {
            stored.iter().all(|(key, value)| {
                serialized
                    .get(key)
                    .is_some_and(|expected| json_members_preserved(value, expected))
            })
        }
        (serde_json::Value::Array(stored), serde_json::Value::Array(serialized)) => {
            stored.len() == serialized.len()
                && stored
                    .iter()
                    .zip(serialized)
                    .all(|(value, expected)| json_members_preserved(value, expected))
        }
        _ => stored == serialized,
    }
}

/// The value a canonical value decodes to as `T` when it does not survive
/// being written back (a member lost or a scalar respelled): a failure of the
/// canonical side's representation, which is labelled apart from projection
/// drift. `None` when the value decodes and survives, or does not decode.
fn canonical_without_its_representation<T: DeserializeOwned + Serialize>(
    stored: &serde_json::Value,
) -> Option<T> {
    let decoded = T::deserialize(stored).ok()?;
    (!preserves_projection_representation(stored, &decoded)).then_some(decoded)
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

fn decode_projection_bytes<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Option<T> {
    // Decode the original bytes first: Value would collapse duplicate known
    // members before serde could reject them, including nested/enum fields.
    let decoded = serde_json::from_slice::<T>(bytes).ok()?;
    let stored = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    preserves_projection_representation(&stored, &decoded).then_some(decoded)
}

// Malformed input never equals malformed input, even when both fail decoding.
fn typed_projection_bytes_equal<T: DeserializeOwned + Serialize + PartialEq>(
    projected: &[u8],
    canonical: &[u8],
) -> bool {
    matches!((decode_projection_bytes::<T>(projected), decode_projection_bytes::<T>(canonical)),
        (Some(projected), Some(canonical)) if projected == canonical)
}

pub(super) fn verify_work_protocol_attempts(
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
        match super::receipts::protocol_result_problem(&operation, result_json.as_deref()) {
            Some(reason) => invalid.push(format!("{label}:{reason}")),
            None if !request_valid || !basis_valid || !result_valid => invalid.push(label),
            None => {}
        }
    }
    Ok(())
}
