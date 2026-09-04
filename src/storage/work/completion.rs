use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::super::{SqliteStore, StoreError};
use super::execution::{
    ensure_restored_execution_state, ensure_run_evidence, validate_gate_evidence_chain,
    validate_work_evidence_event_phase_on,
};
use super::feeds::{
    append_to_work_feeds, append_work_event, checkpoint_feed_end, current_run_feed_cut_on,
    expire_handoff_offers, inspect_work_request, latest_source_mutation_on,
    load_handoff_offer_projection, load_typed_work_object, replay_operation, request_object,
    run_feed_position_for_object_on, verify_anchored_memory_feeds,
};
use super::integrity::{
    combined_graph_is_acyclic_with_dependency, expected_environment_projection,
    expected_verification_projection, verify_blocker_rows, verify_canonical_work_rows,
    verify_completion_rows, verify_evidence_rows, verify_json_projection, verify_obligation_rows,
    verify_prerequisite_rows, verify_restored_evidence_rows, verify_work_catalog_projections,
    verify_work_feed_integrity, verify_work_protocol_attempts, verify_work_scalar_bindings,
};
use super::planning::{
    add_root_contribution, apply_work_relation_transition, assert_actor_session, assert_revision,
    encode_state, expect_root_contributor, first_unaccounted_root_contributor, normalize_text,
    persist_claim, persist_operation_result, persist_root_execution, persist_work_item,
    persist_work_run, unique_hashes, validate_control_work_binding_on, validate_live_claim_on,
    validated_current_work_relation_basis, waive_root_contributor, work_relation_fingerprint,
};
use super::query::{
    active_root_execution, active_root_execution_optional, completion_recovery_snapshot_on,
    feed_parts, incomplete_prerequisite_projections, latest_canonical_work_event_for_item_optional,
    load_root_execution, load_work_claim_optional, load_work_item, load_work_run, parse_work_id,
    parse_work_run_id, work_completed_by_restored_record_on,
};
use super::{
    CompleteWorkStorageResult, ControlWorkObligationWaiverFingerprint, EvidenceProjectionRow,
    MAX_COMPLETION_ENVIRONMENT_EVIDENCE, MAX_OPEN_COMPLETION_OBLIGATIONS, ObligationProjectionRow,
    WorkEventDraft, WorkObligationRecord, WorkObligationWaiverFingerprint, WorkRelationBasis,
    WorkRelationBlockerBasis, empty_work_relation_basis,
};
use crate::{
    CanonicalObject, ObjectHash, RestoredRecord,
    domain::{
        AcceptanceResult, COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
        COMPLETION_OBLIGATION_SCHEMA_VERSION, ChildRequirement, CompleteWorkRequest,
        CompletionObligationBinding, CompletionSeal, ControlWorkBinding, DisposeWorkRequest,
        EnvironmentEvidence, ExecutionObservation, FeedId, FeedPosition, MemoryAssertionEvent,
        MemoryVersion, OpenWorkObligation, ReopenWorkRequest, RequiredChildWaiver, RootExecution,
        RootExecutionId, RootExecutionState, SCHEMA_VERSION, SessionId, VerificationEvidence,
        WaiveRequiredChildRequest, WaiveWorkObligationRequest, WorkBlocker, WorkCheckpoint,
        WorkClaim, WorkClaimState, WorkCompletionRecoveryCause, WorkDisposition, WorkEvent,
        WorkEvidence, WorkEvidenceKind, WorkHandoffOffer, WorkHandoffState, WorkId, WorkItem,
        WorkLifecycle, WorkObligation, WorkObligationId, WorkObligationResolution,
        WorkObligationResolutionEvent, WorkObligationState, WorkObligationWaiverDecision,
        WorkObligationWaiverReceipt, WorkObligationWaiverRefusalCode, WorkRun, WorkRunId,
        WorkRunState, WorkTransition,
    },
    memory::Redactor,
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

    /// Completes one run only after acceptance, evidence, graph, and fence checks.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, acceptance is incomplete,
    /// evidence is invalid, graph barriers remain, or persistence fails.
    pub fn complete_work<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
    ) -> Result<CompletionSeal, StoreError> {
        match self.complete_work_internal(request, redactor, false)? {
            CompleteWorkStorageResult::Completed(seal) => Ok(*seal),
            CompleteWorkStorageResult::Recovery(_) => Err(StoreError::InvalidWorkProjection(
                "core completion unexpectedly returned an ambient recovery receipt".into(),
            )),
        }
    }

    pub(crate) fn complete_work_for_protocol<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
    ) -> Result<CompleteWorkStorageResult, StoreError> {
        self.complete_work_internal(request, redactor, true)
    }

    fn complete_work_internal<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
        return_recovery: bool,
    ) -> Result<CompleteWorkStorageResult, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(seal) = replay_operation::<CompletionSeal>(
            &transaction,
            "complete_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(CompleteWorkStorageResult::Completed(Box::new(seal)));
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.completed_at,
            &request.actor,
        )?;
        let (mut item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.completed_at,
            false,
        )?;
        let offered_handoffs = transaction.query_row(
            "SELECT COUNT(*) FROM work_handoff_offers WHERE run_id = ?1 AND state = 'offered'",
            [run.run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if offered_handoffs != 0 {
            return Err(StoreError::InvalidWorkProjection(
                "completion cannot terminalize a run with an offered handoff".into(),
            ));
        }
        let relation_basis = validated_current_work_relation_basis(&transaction, item.work_id)?;
        if !relation_basis.active_blockers.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "one or more explicit blockers remain active".into(),
            });
        }
        let checkpoint =
            run.last_checkpoint
                .clone()
                .ok_or_else(|| StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "the current run has no checkpoint".into(),
                })?;
        let checkpoint_value: WorkCheckpoint =
            load_typed_work_object(&transaction, &checkpoint, "work_checkpoint")?;
        if checkpoint_value.work_id != item.work_id
            || checkpoint_value.run_id != run.run_id
            || checkpoint_value.claim_id != claim.claim_id
            || checkpoint_value.claim_fence != claim.fence
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "the latest checkpoint was not written under the completing claim fence"
                    .into(),
            });
        }
        let evidence = unique_hashes(&request.evidence);
        if evidence.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "completion requires at least one evidence object".into(),
            });
        }
        ensure_run_evidence(&transaction, run.run_id, &evidence)?;
        if !evidence
            .iter()
            .all(|hash| checkpoint_value.evidence.contains(hash))
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason:
                    "the final checkpoint does not acknowledge every completion evidence object"
                        .into(),
            });
        }
        let acceptance = match validate_acceptance(
            &item,
            &evidence,
            &request.acceptance,
            request.actor.assurance,
        ) {
            Ok(value) => value,
            Err(StoreError::WorkCompletionRecoveryRequired { cause, .. }) if return_recovery => {
                let recovery =
                    completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            Err(error) => return Err(error),
        };
        let drain = request.drain.clone();
        if !drain.reconciled_action_outcomes.is_empty()
            || !drain.released_resource_leases.is_empty()
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "V1 completion drain accepts only a zero-linked-state attestation until action and resource projections are linked to work runs".into(),
            });
        }
        let incomplete = incomplete_prerequisite_projections(&transaction, item.work_id)?;
        if !incomplete.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!("prerequisites remain incomplete: {incomplete:?}"),
            });
        }
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let required_child_seals =
            required_child_seals(&transaction, item.work_id, run.root_execution_id)?;
        let restored_child_completions =
            required_restored_child_completions(&transaction, item.work_id)?;
        let required_child_waivers =
            validated_required_child_waivers(&transaction, item.work_id, &root_execution)?;
        let unfinished_optional_children =
            unfinished_optional_children(&transaction, item.work_id)?;
        let required_child_count = transaction.query_row(
            "SELECT COUNT(*) FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if usize::try_from(required_child_count).ok()
            != Some(
                required_child_seals.len()
                    + restored_child_completions.len()
                    + required_child_waivers.len(),
            )
        {
            let sealed_children = required_child_seals
                .iter()
                .map(|hash| {
                    load_typed_work_object::<CompletionSeal>(&transaction, hash, "completion_seal")
                        .map(|seal| seal.work_id)
                })
                .collect::<Result<HashSet<_>, StoreError>>()?;
            let waived_children = required_child_waivers
                .iter()
                .map(|waiver| waiver.work_id)
                .collect::<HashSet<_>>();
            let restored_children = restored_child_completions
                .iter()
                .map(|hash| {
                    load_typed_work_object::<RestoredRecord>(
                        &transaction,
                        hash,
                        "work_restored_record",
                    )
                    .map(|record| record.work_id)
                })
                .collect::<Result<HashSet<_>, StoreError>>()?;
            let mut statement = transaction.prepare(
                "SELECT work_id FROM work_items
                 WHERE parent_id = ?1 AND child_requirement = 'required'
                 ORDER BY work_id",
            )?;
            let required_children = statement
                .query_map([item.work_id.0.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            let child = required_children
                .into_iter()
                .map(|value| parse_work_id(&value))
                .collect::<Result<Vec<_>, StoreError>>()?
                .into_iter()
                .find(|child| {
                    !sealed_children.contains(child)
                        && !restored_children.contains(child)
                        && !waived_children.contains(child)
                })
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "required-child barrier count disagrees with its accounted identities"
                            .into(),
                    )
                })?;
            let child_item = load_work_item(&transaction, child)?;
            if child_item.lifecycle == WorkLifecycle::Completed {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "completed required child {child:?} has no completion seal in the active root execution"
                )));
            }
            let cause = WorkCompletionRecoveryCause::RequiredChildUnsealed { child };
            if return_recovery {
                let recovery =
                    completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause,
            });
        }
        let completion_cut = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&transaction, &FeedId::RunExecution(run.run_id))?,
        };
        let checkpoint_cut =
            checkpoint_feed_end(checkpoint_value.acknowledged_run_position.position)?;
        if checkpoint_value.acknowledged_run_position.feed != FeedId::RunExecution(run.run_id)
            || checkpoint_cut != completion_cut.position
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "the final checkpoint does not reach the current pre-seal run-feed cut"
                    .into(),
            });
        }
        let obligations = match completion_obligation_basis_on(
            &transaction,
            item.work_id,
            run.run_id,
            &completion_cut,
        ) {
            Ok(value) => value,
            Err(StoreError::OpenWorkObligations { obligations, .. }) if return_recovery => {
                let obligation = obligations.first().ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "open-obligation refusal contains no exact obligation".into(),
                    )
                })?;
                let cause = WorkCompletionRecoveryCause::OpenObligation {
                    obligation_id: obligation.obligation_id,
                    definition: obligation.definition.clone(),
                    required_check: obligation.required_check,
                };
                let recovery =
                    completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            Err(error) => return Err(error),
        };
        let environment =
            completion_environment_basis_on(&transaction, run.run_id, &completion_cut)?;
        if environment.len() > MAX_COMPLETION_ENVIRONMENT_EVIDENCE {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "completion cites {} environment records; checkpoint fewer environment records (maximum {})",
                    environment.len(),
                    MAX_COMPLETION_ENVIRONMENT_EVIDENCE
                ),
            });
        }
        if live_descendant_execution_authority(&transaction, item.work_id, request.completed_at)? {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "completion requires every descendant claim and handoff offer to be released, completed, or expired".into(),
            });
        }
        let accepted_work_revision = CanonicalObject::freeze(&item)?;
        SqliteStore::insert_object(&transaction, "work_item_revision", &accepted_work_revision)?;
        expect_root_contributor(&mut root_execution, &claim.holder);
        add_root_contribution(&mut root_execution, &claim.holder, &checkpoint);
        if item.work_id == item.root_id
            && let Some(participant) = first_unaccounted_root_contributor(&root_execution)
        {
            let cause = WorkCompletionRecoveryCause::MissingContribution {
                participant: participant.clone(),
            };
            if return_recovery {
                let recovery =
                    completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause,
            });
        }
        let child_seal_is_restored = required_child_seals.iter().try_fold(
            false,
            |restored, hash| -> Result<bool, StoreError> {
                let child: CompletionSeal =
                    load_typed_work_object(&transaction, hash, "completion_seal")?;
                Ok(restored || child.restored)
            },
        )?;
        let seal = CompletionSeal {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            root_id: item.root_id,
            root_execution_id: run.root_execution_id,
            run_id: run.run_id,
            run_generation: run.generation,
            accepted_work_revision: item.revision,
            accepted_work_revision_hash: accepted_work_revision.hash().clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            completion_cut,
            checkpoint: Some(checkpoint),
            evidence,
            acceptance,
            obligation_schema_version: COMPLETION_OBLIGATION_SCHEMA_VERSION,
            obligations,
            environment_schema_version: COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
            environment,
            required_child_seals,
            required_child_waivers,
            restored: child_seal_is_restored || !restored_child_completions.is_empty(),
            restored_child_completions,
            unfinished_optional_children,
            expected_contributors: root_execution.expected_contributors.clone(),
            contributions: root_execution.contributions.clone(),
            waivers: root_execution.waivers.clone(),
            drain,
            actor: request.actor.clone(),
            completed_at: request.completed_at,
        };
        validate_completion_seal_obligation_basis_on(&transaction, &seal)?;
        validate_completion_seal_environment_basis_on(&transaction, &seal)?;
        validate_completion_seal_children_on(&transaction, &seal, 0)?;
        let seal_object = CanonicalObject::freeze(&seal)?;
        SqliteStore::insert_object(&transaction, "completion_seal", &seal_object)?;
        transaction.execute(
            "INSERT INTO work_completion_seals (
                 seal_hash, work_id, run_id, root_execution_id, seal_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                seal_object.hash().as_str(),
                item.work_id.0.to_string(),
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                serde_json::to_vec(&seal)?
            ],
        )?;

        claim.state = WorkClaimState::Completed;
        claim.revision += 1;
        claim.fence += 1;
        claim.expires_at = request.completed_at;
        run.state = WorkRunState::Completed;
        run.completion_seal = Some(seal_object.hash().clone());
        run.revision += 1;
        run.updated_at = request.completed_at;
        item.lifecycle = WorkLifecycle::Completed;
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.completed_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        persist_work_item(&transaction, &item)?;

        if item.work_id == item.root_id {
            root_execution.state = RootExecutionState::Completed;
            root_execution
                .required_child_seals
                .clone_from(&seal.required_child_seals);
        } else if item.child_requirement == ChildRequirement::Required {
            root_execution
                .required_child_seals
                .push(seal_object.hash().clone());
            root_execution
                .required_child_seals
                .sort_by(|left, right| left.as_str().cmp(right.as_str()));
            root_execution.required_child_seals.dedup();
        }
        root_execution.revision += 1;
        root_execution.updated_at = request.completed_at;
        persist_root_execution(&transaction, &root_execution)?;

        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Completed {
                seal: seal_object.hash().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.completed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "complete_work",
            &request.idempotency_key,
            request_object.hash(),
            &seal,
        )?;
        transaction.commit()?;
        Ok(CompleteWorkStorageResult::Completed(Box::new(seal)))
    }

    /// Reopens completed work as a clean run generation without reviving authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the expected revision changed, an ancestor
    /// already consumed the child seal, or the new generation cannot be persisted.
    pub fn reopen_work<R: Redactor>(
        &mut self,
        request: &ReopenWorkRequest,
        redactor: &R,
    ) -> Result<WorkRun, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "reopen reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(run) = replay_operation::<WorkRun>(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(run);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.lifecycle != WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(
                "only completed work can be reopened".into(),
            ));
        }
        if item.parent_id.is_some() {
            refuse_completed_ancestor(&transaction, &item)?;
        } else {
            let open_descendants = transaction.query_row(
                "WITH RECURSIVE descendants(work_id) AS (
                     SELECT work_id FROM work_items WHERE parent_id = ?1
                     UNION
                     SELECT child.work_id FROM work_items child
                     JOIN descendants parent ON child.parent_id = parent.work_id
                 )
                 SELECT COUNT(*) FROM descendants
                 JOIN work_items item USING(work_id)
                 WHERE item.lifecycle IN ('proposed', 'open')",
                [item.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            if open_descendants != 0 {
                return Err(StoreError::InvalidWork(
                    "dispose unfinished descendants before reopening a completed root execution"
                        .into(),
                ));
            }
        }
        if work_completed_by_restored_record_on(&transaction, &item)? {
            item.lifecycle = WorkLifecycle::Open;
            item.revision += 1;
            item.updated_at = request.reopened_at;
            let (root_execution, run, created) =
                ensure_restored_execution_state(&transaction, &mut item, request.reopened_at)?;
            if !created {
                return Err(StoreError::InvalidWorkProjection(
                    "restored reopen did not create a fresh run".into(),
                ));
            }
            let event = WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: Some(run.run_id),
                revision: item.revision,
                work: item,
                run: Some(run.clone()),
                root_execution: Some(root_execution),
                claim: None,
                handoff_offer: None,
                blocker: None,
                transition: WorkTransition::Reopened {
                    run_id: run.run_id,
                    generation: run.generation,
                    reason,
                },
                actor: request.actor.clone(),
                created_at: request.reopened_at,
            };
            append_work_event(&transaction, &event)?;
            persist_operation_result(
                &transaction,
                "reopen_work",
                &request.idempotency_key,
                request_object.hash(),
                &run,
            )?;
            transaction.commit()?;
            return Ok(run);
        }
        let generation = transaction.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM work_runs WHERE work_id = ?1",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let root_execution = if item.work_id == item.root_id {
            let root_generation = transaction.query_row(
                "SELECT COALESCE(MAX(generation), 0) + 1
                 FROM work_root_executions WHERE root_id = ?1",
                [item.root_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            let execution = RootExecution {
                schema_version: SCHEMA_VERSION,
                root_execution_id: RootExecutionId::new(),
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                generation: root_generation,
                state: RootExecutionState::Active,
                revision: 1,
                run_ids: Vec::new(),
                required_child_seals: Vec::new(),
                required_child_waivers: Vec::new(),
                expected_contributors: Vec::new(),
                contributions: Vec::new(),
                waivers: Vec::new(),
                created_at: request.reopened_at,
                updated_at: request.reopened_at,
            };
            transaction.execute(
                "INSERT INTO work_root_executions (
                     root_execution_id, project_id, root_id, generation, state,
                     revision, created_at_ms, updated_at_ms, execution_json
                 ) VALUES (?1, ?2, ?3, ?4, 'active', 1, ?5, ?6, ?7)",
                params![
                    execution.root_execution_id.0.to_string(),
                    execution.project_id.0,
                    execution.root_id.0.to_string(),
                    execution.generation,
                    execution.created_at.timestamp_millis(),
                    execution.updated_at.timestamp_millis(),
                    serde_json::to_vec(&execution)?
                ],
            )?;
            execution
        } else {
            let mut execution = active_root_execution(&transaction, item.root_id)?;
            if item.child_requirement == ChildRequirement::Required {
                let old_seal: Option<String> = transaction
                    .query_row(
                        "SELECT seal_hash FROM work_completion_seals WHERE work_id = ?1
                         ORDER BY rowid DESC LIMIT 1",
                        [item.work_id.0.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(old_seal) = old_seal {
                    execution
                        .required_child_seals
                        .retain(|hash| hash.as_str() != old_seal);
                    execution.revision += 1;
                    execution.updated_at = request.reopened_at;
                    persist_root_execution(&transaction, &execution)?;
                }
            }
            execution
        };
        let run = WorkRun {
            schema_version: SCHEMA_VERSION,
            run_id: WorkRunId::new(),
            root_execution_id: root_execution.root_execution_id,
            work_id: item.work_id,
            generation,
            executor: None,
            state: WorkRunState::Open,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: request.reopened_at,
            updated_at: request.reopened_at,
        };
        let mut root_execution = root_execution;
        if !root_execution.run_ids.contains(&run.run_id) {
            root_execution.run_ids.push(run.run_id);
            root_execution.run_ids.sort_by_key(|run_id| run_id.0);
            root_execution.revision += 1;
            root_execution.updated_at = request.reopened_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        transaction.execute(
            "INSERT INTO work_runs (
                 run_id, root_execution_id, work_id, generation,
                 executor_session_id, state, revision, claim_fence_head,
                 last_checkpoint_hash, completion_seal_hash,
                 created_at_ms, updated_at_ms, run_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, 'open', 1, 0, NULL, NULL, ?5, ?6, ?7)",
            params![
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                run.work_id.0.to_string(),
                run.generation,
                run.created_at.timestamp_millis(),
                run.updated_at.timestamp_millis(),
                serde_json::to_vec(&run)?
            ],
        )?;
        item.lifecycle = WorkLifecycle::Open;
        item.active_run_id = Some(run.run_id);
        item.revision += 1;
        item.updated_at = request.reopened_at;
        persist_work_item(&transaction, &item)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Reopened {
                run_id: run.run_id,
                generation: run.generation,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.reopened_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
            &run,
        )?;
        transaction.commit()?;
        Ok(run)
    }

    /// Cancels or supersedes open work without recording false completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority, revision, claim ownership,
    /// replacement linkage, or descendant-drain invariants are not satisfied.
    pub fn dispose_work<R: Redactor>(
        &mut self,
        request: &DisposeWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "work disposal reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let open_descendants = transaction.query_row(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT COUNT(*) FROM descendants
             JOIN work_items item USING(work_id)
             WHERE item.lifecycle IN ('proposed', 'open')",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if open_descendants != 0 {
            return Err(StoreError::InvalidWork(
                "dispose open descendants before disposing their parent".into(),
            ));
        }
        let replacement = match (request.disposition, request.replacement_id) {
            (WorkDisposition::Cancelled, None) => None,
            (WorkDisposition::Cancelled, Some(_)) => {
                return Err(StoreError::InvalidWork(
                    "cancelled work must not name a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, None) => {
                return Err(StoreError::InvalidWork(
                    "superseded work requires a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, Some(replacement_id)) => {
                if replacement_id == item.work_id {
                    return Err(StoreError::InvalidWork(
                        "work cannot supersede itself".into(),
                    ));
                }
                let replacement = load_work_item(&transaction, replacement_id)?;
                if replacement.project_id != item.project_id
                    || matches!(
                        replacement.lifecycle,
                        WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                    )
                {
                    return Err(StoreError::InvalidWork(
                        "replacement must be live or completed work in the same project".into(),
                    ));
                }
                if !combined_graph_is_acyclic_with_dependency(
                    &transaction,
                    &item.project_id.0,
                    Some((item.work_id, replacement.work_id)),
                )? {
                    return Err(StoreError::WorkDependencyCycle);
                }
                Some(replacement)
            }
        };
        let restored_execution = if item.active_run_id.is_none() {
            Some(ensure_restored_execution_state(
                &transaction,
                &mut item,
                request.disposed_at,
            )?)
        } else {
            None
        };
        let mut run = if let Some((_, run, _)) = restored_execution.as_ref() {
            Some(run.clone())
        } else {
            item.active_run_id
                .map(|run_id| load_work_run(&transaction, run_id))
                .transpose()?
        };
        let mut claim = if restored_execution.is_some() {
            None
        } else if let Some(run) = run.as_ref() {
            expire_handoff_offers(
                &transaction,
                run.run_id,
                request.disposed_at,
                &request.actor,
            )?;
            load_work_claim_optional(&transaction, run.run_id)?
        } else {
            None
        };
        let unaccounted_holder = claim
            .as_ref()
            .filter(|claim| claim.state == WorkClaimState::Active)
            .map(|claim| claim.holder.clone());
        if let Some(current) = claim.as_ref()
            && current.state == WorkClaimState::Active
            && current.expires_at > request.disposed_at
        {
            assert_actor_session(&request.actor, &current.holder)?;
        }
        let claim_fence = if let Some(current) = claim.as_mut() {
            if current.state == WorkClaimState::Active {
                current.state = WorkClaimState::Released;
                current.revision += 1;
                current.fence += 1;
                current.expires_at = request.disposed_at;
                persist_claim(&transaction, current)?;
            }
            current.fence
        } else if let Some(run) = run.as_ref() {
            transaction.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [run.run_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?
        } else {
            0
        };
        if let Some(current_run) = run.as_mut() {
            current_run.executor = None;
            current_run.state = WorkRunState::Cancelled;
            current_run.revision += 1;
            current_run.updated_at = request.disposed_at;
            persist_work_run(&transaction, current_run, claim_fence)?;
        }
        item.lifecycle = match request.disposition {
            WorkDisposition::Cancelled => WorkLifecycle::Cancelled,
            WorkDisposition::Superseded => WorkLifecycle::Superseded,
        };
        item.superseded_by = replacement.as_ref().map(|work| work.work_id);
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.disposed_at;
        persist_work_item(&transaction, &item)?;
        let mut root_execution = if let Some((execution, _, _)) = restored_execution {
            execution
        } else if let Some(current_run) = run.as_ref() {
            load_root_execution(&transaction, current_run.root_execution_id)?
        } else {
            active_root_execution(&transaction, item.root_id)?
        };
        let mut root_changed = false;
        if item.work_id != item.root_id
            && let Some(holder) = unaccounted_holder
            && !root_execution
                .contributions
                .iter()
                .any(|contribution| contribution.participant == holder)
            && !root_execution
                .waivers
                .iter()
                .any(|waiver| waiver.participant == holder)
        {
            root_changed |= waive_root_contributor(
                &mut root_execution,
                &holder,
                &request.actor.actor_id,
                &reason,
            );
        }
        if item.work_id == item.root_id {
            root_execution.state = RootExecutionState::Cancelled;
            root_changed = true;
        }
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.disposed_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.as_ref().map(|run| run.run_id),
            revision: item.revision,
            work: item.clone(),
            run,
            root_execution: Some(root_execution),
            claim,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Disposed {
                lifecycle: item.lifecycle,
                replacement_id: item.superseded_by,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.disposed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Accounts for one deliberately cancelled or superseded required child
    /// with an attributed, audited reason from the project-bound session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent revision changed, the child is
    /// not a directly required disposed child, the asserted project binding
    /// is invalid, or the waiver conflicts with an earlier request.
    pub fn waive_required_child<R: Redactor>(
        &mut self,
        request: &WaiveRequiredChildRequest,
        redactor: &R,
    ) -> Result<RequiredChildWaiver, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "required-child waiver reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(waiver) = replay_operation::<RequiredChildWaiver>(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(waiver);
        }
        let parent = load_work_item(&transaction, request.parent_id)?;
        assert_revision(&parent, request.expected_parent_revision)?;
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(parent.work_id));
        }
        let child = load_work_item(&transaction, request.child_id)?;
        if child.parent_id != Some(parent.work_id)
            || child.child_requirement != ChildRequirement::Required
            || !matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
        {
            return Err(StoreError::InvalidWork(
                "completion waiver requires a directly required cancelled or superseded child"
                    .into(),
            ));
        }
        let mut root_execution = active_root_execution(&transaction, parent.root_id)?;
        if root_execution
            .required_child_waivers
            .iter()
            .any(|waiver| waiver.work_id == child.work_id)
        {
            return Err(StoreError::InvalidWork(
                "required child already has a completion waiver in this root execution".into(),
            ));
        }
        let waiver = RequiredChildWaiver {
            work_id: child.work_id,
            work_revision: child.revision,
            waived_by: request.actor.actor_id.clone(),
            reason: reason.clone(),
        };
        root_execution.required_child_waivers.push(waiver.clone());
        root_execution
            .required_child_waivers
            .sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
        root_execution.revision += 1;
        root_execution.updated_at = request.waived_at;
        persist_root_execution(&transaction, &root_execution)?;
        let parent_run = parent
            .active_run_id
            .map(|run_id| load_work_run(&transaction, run_id))
            .transpose()?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            root_id: parent.root_id,
            work_id: parent.work_id,
            run_id: parent.active_run_id,
            revision: parent.revision,
            work: parent,
            run: parent_run,
            root_execution: Some(root_execution),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::RequiredChildWaived {
                child_id: child.work_id,
                child_revision: child.revision,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.waived_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
            &waiver,
        )?;
        transaction.commit()?;
        Ok(waiver)
    }

    /// Resolves one exact open obligation through a bound host-control session.
    /// Policy refusals are frozen as typed results under the control-operation
    /// idempotency key; transport, routing, and integrity faults remain errors.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the session capabilities are invalid, the
    /// request key conflicts, or canonical storage cannot be verified.
    #[allow(clippy::too_many_arguments)]
    pub fn waive_bound_work_obligation<R: Redactor>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        obligation_id: WorkObligationId,
        expected_definition: &ObjectHash,
        waived_by: &str,
        reason: &str,
        actor: &crate::domain::ActorContext,
        idempotency_key: &str,
        waived_at: DateTime<Utc>,
        redactor: &R,
    ) -> Result<WorkObligationWaiverDecision, StoreError> {
        let request = WaiveWorkObligationRequest {
            obligation_id,
            expected_definition: expected_definition.clone(),
            waived_by: waived_by.to_owned(),
            reason: reason.to_owned(),
            actor: actor.clone(),
            idempotency_key: idempotency_key.to_owned(),
            waived_at,
        };
        inspect_work_request(redactor, &request, &request.actor)?;
        let waived_by = normalize_text(&request.waived_by, "obligation waiver actor")?;
        let reason = normalize_text(&request.reason, "obligation waiver reason")?;
        let transaction = self.begin_work_mutation()?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        if actor.session_id.as_ref() != Some(session_id) || actor.actor_id != session.actor.actor_id
        {
            return Err(StoreError::InvalidControlSession(
                "obligation waiver actor is not the server-fixed control session".into(),
            ));
        }
        let intent = CanonicalObject::freeze(&ControlWorkObligationWaiverFingerprint {
            control_schema_version: crate::CONTROL_SCHEMA_VERSION,
            session_id,
            bind_intent_hash: &session.bind_intent_hash,
            obligation_id,
            expected_definition,
            waived_by: &request.waived_by,
            reason: &request.reason,
            idempotency_key,
        })?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "obligation_waive",
            idempotency_key,
            intent.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let record = load_work_obligation_by_id_on(&transaction, obligation_id)?;
        let binding_admitted = session.work_binding.as_ref().is_some_and(|binding| {
            binding.run_id == record.obligation.run_id
                && binding.work_id == record.obligation.work_id
                && binding.root_execution_id == record.obligation.root_execution_id
        });
        if !binding_admitted {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy: "bind the host control session to the live claim for this obligation run"
                    .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let binding = session.work_binding.as_ref().ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "admitted obligation waiver lost its work binding".into(),
            )
        })?;
        if let Err(error) = validate_control_work_binding_on(
            &transaction,
            project_id,
            session_id,
            binding,
            waived_at,
        ) {
            if matches!(error, StoreError::ControlWorkBindingStale { .. }) {
                let decision = WorkObligationWaiverDecision::Refused {
                    code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                    obligation_id,
                    current_definition: Some(record.definition_hash),
                    remedy: "reread the live claim, then bind the current work generation".into(),
                };
                Self::persist_control_operation(
                    &transaction,
                    session_id,
                    "obligation_waive",
                    idempotency_key,
                    &intent,
                    &decision,
                    waived_at,
                )?;
                transaction.commit()?;
                return Ok(decision);
            }
            return Err(error);
        }
        if record.definition_hash != *expected_definition {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::DefinitionChanged,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy:
                    "reread obligation_page and retry only after reviewing the current definition"
                        .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        if record.state != WorkObligationState::Open {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::ObligationNotOpen,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy: "reread obligation_page; this obligation already has a terminal resolution"
                    .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: record.obligation.project_id.clone(),
            obligation_id,
            definition: record.definition_hash.clone(),
            run_id: record.obligation.run_id,
            resolution: WorkObligationResolution::Waived {
                waived_by: waived_by.clone(),
                reason,
            },
            actor: request.actor,
            created_at: waived_at,
        };
        let resolution = append_obligation_resolution_on(&transaction, &record, &event)?;
        let decision = WorkObligationWaiverDecision::Waived {
            receipt: WorkObligationWaiverReceipt {
                obligation_id,
                definition: record.definition_hash,
                resolution,
                state: WorkObligationState::Waived,
                waived_by,
                waived_at,
            },
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "obligation_waive",
            idempotency_key,
            &intent,
            &decision,
            waived_at,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Resolves one exact open obligation through an attributed local shell
    /// action. This operation is absent from the ambient agent work protocol,
    /// but the shell path itself is neither authenticated nor run-bound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the definition changed, the obligation is
    /// already terminal or the request conflicts with an idempotent replay.
    pub fn waive_work_obligation<R: Redactor>(
        &mut self,
        request: &WaiveWorkObligationRequest,
        redactor: &R,
    ) -> Result<WorkObligationResolutionEvent, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let waived_by = normalize_text(&request.waived_by, "obligation waiver actor")?;
        let reason = normalize_text(&request.reason, "obligation waiver reason")?;
        let request_object = request_object(&WorkObligationWaiverFingerprint {
            obligation_id: request.obligation_id,
            expected_definition: &request.expected_definition,
            waived_by: &request.waived_by,
            reason: &request.reason,
            actor: &request.actor,
            idempotency_key: &request.idempotency_key,
        })?;
        let transaction = self.begin_work_mutation()?;
        if let Some(event) = replay_operation::<WorkObligationResolutionEvent>(
            &transaction,
            "waive_work_obligation",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(event);
        }
        let record = load_work_obligation_by_id_on(&transaction, request.obligation_id)?;
        if record.definition_hash != request.expected_definition {
            return Err(StoreError::InvalidWork(format!(
                "obligation {} definition changed: expected {}, current {}",
                request.obligation_id.0, request.expected_definition, record.definition_hash
            )));
        }
        if record.state != WorkObligationState::Open {
            return Err(StoreError::InvalidWork(format!(
                "obligation {} is already terminal",
                request.obligation_id.0
            )));
        }
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: record.obligation.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_hash.clone(),
            run_id: record.obligation.run_id,
            resolution: WorkObligationResolution::Waived { waived_by, reason },
            actor: request.actor.clone(),
            created_at: request.waived_at,
        };
        append_obligation_resolution_on(&transaction, &record, &event)?;
        persist_operation_result(
            &transaction,
            "waive_work_obligation",
            &request.idempotency_key,
            request_object.hash(),
            &event,
        )?;
        transaction.commit()?;
        Ok(event)
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

pub(super) fn applicable_work_obligations_at_cut_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<WorkObligationRecord>, StoreError> {
    if cut.feed != FeedId::RunExecution(run_id) {
        return Err(StoreError::InvalidWorkProjection(
            "obligation cut does not name the requested run feed".into(),
        ));
    }
    if cut.position > feed_head(connection, &cut.feed)? {
        return Err(StoreError::InvalidWorkProjection(
            "obligation cut exceeds the current run-feed head".into(),
        ));
    }
    let records = load_work_obligation_records_on(connection, run_id, None)?;
    let mut applicable = Vec::new();
    for record in records {
        if record.obligation.trigger_position.position > cut.position {
            continue;
        }
        let definition_position =
            run_feed_position_for_object_on(connection, run_id, &record.definition_hash)?;
        if definition_position.position > cut.position {
            return Err(StoreError::InvalidWorkProjection(format!(
                "run-feed cut {} splits mutation obligation {} from its trigger",
                cut.position, record.obligation.obligation_id.0
            )));
        }
        applicable.push(record);
    }
    Ok(applicable)
}

fn completion_obligation_basis_on(
    connection: &Connection,
    work_id: WorkId,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<CompletionObligationBinding>, StoreError> {
    let records = applicable_work_obligations_at_cut_on(connection, run_id, cut)?;
    let mut open = Vec::new();
    let mut bindings = Vec::new();
    for record in records {
        let terminal_at_cut = record
            .resolution_hash
            .as_ref()
            .map(|hash| run_feed_position_for_object_on(connection, run_id, hash))
            .transpose()?
            .filter(|position| position.position <= cut.position);
        let Some(resolution_position) = terminal_at_cut else {
            open.push(OpenWorkObligation {
                obligation_id: record.obligation.obligation_id,
                definition: record.definition_hash,
                required_check: record.obligation.requirement.check_kind,
            });
            continue;
        };
        if resolution_position.feed != cut.feed {
            return Err(StoreError::InvalidWorkProjection(
                "obligation resolution position names another run feed".into(),
            ));
        }
        bindings.push(CompletionObligationBinding {
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_hash,
            resolution: record.resolution_hash.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "terminal work obligation has no resolution hash".into(),
                )
            })?,
        });
    }
    open.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
    });
    if !open.is_empty() {
        let omitted_count = open.len().saturating_sub(MAX_OPEN_COMPLETION_OBLIGATIONS);
        open.truncate(MAX_OPEN_COMPLETION_OBLIGATIONS);
        return Err(StoreError::OpenWorkObligations {
            work: work_id,
            obligations: open,
            omitted_count,
        });
    }
    bindings.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
            .then_with(|| left.definition.as_str().cmp(right.definition.as_str()))
    });
    Ok(bindings)
}

