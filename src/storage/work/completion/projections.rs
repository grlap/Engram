use super::{
    CanonicalObject, ChildRequirement, CompletionSeal, Connection, EvidenceProjectionRow, FeedId,
    HashMap, HashSet, MemoryAssertionEvent, MemoryVersion, ObjectHash, OptionalExtension,
    RestoredRecord, RootExecution, SCHEMA_VERSION, SqliteStore, StoreError, WorkBlocker, WorkClaim,
    WorkClaimState, WorkEvent, WorkEvidence, WorkEvidenceKind, WorkHandoffOffer, WorkId, WorkItem,
    WorkLifecycle, WorkRelationBasis, WorkRelationBlockerBasis, WorkRun, WorkRunState,
    WorkTransition, apply_work_relation_transition, empty_work_relation_basis,
    expected_environment_projection, expected_verification_projection,
    latest_canonical_work_event_for_item_optional, load_typed_work_object, load_work_item,
    parse_work_id, validate_gate_evidence_chain, validate_work_evidence_event_phase_on,
    verify_anchored_memory_feeds, verify_blocker_rows, verify_canonical_work_rows,
    verify_completion_rows, verify_evidence_rows, verify_json_projection, verify_obligation_rows,
    verify_prerequisite_rows, verify_required_child_waiver_bindings, verify_restored_evidence_rows,
    verify_work_catalog_projections, verify_work_feed_integrity, verify_work_protocol_attempts,
    verify_work_scalar_bindings, work_relation_fingerprint,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    pub(in crate::storage) fn verify_work_projections(
        &self,
    ) -> Result<(usize, Vec<String>), StoreError> {
        Self::verify_work_projections_on(&self.connection)
    }

    pub(in crate::storage) fn verify_work_projections_on(
        connection: &Connection,
    ) -> Result<(usize, Vec<String>), StoreError> {
        let mut checked = 0_usize;
        let mut invalid = Vec::new();
        let mut seen_events = HashSet::new();
        let mut work_items = HashMap::new();
        let mut runs = HashMap::new();
        let mut root_executions = HashMap::new();
        let mut claims = HashMap::new();
        let mut handoffs = HashMap::new();
        let mut blockers = HashMap::new();
        let mut prerequisite_rows = HashMap::new();
        let mut blocker_rows = HashMap::new();
        let mut relation_bases = HashMap::new();
        let mut evidence_rows = HashMap::new();
        let mut gate_heads = HashMap::new();
        let mut completion_rows = HashMap::new();

        seed_restored_projection_expectations(
            connection,
            &mut work_items,
            &mut blockers,
            &mut prerequisite_rows,
            &mut blocker_rows,
            &mut relation_bases,
            &mut checked,
            &mut invalid,
        )?;

        let mut statement = connection.prepare(
            "SELECT entry.feed_id, entry.position, entry.object_hash,
                    object.object_kind, object.canonical_json
             FROM work_feed_entries entry
             LEFT JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'project' AND entry.object_kind = 'work_event'
             ORDER BY entry.feed_id, entry.position",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })?;
        for row in rows {
            let (feed_id, position, stored_hash, object_kind, bytes) = row?;
            checked += 1;
            let label = format!("work_event:{feed_id}:{position}:{stored_hash}");
            let Some(hash) = ObjectHash::from_stored(stored_hash) else {
                invalid.push(label);
                continue;
            };
            let Some(bytes) = bytes else {
                invalid.push(label);
                continue;
            };
            if object_kind.as_deref() != Some("work_event") || !seen_events.insert(hash.clone()) {
                invalid.push(label);
                continue;
            }
            let event = CanonicalObject::verify(&hash, bytes).and_then(|object| {
                Ok((
                    object.decode::<WorkEvent>()?,
                    object.decode::<serde_json::Value>()?,
                ))
            });
            let Ok((event, mut event_json)) = event else {
                invalid.push(label);
                continue;
            };
            let internally_bound = event.schema_version == SCHEMA_VERSION
                && event.project_id.0 == feed_id
                && event.work_id == event.work.work_id
                && event.root_id == event.work.root_id
                && event.project_id == event.work.project_id
                && event.revision == event.work.revision
                && event.run.as_ref().is_none_or(|run| {
                    event.run_id == Some(run.run_id) && run.work_id == event.work_id
                })
                && event.root_execution.as_ref().is_none_or(|execution| {
                    execution.project_id == event.project_id && execution.root_id == event.root_id
                })
                && event.claim.as_ref().is_none_or(|claim| {
                    claim.work_id == event.work_id && event.run_id == Some(claim.run_id)
                })
                && event.handoff_offer.as_ref().is_none_or(|offer| {
                    offer.work_id == event.work_id && event.run_id == Some(offer.run_id)
                })
                && event
                    .blocker
                    .as_ref()
                    .is_none_or(|blocker| blocker.work_id == event.work_id);
            if !internally_bound {
                invalid.push(label);
                continue;
            }
            let current_basis = relation_bases
                .entry(event.work_id)
                .or_insert_with(empty_work_relation_basis);
            if apply_work_relation_transition(
                current_basis,
                &event.transition,
                event.blocker.as_ref(),
            )
            .is_err()
                || work_relation_fingerprint(current_basis)
                    .map_or(true, |actual| actual != event.relation_fingerprint)
            {
                invalid.push(format!("{label}:relation_fingerprint"));
                continue;
            }
            match &event.transition {
                WorkTransition::Created { prerequisites } => {
                    for prerequisite in prerequisites {
                        prerequisite_rows.insert(
                            (event.work_id.0.to_string(), prerequisite.0.to_string()),
                            hash.as_str().to_owned(),
                        );
                    }
                }
                WorkTransition::Disposed {
                    lifecycle,
                    replacement_id,
                    ..
                } => {
                    let transition_is_bound = *lifecycle == event.work.lifecycle
                        && *replacement_id == event.work.superseded_by
                        && matches!(
                            lifecycle,
                            WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                        );
                    if !transition_is_bound {
                        invalid.push(format!("{label}:invalid_disposal_binding"));
                    }
                }
                WorkTransition::RequiredChildWaived {
                    child_id,
                    child_revision,
                    ..
                } => {
                    let child = load_work_item(connection, *child_id);
                    let transition_is_bound = child.as_ref().is_ok_and(|child| {
                        child.parent_id == Some(event.work_id)
                            && child.child_requirement == ChildRequirement::Required
                            && matches!(
                                child.lifecycle,
                                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                            )
                            && child.revision == *child_revision
                            && event.root_execution.as_ref().is_some_and(|execution| {
                                execution.required_child_waivers.iter().any(|waiver| {
                                    waiver.work_id == *child_id
                                        && waiver.work_revision == *child_revision
                                })
                            })
                    });
                    if !transition_is_bound {
                        invalid.push(format!("{label}:invalid_required_child_waiver"));
                    }
                }
                WorkTransition::PrerequisiteAdded {
                    prerequisite_id, ..
                } => {
                    prerequisite_rows.insert(
                        (event.work_id.0.to_string(), prerequisite_id.0.to_string()),
                        hash.as_str().to_owned(),
                    );
                }
                WorkTransition::PrerequisiteRemoved {
                    prerequisite_id, ..
                } => {
                    prerequisite_rows
                        .remove(&(event.work_id.0.to_string(), prerequisite_id.0.to_string()));
                }
                WorkTransition::Blocked { blocker_id } => {
                    if event
                        .blocker
                        .as_ref()
                        .map(|blocker| blocker.blocker_id.as_str())
                        == Some(blocker_id.as_str())
                    {
                        blocker_rows.insert(
                            blocker_id.clone(),
                            (
                                "active".to_owned(),
                                hash.as_str().to_owned(),
                                None::<String>,
                            ),
                        );
                    } else {
                        invalid.push(label.clone());
                    }
                }
                WorkTransition::Unblocked { blocker_id } => {
                    if let Some((state, _, cleared)) = blocker_rows.get_mut(blocker_id) {
                        *state = "cleared".into();
                        *cleared = Some(hash.as_str().to_owned());
                    } else {
                        invalid.push(format!("{label}:missing_block_event"));
                    }
                }
                WorkTransition::EvidenceAdded { evidence } => {
                    match load_typed_work_object::<WorkEvidence>(
                        connection,
                        evidence,
                        "work_evidence",
                    ) {
                        Ok(value)
                            if value.work_id == event.work_id
                                && Some(value.run_id) == event.run_id =>
                        {
                            if validate_work_evidence_event_phase_on(
                                connection, evidence, &value, &event,
                            )
                            .is_err()
                            {
                                invalid.push(format!("{label}:invalid_evidence_phase"));
                                continue;
                            }
                            if validate_gate_evidence_chain(evidence, &value, &mut gate_heads)
                                .is_err()
                            {
                                invalid.push(format!("{label}:invalid_gate_chain"));
                                continue;
                            }
                            evidence_rows.insert(
                                evidence.as_str().to_owned(),
                                EvidenceProjectionRow {
                                    work_id: value.work_id.0.to_string(),
                                    run_id: value.run_id.0.to_string(),
                                    evidence_kind: "generic".into(),
                                    workspace_id: None,
                                    source_revision: None,
                                    producer_session_id: None,
                                    producer_observation_hash: None,
                                    check_fingerprint: None,
                                    verification_result: None,
                                    observed_at_ms: None,
                                    environment_fingerprint: None,
                                    environment_evidence_hash: None,
                                    components_json: None,
                                },
                            );
                        }
                        _ => invalid.push(format!("{label}:invalid_evidence_binding")),
                    }
                }
                WorkTransition::MemoryCaptured { version, assertion } => {
                    let binding = load_typed_work_object::<MemoryVersion>(
                        connection,
                        version,
                        "memory_version",
                    )
                    .and_then(|memory| {
                        let assertion_event = load_typed_work_object::<MemoryAssertionEvent>(
                            connection,
                            assertion,
                            "memory_assertion_event",
                        )?;
                        Ok((memory, assertion_event))
                    });
                    let valid = binding.is_ok_and(|(memory, assertion_event)| {
                        matches!(
                            memory.scope,
                            crate::domain::Scope::Work { ref project, work }
                                if project == &event.project_id && work == event.work_id
                        ) && memory.memory_id == assertion_event.memory_id
                            && assertion_event.version == *version
                            && memory.actor == event.actor
                            && assertion_event.actor == event.actor
                            && memory.created_at == event.created_at
                            && assertion_event.created_at == event.created_at
                            && event.claim.as_ref().is_some_and(|claim| {
                                event.actor.session_id.as_ref() == Some(&claim.holder)
                                    && claim.state == WorkClaimState::Active
                                    && claim.expires_at > event.created_at
                            })
                    });
                    if !valid {
                        invalid.push(format!("{label}:invalid_memory_capture_binding"));
                    }
                }
                WorkTransition::TypedEvidenceAdded {
                    evidence,
                    evidence_kind,
                } => {
                    let expected = match evidence_kind {
                        WorkEvidenceKind::Generic => None,
                        WorkEvidenceKind::Verification => {
                            expected_verification_projection(connection, evidence).ok()
                        }
                        WorkEvidenceKind::Environment => {
                            expected_environment_projection(connection, evidence).ok()
                        }
                    };
                    if let Some(expected) = expected.filter(|expected| {
                        expected.work_id == event.work_id.0.to_string()
                            && event
                                .run_id
                                .is_some_and(|run_id| expected.run_id == run_id.0.to_string())
                    }) {
                        evidence_rows.insert(evidence.as_str().to_owned(), expected);
                    } else {
                        invalid.push(format!("{label}:invalid_typed_evidence_binding"));
                    }
                }
                WorkTransition::Completed { seal } => {
                    match load_typed_work_object::<serde_json::Value>(
                        connection,
                        seal,
                        "completion_seal",
                    )
                    .and_then(|json| {
                        let value = serde_json::from_value::<CompletionSeal>(json.clone())?;
                        Ok((value, json))
                    }) {
                        Ok((value, json))
                            if value.work_id == event.work_id
                                && value.root_id == event.root_id
                                && Some(value.run_id) == event.run_id
                                && value.completion_cut.feed
                                    == FeedId::RunExecution(value.run_id)
                                && event.run.as_ref().is_some_and(|run| {
                                    run.state == WorkRunState::Completed
                                        && run.completion_seal.as_ref() == Some(seal)
                                }) =>
                        {
                            completion_rows.insert(
                                seal.as_str().to_owned(),
                                (
                                    value.work_id.0.to_string(),
                                    value.run_id.0.to_string(),
                                    value.root_execution_id.0.to_string(),
                                    json,
                                ),
                            );
                        }
                        _ => invalid.push(format!("{label}:invalid_completion_binding")),
                    }
                }
                _ => {}
            }
            // Retain the verified source representation. The projection verifier
            // decodes both sides into the same domain type: a writer may refresh
            // a projection with explicit defaults without appending a new event.
            work_items.insert(event.work_id.0.to_string(), event_json["work"].take());
            if let Some(run) = event.run {
                runs.insert(run.run_id.0.to_string(), event_json["run"].take());
            }
            if let Some(execution) = event.root_execution {
                root_executions.insert(
                    execution.root_execution_id.0.to_string(),
                    event_json["root_execution"].take(),
                );
            }
            if let Some(claim) = event.claim {
                claims.insert(claim.run_id.0.to_string(), event_json["claim"].take());
            }
            if let Some(offer) = event.handoff_offer {
                handoffs.insert(
                    offer.offer_id.0.to_string(),
                    event_json["handoff_offer"].take(),
                );
            }
            if let Some(blocker) = event.blocker {
                blockers.insert(blocker.blocker_id.clone(), event_json["blocker"].take());
            }
        }
        drop(statement);

        verify_json_projection::<WorkItem>(
            connection,
            "work_item",
            "SELECT work_id, item_json FROM work_items ORDER BY work_id",
            &work_items,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection::<WorkRun>(
            connection,
            "work_run",
            "SELECT run_id, run_json FROM work_runs ORDER BY run_id",
            &runs,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection::<RootExecution>(
            connection,
            "work_root_execution",
            "SELECT root_execution_id, execution_json FROM work_root_executions ORDER BY root_execution_id",
            &root_executions,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection::<WorkClaim>(
            connection,
            "work_claim",
            "SELECT run_id, claim_json FROM work_claims ORDER BY run_id",
            &claims,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection::<WorkHandoffOffer>(
            connection,
            "work_handoff_offer",
            "SELECT offer_id, offer_json FROM work_handoff_offers ORDER BY offer_id",
            &handoffs,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection::<WorkBlocker>(
            connection,
            "work_blocker",
            "SELECT blocker_id, blocker_json FROM work_blockers ORDER BY blocker_id",
            &blockers,
            &mut checked,
            &mut invalid,
        )?;
        verify_prerequisite_rows(connection, &prerequisite_rows, &mut checked, &mut invalid)?;
        verify_blocker_rows(connection, &blocker_rows, &mut checked, &mut invalid)?;
        verify_evidence_rows(connection, &evidence_rows, &mut checked, &mut invalid)?;
        verify_restored_evidence_rows(connection, &mut checked, &mut invalid)?;
        super::observation::verify_rows(connection, &mut checked, &mut invalid)?;
        verify_obligation_rows(connection, &mut checked, &mut invalid)?;
        verify_completion_rows(connection, &completion_rows, &mut checked, &mut invalid)?;
        verify_work_feed_integrity(connection, &work_items, &mut checked, &mut invalid)?;
        verify_work_catalog_projections(connection, &mut checked, &mut invalid)?;
        verify_work_scalar_bindings(connection, &mut checked, &mut invalid)?;
        verify_canonical_work_rows(connection, &mut checked, &mut invalid)?;
        verify_required_child_waiver_bindings(connection, &mut checked, &mut invalid)?;
        verify_work_protocol_attempts(connection, &mut checked, &mut invalid)?;
        verify_anchored_memory_feeds(connection, &mut checked, &mut invalid)?;
        Ok((checked, invalid))
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "integrity reconstruction fills the same projection maps as native event replay"
)]
fn seed_restored_projection_expectations(
    connection: &Connection,
    work_items: &mut HashMap<String, serde_json::Value>,
    blockers: &mut HashMap<String, serde_json::Value>,
    prerequisite_rows: &mut HashMap<(String, String), String>,
    blocker_rows: &mut HashMap<String, (String, String, Option<String>)>,
    relation_bases: &mut HashMap<WorkId, WorkRelationBasis>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let rows = connection
        .prepare(
            "SELECT record.work_id, record.generation_index, record.record_hash,
                    object.object_kind, object.canonical_json
             FROM work_restored_records record
             LEFT JOIN objects object ON object.object_hash = record.record_hash
             ORDER BY record.work_id, record.generation_index",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut latest = HashMap::<WorkId, (i64, ObjectHash, RestoredRecord)>::new();
    let mut next_generation = HashMap::<WorkId, i64>::new();
    for (stored_work_id, generation, stored_hash, object_kind, bytes) in rows {
        *checked += 1;
        let label = format!("work_restored_record:{stored_work_id}:{generation}:{stored_hash}");
        let parsed = parse_work_id(&stored_work_id);
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()));
        let Some(bytes) = bytes else {
            invalid.push(label);
            continue;
        };
        let record = hash.as_ref().ok().and_then(|hash| {
            CanonicalObject::verify(hash, bytes)
                .and_then(|object| object.decode::<RestoredRecord>())
                .ok()
        });
        let (Ok(work_id), Ok(hash), Some(record)) = (parsed, hash, record) else {
            invalid.push(label);
            continue;
        };
        let expected_generation = next_generation.entry(work_id).or_default();
        let internally_bound = object_kind.as_deref() == Some("work_restored_record")
            && record.schema_version == crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
            && record.work_id == work_id
            && record.item.work_id == work_id
            && i64::try_from(record.generation_index).ok() == Some(generation)
            && generation == *expected_generation;
        if !internally_bound {
            invalid.push(label);
            continue;
        }
        *expected_generation += 1;
        latest.insert(work_id, (generation, hash, record));
    }

    let orphaned = connection
        .prepare(
            "SELECT object.object_hash FROM objects object
             LEFT JOIN work_restored_records record
               ON record.record_hash = object.object_hash
             WHERE object.object_kind = 'work_restored_record'
               AND record.record_hash IS NULL
             ORDER BY object.object_hash",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for hash in orphaned {
        *checked += 1;
        invalid.push(format!("work_restored_record:{hash}:missing_projection"));
    }

    for (work_id, (_, anchor, record)) in latest {
        let stored_item = connection
            .query_row(
                "SELECT item_json FROM work_items WHERE work_id = ?1",
                [work_id.0.to_string()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let Some(stored_item) = stored_item else {
            invalid.push(format!("work_restored_record:{work_id:?}:missing_item"));
            continue;
        };
        let Ok(item) = serde_json::from_slice::<WorkItem>(&stored_item) else {
            invalid.push(format!("work_restored_record:{work_id:?}:invalid_item"));
            continue;
        };
        let has_native_history =
            latest_canonical_work_event_for_item_optional(connection, work_id)?.is_some();
        if !has_native_history
            && (!crate::graph_snapshot::restored_item_basis_matches(
                &record.item,
                &record.project_id,
                &item,
            ) || record.item.prerequisites != record.relations.prerequisites)
        {
            invalid.push(format!("work_restored_record:{work_id:?}:item_binding"));
            continue;
        }
        work_items.insert(work_id.0.to_string(), serde_json::to_value(&item)?);

        let mut restored_basis = WorkRelationBasis {
            schema_version: SCHEMA_VERSION,
            prerequisite_ids: record.relations.prerequisites.clone(),
            active_blockers: Vec::with_capacity(record.relations.blockers.len()),
        };
        restored_basis.prerequisite_ids.sort_by_key(|id| id.0);
        for prerequisite in &restored_basis.prerequisite_ids {
            prerequisite_rows.insert(
                (work_id.0.to_string(), prerequisite.0.to_string()),
                anchor.as_str().to_owned(),
            );
        }
        for snapshot in record.relations.blockers {
            let blocker = crate::domain::WorkBlocker {
                blocker_id: snapshot.blocker_id,
                work_id: snapshot.work_id,
                kind: snapshot.kind,
                detail: snapshot.detail,
                created_by: snapshot.created_by,
                created_at: snapshot.created_at,
            };
            if blocker.work_id != work_id {
                invalid.push(format!("work_restored_record:{work_id:?}:blocker_binding"));
                continue;
            }
            let blocker_hash = CanonicalObject::freeze(&blocker)?.hash().clone();
            restored_basis
                .active_blockers
                .push(WorkRelationBlockerBasis {
                    blocker_id: blocker.blocker_id.clone(),
                    blocker_hash,
                });
            blocker_rows.insert(
                blocker.blocker_id.clone(),
                ("active".into(), anchor.as_str().to_owned(), None),
            );
            blockers.insert(blocker.blocker_id.clone(), serde_json::to_value(blocker)?);
        }
        restored_basis
            .active_blockers
            .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        relation_bases.insert(work_id, restored_basis);
    }
    Ok(())
}
