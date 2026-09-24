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
    persist_work_run, unique_hashes, validate_live_claim_on, validated_current_work_relation_basis,
    waive_root_contributor, work_relation_fingerprint,
};
use super::query::{
    active_root_execution, active_root_execution_optional, completion_recovery_snapshot_on,
    feed_parts, incomplete_prerequisite_projections, latest_canonical_work_event_for_item_optional,
    load_root_execution, load_root_execution_with_ref, load_work_claim_optional, load_work_item,
    load_work_run, parse_work_id, parse_work_run_id, work_completed_by_restored_record_on,
};
use super::{
    CompleteWorkStorageResult, EvidenceProjectionRow, MAX_COMPLETION_ENVIRONMENT_EVIDENCE,
    MAX_OPEN_COMPLETION_OBLIGATIONS, ObligationProjectionRow, WorkEventDraft, WorkObligationRecord,
    WorkObligationWaiverFingerprint, WorkRelationBasis, WorkRelationBlockerBasis,
    empty_work_relation_basis,
};
use crate::{
    CanonicalObject, ObjectId, RestoredRecord,
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
        WorkObligationResolutionEvent, WorkObligationState, WorkRun, WorkRunId, WorkRunState,
        WorkTransition,
    },
    memory::Redactor,
};

mod child_barriers;
mod child_resolutions;
mod lifecycle;
mod projections;
mod root_binding;

pub(super) use root_binding::{validate_seal_root_event, validate_stored_seal_root};