fn completion_environment_basis_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<ObjectHash>, StoreError> {
    if cut.feed != FeedId::RunExecution(run_id) {
        return Err(StoreError::InvalidWorkProjection(
            "environment cut does not name the requested run feed".into(),
        ));
    }
    if cut.position > feed_head(connection, &cut.feed)? {
        return Err(StoreError::InvalidWorkProjection(
            "environment cut exceeds the current run-feed head".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT DISTINCT object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1
           AND position <= ?2 AND object_kind = 'environment_evidence'
         ORDER BY object_hash LIMIT ?3",
    )?;
    let limit = i64::try_from(MAX_COMPLETION_ENVIRONMENT_EVIDENCE + 1).map_err(|_| {
        StoreError::InvalidWorkProjection("completion environment limit does not fit SQLite".into())
    })?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), cut.position, limit], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut environment = Vec::with_capacity(rows.len());
    for stored in rows {
        let hash =
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))?;
        expected_environment_projection(connection, &hash)?;
        environment.push(hash);
    }
    Ok(environment)
}

pub(super) fn validate_completion_seal_environment_basis_on(
    connection: &Connection,
    seal: &CompletionSeal,
) -> Result<(), StoreError> {
    if seal.environment_schema_version != COMPLETION_ENVIRONMENT_SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has unsupported environment schema {}",
            seal.run_id.0, seal.environment_schema_version
        )));
    }
    let expected = completion_environment_basis_on(connection, seal.run_id, &seal.completion_cut)?;
    if expected.len() > MAX_COMPLETION_ENVIRONMENT_EVIDENCE || seal.environment != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} does not bind the exact environment cut",
            seal.run_id.0
        )));
    }
    Ok(())
}

