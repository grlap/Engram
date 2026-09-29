//! Named-root history on each run feed. Freshness trusts a binding event and
//! the generation a sighting states, so the doctor reads them back in feed
//! order: a binding belongs to its run's claim and carries this schema, each
//! claim's bound generations only increase, an end repeats the generation,
//! workspace and naming time of the bound event it ends, and a sighting or
//! evidence record that states a generation names an event recorded before
//! it.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::{NamedRootBindingEvent, NamedRootBindingKind, SCHEMA_VERSION};
use crate::storage::StoreError;
use crate::{CanonicalObject, ObjectId};

/// One claim's bound generations on a run, each with its workspace, naming
/// time and whether it has ended.
#[derive(Default)]
struct ClaimRoots {
    bound: HashMap<i64, (String, chrono::DateTime<chrono::Utc>, bool)>,
    newest: i64,
}

pub(in crate::storage::work) fn verify_named_root_history(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT entry.feed_id, entry.object_kind, entry.object_id, object.canonical_json
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution'
           AND (entry.object_kind = 'named_root_binding'
                OR (entry.object_kind IN
                        ('execution_observation', 'verification_evidence', 'environment_evidence')
                    AND (json_extract(object.canonical_json,
                                      '$.source_basis.source_root_generation') IS NOT NULL
                         OR json_extract(object.canonical_json,
                                         '$.source_basis.source_root_state') IS NOT NULL)))
         ORDER BY entry.feed_id, entry.position",
    )?;
    let mut rows = statement.query([])?;
    let mut claims: HashMap<(String, String), ClaimRoots> = HashMap::new();
    let mut run_claims: HashMap<String, Option<String>> = HashMap::new();
    while let Some(row) = rows.next()? {
        *checked += 1;
        let run_id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let stored_id: String = row.get(2)?;
        let bytes: Vec<u8> = row.get(3)?;
        if kind == "named_root_binding" {
            let run_claim = if let Some(claim) = run_claims.get(&run_id) {
                claim.clone()
            } else {
                let claim: Option<String> = connection
                    .query_row(
                        "SELECT claim_id FROM work_claims WHERE run_id = ?1",
                        params![run_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                run_claims.insert(run_id.clone(), claim.clone());
                claim
            };
            let event = ObjectId::from_stored(stored_id.clone())
                .and_then(|id| CanonicalObject::stored(&id, bytes).ok())
                .and_then(|object| object.decode::<NamedRootBindingEvent>().ok());
            let valid = event.is_some_and(|event| {
                let claim = event.claim_id.0.to_string();
                event.schema_version == SCHEMA_VERSION
                    && event.run_id.0.to_string() == run_id
                    && run_claim.as_deref() == Some(claim.as_str())
                    && record_binding(claims.entry((run_id.clone(), claim)).or_default(), &event)
            });
            if !valid {
                invalid.push(format!("named_root_binding:{stored_id}"));
            }
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let basis = &value["source_basis"];
        let generation = basis["source_root_generation"].as_i64();
        let state = basis["source_root_state"].as_str();
        let claim = value["binding"]["claim_id"].as_str().unwrap_or_default();
        let named = claims.get(&(run_id.clone(), claim.to_owned()));
        let valid = match (generation, state) {
            (Some(generation), Some(state)) if generation > 0 => named
                .and_then(|roots| roots.bound.get(&generation))
                .is_some_and(|(_, _, ended)| match state {
                    "named" => true,
                    "ended" => *ended,
                    _ => false,
                }),
            _ => false,
        };
        if !valid {
            invalid.push(format!(
                "{kind}:{stored_id}:source root generation without its recorded binding"
            ));
        }
    }
    Ok(())
}

/// Applies one binding event to its claim's history, or says it does not
/// fit: a bound generation must exceed every earlier one, and an end must
/// name the newest bound generation, once, repeating its workspace and
/// naming time, as the host writer requires.
fn record_binding(roots: &mut ClaimRoots, event: &NamedRootBindingEvent) -> bool {
    match event.kind {
        NamedRootBindingKind::Bound => {
            if event.generation <= roots.newest {
                return false;
            }
            roots.newest = event.generation;
            roots.bound.insert(
                event.generation,
                (event.workspace_id.clone(), event.named_at, false),
            );
            true
        }
        // Only the newest bound generation can end: a later name has already
        // superseded an older one.
        NamedRootBindingKind::Ended => match roots.bound.get_mut(&event.generation) {
            Some((workspace, named_at, ended))
                if event.generation == roots.newest
                    && !*ended
                    && *workspace == event.workspace_id
                    && *named_at == event.named_at =>
            {
                *ended = true;
                true
            }
            _ => false,
        },
    }
}
