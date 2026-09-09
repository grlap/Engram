//! Locate observed pre-seal states. Never synthesize a missing predecessor.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CanonicalObject, ObjectHash, RootExecution};

use super::{MigrationError, read_only, refused};

#[cfg(test)]
mod tests;

/// All header fields of a migrated pre-seal head come from this exact previous
/// event's `root_execution`: `schema_version`, `root_execution_id`, `project_id`,
/// `root_id`, `generation`, `state`, `revision`, `created_at` and `updated_at`. None is
/// defaulted, inferred from the clock, or copied backward from completion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreSealBinding {
    pub seal: ObjectHash,
    pub pre_event: ObjectHash,
    pub completed_event: ObjectHash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreSealPlan {
    pub events: u64,
    pub root_executions: usize,
    pub bindings: Vec<PreSealBinding>,
}

/// Audits the aggregate-root source representation using verified canonical
/// objects in project-feed order and the complete current root projections.
/// This produces a conversion basis, not an import or an authority grant.
///
/// # Errors
/// Refuses missing or ambiguous predecessors, altered canonical bytes, missing
/// seals, mismatched accounting or a completion whose exact transition does
/// not produce the observed post-state. There is no subtraction fallback.
pub fn inspect_pre_seal_history(source: &Path) -> Result<PreSealPlan, MigrationError> {
    let source = read_only(source)?;
    let snapshot = source.unchecked_transaction()?;
    let plan = inspect_on(&snapshot)?;
    snapshot.commit()?;
    Ok(plan)
}

struct ObservedRoot {
    event: ObjectHash,
    value: Value,
}

fn verified_value(hash: &str, bytes: Vec<u8>) -> Result<(ObjectHash, Value), MigrationError> {
    let hash = hash
        .parse::<ObjectHash>()
        .map_err(|_| refused("invalid canonical address"))?;
    let object = CanonicalObject::verify(&hash, bytes)?;
    Ok((hash, object.decode()?))
}

fn root_identity(root: &Value) -> Result<String, MigrationError> {
    // Round-trip equality refuses unknown or silently defaulted fields.
    let typed: RootExecution = serde_json::from_value(root.clone())?;
    if serde_json::to_value(&typed)? != *root {
        return Err(refused("unrecognized aggregate-root fields"));
    }
    Ok(typed.root_execution_id.0.to_string())
}

pub(super) fn inspect_on(source: &Connection) -> Result<PreSealPlan, MigrationError> {
    let mut query = source.prepare("SELECT f.object_hash, o.canonical_json FROM work_feed_entries f JOIN objects o ON o.object_hash = f.object_hash WHERE f.feed_kind = 'project' AND f.object_kind = 'work_event' AND o.object_kind = 'work_event' ORDER BY f.feed_id, f.position")?;
    let mut selected = query.query([])?;
    let mut previous: HashMap<String, ObservedRoot> = HashMap::new();
    let mut used_seals = HashSet::new();
    let mut bindings = Vec::new();
    let mut events = 0_u64;
    while let Some(row) = selected.next()? {
        let (hash, event) = verified_value(&row.get::<_, String>(0)?, row.get(1)?)?;
        events = events
            .checked_add(1)
            .ok_or_else(|| refused("event count overflow"))?;
        let post = event
            .get("root_execution")
            .ok_or_else(|| refused("work event has no root field"))?;
        if post.is_null() {
            continue;
        }
        let identity = root_identity(post)?;
        if post.get("project_id") != event.get("project_id")
            || post.get("root_id") != event.get("root_id")
        {
            return Err(refused("event and aggregate-root identity differ"));
        }
        if event.pointer("/transition/kind").and_then(Value::as_str) == Some("completed") {
            let prior = previous
                .get(&identity)
                .ok_or_else(|| refused("completion has no observed root predecessor"))?;
            let seal_hash = event
                .pointer("/transition/seal")
                .and_then(Value::as_str)
                .ok_or_else(|| refused("completion has no seal address"))?;
            let bytes = source.query_row("SELECT canonical_json FROM objects WHERE object_hash = ?1 AND object_kind = 'completion_seal'", [seal_hash], |row| row.get(0))?;
            let (seal_hash, seal) = verified_value(seal_hash, bytes)?;
            if !used_seals.insert(seal_hash.clone()) {
                return Err(refused("seal has multiple completion events"));
            }
            require_completion(&prior.value, post, &event, &seal, &seal_hash)?;
            bindings.push(PreSealBinding {
                seal: seal_hash,
                pre_event: prior.event.clone(),
                completed_event: hash.clone(),
            });
        }
        previous.insert(
            identity,
            ObservedRoot {
                event: hash,
                value: post.clone(),
            },
        );
    }
    let seals: i64 = source.query_row(
        "SELECT COUNT(*) FROM objects WHERE object_kind = 'completion_seal'",
        [],
        |row| row.get(0),
    )?;
    if usize::try_from(seals).ok() != Some(bindings.len()) {
        return Err(refused(
            "not every canonical seal has an observed predecessor",
        ));
    }
    verify_current_roots(source, &previous)?;
    Ok(PreSealPlan {
        events,
        root_executions: previous.len(),
        bindings,
    })
}