pub(super) fn validate_completion_seal_obligation_basis_on(
    connection: &Connection,
    seal: &CompletionSeal,
) -> Result<(), StoreError> {
    if seal.obligation_schema_version != COMPLETION_OBLIGATION_SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has unsupported obligation schema {}",
            seal.run_id.0, seal.obligation_schema_version
        )));
    }
    let expected =
        completion_obligation_basis_on(connection, seal.work_id, seal.run_id, &seal.completion_cut)
            .map_err(|error| match error {
                StoreError::OpenWorkObligations { .. } => {
                    StoreError::InvalidWorkProjection(format!(
                        "completion seal for run {} was frozen with open obligations",
                        seal.run_id.0
                    ))
                }
                other => other,
            })?;
    if seal.obligations != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} does not bind the exact obligation cut",
            seal.run_id.0
        )));
    }
    Ok(())
}

pub(super) fn load_work_obligation_records_on(
    connection: &Connection,
    run_id: WorkRunId,
    state: Option<WorkObligationState>,
) -> Result<Vec<WorkObligationRecord>, StoreError> {
    let state = state.map(encode_state).transpose()?;
    let mut statement = connection.prepare(
        "SELECT obligation_id, definition_hash, project_id, root_execution_id,
                root_id, work_id, run_id, work_revision, rule_set_hash, rule_id, rule_version,
                triggering_observation_hash, trigger_position, check_kind,
                check_fingerprint, state, resolution_hash, resolution_kind,
                evidence_hash, opened_at_ms, resolved_at_ms
         FROM work_run_obligations
         WHERE run_id = ?1 AND (?2 IS NULL OR state = ?2)
         ORDER BY trigger_position, obligation_id",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), state], |row| {
            Ok(ObligationProjectionRow {
                obligation_id: row.get(0)?,
                definition_hash: row.get(1)?,
                project_id: row.get(2)?,
                root_execution_id: row.get(3)?,
                root_id: row.get(4)?,
                work_id: row.get(5)?,
                run_id: row.get(6)?,
                work_revision: row.get(7)?,
                rule_set_hash: row.get(8)?,
                rule_id: row.get(9)?,
                rule_version: row.get(10)?,
                triggering_observation_hash: row.get(11)?,
                trigger_position: row.get(12)?,
                check_kind: row.get(13)?,
                check_fingerprint: row.get(14)?,
                state: row.get(15)?,
                resolution_hash: row.get(16)?,
                resolution_kind: row.get(17)?,
                evidence_hash: row.get(18)?,
                opened_at_ms: row.get(19)?,
                resolved_at_ms: row.get(20)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let records = rows
        .into_iter()
        .map(|row| load_work_obligation_record_on(connection, &row))
        .collect::<Result<Vec<_>, _>>()?;
    if state.is_none() {
        require_expected_obligations_on(connection, run_id, &records)?;
    }
    Ok(records)
}

fn require_expected_obligations_on(
    connection: &Connection,
    run_id: WorkRunId,
    records: &[WorkObligationRecord],
) -> Result<(), StoreError> {
    let expected = connection
        .prepare(
            "SELECT entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.position",
        )?
        .query_map([run_id.0.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (position, stored_hash, bytes) in expected {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let observation: ExecutionObservation = CanonicalObject::verify(&hash, bytes)?.decode()?;
        let rule_set = obligation_rule_set_for_observation_on(connection, &observation)?;
        for (rule, requirement) in
            crate::control::evaluate_obligation_rules(&rule_set, &observation)
        {
            let matches = records
                .iter()
                .filter(|record| {
                    record.obligation.run_id == run_id
                        && record.obligation.triggering_observation == hash
                        && record.obligation.trigger_position.position == position
                        && record.obligation.rule_set == observation.obligation_rule_set
                        && record.obligation.rule == rule
                        && record.obligation.requirement == requirement
                })
                .count();
            if matches != 1 {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "run {run_id:?} source mutation {hash} has {matches} matching builtin obligation definitions"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn load_work_obligation_by_id_on(
    connection: &Connection,
    obligation_id: WorkObligationId,
) -> Result<WorkObligationRecord, StoreError> {
    let run_id = connection
        .query_row(
            "SELECT run_id FROM work_run_obligations WHERE obligation_id = ?1",
            [obligation_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "work obligation {} does not exist",
                obligation_id.0
            ))
        })?;
    let run_id = parse_work_run_id(&run_id)?;
    load_work_obligation_records_on(connection, run_id, None)?
        .into_iter()
        .find(|record| record.obligation.obligation_id == obligation_id)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "obligation {} disappeared during its verified load",
                obligation_id.0
            ))
        })
}

fn load_work_obligation_record_on(
    connection: &Connection,
    row: &ObligationProjectionRow,
) -> Result<WorkObligationRecord, StoreError> {
    let definition_hash = ObjectHash::from_stored(row.definition_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(row.definition_hash.clone()))?;
    let obligation =
        load_typed_work_object::<WorkObligation>(connection, &definition_hash, "work_obligation")?;
    let state: WorkObligationState =
        serde_json::from_value(serde_json::Value::String(row.state.clone()))?;
    let check_kind: crate::domain::VerificationKind =
        serde_json::from_value(serde_json::Value::String(row.check_kind.clone()))?;
    let check_fingerprint = row
        .check_fingerprint
        .as_ref()
        .map(|value| {
            ObjectHash::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(value.clone()))
        })
        .transpose()?;
    let expected_rule_set = ObjectHash::from_stored(row.rule_set_hash.clone())
        .ok_or_else(|| StoreError::InvalidStoredHash(row.rule_set_hash.clone()))?;
    let expected_trigger = ObjectHash::from_stored(row.triggering_observation_hash.clone()).ok_or(
        StoreError::InvalidStoredHash(row.triggering_observation_hash.clone()),
    )?;
    let scalar_matches = obligation.obligation_id.0.to_string() == row.obligation_id
        && obligation.project_id.0 == row.project_id
        && obligation.root_execution_id.0.to_string() == row.root_execution_id
        && obligation.root_id.0.to_string() == row.root_id
        && obligation.work_id.0.to_string() == row.work_id
        && obligation.run_id.0.to_string() == row.run_id
        && obligation.work_revision == row.work_revision
        && obligation.rule_set == expected_rule_set
        && obligation.rule.rule_id == row.rule_id
        && i64::from(obligation.rule.rule_version) == row.rule_version
        && obligation.triggering_observation == expected_trigger
        && obligation.trigger_position
            == (FeedPosition {
                feed: FeedId::RunExecution(obligation.run_id),
                position: row.trigger_position,
            })
        && obligation.requirement.check_kind == check_kind
        && obligation.requirement.check_fingerprint == check_fingerprint
        && obligation.opened_at.timestamp_millis() == row.opened_at_ms;
    if !scalar_matches {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} does not match its redundant projection",
            row.obligation_id
        )));
    }
    let trigger = load_typed_work_object::<ExecutionObservation>(
        connection,
        &obligation.triggering_observation,
        "execution_observation",
    )?;
    let trigger_entry_matches = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND position = ?2 AND object_kind = 'execution_observation'
               AND object_hash = ?3
         )",
        params![
            obligation.run_id.0.to_string(),
            obligation.trigger_position.position,
            obligation.triggering_observation.as_str()
        ],
        |query| query.get::<_, bool>(0),
    )?;
    let definition_position: Option<i64> = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND object_kind = 'work_obligation' AND object_hash = ?2",
            params![obligation.run_id.0.to_string(), definition_hash.as_str()],
            |query| query.get(0),
        )
        .optional()?;
    if !trigger.source_changed
        || trigger.project_id != obligation.project_id
        || trigger.binding.root_execution_id != obligation.root_execution_id
        || trigger.binding.work_id != obligation.work_id
        || trigger.binding.run_id != obligation.run_id
        || trigger.binding.work_revision != obligation.work_revision
        || trigger.recorded_at != obligation.opened_at
        || !trigger_entry_matches
        || definition_position
            .is_none_or(|position| position <= obligation.trigger_position.position)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} has an invalid trigger or feed binding",
            obligation.obligation_id.0
        )));
    }
    let resolution_hash = row
        .resolution_hash
        .as_ref()
        .map(|value| {
            ObjectHash::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(value.clone()))
        })
        .transpose()?;
    let resolution = resolution_hash
        .as_ref()
        .map(|hash| {
            load_typed_work_object::<WorkObligationResolutionEvent>(
                connection,
                hash,
                "work_obligation_resolution",
            )
        })
        .transpose()?;
    let resolution_position = validate_obligation_resolution_projection(
        connection,
        &definition_hash,
        &obligation,
        state,
        resolution_hash.as_ref(),
        resolution.as_ref(),
        row.resolution_kind.as_deref(),
        row.evidence_hash.as_deref(),
        row.resolved_at_ms,
    )?;
    Ok(WorkObligationRecord {
        definition_hash,
        obligation,
        state,
        resolution_hash,
        resolution,
        resolution_position,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "obligation resolution validation keeps every redundant binding explicit"
)]
fn validate_obligation_resolution_projection(
    connection: &Connection,
    definition_hash: &ObjectHash,
    obligation: &WorkObligation,
    state: WorkObligationState,
    resolution_hash: Option<&ObjectHash>,
    event: Option<&WorkObligationResolutionEvent>,
    projected_kind: Option<&str>,
    projected_evidence: Option<&str>,
    resolved_at_ms: Option<i64>,
) -> Result<Option<FeedPosition>, StoreError> {
    if state == WorkObligationState::Open {
        if resolution_hash.is_some()
            || event.is_some()
            || projected_kind.is_some()
            || projected_evidence.is_some()
            || resolved_at_ms.is_some()
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "open obligation {} carries terminal projection data",
                obligation.obligation_id.0
            )));
        }
        return Ok(None);
    }
    let (resolution_hash, event, resolved_at_ms) = resolution_hash
        .zip(event)
        .zip(resolved_at_ms)
        .map(|((hash, event), at)| (hash, event, at))
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "terminal obligation {} has incomplete resolution data",
                obligation.obligation_id.0
            ))
        })?;
    if event.project_id != obligation.project_id
        || event.obligation_id != obligation.obligation_id
        || event.definition != *definition_hash
        || event.run_id != obligation.run_id
        || event.created_at.timestamp_millis() != resolved_at_ms
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation resolution {resolution_hash} crosses its definition binding"
        )));
    }
    let resolution_position =
        run_feed_position_for_object_on(connection, obligation.run_id, resolution_hash)?;
    match &event.resolution {
        WorkObligationResolution::Satisfied {
            evidence,
            evaluated_cut,
        } => {
            if state != WorkObligationState::Satisfied
                || projected_kind != Some("satisfied")
                || projected_evidence != Some(evidence.as_str())
                || evaluated_cut.feed != FeedId::RunExecution(obligation.run_id)
                || evaluated_cut.position >= resolution_position.position
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "satisfied obligation {} has inconsistent terminal bindings",
                    obligation.obligation_id.0
                )));
            }
            let verification = load_typed_work_object::<VerificationEvidence>(
                connection,
                evidence,
                "verification_evidence",
            )?;
            let producer = load_typed_work_object::<ExecutionObservation>(
                connection,
                &verification.producer_observation,
                "execution_observation",
            )?;
            let evidence_position =
                run_feed_position_for_object_on(connection, obligation.run_id, evidence)?;
            let (mutation_position, latest_mutation) =
                latest_source_mutation_on(connection, obligation.run_id, evaluated_cut.position)?
                    .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "satisfied obligation has no source mutation at its evaluated cut".into(),
                    )
                })?;
            let satisfied = crate::control::evaluate_obligation_satisfaction(
                &crate::control::ObligationSatisfactionInput {
                    open_obligations: std::slice::from_ref(obligation),
                    evidence: &verification,
                    producer: &producer,
                    latest_mutation: &latest_mutation,
                    evidence_position: evidence_position.position,
                    latest_mutation_position: mutation_position,
                    evaluated_cut,
                },
            );
            if satisfied != [obligation.obligation_id] {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "satisfied obligation {} does not match its verification evidence",
                    obligation.obligation_id.0
                )));
            }
        }
        WorkObligationResolution::Waived { waived_by, reason } => {
            if state != WorkObligationState::Waived
                || projected_kind != Some("waived")
                || projected_evidence.is_some()
                || waived_by.trim().is_empty()
                || waived_by.trim() != waived_by
                || reason.trim().is_empty()
                || reason.trim() != reason
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "waived obligation {} has inconsistent terminal bindings",
                    obligation.obligation_id.0
                )));
            }
        }
    }
    Ok(Some(resolution_position))
}