pub(super) use child_barriers::{
    ancestors_admit_execution, feed_head, run_uses_active_root_execution,
    validate_completion_seal_children_on, validated_required_child_waivers, work_is_ancestor_of,
    work_run_uses_active_root_execution,
};
use child_barriers::{
    live_descendant_execution_authority, refuse_completed_ancestor, required_child_seals,
    required_restored_child_completions, unfinished_optional_children,
    verify_required_child_waiver_bindings,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
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
            request_object.key(),
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
        let acceptance_policy = SqliteStore::load_acceptance_evaluation_policy_on(&transaction)?;
        let (acceptance_results, acceptance_evaluation) = if acceptance_policy.is_evaluated() {
            if !request.acceptance.is_empty() {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "explicit acceptance results are not accepted under an evaluated acceptance policy; record an acceptance evaluation with evaluate instead".into(),
                });
            }
            let assessment = super::acceptance_evaluation::assess_on(
                &transaction,
                &item,
                run.run_id,
                &acceptance_policy,
                request.source_fingerprint.as_deref(),
            )?;
            // A fresh, all-pass evaluation is the only path to a sealed vector;
            // everything else is a recovery cause the evaluator must resolve.
            let outcome: Result<(Vec<AcceptanceResult>, ObjectId), WorkCompletionRecoveryCause> =
                match assessment {
                    super::acceptance_evaluation::AcceptanceEvaluationAssessment::Absent => {
                        Err(WorkCompletionRecoveryCause::MissingAcceptanceEvaluation {
                            criterion: item.acceptance.first().cloned().unwrap_or_default(),
                        })
                    }
                    super::acceptance_evaluation::AcceptanceEvaluationAssessment::Stale(reason) => {
                        Err(WorkCompletionRecoveryCause::AcceptanceEvaluationStale { reason })
                    }
                    super::acceptance_evaluation::AcceptanceEvaluationAssessment::Fresh {
                        hash,
                        evaluation,
                    } => match super::acceptance_evaluation::blocking_cause(&evaluation) {
                        Some(cause) => Err(cause),
                        None => Ok((
                            super::acceptance_evaluation::derive_acceptance_results(
                                &evaluation,
                                request.actor.assurance,
                            ),
                            hash,
                        )),
                    },
                };
            match outcome {
                Ok((derived, hash)) => {
                    // The evaluation froze its own run-evidence selection when
                    // it was recorded; shape, coverage, and assurance are
                    // re-checked against the item being sealed, and the
                    // closure invariant still holds: every citation the sealed
                    // vector carries must be named by the completion evidence
                    // set the checkpoint acknowledged.
                    let derived = normalize_completion_acceptance_shape(
                        &item,
                        &derived,
                        request.actor.assurance,
                    )?;
                    ensure_acceptance_citations_within(&item, &evidence, &derived)?;
                    (derived, Some(hash))
                }
                Err(cause) if return_recovery => {
                    let recovery =
                        completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                    return Ok(CompleteWorkStorageResult::Recovery(recovery));
                }
                Err(cause) => {
                    return Err(StoreError::WorkCompletionRecoveryRequired {
                        work: item.work_id,
                        cause,
                    });
                }
            }
        } else {
            let results = match validate_acceptance(
                &item,
                &evidence,
                &request.acceptance,
                request.actor.assurance,
            ) {
                Ok(value) => value,
                Err(StoreError::WorkCompletionRecoveryRequired { cause, .. })
                    if return_recovery =>
                {
                    let recovery =
                        completion_recovery_snapshot_on(&transaction, &item, run.run_id, cause)?;
                    return Ok(CompleteWorkStorageResult::Recovery(recovery));
                }
                Err(error) => return Err(error),
            };
            (results, None)
        };
        let acceptance = acceptance_results;
        let drain = request.drain.clone();
        if !drain.reconciled_action_outcomes.is_empty()
            || !drain.released_resource_leases.is_empty()
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "V1 completion drain accepts only a zero-linked-state attestation: action outcomes are not yet linked to work runs, and the historical resource-lease drain field must be empty".into(),
            });
        }
        let incomplete = incomplete_prerequisite_projections(&transaction, item.work_id)?;
        if !incomplete.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!("prerequisites remain incomplete: {incomplete:?}"),
            });
        }
        let (mut root_execution, pre_seal_root) =
            load_root_execution_with_ref(&transaction, run.root_execution_id)?;
        let required_child_seals =
            required_child_seals(&transaction, item.work_id, run.root_execution_id)?;
        let restored_child_completions =
            required_restored_child_completions(&transaction, item.work_id)?;
        let required_child_waivers =
            validated_required_child_waivers(&transaction, item.work_id, &root_execution)?;
        let required_child_resolutions =
            super::child_resolution::required_successor_resolutions_on(
                &transaction,
                item.work_id,
                run.root_execution_id,
                &required_child_waivers
                    .iter()
                    .map(|waiver| waiver.work_id)
                    .collect(),
            )?;
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
                    + required_child_waivers.len()
                    + required_child_resolutions.len(),
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
                        && !required_child_resolutions
                            .iter()
                            .any(|resolution| resolution.work_id() == *child)
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
        let run_feed = FeedId::RunExecution(run.run_id);
        let checkpoint_cut =
            checkpoint_feed_end(checkpoint_value.acknowledged_run_position.position)?;
        if checkpoint_value.acknowledged_run_position.feed != run_feed
            || checkpoint_cut != feed_head(&transaction, &run_feed)?
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "the final checkpoint does not reach the current pre-seal run-feed cut"
                    .into(),
            });
        }
        // The stock source-change rule records rather than blocks. Each of its
        // obligations still open here is resolved as a waiver in the completing
        // actor's name, after the checkpoint and inside the sealed cut, so the
        // seal still binds only terminal obligations and the untested change
        // stays on record. A refusal below rolls the waivers back with the rest,
        // and a recovery answer is read from the state before them.
        transaction.execute_batch(&format!("SAVEPOINT {UNTESTED_WAIVERS_SAVEPOINT}"))?;
        waive_untested_source_changes_on(
            &transaction,
            &item,
            run.run_id,
            &request.actor,
            request.completed_at,
        )?;
        let completion_cut = FeedPosition {
            position: feed_head(&transaction, &run_feed)?,
            feed: run_feed,
        };
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
                return recovery_before_untested_waivers(&transaction, &item, run.run_id, cause);
            }
            Err(error) => return Err(error),
        };
        let acceptance = bind_acceptance_to_obligations_on(
            &transaction,
            &item,
            run.run_id,
            &completion_cut,
            &evidence,
            acceptance_evaluation.is_some(),
            acceptance,
        )?;
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
        let accepted_work_revision = CanonicalObject::mint(&item)?;
        SqliteStore::insert_object(&transaction, "work_item_revision", &accepted_work_revision)?;
        // The checkpoint commits both facts atomically. Completion must not
        // heal missing canonical accounting or seal a state at another address.
        if !root_execution.expected_contributors.contains(&claim.holder)
            || !root_execution.contributions.iter().any(|contribution| {
                contribution.participant == claim.holder && contribution.object == checkpoint
            })
        {
            return Err(StoreError::InvalidWorkProjection(
                "completion root accounting is missing the holder or current checkpoint; run `engram doctor` and inspect the recorded history before restoring a verified store; completion does not repair canonical accounting".into(),
            ));
        }
        if item.work_id == item.root_id
            && let Some(participant) = first_unaccounted_root_contributor(&root_execution)
        {
            let cause = WorkCompletionRecoveryCause::MissingContribution {
                participant: participant.clone(),
            };
            if return_recovery {
                return recovery_before_untested_waivers(&transaction, &item, run.run_id, cause);
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
            root_execution: pre_seal_root.clone(),
            run_id: run.run_id,
            run_generation: run.generation,
            accepted_work_revision: item.revision,
            accepted_work_revision_hash: accepted_work_revision.key().clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            completion_cut,
            checkpoint: Some(checkpoint),
            evidence,
            acceptance,
            acceptance_evaluation,
            obligation_schema_version: COMPLETION_OBLIGATION_SCHEMA_VERSION,
            obligations,
            environment_schema_version: COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
            environment,
            required_child_seals,
            required_child_waivers,
            required_child_resolutions,
            restored: child_seal_is_restored || !restored_child_completions.is_empty(),
            restored_child_completions,
            unfinished_optional_children,
            drain,
            actor: request.actor.clone(),
            completed_at: request.completed_at,
        };
        validate_completion_seal_obligation_basis_on(&transaction, &seal)?;
        validate_completion_seal_environment_basis_on(&transaction, &seal)?;
        validate_completion_seal_children_on(&transaction, &seal, 0)?;
        super::acceptance_evaluation::validate_completion_seal_acceptance_evaluation_on(
            &transaction,
            &seal,
        )?;
        let seal_object = CanonicalObject::mint(&seal)?;
        SqliteStore::insert_object(&transaction, "completion_seal", &seal_object)?;
        transaction.execute(
            "INSERT INTO work_completion_seals (
                 seal_id, work_id, run_id, root_execution_id, seal_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                seal_object.key().as_str(),
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
        run.completion_seal = Some(seal_object.key().clone());
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
            // Root membership is a set in canonical member order. The seal
            // retains its separate child-proof order.
            root_execution
                .required_child_seals
                .sort_by(super::root_state::compare_seals);
        } else if item.child_requirement == ChildRequirement::Required {
            root_execution
                .required_child_seals
                .push(seal_object.key().clone());
            root_execution
                .required_child_seals
                .sort_by(super::root_state::compare_seals);
            root_execution.required_child_seals.dedup();
        }
        root_execution.revision += 1;
        root_execution.updated_at = request.completed_at;
        super::root_state::persist_completion(&transaction, &root_execution, &pre_seal_root)?;

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
                seal: seal_object.key().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.completed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "complete_work",
            &request.idempotency_key,
            request_object.key(),
            &seal,
        )?;
        transaction.commit()?;
        Ok(CompleteWorkStorageResult::Completed(Box::new(seal)))
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
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(event);
        }
        let record = load_work_obligation_by_id_on(&transaction, request.obligation_id)?;
        if record.definition_id != request.expected_definition {
            return Err(StoreError::InvalidWork(format!(
                "obligation {} definition changed: expected {}, current {}",
                request.obligation_id.0, request.expected_definition, record.definition_id
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
            definition: record.definition_id.clone(),
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
            request_object.key(),
            &event,
        )?;
        transaction.commit()?;
        Ok(event)
    }
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
            run_feed_position_for_object_on(connection, run_id, &record.definition_id)?;
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
            .resolution_id
            .as_ref()
            .map(|hash| run_feed_position_for_object_on(connection, run_id, hash))
            .transpose()?
            .filter(|position| position.position <= cut.position);
        let Some(resolution_position) = terminal_at_cut else {
            open.push(OpenWorkObligation {
                obligation_id: record.obligation.obligation_id,
                definition: record.definition_id,
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
            definition: record.definition_id,
            resolution: record.resolution_id.ok_or_else(|| {
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
) -> Result<Vec<ObjectId>, StoreError> {
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
        "SELECT DISTINCT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1
           AND position <= ?2 AND object_kind = 'environment_evidence'
         ORDER BY object_id LIMIT ?3",
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
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
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
        "SELECT obligation_id, definition_id, project_id, root_execution_id,
                root_id, work_id, run_id, work_revision, rule_set_id, rule_id, rule_version,
                triggering_observation_id, trigger_position, check_kind,
                check_fingerprint, state, resolution_id, resolution_kind,
                evidence_id, opened_at_ms, resolved_at_ms
         FROM work_run_obligations
         WHERE run_id = ?1 AND (?2 IS NULL OR state = ?2)
         ORDER BY trigger_position, obligation_id",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), state], |row| {
            Ok(ObligationProjectionRow {
                obligation_id: row.get(0)?,
                definition_id: row.get(1)?,
                project_id: row.get(2)?,
                root_execution_id: row.get(3)?,
                root_id: row.get(4)?,
                work_id: row.get(5)?,
                run_id: row.get(6)?,
                work_revision: row.get(7)?,
                rule_set_id: row.get(8)?,
                rule_id: row.get(9)?,
                rule_version: row.get(10)?,
                triggering_observation_id: row.get(11)?,
                trigger_position: row.get(12)?,
                check_kind: row.get(13)?,
                check_fingerprint: row.get(14)?,
                state: row.get(15)?,
                resolution_id: row.get(16)?,
                resolution_kind: row.get(17)?,
                evidence_id: row.get(18)?,
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
            "SELECT entry.position, entry.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
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
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let observation: ExecutionObservation = CanonicalObject::stored(&hash, bytes)?.decode()?;
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
    let definition_id = ObjectId::from_stored(row.definition_id.clone())
        .ok_or(StoreError::InvalidStoredKey(row.definition_id.clone()))?;
    let obligation =
        load_typed_work_object::<WorkObligation>(connection, &definition_id, "work_obligation")?;
    let state: WorkObligationState =
        serde_json::from_value(serde_json::Value::String(row.state.clone()))?;
    let check_kind: crate::domain::VerificationKind =
        serde_json::from_value(serde_json::Value::String(row.check_kind.clone()))?;
    let check_fingerprint = row
        .check_fingerprint
        .as_ref()
        .map(|value| {
            ObjectId::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredKey(value.clone()))
        })
        .transpose()?;
    let expected_rule_set = ObjectId::from_stored(row.rule_set_id.clone())
        .ok_or_else(|| StoreError::InvalidStoredKey(row.rule_set_id.clone()))?;
    let expected_trigger = ObjectId::from_stored(row.triggering_observation_id.clone()).ok_or(
        StoreError::InvalidStoredKey(row.triggering_observation_id.clone()),
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
    let trigger_entry_matches = |kind: &str| -> Result<bool, StoreError> {
        Ok(connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1
                   AND position = ?2 AND object_id = ?3
                   AND object_kind = ?4
             )",
            params![
                obligation.run_id.0.to_string(),
                obligation.trigger_position.position,
                obligation.triggering_observation.as_str(),
                kind
            ],
            |query| query.get::<_, bool>(0),
        )?)
    };
    let definition_position: Option<i64> = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND object_kind = 'work_obligation' AND object_id = ?2",
            params![obligation.run_id.0.to_string(), definition_id.as_str()],
            |query| query.get(0),
        )
        .optional()?;
    // A builtin rule is triggered by a source mutation the host observed; an
    // acceptance binding by the planning event (creation, claim or revision)
    // that authored the binding, which must still carry it.
    let trigger_matches = if let Some(criterion) = binding_rule_criterion(&obligation.rule) {
        let trigger = load_typed_work_object::<crate::domain::WorkEvent>(
            connection,
            &obligation.triggering_observation,
            "work_event",
        )?;
        trigger.project_id == obligation.project_id
            && trigger.root_id == obligation.root_id
            && trigger.work_id == obligation.work_id
            && trigger.run_id == Some(obligation.run_id)
            && trigger.revision == obligation.work_revision
            && trigger.created_at == obligation.opened_at
            && trigger.work.acceptance_bindings.iter().any(|binding| {
                binding.criterion == criterion && binding.requirement == obligation.requirement
            })
            && trigger_entry_matches("work_event")?
    } else {
        let trigger = load_typed_work_object::<ExecutionObservation>(
            connection,
            &obligation.triggering_observation,
            "execution_observation",
        )?;
        trigger.source_changed
            && trigger.project_id == obligation.project_id
            && trigger.binding.root_execution_id == obligation.root_execution_id
            && trigger.binding.work_id == obligation.work_id
            && trigger.binding.run_id == obligation.run_id
            && trigger.binding.work_revision == obligation.work_revision
            && trigger.recorded_at == obligation.opened_at
            && trigger_entry_matches("execution_observation")?
    };
    if !trigger_matches
        || definition_position
            .is_none_or(|position| position <= obligation.trigger_position.position)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} has an invalid trigger or feed binding",
            obligation.obligation_id.0
        )));
    }
    let resolution_id = row
        .resolution_id
        .as_ref()
        .map(|value| {
            ObjectId::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredKey(value.clone()))
        })
        .transpose()?;
    let resolution = resolution_id
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
        &definition_id,
        &obligation,
        state,
        resolution_id.as_ref(),
        resolution.as_ref(),
        row.resolution_kind.as_deref(),
        row.evidence_id.as_deref(),
        row.resolved_at_ms,
    )?;
    Ok(WorkObligationRecord {
        definition_id,
        obligation,
        state,
        resolution_id,
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
    definition_id: &ObjectId,
    obligation: &WorkObligation,
    state: WorkObligationState,
    resolution_id: Option<&ObjectId>,
    event: Option<&WorkObligationResolutionEvent>,
    projected_kind: Option<&str>,
    projected_evidence: Option<&str>,
    resolved_at_ms: Option<i64>,
) -> Result<Option<FeedPosition>, StoreError> {
    if state == WorkObligationState::Open {
        if resolution_id.is_some()
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
    let (resolution_id, event, resolved_at_ms) = resolution_id
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
        || event.definition != *definition_id
        || event.run_id != obligation.run_id
        || event.created_at.timestamp_millis() != resolved_at_ms
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation resolution {resolution_id} crosses its definition binding"
        )));
    }
    let resolution_position =
        run_feed_position_for_object_on(connection, obligation.run_id, resolution_id)?;
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
            let latest =
                latest_source_mutation_on(connection, obligation.run_id, evaluated_cut.position)?;
            let satisfied = crate::control::evaluate_obligation_satisfaction(
                &crate::control::ObligationSatisfactionInput {
                    open_obligations: std::slice::from_ref(obligation),
                    evidence: &verification,
                    producer: &producer,
                    latest_mutation: latest
                        .as_ref()
                        .map(|(position, mutation)| (mutation, *position)),
                    evidence_position: evidence_position.position,
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
    producer_observation: Option<&'a ObjectId>,
    check_fingerprint: Option<&'a ObjectId>,
    verification_result: Option<String>,
    observed_at: DateTime<Utc>,
    environment_fingerprint: Option<&'a ObjectId>,
    environment_evidence: Option<&'a ObjectId>,
    components_json: Option<Vec<u8>>,
}

pub(in crate::storage) fn append_control_verification_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
) -> Result<ObjectId, StoreError> {
    let object = CanonicalObject::mint(evidence)?;
    let result = encode_state(evidence.result)?;
    let evidence_id = append_control_typed_evidence_on(
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
    satisfy_open_obligations_on(transaction, evidence, &evidence_id)?;
    Ok(evidence_id)
}

pub(in crate::storage) fn append_control_environment_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &EnvironmentEvidence,
) -> Result<ObjectId, StoreError> {
    let object = CanonicalObject::mint(evidence)?;
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
) -> Result<ObjectId, StoreError> {
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
             evidence_id, work_id, run_id, evidence_kind,
             workspace_id, source_revision, producer_session_id,
             producer_observation_id, check_fingerprint,
             verification_result, observed_at_ms, environment_fingerprint,
             environment_evidence_id, components_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            object.key().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string(),
            encode_state(projection.kind)?,
            projection.workspace_id,
            projection.source_revision,
            projection.producer_session_id.0,
            projection.producer_observation.map(ObjectId::as_str),
            projection.check_fingerprint.map(ObjectId::as_str),
            projection.verification_result.as_deref(),
            projection.observed_at.timestamp_millis(),
            projection.environment_fingerprint.map(ObjectId::as_str),
            projection.environment_evidence.map(ObjectId::as_str),
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
        | add_root_contribution(&mut root_execution, session_id, object.key());
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
            evidence: object.key().clone(),
            evidence_kind: projection.kind,
        },
        actor: actor.clone(),
        created_at: recorded_at,
    };
    append_work_event(transaction, &event)?;
    Ok(object.key().clone())
}

