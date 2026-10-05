//! Transport-neutral structured store errors shared by CLI, MCP and agent projection.

pub(crate) mod remedies;

use crate::{
    argument_names::ArgumentNames,
    storage::{PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, StoreError},
    work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL,
};
use chrono::Utc;
use remedies::{
    CATALOG_CURSOR_REMEDY, CRITERION_LINK_REMEDY, PEER_DECOMPOSITION_REMEDY, SHOW_CURSOR_REMEDY,
    project_memory_remedy,
};
use serde_json::{Value, json};

/// Stable structured rendering shared by MCP and native JSON/core errors.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one shared structured renderer keeps every CLI and MCP error surface identical"
)]
pub fn store_error_value(error: &StoreError) -> Value {
    let details = match error {
        StoreError::NoteIdempotencyConflict(key) => json!({ "idempotency_key": key }),
        StoreError::TaskAccessDenied { task, session } => json!({
            "task_id": task.0,
            "session_id": session,
        }),
        StoreError::MemoryAccessDenied(hash) | StoreError::MemoryNotFound(hash) => {
            json!({ "object_id": hash })
        }
        StoreError::ProjectMemoryExists(key) => json!({
            "key": key,
            "remedy": project_memory_remedy(error, ArgumentNames::Cli),
        }),
        StoreError::ProjectMemoryRevisionConflict {
            key,
            expected,
            current,
        } => json!({
            "key": key, "expected_revision": expected, "current_revision": current,
            "remedy": project_memory_remedy(error, ArgumentNames::Cli),
        }),
        StoreError::ProjectMemoryRevisionNotFound {
            key,
            revision,
            current,
        } => json!({
            "key": key, "revision": revision, "current_revision": current,
            "remedy": project_memory_remedy(error, ArgumentNames::Cli),
        }),
        StoreError::ProjectMemorySectionNotFound(missing) => json!({
            "key": missing.key, "revision": missing.revision, "section": missing.section,
            "sections": missing.sections,
            "remedy": format!(
                "name one of the existing sections, or add `{}` with append and its markers",
                missing.section
            ),
        }),
        StoreError::ProjectMemoryRetired(key) => json!({
            "key": key,
            "remedy": "run memories and choose a key that has never been used",
        }),
        StoreError::ProjectMemoryNotFound(key) => json!({
            "key": key,
            "remedy": "run memories to list retained project memories",
        }),
        StoreError::ProjectMemoryBindingInvalid => json!({
            "remedy": "use a non-empty asserted actor/session binding for this project",
        }),
        StoreError::InvalidProjectMemory(reason) if reason.contains("context_generation") => {
            json!({
                "reason": reason,
                "remedy": "omit context_generation or use 1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash",
            })
        }
        StoreError::InvalidProjectMemory(reason) => json!({
            "reason": reason,
            "remedy": "follow the key and size bounds, or run memories for valid keys",
        }),
        StoreError::WorkNotFound(work) => missing_work_details(*work),
        StoreError::WorkReferenceAmbiguous {
            reference,
            candidates,
            more,
        } => ambiguous_work_reference_details(reference, candidates, *more),
        StoreError::WorkImplicitTargetConflict(conflict) => json!({
            "operation": conflict.operation,
            "focused_ref": conflict.focus,
            "focus_state": conflict.focus_state.as_str(),
            "held_refs": conflict.held,
            "more": conflict.more,
            "remedy": "repeat the word with the intended item named; nothing was recorded",
        }),
        StoreError::WorkBareTargetAmbiguous(ambiguity) => json!({
            "operation": ambiguity.operation,
            "focused_ref": ambiguity.focus,
            "held_refs": ambiguity.held,
            "remedy": "repeat the word with the intended item named; nothing was recorded",
        }),
        StoreError::InvalidWork(message)
            if message == PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL =>
        {
            json!({
                "reason": message,
                "remedy": PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL,
            })
        }
        StoreError::InvalidWork(message) if message == COMPLETED_WORK_LATE_FINDING_REFUSAL => {
            json!({
                "reason": message,
                "remedy": "use note to record a late finding without reopening the completed item",
            })
        }
        StoreError::InvalidWork(message) if message == crate::verbs::GATE_WORK_REF_REQUIRED => {
            json!({
                "reason": message,
                "remedy": crate::verbs::GATE_WORK_REF_REQUIRED,
            })
        }
        StoreError::InvalidWork(message) if message == crate::storage::PENDING_HANDOFF_REFUSAL => {
            json!({
                "reason": message,
                "remedy": "cancel the handoff offer, or let it be accepted or expire before retrying",
            })
        }
        StoreError::WorkCatalogCursorInvalid { reason } => json!({
            "reason": reason,
            "remedy": CATALOG_CURSOR_REMEDY.cli,
        }),
        StoreError::WorkShowCursorInvalid { reason } => json!({
            "reason": reason,
            "remedy": SHOW_CURSOR_REMEDY.cli,
        }),
        StoreError::WorkNoteReferenceInvalid {
            reason,
            candidates,
            more,
        } => json!({
            "reason": reason, "candidates": candidates, "more": more,
            "remedy": "use the complete locator printed beside the note",
        }),
        StoreError::WorkCriterionLinkInvalid { criterion, reason } => {
            let mut details = json!({
                "reason": reason,
                "remedy": CRITERION_LINK_REMEDY.cli,
            });
            if let (Some(position), Value::Object(fields)) = (criterion, &mut details) {
                fields.insert("criterion".into(), json!(position));
            }
            details
        }
        StoreError::WorkNoteTooLarge { bytes, limit } => json!({
            "bytes": bytes, "limit": limit,
            "reason": "note body exceeds the UTF-8 byte limit",
            "remedy": "carry bulk content as a reference",
        }),
        StoreError::WorkAncestorNotOpen { work, ancestor } => json!({
            "work_id": work,
            "blocking_ancestor": {"ref": ancestor.short_ref, "lifecycle": ancestor.lifecycle},
            "reason": error.to_string(),
            "remedy": "inspect the affected item and ancestor with show; follow the affected item's admitted next commands",
        }),
        StoreError::InvalidWork(message) | StoreError::InvalidWorkProjection(message) => json!({
            "reason": message,
            "remedy": "run next, then show the affected item and follow next",
        }),
        StoreError::WorkRevisionConflict {
            work,
            expected,
            current,
        } => json!({
            "work_id": work,
            "expected_revision": expected,
            "current_revision": current,
            "remedy": "run show for the affected item before retrying with a new idempotency_key",
        }),
        StoreError::WorkOperationIdempotencyConflict { operation, key } => json!({
            "operation": operation,
            "idempotency_key": key,
            "remedy": "retry the original payload exactly or use a new key for a different intent",
        }),
        StoreError::WorkDecompositionRetryConflict { parent_ref, reason } => json!({
            "parent_ref": parent_ref,
            "reason": reason,
            "remedy": crate::storage::DECOMPOSITION_RETRY_REMEDY,
        }),
        StoreError::WorkDependencyCycle => json!({
            "remedy": "remove or change the prerequisite edge that introduces the cycle",
        }),
        StoreError::WorkNotOpen(work) => json!({
            "work_id": work,
            "remedy": "run show for the affected item and follow next",
        }),
        StoreError::WorkParentNotOpen { lifecycle, .. } => json!({
            "parent_lifecycle": lifecycle,
            "remedy": crate::storage::parent_not_open_remedy(*lifecycle),
        }),
        StoreError::WorkDetachRefused {
            work_id,
            reason,
            remedy,
        } => json!({
            "work_id": work_id, "reason": reason, "remedy": remedy,
        }),
        StoreError::WorkRejectRefused {
            child_ref,
            parent_ref,
            reason,
            remedy,
            ..
        } => json!({
            "child_ref": child_ref, "parent_ref": parent_ref, "reason": reason, "remedy": remedy,
        }),
        StoreError::WorkPeerDecompositionRefused { parent } => json!({
            "work_id": parent,
            "remedy": PEER_DECOMPOSITION_REMEDY.cli,
        }),
        StoreError::WorkPrerequisiteAlreadySatisfied(work) => json!({
            "work_id": work,
            "remedy": "no edge is needed; run show for the prerequisite before choosing another action",
        }),
        StoreError::WorkClaimHeld {
            work,
            holder,
            expires_at,
        } => json!({
            "work_id": work,
            "holder_session_id": holder,
            "expires_at_ms": expires_at,
            "expires_at": chrono::DateTime::<Utc>::from_timestamp_millis(*expires_at)
                .map(|value| value.to_rfc3339()),
            "remedy": "wait for expiry or coordinate an explicit checkpointed handoff",
        }),
        StoreError::WorkClaimMismatch { work } => json!({
            "work_id": work,
            "remedy": "run show; claim the item again or accept its handoff before mutating",
        }),
        StoreError::WorkClaimLapsed { work, expired_at } => json!({
            "work_id": work,
            "expired_at_ms": expired_at.timestamp_millis(),
            "expired_at": expired_at.to_rfc3339(),
            "remedy": "run claim REF before mutating",
        }),
        StoreError::WorkReleaseWaiverRequired { work } => json!({
            "work_id": work,
            "remedy": "repeat the release with a nonblank reason; it is recorded as the attributed waiver of this session's missing contribution",
        }),
        StoreError::WorkCompletionRefused { work, reason } => json!({
            "work_id": work,
            "reason": reason,
            "remedy": "record evidence, checkpoint the current feed cut, and satisfy every current acceptance criterion",
        }),
        StoreError::AcceptanceEvaluationAdmissionRefused {
            work,
            reason,
            cause,
        } => json!({
            "work_id": work,
            "reason": reason,
            "cause": cause,
            "remedy": crate::work_service::evaluation_admission_remedy(cause),
        }),
        StoreError::WorkBoundVerificationRefused {
            work,
            reason,
            cause,
        } => json!({
            "work_id": work,
            "reason": reason,
            "cause": cause,
            "remedy": crate::work_service::bound_verification_remedy(cause),
        }),
        StoreError::WorkCompletionRecoveryRequired {
            work,
            cause,
            context,
        } => {
            let mut details = json!({
                "work_id": work,
                "cause": cause,
            });
            if let Some(observation) = &context.deciding_observation {
                details["deciding_observation"] = json!(observation);
            }
            if let Some(source) = &context.source {
                details["source"] = json!(source);
            }
            details
        }
        StoreError::AcceptanceCriteriaRequired { work } => json!({
            "work_id": work,
            "reason": "the item has no acceptance criteria; an acceptance evaluation needs at least one, and the host refuses to evaluate an item without criteria",
            "remedy": "add at least one criterion with `engram work update REF --accept \"criterion\"`, then have the host evaluate it, then run `engram work done REF` again",
        }),
        StoreError::AcceptanceEvaluationCarriedFailure {
            work,
            refusal,
            failed,
            ..
        } => json!({
            "work_id": work,
            "reason": refusal.word(),
            "failed_evaluation": failed,
            "remedy": refusal.remedy(),
        }),
        StoreError::AcceptanceEvaluationBasisMoved {
            work,
            moved,
            reason,
            observation,
        } => {
            let mut details = json!({
                "work_id": work,
                "reason": reason,
                "remedy": moved.remedy(),
            });
            // Added beside the unchanged fields, only when an observation
            // decided the move.
            if let Some(observation) = observation {
                details["deciding_observation"] = json!(observation);
            }
            details
        }
        _ => Value::Null,
    };
    json!({
        "error": {
            "code": error_code(error),
            "message": error.to_string(),
            "details": details,
        }
    })
}