struct TypedEvidenceProjection<'a> {
    kind: WorkEvidenceKind,
    workspace_id: &'a str,
    source_revision: &'a str,
    producer_session_id: &'a SessionId,
    producer_observation: Option<&'a ObjectHash>,
    check_fingerprint: Option<&'a ObjectHash>,
    verification_result: Option<String>,
    observed_at: DateTime<Utc>,
    environment_fingerprint: Option<&'a ObjectHash>,
    environment_evidence: Option<&'a ObjectHash>,
    components_json: Option<Vec<u8>>,
}

pub(in crate::storage) fn append_control_verification_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(evidence)?;
    let result = encode_state(evidence.result)?;
    let evidence_hash = append_control_typed_evidence_on(
        transaction,
        &evidence.project_id,
        &evidence.binding,
        &evidence.session_id,
        &evidence.actor,
        evidence.recorded_at,
        &object,
        &TypedEvidenceProjection {
            kind: WorkEvidenceKind::Verification,
            workspace_id: &evidence.source_basis.workspace_id,
            source_revision: &evidence.source_basis.source_revision,
            producer_session_id: &evidence.session_id,
            producer_observation: Some(&evidence.producer_observation),
            check_fingerprint: Some(&evidence.check_fingerprint),
            verification_result: Some(result),
            observed_at: evidence.completed_at,
            environment_fingerprint: None,
            environment_evidence: evidence.environment.as_ref(),
            components_json: None,
        },
    )?;
    satisfy_open_obligations_on(transaction, evidence, &evidence_hash)?;
    Ok(evidence_hash)
}

