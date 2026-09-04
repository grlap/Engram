use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::StoreError;
use super::EvidenceProjectionRow;
use super::completion::{
    load_work_obligation_by_id_on, obligation_rule_set_for_observation_on,
    validate_completion_seal_children_on, validate_completion_seal_environment_basis_on,
    validate_completion_seal_obligation_basis_on,
};
use super::feeds::{load_typed_work_object, validate_work_protocol_result_binding};
use super::planning::{encode_state, normalize_work_catalog_key, work_catalog_search_text};
use super::query::{catalog_literal_fts_query, parse_work_id};
use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        CompletionSeal, EnvironmentEvidence, ExecutionObservation, MemoryAssertionEvent,
        MemoryVersion, RootExecution, SCHEMA_VERSION, VerificationEvidence, WorkCheckpoint,
        WorkClaim, WorkEvent, WorkEvidence, WorkHandoffOffer, WorkId, WorkItem, WorkObligation,
        WorkObligationId, WorkObligationResolutionEvent, WorkRun, WorkRunId,
    },
};

#[cfg(test)]
mod tests;

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

pub(super) fn verify_json_projection(
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
        let projected = serde_json::from_slice::<serde_json::Value>(&bytes);
        if projected.as_ref().ok() != expected.get(&id) {
            invalid.push(format!("{kind}:{id}"));
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
        "SELECT work_id, prerequisite_id, event_hash
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
        let (work_id, prerequisite_id, event_hash) = row?;
        *checked += 1;
        let key = (work_id, prerequisite_id);
        seen.insert(key.clone());
        if expected.get(&key) != Some(&event_hash) {
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
        "SELECT blocker_id, state, created_event_hash, cleared_event_hash
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
    evidence_hash: &ObjectHash,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<VerificationEvidence>(
        connection,
        evidence_hash,
        "verification_evidence",
    )?;
    let producer = load_typed_work_object::<ExecutionObservation>(
        connection,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    let environment_matches = if let Some(environment_hash) = &evidence.environment {
        expected_environment_projection(connection, environment_hash)?;
        let environment = load_typed_work_object::<EnvironmentEvidence>(
            connection,
            environment_hash,
            "environment_evidence",
        )?;
        environment.project_id == evidence.project_id
            && environment.binding.root_execution_id == evidence.binding.root_execution_id
            && environment.binding.work_id == evidence.binding.work_id
            && environment.binding.run_id == evidence.binding.run_id
            && environment.source_basis.source_revision == evidence.source_basis.source_revision
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
    let bound = evidence.schema_version == SCHEMA_VERSION
        && producer.project_id == evidence.project_id
        && producer.binding == evidence.binding
        && producer.session_id == evidence.session_id
        && producer.source_basis.as_ref() == Some(&evidence.source_basis)
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
            "verification evidence {evidence_hash} is not bound to its producer observation"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "verification".into(),
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_hash: Some(evidence.producer_observation.to_string()),
        check_fingerprint: Some(evidence.check_fingerprint.to_string()),
        verification_result: Some(encode_state(evidence.result)?),
        observed_at_ms: Some(evidence.completed_at.timestamp_millis()),
        environment_fingerprint: None,
        environment_evidence_hash: evidence.environment.map(|hash| hash.to_string()),
        components_json: None,
    })
}

pub(super) fn expected_environment_projection(
    connection: &Connection,
    evidence_hash: &ObjectHash,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<EnvironmentEvidence>(
        connection,
        evidence_hash,
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
            && CanonicalObject::freeze(components)?.hash() == &evidence.environment_fingerprint
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
            "environment evidence {evidence_hash} has an invalid run/session binding"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "environment".into(),
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_hash: None,
        check_fingerprint: None,
        verification_result: None,
        observed_at_ms: Some(evidence.observed_at.timestamp_millis()),
        environment_fingerprint: Some(evidence.environment_fingerprint.to_string()),
        environment_evidence_hash: None,
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
        "SELECT evidence_hash, work_id, run_id, evidence_kind,
                workspace_id, source_revision, producer_session_id,
                producer_observation_hash, check_fingerprint,
                verification_result, observed_at_ms, environment_fingerprint,
                environment_evidence_hash, components_json
         FROM work_run_evidence ORDER BY evidence_hash",
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
                producer_observation_hash: row.get(7)?,
                check_fingerprint: row.get(8)?,
                verification_result: row.get(9)?,
                observed_at_ms: row.get(10)?,
                environment_fingerprint: row.get(11)?,
                environment_evidence_hash: row.get(12)?,
                components_json: row.get(13)?,
            },
        ))
    })?;
    for row in rows {
        let (evidence_hash, projected) = row?;
        *checked += 1;
        seen.insert(evidence_hash.clone());
        if expected.get(&evidence_hash) != Some(&projected) {
            invalid.push(format!("work_evidence:{evidence_hash}:run_binding"));
        }
    }
    for evidence_hash in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("work_evidence:{evidence_hash}:missing"));
    }
    Ok(())
}