pub(in crate::storage) fn append_control_execution_observation_on(
    transaction: &Transaction<'_>,
    observation: &ExecutionObservation,
) -> Result<ObjectId, StoreError> {
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
    let object = CanonicalObject::mint(observation)?;
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
    append_builtin_obligations_on(transaction, observation, object.key(), &trigger_position)?;
    Ok(object.key().clone())
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
    observation_id: &ObjectId,
    trigger_position: &FeedPosition,
) -> Result<Vec<ObjectId>, StoreError> {
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
            triggering_observation: observation_id.clone(),
            trigger_position: trigger_position.clone(),
            requirement,
            opened_at: observation.recorded_at,
        };
        definitions.push(persist_obligation_on(transaction, &obligation)?);
    }
    Ok(definitions)
}

/// Stores one obligation as a record, on the feeds and in the projection,
/// returning the definition's id.
fn persist_obligation_on(
    transaction: &Transaction<'_>,
    obligation: &WorkObligation,
) -> Result<ObjectId, StoreError> {
    let object = CanonicalObject::mint(obligation)?;
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
             obligation_id, definition_id, project_id, root_execution_id,
             root_id, work_id, run_id, work_revision, rule_set_id, rule_id, rule_version,
             triggering_observation_id, trigger_position, check_kind,
             check_fingerprint, state, opened_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            obligation.obligation_id.0.to_string(),
            object.key().as_str(),
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
                .map(ObjectId::as_str),
            encode_state(WorkObligationState::Open)?,
            obligation.opened_at.timestamp_millis(),
        ],
    )?;
    Ok(object.key().clone())
}