pub(in crate::storage) fn append_control_environment_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &EnvironmentEvidence,
) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(evidence)?;
    append_control_typed_evidence_on(
        transaction,
        &evidence.project_id,
        &evidence.binding,
        &evidence.session_id,
        &evidence.actor,
        evidence.recorded_at,
        &object,
        &TypedEvidenceProjection {
            kind: WorkEvidenceKind::Environment,
            workspace_id: &evidence.source_basis.workspace_id,
            source_revision: &evidence.source_basis.source_revision,
            producer_session_id: &evidence.session_id,
            producer_observation: None,
            check_fingerprint: None,
            verification_result: None,
            observed_at: evidence.observed_at,
            environment_fingerprint: Some(&evidence.environment_fingerprint),
            environment_evidence: None,
            components_json: evidence
                .components
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?,
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "typed evidence persistence keeps every redundant binding explicit"
)]
fn append_control_typed_evidence_on(
    transaction: &Transaction<'_>,
    project_id: &crate::domain::ProjectId,
    binding: &ControlWorkBinding,
    session_id: &SessionId,
    actor: &crate::domain::ActorContext,
    recorded_at: DateTime<Utc>,
    object: &CanonicalObject,
    projection: &TypedEvidenceProjection<'_>,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, binding.work_id)?;
    let run = load_work_run(transaction, binding.run_id)?;
    let mut root_execution = load_root_execution(transaction, binding.root_execution_id)?;
    if &item.project_id != project_id
        || item.root_id != root_execution.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root_execution.root_execution_id
        || binding.root_execution_id != run.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "typed evidence binding does not match canonical work state".into(),
        ));
    }
    let object_kind = match projection.kind {
        WorkEvidenceKind::Generic => {
            return Err(StoreError::InvalidWorkProjection(
                "generic evidence cannot use the typed evidence writer".into(),
            ));
        }
        WorkEvidenceKind::Verification => "verification_evidence",
        WorkEvidenceKind::Environment => "environment_evidence",
    };
    SqliteStore::insert_object(transaction, object_kind, object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (
             evidence_hash, work_id, run_id, evidence_kind,
             workspace_id, source_revision, producer_session_id,
             producer_observation_hash, check_fingerprint,
             verification_result, observed_at_ms, environment_fingerprint,
             environment_evidence_hash, components_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string(),
            encode_state(projection.kind)?,
            projection.workspace_id,
            projection.source_revision,
            projection.producer_session_id.0,
            projection.producer_observation.map(ObjectHash::as_str),
            projection.check_fingerprint.map(ObjectHash::as_str),
            projection.verification_result.as_deref(),
            projection.observed_at.timestamp_millis(),
            projection.environment_fingerprint.map(ObjectHash::as_str),
            projection.environment_evidence.map(ObjectHash::as_str),
            projection.components_json.as_deref(),
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        object_kind,
        object,
    )?;
    let root_changed = expect_root_contributor(&mut root_execution, session_id)
        | add_root_contribution(&mut root_execution, session_id, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = recorded_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    let claim = load_work_claim_optional(transaction, run.run_id)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: Some(run.run_id),
        revision: item.revision,
        work: item,
        run: Some(run),
        root_execution: Some(root_execution),
        claim,
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::TypedEvidenceAdded {
            evidence: object.hash().clone(),
            evidence_kind: projection.kind,
        },
        actor: actor.clone(),
        created_at: recorded_at,
    };
    append_work_event(transaction, &event)?;
    Ok(object.hash().clone())
}