fn missing_work_details(work: crate::WorkId) -> Value {
    json!({
        "work_id": work,
        "remedy": "run search or ls, then show a returned short_ref",
    })
}

fn ambiguous_work_reference_details(
    reference: &str,
    candidates: &[crate::WorkReferenceCandidate],
    more: usize,
) -> Value {
    json!({
        "reference": reference,
        "candidates": candidates,
        "more": more,
        "remedy": "repeat the operation with one candidate's full work_id",
    })
}

fn error_code(error: &StoreError) -> &'static str {
    match error {
        StoreError::StoreNotInitialized => "store_not_initialized",
        StoreError::NoteIdempotencyConflict(_) => "note_idempotency_conflict",
        StoreError::NoActiveTask(_) => "no_active_task",
        StoreError::TaskAccessDenied { .. } => "task_access_denied",
        StoreError::MemoryAccessDenied(_) => "memory_access_denied",
        StoreError::MemoryNotFound(_) | StoreError::ProjectMemoryNotFound(_) => "memory_not_found",
        StoreError::ProjectMemoryExists(_) => "memory_exists",
        StoreError::ProjectMemoryRevisionConflict { .. } => "memory_revision_conflict",
        StoreError::ProjectMemoryRevisionNotFound { .. } => "memory_revision_not_found",
        StoreError::ProjectMemorySectionNotFound(_) => "memory_section_not_found",
        StoreError::ProjectMemoryRetired(_) => "memory_retired",
        StoreError::ProjectMemoryBindingInvalid => "memory_binding_invalid",
        StoreError::InvalidProjectMemory(_) => "memory_invalid",
        StoreError::EmptyNote => "empty_note",
        StoreError::RedactionRefused(_) => "redaction_refused",
        StoreError::WorkNotFound(_) => "work_not_found",
        StoreError::WorkReferenceAmbiguous { .. } => "work_reference_ambiguous",
        StoreError::WorkImplicitTargetConflict(_) => "work_implicit_target_conflict",
        StoreError::WorkBareTargetAmbiguous(_) => "work_bare_target_ambiguous",
        StoreError::InvalidWork(_) | StoreError::WorkAncestorNotOpen { .. } => "work_invalid",
        StoreError::InvalidWorkProjection(_) => "work_projection_invalid",
        StoreError::WorkRevisionConflict { .. } => "work_revision_conflict",
        StoreError::WorkOperationIdempotencyConflict { .. } => "work_idempotency_conflict",
        StoreError::WorkDecompositionRetryConflict { .. } => "work_decomposition_retry_conflict",
        StoreError::WorkDependencyCycle => "work_dependency_cycle",
        StoreError::WorkPrerequisiteAlreadySatisfied(_) => "work_prerequisite_already_satisfied",
        StoreError::WorkNotOpen(_) => "work_not_open",
        StoreError::WorkParentNotOpen { .. } => "work_parent_not_open",
        StoreError::WorkDetachRefused { .. } => "work_detach_refused",
        StoreError::WorkRejectRefused { .. } => "work_reject_refused",
        StoreError::WorkCatalogCursorInvalid { .. } => "work_catalog_cursor_invalid",
        StoreError::WorkShowCursorInvalid { .. } => "work_show_cursor_invalid",
        StoreError::WorkNoteReferenceInvalid { .. } => "work_note_reference_invalid",
        StoreError::WorkCriterionLinkInvalid { .. } => "work_criterion_link_invalid",
        StoreError::WorkNoteTooLarge { .. } => "work_note_too_large",
        StoreError::WorkPeerDecompositionRefused { .. } => "work_peer_decomposition_refused",
        StoreError::WorkClaimHeld { .. } => "work_claim_held",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        StoreError::WorkClaimLapsed { .. } => "work_claim_lapsed",
        StoreError::WorkCompletionRefused { .. }
        | StoreError::WorkBoundVerificationRefused { .. } => "work_completion_refused",
        StoreError::WorkReleaseWaiverRequired { .. } => "work_release_waiver_required",
        StoreError::WorkCompletionRecoveryRequired { .. } => "work_completion_recovery_required",
        StoreError::AcceptanceCriteriaRequired { .. } => "acceptance_criteria_required",
        StoreError::AcceptanceEvaluationRefused { .. }
        | StoreError::AcceptanceEvaluationAdmissionRefused { .. }
        | StoreError::AcceptanceEvaluationCarriedFailure { .. } => "acceptance_evaluation_refused",
        StoreError::AcceptanceEvaluationBasisMoved { moved, .. } => {
            evaluation_basis_move_code(*moved)
        }
        StoreError::GraphDestinationNotEmpty => "graph_destination_not_empty",
        StoreError::GraphProjectMismatch { .. } => "graph_project_mismatch",
        StoreError::GraphDifferentBuild => "different_build",
        StoreError::InvalidGraphSnapshot(_) => "graph_snapshot_corrupt",
        StoreError::Json(_)
        | StoreError::Sqlite(_)
        | StoreError::ImmutableCollision(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::InvalidStoredKey(_)
        | StoreError::InvalidMemoryProjection(_)
        | StoreError::InvalidTaskProjection(_)
        | StoreError::InvalidControlSession(_)
        | StoreError::NamedRootBindingRefused(_)
        | StoreError::SourceBasisTextRefused { .. }
        | StoreError::NamedRootReadRefused(_)
        | StoreError::ExecutionObservationInvalid(_)
        | StoreError::VerificationBindRefused(_)
        | StoreError::ExecutionObservationBasisMismatch(_)
        | StoreError::ExecutionObservationPolicyBasisMismatch(_)
        | StoreError::AcceptanceBindingReadRefused { .. }
        | StoreError::AcceptanceVerificationReadRefused { .. }
        | StoreError::NamedRootSightingReadRefused { .. }
        | StoreError::HostPathIdentityUnresolved
        | StoreError::ControlSessionNotBound(_)
        | StoreError::ControlSessionTokenMismatch(_)
        | StoreError::ControlConnectionSuperseded(_)
        | StoreError::ControlSessionBindConflict(_)
        | StoreError::ControlTurnIdempotencyConflict(_)
        | StoreError::ControlOperationIdempotencyConflict { .. }
        | StoreError::ControlWorkBindingStale { .. }
        | StoreError::ControlGrantScopeMismatch { .. }
        | StoreError::ControlObservationScopeMismatch { .. }
        | StoreError::VerificationProducerObservationNotFound(_)
        | StoreError::EnvironmentFingerprintMismatch
        | StoreError::EnvironmentEvidenceNotFound(_)
        | StoreError::EnvironmentBasisMismatch(_)
        | StoreError::ControlTurnGrantNotFound(_)
        | StoreError::DifferentBuildSchema
        | StoreError::InvalidControlProjection(_)
        | StoreError::ControlPolicyConflict { .. }
        | StoreError::OpenWorkObligations { .. } => "engram_store_error",
    }
}

/// Stable code for each way a run can move past an evaluation's basis: a
/// check asks for a resubmission, an unseen source change voids it.
pub(crate) const fn evaluation_basis_move_code(moved: crate::EvaluationBasisMove) -> &'static str {
    match moved {
        crate::EvaluationBasisMove::CheckRecorded => "acceptance_evaluation_resubmit",
        crate::EvaluationBasisMove::SourceChanged => "acceptance_evaluation_void",
    }
}