pub(super) use crate::control::{
    acceptance_binding_criterion as binding_rule_criterion, acceptance_binding_rule as binding_rule,
};

/// Opens one obligation on the item's run for each bound criterion the run
/// does not already hold an obligation for, triggered by the planning event
/// (creation, claim or revision) at `trigger_position`. An obligation waived
/// by an earlier revision does not count as held: the binding was re-authored
/// and opens again from this trigger. `reauthored` names the positions whose
/// criterion this revision rewrote under an unchanged binding: an obligation
/// an earlier revision opened there answers for the old sentence, so it does
/// not count as held either. Returns the definitions opened.
pub(super) fn open_binding_obligations_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    trigger: &ObjectId,
    trigger_position: &FeedPosition,
    reauthored: &[usize],
    now: DateTime<Utc>,
) -> Result<Vec<ObjectId>, StoreError> {
    if item.acceptance_bindings.is_empty() {
        return Ok(Vec::new());
    }
    let existing = load_work_obligation_records_on(transaction, run.run_id, None)?;
    let (_, policy, _) = SqliteStore::load_control_policy_head(transaction)?;
    let mut definitions = Vec::new();
    for binding in &item.acceptance_bindings {
        let rule = binding_rule(binding.criterion);
        let held = existing.iter().any(|record| {
            record.obligation.rule == rule
                && record.obligation.requirement == binding.requirement
                && record.state != WorkObligationState::Waived
                && !(reauthored.contains(&binding.criterion)
                    && record.obligation.work_revision < item.revision)
        });
        if held {
            continue;
        }
        let obligation = WorkObligation {
            schema_version: SCHEMA_VERSION,
            obligation_id: WorkObligationId::new(),
            project_id: item.project_id.clone(),
            root_execution_id: run.root_execution_id,
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.run_id,
            work_revision: item.revision,
            rule_set: policy.obligation_rule_set.clone(),
            rule,
            triggering_observation: trigger.clone(),
            trigger_position: trigger_position.clone(),
            requirement: binding.requirement.clone(),
            opened_at: now,
        };
        definitions.push(persist_obligation_on(transaction, &obligation)?);
    }
    Ok(definitions)
}