pub(in crate::storage) fn append_control_execution_observation_on(
    transaction: &Transaction<'_>,
    observation: &ExecutionObservation,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, observation.binding.work_id)?;
    let run = load_work_run(transaction, observation.binding.run_id)?;
    let root_execution = load_root_execution(transaction, observation.binding.root_execution_id)?;
    if item.project_id != observation.project_id
        || item.root_id != root_execution.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root_execution.root_execution_id
        || observation.binding.root_execution_id != run.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "execution observation binding does not match canonical work state".into(),
        ));
    }
    let object = CanonicalObject::freeze(observation)?;
    SqliteStore::insert_object(transaction, "execution_observation", &object)?;
    let positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "execution_observation",
        &object,
    )?;
    let trigger_position = positions
        .iter()
        .find(|position| position.feed == FeedId::RunExecution(run.run_id))
        .cloned()
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "execution observation did not receive a run-feed position".into(),
            )
        })?;
    append_builtin_obligations_on(transaction, observation, object.hash(), &trigger_position)?;
    Ok(object.hash().clone())
}

pub(super) fn obligation_rule_set_for_observation_on(
    connection: &Connection,
    observation: &ExecutionObservation,
) -> Result<crate::domain::ObligationRuleSet, StoreError> {
    SqliteStore::load_obligation_rule_set_on(connection, &observation.obligation_rule_set)
}

