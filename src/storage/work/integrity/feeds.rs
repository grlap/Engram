//! Work-feed membership and ordering checks.

use super::{
    CanonicalObject, Connection, EnvironmentEvidence, ExecutionObservation, HashMap, HashSet,
    MemoryAssertionEvent, MemoryVersion, NamedRootBindingEvent, NamedRootBindingKind, ObjectId,
    ObligationAudit, OptionalExtension, RestoredWorkEvidence, SCHEMA_VERSION, StoreError,
    VerificationEvidence, WorkCheckpoint, WorkEvent, WorkEvidence, WorkId, WorkObligation,
    WorkObligationResolutionEvent, WorkRunId, expected_environment_projection,
    expected_verification_projection, load_typed_work_object, params,
};

pub(in crate::storage::work) fn verify_work_feed_integrity(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    obligations: &mut ObligationAudit<'_>,
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
            "task_sync_reconciliation_hold" => object
                .decode::<crate::task_sync::reconciliation::ReconciliationHold>()
                .ok()
                .map(|hold| HashSet::from([format!("project:{}", hold.project_id.0)])),
            "task_sync_shared_import_version" => object
                .decode::<crate::task_sync::reconciliation::SharedImportVersion>()
                .ok()
                .map(|version| HashSet::from([format!("project:{}", version.project_id.0)])),
            "registered_host_run" => object
                .decode::<crate::domain::RegisteredHostRun>()
                .ok()
                .filter(|producer| {
                    producer.schema_version == SCHEMA_VERSION
                        && producer.session_id
                            == producer.request.registration.initiating_session_id
                        && producer.actor.session_id.as_ref() == Some(&producer.session_id)
                        && producer.actor.run_id.as_deref()
                            == Some(
                                producer
                                    .request
                                    .registration
                                    .binding
                                    .run_id
                                    .0
                                    .to_string()
                                    .as_str(),
                            )
                        && crate::storage::work::registered_verification::validate_request(
                            &producer.request,
                            producer.recorded_at,
                        )
                        .is_ok()
                })
                .and_then(|producer| {
                    expected_feeds_for_work(
                        work_items,
                        producer.request.registration.binding.work_id,
                        Some(producer.request.registration.binding.run_id),
                    )
                }),
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
                    crate::storage::work::query::load_work_item(connection, proposal.work_id)
                        .ok()
                        .filter(|item| {
                            crate::storage::work::import::validate_proposal_on(
                                connection, item, &proposal,
                            )
                            .is_ok()
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
            crate::storage::work::UNADMITTED_OBSERVATION_KIND => {
                match object.decode::<crate::domain::UnadmittedExecutionObservation>() {
                    Ok(observation)
                        if crate::storage::work::unadmitted_observation_is_consistent_on(
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
                    obligations
                        .binding(obligation.obligation_id)
                        .ok()
                        .filter(|record| record.definition == hash)
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
                    obligations
                        .binding(event.obligation_id)
                        .ok()
                        .filter(|record| record.resolution.as_ref() == Some(&hash))
                })
                .map(|obligation| {
                    expected_work_feeds(
                        &obligation.project.0,
                        obligation.root,
                        Some(obligation.run),
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
            crate::storage::work::task_sync::folder::IMPORT_PLANNING_KIND => {
                crate::storage::work::task_sync::folder::planning_project(&object).map(|project| {
                    std::collections::HashSet::from([format!("project:{}", project.0)])
                })
            }
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
              'verification_evidence', 'environment_evidence', 'named_root_binding', 'registered_host_run',
             'work_obligation', 'work_obligation_resolution', 'unadmitted_execution_observation',
             'task_sync_import_planning', 'task_sync_reconciliation_hold',
             'task_sync_shared_import_version'
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