/// Holds each bound criterion to its obligation at the completion cut. The
/// obligation is resolved there, or completion refused before this; a
/// satisfied one is contradicted when the newest verification of its kind at
/// the cut did not pass, since a later failed check outranks an earlier pass,
/// and is stale when that verification does not verify the run's latest
/// observed source change under the rule that matches verification evidence
/// to a mutation (source revision, position and time), since it certifies
/// code that has since moved. A criterion the author asserted then cites the
/// verification that carried it, so the seal says what the pass rested on; a
/// criterion an evaluation judged keeps exactly the citations the evaluation
/// recorded, and the obligation binding names the satisfying record.
fn bind_acceptance_to_obligations_on(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    cut: &FeedPosition,
    completion_evidence: &[ObjectId],
    evaluated: bool,
    mut acceptance: Vec<AcceptanceResult>,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    if item.acceptance_bindings.is_empty() {
        return Ok(acceptance);
    }
    let records = load_work_obligation_records_on(connection, run_id, None)?;
    let latest_mutation = latest_source_mutation_on(connection, run_id, cut.position)?;
    for binding in &item.acceptance_bindings {
        let Some(index) = binding.criterion.checked_sub(1) else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work {} binds criterion 0; positions count from 1",
                item.work_id.0
            )));
        };
        let rule = binding_rule(binding.criterion);
        // The newest obligation speaks for the binding: one a revision opened
        // again supersedes the record an earlier sentence satisfied.
        let newest = records
            .iter()
            .filter(|record| {
                record.obligation.rule == rule
                    && record.obligation.requirement == binding.requirement
            })
            .max_by_key(|record| record.obligation.trigger_position.position);
        let Some(record) = newest.filter(|record| record.state == WorkObligationState::Satisfied)
        else {
            // Waived by an authority the obligation path admitted; the seal
            // binds that waiver where it binds every obligation.
            continue;
        };
        let Some(WorkObligationResolution::Satisfied {
            evidence: satisfying,
            ..
        }) = record.resolution.as_ref().map(|event| &event.resolution)
        else {
            continue;
        };
        let mut carried_by = satisfying.clone();
        if let Some((position, newest, evidence)) =
            newest_verification_of_kind_on(connection, run_id, &binding.requirement, cut)?
        {
            let kind = encode_state(binding.requirement.check_kind)?;
            if evidence.result != crate::domain::VerificationResult::Passed {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: format!(
                        "criterion {} requires {kind} verification and is contradicted by newer verification evidence {newest} that did not pass; record a passing check after it, or drop the binding",
                        binding.criterion
                    ),
                });
            }
            if let Some((mutation_position, mutation)) = latest_mutation.as_ref() {
                let producer = load_typed_work_object::<ExecutionObservation>(
                    connection,
                    &evidence.producer_observation,
                    "execution_observation",
                )?;
                if let Some(mismatch) = binding_freshness_mismatch(
                    (mutation, *mutation_position),
                    (&evidence, position),
                    &producer,
                    &binding.requirement,
                ) {
                    use crate::domain::VerificationEvidenceMismatch as Mismatch;
                    let cause = match mismatch {
                        Mismatch::StaleSourceRevision
                        | Mismatch::NotAfterMutation
                        | Mismatch::InvalidTime => "does not verify the run's latest source change",
                        _ => {
                            "is not admissible for it under the verification rule at the completion cut"
                        }
                    };
                    return Err(StoreError::WorkCompletionRefused {
                        work: item.work_id,
                        reason: format!(
                            "criterion {} requires {kind} verification, and the newest one ({newest}) {cause} ({}); record a passing check of the current source, or drop the binding",
                            binding.criterion,
                            encode_state(mismatch)?
                        ),
                    });
                }
                // The rule ran and accepted this record, so it is what the
                // criterion rests on now. Without a source change no rule
                // ran, and the record that satisfied the obligation stays.
                carried_by = newest;
            }
        }
        // An evaluation's citations are its own and are sealed as recorded.
        // An asserted criterion cites the verification that carried it when
        // the completion's evidence set holds it, so the citation closure the
        // checkpoint acknowledged still holds.
        if !evaluated
            && completion_evidence.contains(&carried_by)
            && let Some(result) = acceptance.get_mut(index)
            && !result.evidence.contains(&carried_by)
        {
            result.evidence.push(carried_by);
        }
    }
    Ok(acceptance)
}