fn append_builtin_obligations_on(
    transaction: &Transaction<'_>,
    observation: &ExecutionObservation,
    observation_hash: &ObjectHash,
    trigger_position: &FeedPosition,
) -> Result<Vec<ObjectHash>, StoreError> {
    let item = load_work_item(transaction, observation.binding.work_id)?;
    let mut definitions = Vec::new();
    let rule_set = obligation_rule_set_for_observation_on(transaction, observation)?;
    for (rule, requirement) in crate::control::evaluate_obligation_rules(&rule_set, observation) {
        let obligation = WorkObligation {
            schema_version: SCHEMA_VERSION,
            obligation_id: WorkObligationId::new(),
            project_id: item.project_id.clone(),
            root_execution_id: observation.binding.root_execution_id,
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: observation.binding.run_id,
            work_revision: observation.binding.work_revision,
            rule_set: observation.obligation_rule_set.clone(),
            rule,
            triggering_observation: observation_hash.clone(),
            trigger_position: trigger_position.clone(),
            requirement,
            opened_at: observation.recorded_at,
        };
        let object = CanonicalObject::freeze(&obligation)?;
        SqliteStore::insert_object(transaction, "work_obligation", &object)?;
        append_to_work_feeds(
            transaction,
            &obligation.project_id,
            obligation.root_id,
            Some(obligation.run_id),
            None,
            "work_obligation",
            &object,
        )?;
        transaction.execute(
            "INSERT INTO work_run_obligations (
                 obligation_id, definition_hash, project_id, root_execution_id,
                 root_id, work_id, run_id, work_revision, rule_set_hash, rule_id, rule_version,
                 triggering_observation_hash, trigger_position, check_kind,
                 check_fingerprint, state, opened_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                obligation.obligation_id.0.to_string(),
                object.hash().as_str(),
                obligation.project_id.0,
                obligation.root_execution_id.0.to_string(),
                obligation.root_id.0.to_string(),
                obligation.work_id.0.to_string(),
                obligation.run_id.0.to_string(),
                obligation.work_revision,
                obligation.rule_set.as_str(),
                obligation.rule.rule_id,
                obligation.rule.rule_version,
                obligation.triggering_observation.as_str(),
                obligation.trigger_position.position,
                encode_state(obligation.requirement.check_kind)?,
                obligation
                    .requirement
                    .check_fingerprint
                    .as_ref()
                    .map(ObjectHash::as_str),
                encode_state(WorkObligationState::Open)?,
                obligation.opened_at.timestamp_millis(),
            ],
        )?;
        definitions.push(object.hash().clone());
    }
    Ok(definitions)
}

fn satisfy_open_obligations_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
    evidence_hash: &ObjectHash,
) -> Result<Vec<ObjectHash>, StoreError> {
    let evidence_position =
        run_feed_position_for_object_on(transaction, evidence.binding.run_id, evidence_hash)?;
    let evaluated_cut = current_run_feed_cut_on(transaction, evidence.binding.run_id)?;
    let Some((latest_mutation_position, latest_mutation)) =
        latest_source_mutation_on(transaction, evidence.binding.run_id, evaluated_cut.position)?
    else {
        return Ok(Vec::new());
    };
    let producer = load_typed_work_object::<ExecutionObservation>(
        transaction,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    let records = load_work_obligation_records_on(transaction, evidence.binding.run_id, None)?
        .into_iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .collect::<Vec<_>>();
    let obligations = records
        .iter()
        .map(|record| record.obligation.clone())
        .collect::<Vec<_>>();
    let satisfied = crate::control::evaluate_obligation_satisfaction(
        &crate::control::ObligationSatisfactionInput {
            open_obligations: &obligations,
            evidence,
            producer: &producer,
            latest_mutation: &latest_mutation,
            evidence_position: evidence_position.position,
            latest_mutation_position,
            evaluated_cut: &evaluated_cut,
        },
    );
    let by_id = records
        .into_iter()
        .map(|record| (record.obligation.obligation_id, record))
        .collect::<HashMap<_, _>>();
    let mut resolution_hashes = Vec::new();
    for obligation_id in satisfied {
        let record = by_id.get(&obligation_id).ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "pure obligation evaluation returned an unknown definition".into(),
            )
        })?;
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: evidence.project_id.clone(),
            obligation_id,
            definition: record.definition_hash.clone(),
            run_id: evidence.binding.run_id,
            resolution: WorkObligationResolution::Satisfied {
                evidence: evidence_hash.clone(),
                evaluated_cut: evaluated_cut.clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.recorded_at,
        };
        let object = append_obligation_resolution_on(transaction, record, &event)?;
        resolution_hashes.push(object);
    }
    Ok(resolution_hashes)
}

fn append_obligation_resolution_on(
    transaction: &Transaction<'_>,
    record: &WorkObligationRecord,
    event: &WorkObligationResolutionEvent,
) -> Result<ObjectHash, StoreError> {
    let (state, kind, evidence_hash) = match &event.resolution {
        WorkObligationResolution::Satisfied { evidence, .. } => (
            WorkObligationState::Satisfied,
            "satisfied",
            Some(evidence.as_str()),
        ),
        WorkObligationResolution::Waived { .. } => (WorkObligationState::Waived, "waived", None),
    };
    let object = CanonicalObject::freeze(event)?;
    SqliteStore::insert_object(transaction, "work_obligation_resolution", &object)?;
    append_to_work_feeds(
        transaction,
        &record.obligation.project_id,
        record.obligation.root_id,
        Some(record.obligation.run_id),
        None,
        "work_obligation_resolution",
        &object,
    )?;
    let changed = transaction.execute(
        "UPDATE work_run_obligations SET
             state = ?3, resolution_hash = ?4, resolution_kind = ?5,
             evidence_hash = ?6, resolved_at_ms = ?7
         WHERE obligation_id = ?1 AND definition_hash = ?2
           AND state = 'open' AND resolution_hash IS NULL",
        params![
            event.obligation_id.0.to_string(),
            record.definition_hash.as_str(),
            encode_state(state)?,
            object.hash().as_str(),
            kind,
            evidence_hash,
            event.created_at.timestamp_millis(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} lost its open-state compare-and-swap",
            event.obligation_id.0
        )));
    }
    Ok(object.hash().clone())
}

/// Every work-anchored memory version and contradiction must sit in the
/// project feed and its root-work feed; a missing entry would let peers miss a
/// contested or new rule without any cursor gap.
fn validate_acceptance(
    item: &WorkItem,
    completion_evidence: &[ObjectHash],
    results: &[AcceptanceResult],
    actor_assurance: crate::domain::AssuranceLevel,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    let shaped = normalize_completion_acceptance_shape(item, results, actor_assurance)?;
    let completion_evidence = completion_evidence
        .iter()
        .map(ObjectHash::as_str)
        .collect::<HashSet<_>>();
    let mut normalized = Vec::with_capacity(shaped.len());
    for mut result in shaped {
        let evidence = unique_hashes(&result.evidence);
        if evidence.is_empty()
            || evidence
                .iter()
                .any(|hash| !completion_evidence.contains(hash.as_str()))
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "acceptance criterion {:?} must cite completion evidence",
                    result.criterion
                ),
            });
        }
        result.evidence = evidence;
        normalized.push(result);
    }
    Ok(normalized)
}

pub(crate) fn normalize_completion_acceptance_shape(
    item: &WorkItem,
    results: &[AcceptanceResult],
    actor_assurance: crate::domain::AssuranceLevel,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    if item.acceptance.len() != results.len() {
        let missing = item.acceptance.iter().find(|criterion| {
            !results
                .iter()
                .any(|result| result.criterion.trim() == criterion.as_str())
        });
        if let Some(criterion) = missing {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        }
        return Err(StoreError::WorkCompletionRefused {
            work: item.work_id,
            reason: "acceptance results do not cover every current criterion".into(),
        });
    }
    let mut by_criterion = HashMap::new();
    for result in results {
        if result.assurance != actor_assurance {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "acceptance assurance must equal the completing actor assurance".into(),
            });
        }
        let criterion = normalize_text(&result.criterion, "acceptance criterion")?;
        if by_criterion.insert(criterion, result).is_some() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "acceptance results contain a duplicate criterion".into(),
            });
        }
    }
    let mut normalized = Vec::with_capacity(item.acceptance.len());
    for criterion in &item.acceptance {
        let Some(result) = by_criterion.get(criterion) else {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        };
        if !result.satisfied {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        }
        normalized.push(AcceptanceResult {
            criterion: criterion.clone(),
            satisfied: true,
            evidence: result.evidence.clone(),
            assurance: result.assurance,
            note: result.note.trim().to_owned(),
        });
    }
    Ok(normalized)
}

pub(super) fn validate_completion_seal_children_on(
    connection: &Connection,
    seal: &CompletionSeal,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > 1_024 {
        return Err(StoreError::InvalidWorkProjection(
            "completion-seal child graph exceeds the corruption guard".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut seen_children = HashSet::new();
    let mut transitively_restored = false;
    for child_hash in &seal.required_child_seals {
        if !seen.insert(child_hash.clone()) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats child seal {child_hash}",
                seal.run_id.0
            )));
        }
        let child_seal: CompletionSeal =
            load_typed_work_object(connection, child_hash, "completion_seal")?;
        let child = load_work_item(connection, child_seal.work_id)?;
        if !seen_children.insert(child.work_id) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats child work {:?}",
                seal.run_id.0, child.work_id
            )));
        }
        if child.parent_id != Some(seal.work_id)
            || child.child_requirement != ChildRequirement::Required
            || child_seal.root_id != seal.root_id
            || child_seal.root_execution_id != seal.root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} cites unrelated child seal {child_hash}",
                seal.run_id.0
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &child_seal)?;
        validate_completion_seal_environment_basis_on(connection, &child_seal)?;
        validate_completion_seal_children_on(connection, &child_seal, depth + 1)?;
        transitively_restored |= child_seal.restored;
    }
    for record_hash in &seal.restored_child_completions {
        if !seen.insert(record_hash.clone()) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats restored child record {record_hash}",
                seal.run_id.0
            )));
        }
        let record: RestoredRecord =
            load_typed_work_object(connection, record_hash, "work_restored_record")?;
        let child = load_work_item(connection, record.work_id)?;
        let latest = super::query::latest_restored_record_hash(connection, child.work_id)?;
        if latest.as_ref() != Some(record_hash)
            || record.history.completion.is_none()
            || !seen_children.insert(child.work_id)
            || child.parent_id != Some(seal.work_id)
            || child.root_id != seal.root_id
            || child.child_requirement != ChildRequirement::Required
            || child.lifecycle != WorkLifecycle::Completed
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} cites unrelated restored child record {record_hash}",
                seal.run_id.0
            )));
        }
        transitively_restored = true;
    }
    if seal.restored != transitively_restored {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has an invalid restored marker",
            seal.run_id.0
        )));
    }
    Ok(())
}

