use chrono::{Duration, TimeZone};

mod audit_validation;
mod catalog_restore;
mod hierarchy_limits;
mod input_validation;
mod memory_revisions;
mod persisted_text;
mod planning_scale;
mod redaction;
mod relation_bases;
mod restored_identity;
mod restored_repair;

use super::load::GRAPH_VALIDATION_WORK;
use super::*;
use crate::storage::test_support::{
    ascii65_session, assert_oversized_session_refusal, exact_64_ascii_session,
    utf8_oversized_session,
};
use crate::{
    AddWorkBlockerRequest, Authority, ChangeWorkPrerequisiteRequest, ChildRequirement,
    ChildWorkDraft, ClaimWorkRequest, CreateWorkRequest, DecomposeWorkRequest, Delivery,
    DevelopmentNoopRedactor, DisposeWorkRequest, MemoryId, MemoryKind, MemoryStatus, MemoryVersion,
    NoteRequest, NoteVisibility, RecordWorkEvidenceRequest, RememberProjectMemoryRequest, Scope,
    Sensitivity, WorkBlockerKind, WorkDisposition, WorkGraphSnapshotMemoryState,
    WorkGraphSnapshotText, WorkItemKind, WorkOrigin, WorkPlanningAuthority, WorkSourceSnapshot,
    domain::{AssuranceLevel, ProvenanceLink, SourceSnapshot},
    storage::test_support::SentinelRedactor,
};

mod catalog_admission;
mod fixtures;
mod restored_round_trip;
mod save_integrity;
mod save_policy;
mod session_admission;
mod snapshot_content;

use fixtures::{
    actor, at, classified_project_memory, create_imported_root, create_root,
    insert_classified_project_memory, insert_project_memory_version, rebind_snapshot_body,
    refuse_load_before_effects, refuse_save_before_effects, root_create_request, snapshot_bytes,
};
