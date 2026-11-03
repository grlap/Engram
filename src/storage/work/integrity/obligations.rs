//! Obligation and completion integrity checks.

use super::{
    CanonicalObject, CompletionSeal, Connection, HashMap, HashSet, ObjectId, ObligationAudit,
    StoreError, WorkObligation, WorkObligationId, canonical_without_its_representation,
    decode_failure_label, decode_preserved_projection, decode_projection_bytes,
    load_typed_work_object, obligation_rule_set_for_observation_on, params,
    validate_completion_seal_children_on, validate_completion_seal_environment_basis_on,
    validate_completion_seal_obligation_basis_on,
};

pub(in crate::storage::work) fn verify_obligation_rows(
    connection: &Connection,
    obligations: &mut ObligationAudit<'_>,
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
    let mut reported_decode_failures = HashSet::new();
    for (stored_id, stored_definition) in obligation_rows {
        *checked += 1;
        let id = uuid::Uuid::parse_str(&stored_id)
            .map(WorkObligationId)
            .map_err(|error| {
                StoreError::InvalidWorkProjection(format!(
                    "obligation projection id {stored_id:?} is invalid: {error}"
                ))
            });
        if let Ok(record) = id.and_then(|id| obligations.binding(id)) {
            projected_definitions.insert(record.definition);
            if let Some(resolution) = record.resolution {
                projected_resolutions.insert(resolution);
            }
        } else {
            // The run's obligations load together, so one unreadable record
            // fails its siblings too. Only a record whose own definition
            // cannot be decoded carries the reason, such as a member the
            // current format has no place for.
            let label = format!("work_obligation:{stored_id}");
            let own_definition =
                ObjectId::from_stored(stored_definition.clone()).map(|definition| {
                    load_typed_work_object::<WorkObligation>(
                        connection,
                        &definition,
                        "work_obligation",
                    )
                });
            invalid.push(if let Some(Err(error)) = own_definition {
                let finding = decode_failure_label(label.clone(), &error);
                if finding != label {
                    reported_decode_failures.insert(stored_definition);
                }
                finding
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
                if kind == "work_obligation" && !reported_decode_failures.contains(&stored_hash) {
                    // Successful audit facts already validated their definitions.
                    // Decode other bodies, including orphans with invalid ids,
                    // without repeating a definition's already reported reason.
                    let bytes: Vec<u8> = connection.query_row(
                        "SELECT canonical_json FROM objects WHERE object_id = ?1",
                        [&stored_hash],
                        |row| row.get(0),
                    )?;
                    if let Err(error) = CanonicalObject::decode_bytes::<WorkObligation>(&bytes) {
                        invalid.push(decode_failure_label(
                            format!("{kind}:{stored_hash}"),
                            &error,
                        ));
                    }
                }
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
            crate::storage::work::feeds::SOURCE_RECORD_SQL,
            crate::storage::work::feeds::SOURCE_CHANGED_SQL
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
        let Ok(observation) =
            crate::storage::work::feeds::load_source_observation_on(connection, &hash)
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
    crate::storage::work::DOCTOR_FINISHED_RUN_CUT_READS.with(|count| count.set(count.get() + 1));
    let Ok(parsed) = crate::storage::work::query::parse_work_run_id(run_id) else {
        return Ok(None);
    };
    crate::storage::work::completion::finished_run_cut_on(connection, parsed)
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

pub(in crate::storage::work) fn verify_completion_rows(
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
        if crate::storage::work::acceptance_evaluation::validate_completion_seal_acceptance_evaluation_on(
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