fn required_restored_child_completions(
    connection: &Connection,
    parent_id: WorkId,
) -> Result<Vec<ObjectHash>, StoreError> {
    let child_ids = connection
        .prepare(
            "SELECT child.work_id FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'
               AND child.lifecycle = 'completed'
             ORDER BY child.work_id",
        )?
        .query_map([parent_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut records = Vec::with_capacity(child_ids.len());
    for child_id in child_ids {
        let child_id = parse_work_id(&child_id)?;
        let child = load_work_item(connection, child_id)?;
        if !work_completed_by_restored_record_on(connection, &child)? {
            continue;
        }
        let Some((hash, record)) = super::query::latest_restored_record(connection, child_id)?
        else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completed restored child {child_id:?} has no restored completion record"
            )));
        };
        if record.history.completion.is_none() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completed restored child {child_id:?} has no completion proof"
            )));
        }
        records.push(hash);
    }
    Ok(records)
}

pub(super) fn required_child_seals(
    connection: &Connection,
    parent_id: WorkId,
    root_execution_id: RootExecutionId,
) -> Result<Vec<ObjectHash>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT child.work_id, run.run_id, seals.seal_hash
         FROM work_items child
         JOIN work_runs run ON run.work_id = child.work_id
         JOIN work_completion_seals seals ON seals.run_id = run.run_id
         WHERE child.parent_id = ?1
           AND run.root_execution_id = ?2
           AND child.child_requirement = 'required'
           AND child.lifecycle = 'completed'
           AND run.state = 'completed'
           AND run.generation = (
               SELECT MAX(latest.generation) FROM work_runs latest
               WHERE latest.work_id = child.work_id
           )
         ORDER BY child.work_id",
    )?;
    let rows = statement
        .query_map(
            params![parent_id.0.to_string(), root_execution_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut hashes = Vec::with_capacity(rows.len());
    for (child_work, child_run, stored_hash) in rows {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let seal: CompletionSeal = load_typed_work_object(connection, &hash, "completion_seal")?;
        if seal.work_id.0.to_string() != child_work
            || seal.run_id.0.to_string() != child_run
            || seal.root_execution_id != root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required child seal {hash} does not match its child run"
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &seal)?;
        validate_completion_seal_environment_basis_on(connection, &seal)?;
        validate_completion_seal_children_on(connection, &seal, 0)?;
        hashes.push(hash);
    }
    Ok(hashes)
}

pub(super) fn validated_required_child_waivers(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
) -> Result<Vec<RequiredChildWaiver>, StoreError> {
    // The root-execution projection is already bound to the latest canonical
    // event. An empty projected waiver set cannot authorize completion, so it
    // is safe to avoid replaying the retained root history here. Doctor still
    // performs the exhaustive comparison below for nonempty projected sets.
    if execution.required_child_waivers.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT object_hash FROM work_feed_entries
         WHERE feed_kind = 'root_work' AND feed_id = ?1
           AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let hashes = statement
        .query_map([execution.root_id.0.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut events = HashMap::new();
    for stored_hash in hashes {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let event: WorkEvent = load_typed_work_object(connection, &hash, "work_event")?;
        let Some(event_execution) = event.root_execution.as_ref() else {
            continue;
        };
        if event_execution.root_execution_id != execution.root_execution_id {
            continue;
        }
        let WorkTransition::RequiredChildWaived {
            child_id,
            child_revision,
            reason,
        } = &event.transition
        else {
            continue;
        };
        let child = load_work_item(connection, *child_id)?;
        let waiver = RequiredChildWaiver {
            work_id: *child_id,
            work_revision: *child_revision,
            waived_by: event.actor.actor_id.clone(),
            reason: reason.clone(),
        };
        let event_contains_exact_waiver = event_execution
            .required_child_waivers
            .iter()
            .filter(|candidate| *candidate == &waiver)
            .count()
            == 1;
        let valid = event.schema_version == SCHEMA_VERSION
            && event.project_id == execution.project_id
            && event.root_id == execution.root_id
            && event.work_id == child.parent_id.unwrap_or(event.work_id)
            && child.parent_id == Some(event.work_id)
            && child.root_id == execution.root_id
            && child.child_requirement == ChildRequirement::Required
            && matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
            && child.revision == *child_revision
            && event_contains_exact_waiver;
        if !valid || events.insert(*child_id, waiver).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required-child waiver event {hash} is not uniquely bound"
            )));
        }
    }

    let mut projected = HashMap::new();
    for waiver in &execution.required_child_waivers {
        if projected.insert(waiver.work_id, waiver.clone()).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "root execution {:?} duplicates a required-child waiver for {:?}",
                execution.root_execution_id, waiver.work_id
            )));
        }
    }
    if projected != events {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} required-child waivers do not match canonical events",
            execution.root_execution_id
        )));
    }

    let mut direct = Vec::new();
    for waiver in projected.into_values() {
        let child = load_work_item(connection, waiver.work_id)?;
        if child.parent_id == Some(parent_id) {
            direct.push(waiver);
        }
    }
    direct.sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
    Ok(direct)
}

fn verify_required_child_waiver_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT root_execution_id, execution_json
         FROM work_root_executions ORDER BY root_execution_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (root_execution_id, bytes) in rows {
        *checked += 1;
        let valid = serde_json::from_slice::<RootExecution>(&bytes).is_ok_and(|execution| {
            validated_required_child_waivers(connection, execution.root_id, &execution).is_ok()
        });
        if !valid {
            invalid.push(format!(
                "work_root_execution:{root_execution_id}:invalid_required_child_waivers"
            ));
        }
    }
    Ok(())
}

fn unfinished_optional_children(
    connection: &Connection,
    parent_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT work_id FROM work_items
         WHERE parent_id = ?1 AND child_requirement = 'optional'
           AND lifecycle != 'completed'
         ORDER BY work_id",
    )?;
    statement
        .query_map([parent_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| parse_work_id(&row?))
        .collect()
}

fn live_descendant_execution_authority(
    connection: &Connection,
    root_id: WorkId,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let descendant_ids = {
        let mut statement = connection.prepare(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT work_id FROM descendants ORDER BY work_id",
        )?;
        statement
            .query_map([root_id.0.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for stored_id in descendant_ids {
        let item = load_work_item(connection, parse_work_id(&stored_id)?)?;
        let Some(run_id) = item.active_run_id else {
            continue;
        };
        let run = load_work_run(connection, run_id)?;
        if run.work_id != item.work_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "active run {run_id:?} belongs to a different work item"
            )));
        }
        if load_work_claim_optional(connection, run_id)?
            .is_some_and(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now)
        {
            return Ok(true);
        }
        let offers = {
            let mut statement = connection.prepare(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE run_id = ?1 ORDER BY offer_id",
            )?;
            statement
                .query_map([run_id.0.to_string()], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for row in offers {
            let offer = load_handoff_offer_projection(connection, row)?;
            if offer.state == WorkHandoffState::Offered && offer.expires_at > now {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn ancestors_admit_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut reached_root = item.work_id == item.root_id;
    while let Some(parent) = parent_id {
        if !visited.insert(parent) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != item.project_id || ancestor.root_id != item.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.lifecycle != WorkLifecycle::Open {
            return Ok(false);
        }
        reached_root |= ancestor.work_id == item.root_id;
        parent_id = ancestor.parent_id;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work {:?} does not reach its declared root {:?}",
            item.work_id, item.root_id
        )));
    }
    Ok(true)
}

pub(super) fn work_run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let run_id = item.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", item.work_id))
    })?;
    let run = load_work_run(connection, run_id)?;
    run_uses_active_root_execution(connection, item, &run)
}

pub(super) fn run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
    run: &WorkRun,
) -> Result<bool, StoreError> {
    if run.work_id != item.work_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "run {:?} does not belong to work {:?}",
            run.run_id, item.work_id
        )));
    }
    let Some(execution) = active_root_execution_optional(connection, item.root_id)? else {
        return Ok(false);
    };
    if execution.project_id != item.project_id || execution.root_id != item.root_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} crosses the work project or root boundary",
            execution.root_execution_id
        )));
    }
    Ok(execution.root_execution_id == run.root_execution_id)
}

pub(super) fn feed_head(connection: &Connection, feed: &FeedId) -> Result<i64, StoreError> {
    let (feed_kind, feed_id) = feed_parts(feed);
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind = ?1 AND feed_id = ?2",
            params![feed_kind, feed_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn refuse_completed_ancestor(connection: &Connection, item: &WorkItem) -> Result<(), StoreError> {
    let mut parent_id = item.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.lifecycle == WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(format!(
                "cannot reopen child work while completed ancestor {:?} consumes its seal",
                ancestor.work_id
            )));
        }
        parent_id = ancestor.parent_id;
    }
    Ok(())
}

pub(super) fn work_is_ancestor_of(
    connection: &Connection,
    candidate: WorkId,
    descendant: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = descendant.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != descendant.project_id || ancestor.root_id != descendant.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.work_id == candidate {
            return Ok(true);
        }
        parent_id = ancestor.parent_id;
    }
    Ok(false)
}