fn require_completion(
    pre: &Value,
    post: &Value,
    event: &Value,
    seal: &Value,
    hash: &ObjectHash,
) -> Result<(), MigrationError> {
    for field in ["root_execution_id", "root_id"] {
        if pre.get(field) != post.get(field) || pre.get(field) != seal.get(field) {
            return Err(refused("completion crosses root identity"));
        }
    }
    for field in ["work_id", "run_id"] {
        if seal.get(field) != event.get(field) {
            return Err(refused("seal and completion target differ"));
        }
    }
    for field in ["expected_contributors", "contributions", "waivers"] {
        if pre.get(field).is_none() || pre.get(field) != seal.get(field) {
            return Err(refused(format!("pre-seal accounting differs: {field}")));
        }
    }
    let mut expected = pre.clone();
    let fields = expected
        .as_object_mut()
        .ok_or_else(|| refused("root is not an object"))?;
    if event.get("work_id") == event.get("root_id") {
        fields.insert("state".into(), Value::String("completed".into()));
        fields.insert(
            "required_child_seals".into(),
            seal.get("required_child_seals")
                .cloned()
                .ok_or_else(|| refused("seal has no child set"))?,
        );
    } else if event
        .pointer("/work/child_requirement")
        .and_then(Value::as_str)
        == Some("required")
    {
        let mut children: Vec<ObjectHash> = serde_json::from_value(
            fields
                .get("required_child_seals")
                .cloned()
                .ok_or_else(|| refused("root has no child set"))?,
        )?;
        children.push(hash.clone());
        children.sort();
        children.dedup();
        fields.insert(
            "required_child_seals".into(),
            serde_json::to_value(children)?,
        );
    }
    let revision = pre
        .get("revision")
        .and_then(Value::as_i64)
        .and_then(|revision| revision.checked_add(1))
        .ok_or_else(|| refused("invalid pre-seal revision"))?;
    fields.insert("revision".into(), Value::from(revision));
    fields.insert(
        "updated_at".into(),
        seal.get("completed_at")
            .cloned()
            .ok_or_else(|| refused("seal has no completion time"))?,
    );
    if expected != *post {
        return Err(refused(
            "observed predecessor plus completion does not equal post-state",
        ));
    }
    Ok(())
}

fn verify_current_roots(
    source: &Connection,
    previous: &HashMap<String, ObservedRoot>,
) -> Result<(), MigrationError> {
    let mut query =
        source.prepare("SELECT root_execution_id, execution_json FROM work_root_executions")?;
    let mut rows = query.query([])?;
    let mut count = 0_usize;
    while let Some(row) = rows.next()? {
        let identity: String = row.get(0)?;
        let value: Value = serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?;
        if previous
            .get(&identity)
            .is_none_or(|observed| observed.value != value)
        {
            return Err(refused("current root differs from its last observed state"));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| refused("root count overflow"))?;
    }
    if count != previous.len() {
        return Err(refused("observed and projected root sets differ"));
    }
    Ok(())
}