/// Why the newest verification of a bound kind does not carry its criterion
/// past the run's latest source change, or `None` when it does. Recording
/// order alone proves nothing about what was checked: a record appended after
/// the change may still verify the older source, so the rule that matches
/// verification evidence to a mutation decides. A change the host recorded
/// with no source revision or time gives that rule nothing to compare, and
/// would refuse every verification forever; recording order is then all that
/// can be said, and it is what is asked.
fn binding_freshness_mismatch(
    (mutation, mutation_position): (&ExecutionObservation, i64),
    (evidence, evidence_position): (&VerificationEvidence, i64),
    producer: &ExecutionObservation,
    requirement: &crate::domain::VerificationRequirement,
) -> Option<crate::domain::VerificationEvidenceMismatch> {
    if mutation.source_basis.is_none() || mutation.observed_at.is_none() {
        return (evidence_position <= mutation_position)
            .then_some(crate::domain::VerificationEvidenceMismatch::NotAfterMutation);
    }
    crate::control::match_verification_evidence(&crate::control::VerificationEvidenceMatchInput {
        candidate_kind: crate::domain::WorkEvidenceKind::Verification,
        evidence: Some(evidence),
        producer: Some(producer),
        latest_mutation: Some((mutation, mutation_position)),
        evidence_position,
        requirement,
    })
    .err()
}

