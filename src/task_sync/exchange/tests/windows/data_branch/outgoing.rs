//! Public strict export preparation, actual retained inspection and `GitIo`.

use super::*;
use crate::{
    ActorContext, CanonicalObject, CreateWorkRequest, FeedId, FeedPosition, ObjectId, ProjectId,
    SessionId,
    domain::{AssuranceLevel, ChildRequirement, WorkItemKind, WorkOrigin},
    storage::SqliteStore,
    task_sync::{
        self, CapturedTask,
        delta::*,
        exchange,
        inventory::InventoryRoot,
        ledger::{Binding, Direction, RepositoryBasis, StoreBasis, TargetBasis, Transition},
        progress::{ExportCut, ExportProfile},
    },
};
use fixture::{Repositories, git};
use std::collections::{BTreeMap, BTreeSet};

mod drift;
mod handoff;
mod observation;
mod prepare;
mod recovery;
pub(super) use drift::refuses_captured_head_drift;
pub(super) use handoff::{digest as handoff_digest, publish_into_refusal, recover, require};
pub(super) use observation::{observes_the_retry_remote_tip, observes_the_selected_remote_tip};
pub(super) use prepare::incoming_carrier;

pub(super) fn actor() -> ActorContext {
    ActorContext {
        actor_id: "data-fixture".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        session_id: Some(SessionId("data-fixture".into())),
        run_id: None,
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "explicit isolated data transport fixture".into(),
    }
}

pub(super) fn create(
    store: &mut SqliteStore,
    project: &ProjectId,
    key: &str,
) -> crate::domain::WorkItem {
    store
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: format!("Native {key}"),
                outcome: "Exchange selected native task text".into(),
                acceptance: vec!["Task text is retained".into()],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor(),
                idempotency_key: key.into(),
                created_at: chrono::Utc::now(),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
}

/// Exercise strict publication and retry with the ordinary product limits.
pub(super) fn publication_limits() -> RunLimits {
    RunLimits::default()
}

pub(super) fn cut(
    store: &SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
) -> OriginQualifiedCut {
    let cut = store
        .capture_task_changes(
            &binding.project,
            &ExportCut {
                position: 0,
                anchor: None,
            },
        )
        .unwrap()
        .cut;
    OriginQualifiedCut {
        binding: binding_id.clone(),
        origin: binding.store.origin.clone().unwrap(),
        restore_occurrence: None,
        position: FeedPosition {
            feed: FeedId::Project(binding.project.clone()),
            position: cut.position,
        },
        anchor: cut.anchor,
    }
}