pub(super) fn verify_obligation_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let obligation_ids = connection
        .prepare("SELECT obligation_id FROM work_run_obligations ORDER BY obligation_id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut projected_definitions = HashSet::new();
    let mut projected_resolutions = HashSet::new();
    for stored_id in obligation_ids {
        *checked += 1;
        let id = uuid::Uuid::parse_str(&stored_id)
            .map(WorkObligationId)
            .map_err(|error| {
                StoreError::InvalidWorkProjection(format!(
                    "obligation projection id {stored_id:?} is invalid: {error}"
                ))
            });
        match id.and_then(|id| load_work_obligation_by_id_on(connection, id)) {
            Ok(record) => {
                projected_definitions.insert(record.definition_hash);
                if let Some(resolution) = record.resolution_hash {
                    projected_resolutions.insert(resolution);
                }
            }
            Err(_) => invalid.push(format!("work_obligation:{stored_id}")),
        }
    }
    for (kind, projected) in [
        ("work_obligation", &projected_definitions),
        ("work_obligation_resolution", &projected_resolutions),
    ] {
        let hashes = connection
            .prepare("SELECT object_hash FROM objects WHERE object_kind = ?1 ORDER BY object_hash")?
            .query_map([kind], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in hashes {
            *checked += 1;
            let hash = ObjectHash::from_stored(stored_hash.clone());
            if hash.as_ref().is_none_or(|hash| !projected.contains(hash)) {
                invalid.push(format!("{kind}:{stored_hash}:missing_projection"));
            }
        }
    }
    let expected = connection
        .prepare(
            "SELECT entry.feed_id, entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution'
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.feed_id, entry.position",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (run_id, position, stored_hash, bytes) in expected {
        *checked += 1;
        let Some(hash) = ObjectHash::from_stored(stored_hash.clone()) else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(observation) = CanonicalObject::verify(&hash, bytes)
            .and_then(|object| object.decode::<ExecutionObservation>())
        else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(rule_set) = obligation_rule_set_for_observation_on(connection, &observation) else {
            invalid.push(format!(
                "work_obligation_trigger:{run_id}:{position}:invalid_rule_set"
            ));
            continue;
        };
        for (rule, _) in crate::control::evaluate_obligation_rules(&rule_set, &observation) {
            let exists = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_run_obligations
                     WHERE run_id = ?1 AND rule_id = ?2 AND rule_version = ?3
                       AND triggering_observation_hash = ?4 AND trigger_position = ?5
                       AND rule_set_hash = ?6
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

pub(super) fn verify_completion_rows(
    connection: &Connection,
    expected: &HashMap<String, (String, String, String, serde_json::Value)>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT seal_hash, work_id, run_id, root_execution_id, seal_json
         FROM work_completion_seals ORDER BY seal_hash",
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
        let (seal_hash, work_id, run_id, root_execution_id, bytes) = row?;
        *checked += 1;
        seen.insert(seal_hash.clone());
        let projected = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let valid = expected.get(&seal_hash).is_some_and(|expected| {
            expected.0 == work_id
                && expected.1 == run_id
                && expected.2 == root_execution_id
                && projected.as_ref() == Some(&expected.3)
        });
        if !valid {
            invalid.push(format!("completion_seal:{seal_hash}:projection_binding"));
            continue;
        }
        let Ok(seal) = serde_json::from_slice::<CompletionSeal>(&bytes) else {
            invalid.push(format!("completion_seal:{seal_hash}:decode"));
            continue;
        };
        if validate_completion_seal_obligation_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_hash}:obligation_basis"));
            continue;
        }
        if validate_completion_seal_environment_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_hash}:environment_basis"));
            continue;
        }
        if validate_completion_seal_children_on(connection, &seal, 0).is_err() {
            invalid.push(format!(
                "completion_seal:{seal_hash}:child_obligation_basis"
            ));
        }
    }
    for seal_hash in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("completion_seal:{seal_hash}:missing"));
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
                entry.object_hash, entry.work_id, object.object_kind, object.canonical_json
         FROM work_feed_entries entry
         LEFT JOIN objects object ON object.object_hash = entry.object_hash
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
        let Some(hash) = ObjectHash::from_stored(stored_hash.clone()) else {
            invalid.push(label);
            continue;
        };
        let Some(bytes) = bytes else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::verify(&hash, bytes) else {
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
            "execution_observation" => object
                .decode::<ExecutionObservation>()
                .ok()
                .map(|observation| {
                    expected_execution_observation_feeds(connection, work_items, &observation)
                })
                .transpose()?
                .flatten(),
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
                        .filter(|record| record.definition_hash == hash)
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
                        .filter(|record| record.resolution_hash.as_ref() == Some(&hash))
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
            "memory_contradiction_event" => object
                .decode::<crate::domain::MemoryContradictionEvent>()
                .ok()
                .map(|event| {
                    expected_work_contradiction_feeds(connection, work_items, &stored_hash, &event)
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
        "SELECT object.object_kind, object.object_hash FROM objects object
         LEFT JOIN work_feed_entries entry
           ON entry.object_hash = object.object_hash
         WHERE object.object_kind IN (
             'work_event', 'work_checkpoint', 'work_evidence',
             'verification_evidence', 'environment_evidence',
             'work_obligation', 'work_obligation_resolution'
         )
           AND entry.object_hash IS NULL
         ORDER BY object.object_hash",
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
    object_hash: &str,
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
         WHERE object_hash = ?1 ORDER BY feed_kind, feed_id",
    )?;
    let feeds = statement
        .query_map([object_hash], |row| {
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

fn expected_work_contradiction_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    object_hash: &str,
    event: &crate::domain::MemoryContradictionEvent,
) -> Result<Option<HashSet<String>>, StoreError> {
    let Some(root_id) = event.work_root_id else {
        return Ok(None);
    };
    let project_id = &event.project_id;
    let Some(root) = work_items.get(&root_id.0.to_string()) else {
        return Ok(None);
    };
    let root_id_text = root_id.0.to_string();
    if root.get("project_id").and_then(serde_json::Value::as_str) != Some(project_id.0.as_str())
        || root.get("root_id").and_then(serde_json::Value::as_str) != Some(root_id_text.as_str())
    {
        return Ok(None);
    }
    let feeds = {
        let mut statement = connection.prepare(
            "SELECT feed_kind, feed_id FROM work_feed_entries
             WHERE object_hash = ?1 ORDER BY feed_kind, feed_id",
        )?;
        statement
            .query_map([object_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut expected = HashSet::new();
    for (kind, id) in feeds {
        let valid = match kind.as_str() {
            "project" => id == project_id.0,
            "root_work" => id == root_id_text,
            "run_execution" => connection
                .query_row(
                    "SELECT item.root_id FROM work_runs run
                     JOIN work_items item ON item.work_id = run.work_id
                     WHERE run.run_id = ?1",
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .is_some_and(|run_root| run_root == root_id_text),
            _ => false,
        };
        if !valid || !expected.insert(format!("{kind}:{id}")) {
            return Ok(None);
        }
    }
    Ok((expected.contains(&format!("project:{}", project_id.0))
        && expected.contains(&format!("root_work:{}", root_id.0)))
    .then_some(expected))
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
        let Ok(item) = serde_json::from_slice::<WorkItem>(&item_json) else {
            invalid.push(format!("work_catalog:{work_id}:item_decode"));
            continue;
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
        let mut fts_statement =
            connection.prepare("SELECT search_text FROM work_catalog_fts WHERE work_id = ?1")?;
        let fts_rows = fts_statement
            .query_map([work_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let fts_index_valid = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_catalog_fts
                     WHERE work_id = ?1 AND work_catalog_fts MATCH ?2
                 )",
                params![
                    work_id.as_str(),
                    catalog_literal_fts_query(&expected_search)
                ],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if item.work_id.0.to_string() != work_id
            || assigned_to_key != expected_assigned
            || search_text_key != expected_search
            || actual_labels != expected_labels
            || fts_rows.len() != 1
            || fts_rows.first() != Some(&expected_search)
        {
            invalid.push(format!("work_catalog:{work_id}:projection_binding"));
        }
        if !fts_index_valid {
            invalid.push(format!("work_catalog:{work_id}:fts_index"));
        }
    }
    drop(statement);
    let orphaned_fts = connection.query_row(
        "SELECT COUNT(*) FROM work_catalog_fts catalog
         WHERE NOT EXISTS (
             SELECT 1 FROM work_items item WHERE item.work_id = catalog.work_id
         )",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if orphaned_fts != 0 {
        invalid.push("work_catalog:orphaned_fts_rows".into());
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
             COALESCE(source_snapshot_hash, '') != COALESCE(json_extract(item_json, '$.source_snapshot_id'), '')",
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
             COALESCE(last_checkpoint_hash, '') != COALESCE(json_extract(run_json, '$.last_checkpoint'), '') OR
             COALESCE(completion_seal_hash, '') != COALESCE(json_extract(run_json, '$.completion_seal'), '')",
        ),
        (
            "work_root_execution",
            "SELECT root_execution_id FROM work_root_executions WHERE
             root_execution_id != json_extract(execution_json, '$.root_execution_id') OR
             project_id != json_extract(execution_json, '$.project_id') OR
             root_id != json_extract(execution_json, '$.root_id') OR
             generation != json_extract(execution_json, '$.generation') OR
             state != json_extract(execution_json, '$.state') OR
             revision != json_extract(execution_json, '$.revision')",
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
             offer_hash IS NULL OR
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
         COALESCE(item.latest_event_hash, '') != COALESCE((
             SELECT entry.object_hash FROM work_feed_entries entry
             WHERE entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = item.work_id
             ORDER BY entry.position DESC LIMIT 1
         ), '')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        *checked += 1;
        invalid.push(format!("work_item:{}:latest_event_hash", row?));
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
        "SELECT root_execution_id, created_at_ms, updated_at_ms, execution_json
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
        let valid = serde_json::from_slice::<RootExecution>(&bytes).is_ok_and(|execution| {
            created_at == execution.created_at.timestamp_millis()
                && updated_at == execution.updated_at.timestamp_millis()
        });
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
            "SELECT projection.seal_hash, projection.seal_json,
                    object.object_kind, object.canonical_json
             FROM work_completion_seals projection
             LEFT JOIN objects object ON object.object_hash = projection.seal_hash
             ORDER BY projection.seal_hash",
            "completion_seal",
        ),
        (
            "work_handoff_offer",
            "SELECT projection.offer_hash, projection.offer_json,
                    object.object_kind, object.canonical_json
             FROM work_handoff_offers projection
             LEFT JOIN objects object ON object.object_hash = projection.offer_hash
             ORDER BY projection.offer_id",
            "work_handoff_offer",
        ),
    ];
    for (label, sql, expected_kind) in projections {
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
                ObjectHash::from_stored(stored_hash.clone()),
                canonical.as_ref(),
            ) {
                (Some(hash), Some(bytes)) => {
                    CanonicalObject::verify(&hash, bytes.clone()).is_ok()
                        && object_kind.as_deref() == Some(expected_kind)
                        && serde_json::from_slice::<serde_json::Value>(&projection).ok()
                            == serde_json::from_slice::<serde_json::Value>(bytes).ok()
                }
                _ => false,
            };
            if !valid {
                invalid.push(format!("{label}:{stored_hash}"));
            }
        }
    }
    Ok(())
}

pub(super) fn verify_work_protocol_attempts(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT project_id, session_id, operation, idempotency_key,
                request_hash, basis_hash, basis_json, result_hash, result_json
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
            result_hash,
            result_json,
        ) = row?;
        *checked += 1;
        let label = format!("work_protocol_attempt:{project_id}:{session_id}:{operation}:{key}");
        let request_valid = ObjectHash::from_stored(request_hash).is_some();
        let basis_valid = match (&basis_hash, &basis_json, &result_hash, &result_json) {
            (Some(stored_hash), Some(bytes), None, None) => {
                ObjectHash::from_stored(stored_hash.clone())
                    .is_some_and(|hash| CanonicalObject::verify(&hash, bytes.clone()).is_ok())
            }
            (stored_hash, None, Some(_), Some(_)) => stored_hash
                .as_ref()
                .is_none_or(|hash| ObjectHash::from_stored(hash.clone()).is_some()),
            _ => false,
        };
        let result_valid = match (result_hash, result_json) {
            (None, None) => true,
            (Some(stored_hash), Some(bytes)) => ObjectHash::from_stored(stored_hash)
                .and_then(|hash| {
                    load_typed_work_object::<serde_json::Value>(
                        connection,
                        &hash,
                        "work_protocol_result",
                    )
                    .ok()
                    .map(|value| (hash, value))
                })
                .is_some_and(|(hash, value)| {
                    CanonicalObject::freeze(&value).is_ok_and(|object| {
                        object.hash() == &hash
                            && object.bytes() == bytes
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
        if !request_valid || !basis_valid || !result_valid {
            invalid.push(label);
        }
    }
    Ok(())
}