/// The newest host-minted verification `requirement` recognizes (its kind,
/// its pinned check and its required environment, when it names them) on the
/// run at or before `cut`, with its run-feed position.
fn newest_verification_of_kind_on(
    connection: &Connection,
    run_id: WorkRunId,
    requirement: &crate::domain::VerificationRequirement,
    cut: &FeedPosition,
) -> Result<Option<(i64, ObjectId, VerificationEvidence)>, StoreError> {
    let stored: Vec<String> = connection
        .prepare(
            "SELECT evidence_id FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_kind = 'verification'",
        )?
        .query_map([run_id.0.to_string()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut newest: Option<(i64, ObjectId, VerificationEvidence)> = None;
    for stored_hash in stored {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let position = run_feed_position_for_object_on(connection, run_id, &hash)?;
        if position.position > cut.position {
            continue;
        }
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, &hash, "verification_evidence")?;
        if evidence.check_kind != requirement.check_kind
            || requirement
                .check_fingerprint
                .as_ref()
                .is_some_and(|required| required != &evidence.check_fingerprint)
            || requirement
                .required_environment
                .as_ref()
                .is_some_and(|required| evidence.environment.as_ref() != Some(required))
        {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|(known, _, _)| position.position > *known)
        {
            newest = Some((position.position, hash, evidence));
        }
    }
    Ok(newest)
}

/// Resolves as waived, in the revising actor's name, every open obligation on
/// `run_id` that an acceptance binding opened and the revised item no longer
/// binds, or binds at a position in `reauthored`, whose criterion this
/// revision rewrote. Revision is how a requirement changes; the waiver is the
/// audited record of that change on the obligation it retires.
pub(super) fn waive_unbound_obligations_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run_id: WorkRunId,
    reauthored: &[usize],
    actor: &crate::domain::ActorContext,
    now: DateTime<Utc>,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut resolutions = Vec::new();
    for record in
        load_work_obligation_records_on(transaction, run_id, Some(WorkObligationState::Open))?
    {
        let Some(criterion) = binding_rule_criterion(&record.obligation.rule) else {
            continue;
        };
        let rewritten =
            reauthored.contains(&criterion) && record.obligation.work_revision < item.revision;
        let still_bound = !rewritten
            && item.acceptance_bindings.iter().any(|binding| {
                binding.criterion == criterion
                    && binding.requirement == record.obligation.requirement
            });
        if still_bound {
            continue;
        }
        let change = if rewritten {
            "was rewritten, and its verification is owed again"
        } else {
            "no longer requires this verification"
        };
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_id.clone(),
            run_id,
            resolution: WorkObligationResolution::Waived {
                waived_by: actor.actor_id.clone(),
                reason: format!(
                    "acceptance revised at revision {}: criterion {criterion} {change}",
                    item.revision
                ),
            },
            actor: actor.clone(),
            created_at: now,
        };
        resolutions.push(append_obligation_resolution_on(
            transaction,
            &record,
            &event,
        )?);
    }
    Ok(resolutions)
}

/// Savepoint that brackets completion's untested-change waivers, so a
/// recovery answer can read the state before them.
const UNTESTED_WAIVERS_SAVEPOINT: &str = "completion_untested_waivers";

/// The recovery answer for `cause`, read after rolling back to
/// [`UNTESTED_WAIVERS_SAVEPOINT`]: the returned page shows what the store
/// holds once the refused completion's transaction is dropped, never a waiver
/// that rollback discards.
fn recovery_before_untested_waivers(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run_id: WorkRunId,
    cause: WorkCompletionRecoveryCause,
) -> Result<CompleteWorkStorageResult, StoreError> {
    transaction.execute_batch(&format!("ROLLBACK TO {UNTESTED_WAIVERS_SAVEPOINT}"))?;
    let recovery = completion_recovery_snapshot_on(transaction, item, run_id, cause)?;
    Ok(CompleteWorkStorageResult::Recovery(recovery))
}

