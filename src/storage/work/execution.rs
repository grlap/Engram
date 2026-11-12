use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use super::super::{
    BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, SqliteStore, StoreError,
};
use super::completion::feed_head;
use super::feeds::{
    append_to_work_feeds, append_work_event, checkpoint_feed_end, expire_handoff_offers,
    inspect_work_request, load_handoff_offer_projection, load_typed_work_object, replay_operation,
    request_object, run_feed_position_for_object_on,
};
use super::integrity::{expected_environment_projection, expected_verification_projection};
use super::planning::{
    add_root_contribution, assert_actor_session, assert_revision, claim_expiry,
    expect_root_contributor, normalize_strings, normalize_text, persist_claim,
    persist_operation_result, persist_root_execution, persist_work_item, persist_work_run,
    renew_holder_claim, root_participant_is_accounted, unique_hashes,
    validate_live_claim_for_item_on, validate_live_claim_on, waive_root_contributor,
};
use super::query::{
    active_root_execution_optional, held_open_child_on, inspect_work_canonical_on,
    latest_restored_record, load_root_execution, load_work_claim_optional, load_work_item,
    load_work_run, open_children_by_ready_order_on, work_completed_by_restored_record_on,
};
use super::session::begin_work_protocol_attempt_on;
use super::{
    EvidenceProjectionRow, GateWorkProtocolAttempt, GateWorkProtocolIntent,
    StoredWorkEvidenceSelectionRow, WorkEventDraft, WorkEvidenceProjectionSummary, WorkNoteCapture,
};
use crate::{
    CanonicalObject, ObjectId, RestoredWorkEvidence, WorkId,
    domain::{
        AcceptWorkHandoffRequest, ActorContext, AppendRestoredWorkGateRequest,
        CancelWorkHandoffRequest, ClaimNextReadyChildRequest, ClaimWorkRequest, CompletionSeal,
        EnvironmentEvidence, FeedId, FeedPosition, GATE_EVIDENCE_SUMMARY, GateEvidenceRecord,
        NextReadyChildClaim, OfferWorkHandoffRequest,
        POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
        RecordGateEvidenceRequest, RecordRestoredWorkEvidenceRequest, RecordWorkEvidenceRequest,
        RecordWorkNoteRequest, ReleaseWorkRequest, RestoredWorkEvidenceInput, RootExecution,
        RootExecutionState, SCHEMA_VERSION, VerificationEvidence, WorkAvailability, WorkCheckpoint,
        WorkClaim, WorkClaimId, WorkClaimState, WorkEvent, WorkEvidence, WorkEvidenceKind,
        WorkHandoffOffer, WorkHandoffOfferId, WorkHandoffState, WorkItem, WorkLifecycle,
        WorkRelease, WorkRun, WorkRunId, WorkRunState, WorkTransition,
        normalize_gate_evidence_input, validate_gate_evidence_payload,
    },
    memory::Redactor,
};

mod gate_evidence;
use gate_evidence::validate_gate_evidence;
pub(super) use gate_evidence::validate_gate_evidence_chain;
#[cfg(test)]
use gate_evidence::{LATEST_GATE_EVIDENCE_SQL, latest_gate_evidence_on};

mod handoffs;
pub(super) use handoffs::{latest_canonical_handoff_offer, live_handoff_on};

#[cfg(test)]
mod tests;

mod checkpoints;
mod claims;
mod evidence;
mod restored;

use checkpoints::persist_work_checkpoint_on;
pub(super) use evidence::{
    ensure_run_evidence, validate_evidence_phase_marker, validate_work_evidence_event_phase_on,
    work_evidence_kind_on, work_run_evidence_on, work_run_evidence_projection_on,
};
use evidence::{
    persist_post_completion_work_evidence_on, persist_work_evidence_on,
    validate_post_completion_evidence_basis_on,
};
pub(super) use restored::{
    ensure_restored_execution_state, ensure_restored_execution_state_for_event,
};
