use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::super::{StoreError, decode_failure_label};
use super::EvidenceProjectionRow;
use super::completion::{
    ObligationAudit, obligation_rule_set_for_observation_on, validate_completion_seal_children_on,
    validate_completion_seal_environment_basis_on, validate_completion_seal_obligation_basis_on,
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

mod evidence;
pub(super) use evidence::{
    expected_environment_projection, expected_verification_projection, verify_evidence_rows,
    verify_restored_evidence_rows,
};
mod obligations;
pub(super) use obligations::{verify_completion_rows, verify_obligation_rows};
mod feeds;
pub(super) use feeds::verify_work_feed_integrity;
mod projections;
pub(super) use projections::{
    verify_canonical_work_rows, verify_work_catalog_projections, verify_work_protocol_attempts,
    verify_work_scalar_bindings,
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

fn decode_projection_bytes<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Option<T> {
    // Decode the original bytes first: Value would collapse duplicate known
    // members before serde could reject them, including nested/enum fields.
    let decoded = serde_json::from_slice::<T>(bytes).ok()?;
    let stored = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    preserves_projection_representation(&stored, &decoded).then_some(decoded)
}