/// Resolves as waived, in the completing actor's name, every obligation the
/// stock source-change rule opened on `run_id` that is still open: no
/// matching passing test followed that change. The waiver reason names the
/// change and its source revision for the host record.
fn waive_untested_source_changes_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run_id: WorkRunId,
    actor: &crate::domain::ActorContext,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    for record in
        load_work_obligation_records_on(transaction, run_id, Some(WorkObligationState::Open))?
    {
        if !crate::control::is_stock_source_change_obligation(
            &record.obligation.rule,
            &record.obligation.requirement,
        ) {
            continue;
        }
        let change = load_typed_work_object::<ExecutionObservation>(
            transaction,
            &record.obligation.triggering_observation,
            "execution_observation",
        )?;
        let revision = change.source_basis.as_ref().map_or_else(
            || "no recorded source revision".to_owned(),
            |basis| format!("source revision {}", basis.source_revision),
        );
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_id.clone(),
            run_id,
            resolution: WorkObligationResolution::Waived {
                waived_by: actor.actor_id.clone(),
                reason: format!(
                    "completed at revision {} with no matching passing test after source change {} ({revision})",
                    item.revision, change.observation_id
                ),
            },
            actor: actor.clone(),
            created_at: now,
        };
        append_obligation_resolution_on(transaction, &record, &event)?;
    }
    Ok(())
}

fn satisfy_open_obligations_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
    evidence_id: &ObjectId,
) -> Result<Vec<ObjectId>, StoreError> {
    let evidence_position =
        run_feed_position_for_object_on(transaction, evidence.binding.run_id, evidence_id)?;
    let evaluated_cut = current_run_feed_cut_on(transaction, evidence.binding.run_id)?;
    let latest =
        latest_source_mutation_on(transaction, evidence.binding.run_id, evaluated_cut.position)?;
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
            latest_mutation: latest
                .as_ref()
                .map(|(position, mutation)| (mutation, *position)),
            evidence_position: evidence_position.position,
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
            definition: record.definition_id.clone(),
            run_id: evidence.binding.run_id,
            resolution: WorkObligationResolution::Satisfied {
                evidence: evidence_id.clone(),
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
) -> Result<ObjectId, StoreError> {
    let (state, kind, evidence_id) = match &event.resolution {
        WorkObligationResolution::Satisfied { evidence, .. } => (
            WorkObligationState::Satisfied,
            "satisfied",
            Some(evidence.as_str()),
        ),
        WorkObligationResolution::Waived { .. } => (WorkObligationState::Waived, "waived", None),
    };
    let object = CanonicalObject::mint(event)?;
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
             state = ?3, resolution_id = ?4, resolution_kind = ?5,
             evidence_id = ?6, resolved_at_ms = ?7
         WHERE obligation_id = ?1 AND definition_id = ?2
           AND state = 'open' AND resolution_id IS NULL",
        params![
            event.obligation_id.0.to_string(),
            record.definition_id.as_str(),
            encode_state(state)?,
            object.key().as_str(),
            kind,
            evidence_id,
            event.created_at.timestamp_millis(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} lost its open-state compare-and-swap",
            event.obligation_id.0
        )));
    }
    Ok(object.key().clone())
}

/// Empty criterion evidence is an asserted result without a linked artifact.
/// Any explicit citation must still belong to the completion evidence set.
fn validate_acceptance(
    item: &WorkItem,
    completion_evidence: &[ObjectId],
    results: &[AcceptanceResult],
    actor_assurance: crate::domain::AssuranceLevel,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    let shaped = normalize_completion_acceptance_shape(item, results, actor_assurance)?;
    let mut normalized = Vec::with_capacity(shaped.len());
    for mut result in shaped {
        result.evidence = unique_hashes(&result.evidence);
        normalized.push(result);
    }
    ensure_acceptance_citations_within(item, completion_evidence, &normalized)?;
    Ok(normalized)
}

/// The closure invariant shared by the self-asserted and evaluated routes:
/// every citation a sealed criterion carries belongs to the completion
/// evidence set, so the seal names it and the final checkpoint acknowledged
/// it.
fn ensure_acceptance_citations_within(
    item: &WorkItem,
    completion_evidence: &[ObjectId],
    results: &[AcceptanceResult],
) -> Result<(), StoreError> {
    let completion_evidence = completion_evidence
        .iter()
        .map(ObjectId::as_str)
        .collect::<HashSet<_>>();
    for result in results {
        if result
            .evidence
            .iter()
            .any(|hash| !completion_evidence.contains(hash.as_str()))
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "acceptance criterion {:?} cites evidence outside the completion evidence set",
                    result.criterion
                ),
            });
        }
    }
    Ok(())
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
