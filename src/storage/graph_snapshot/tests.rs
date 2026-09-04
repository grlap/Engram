use chrono::{Duration, TimeZone};
use tempfile::tempdir;

use super::*;
use crate::{
    Authority, ChildRequirement, CreateWorkRequest, Delivery, DevelopmentNoopRedactor, MemoryId,
    MemoryKind, MemoryStatus, MemoryVersion, NoteRequest, NoteVisibility,
    RememberProjectMemoryRequest, Scope, Sensitivity, WorkGraphSnapshotMemoryState,
    WorkGraphSnapshotText, WorkItemKind, WorkOrigin,
    domain::{AssuranceLevel, ProvenanceLink},
    storage::test_support::SentinelRedactor,
};

fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::seconds(second)
}

fn actor(session: &str) -> ActorContext {
    ActorContext {
        actor_id: session.into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(crate::SessionId(session.into())),
        source_tool: Some("graph_snapshot_test".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "exercise graph snapshot save".into(),
    }
}

fn create_root(
    store: &mut SqliteStore,
    project: &ProjectId,
    title: &str,
    key: &str,
) -> crate::WorkItem {
    store
        .create_work(
            &CreateWorkRequest {
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: title.into(),
                outcome: "the snapshot preserves planning state".into(),
                acceptance: vec!["the saved body is deterministic".into()],
                kind: WorkItemKind::Feature,
                priority: 1,
                labels: vec!["snapshot".into()],
                assigned_to: Some("planner".into()),
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor("planner-session"),
                idempotency_key: key.into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create root")
}

fn classified_project_memory(
    project: &ProjectId,
    key: &str,
    body: &str,
    sensitivity: Sensitivity,
    created_at: DateTime<Utc>,
) -> MemoryVersion {
    let memory_id = MemoryId::new();
    MemoryVersion {
        schema_version: crate::schema::SCHEMA_VERSION,
        memory_id,
        project_key: Some(key.into()),
        parents: Vec::new(),
        kind: MemoryKind::Episode,
        authority: Authority::Soft,
        delivery: Delivery::OnDemand,
        scope: Scope::Project {
            project: project.clone(),
        },
        title: format!("Project memory {key}"),
        body: body.into(),
        structured_value: None,
        tags: vec!["project-memory".into()],
        evidence: Vec::new(),
        refs: Vec::new(),
        source_snapshot: None,
        confidence: None,
        sensitivity,
        classification_reason: "snapshot test support classification".into(),
        delivery_override_reason: None,
        valid_from: None,
        valid_until: None,
        review_by: None,
        last_verified: None,
        actor: actor("memory-session"),
        created_at,
    }
}

fn insert_classified_project_memory(
    store: &mut SqliteStore,
    project: &ProjectId,
    key: &str,
    body: &str,
    sensitivity: Sensitivity,
    created_at: DateTime<Utc>,
) {
    let version = classified_project_memory(project, key, body, sensitivity, created_at);
    let version_object = CanonicalObject::freeze(&version).expect("freeze memory version");
    let assertion = crate::domain::MemoryAssertionEvent {
        schema_version: crate::schema::SCHEMA_VERSION,
        memory_id: version.memory_id,
        version: version_object.hash().clone(),
        status: MemoryStatus::Active,
        policy_reason: "snapshot test support activation".into(),
        actor: actor("memory-session"),
        created_at,
    };
    let assertion_object = CanonicalObject::freeze(&assertion).expect("freeze memory assertion");
    let transaction = store.connection.transaction().expect("memory transaction");
    SqliteStore::insert_object(&transaction, "memory_version", &version_object)
        .expect("insert memory version");
    SqliteStore::insert_object(&transaction, "memory_assertion_event", &assertion_object)
        .expect("insert memory assertion");
    transaction
        .execute(
            "INSERT INTO memory_heads (
                 memory_id, version_hash, assertion_hash, schema_version,
                 status, scope_kind, project_id, task_id, work_id, agent_id,
                 memory_kind, authority, delivery, sensitivity, title, body,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'active', 'project', ?5, NULL, NULL, NULL,
                 'episode', 'soft', 'on_demand', ?6, ?7, ?8, ?9
             )",
            rusqlite::params![
                version.memory_id.0.to_string(),
                version_object.hash().as_str(),
                assertion_object.hash().as_str(),
                i64::from(crate::schema::SCHEMA_VERSION),
                project.0,
                serde_json::to_value(sensitivity)
                    .expect("serialize sensitivity")
                    .as_str()
                    .expect("sensitivity word"),
                version.title,
                version.body,
                created_at.timestamp_millis(),
            ],
        )
        .expect("insert memory head");
    transaction
        .execute(
            "INSERT INTO project_memory_state (project_id, active_count, change_position)
             VALUES (?1, 1, 1)
             ON CONFLICT(project_id) DO UPDATE SET
                 active_count = project_memory_state.active_count + 1,
                 change_position = project_memory_state.change_position + 1",
            [project.0.as_str()],
        )
        .expect("advance project memory state");
    transaction.commit().expect("commit memory fixture");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves snapshot determinism, identity, audit order, and feed isolation"
)]
fn consecutive_idle_saves_keep_body_cut_digest_and_order() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot/project with spaces".into());
    let root = create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "Preserve the graph deterministically".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    let work_head = store
        .work_feed_head(&crate::FeedId::Project(project.clone()))
        .expect("work feed head");

    let first = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("first save");
    let second = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("second save");
    let repeated_timestamp = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("same-timestamp save");

    assert_eq!(first.document.body, second.document.body);
    assert_eq!(first.body_sha256, second.body_sha256);
    assert_eq!(
        first.document.manifest.summary,
        second.document.manifest.summary
    );
    assert_ne!(
        first.document.manifest.exported_at,
        second.document.manifest.exported_at
    );
    assert_eq!(
        first.document.manifest.exporting_build,
        work_graph_snapshot_exporting_build()
    );
    assert_eq!(first.document.body.summary.widening_reason, None);
    assert_eq!(second.document, repeated_timestamp.document);
    assert_eq!(first.document.body.summary.as_of.work_feed, work_head);
    assert_eq!(first.document.body.summary.as_of.project_memory, 1);
    assert_eq!(first.document.body.summary.section_counts.items, 1);
    assert_eq!(first.document.body.summary.section_counts.records, 1);
    assert_eq!(first.document.body.summary.section_counts.memories, 1);
    assert_eq!(first.document.body.items[0].work_id, root.work_id);
    assert_eq!(first.document.body.records[0].work_id, root.work_id);
    assert_eq!(
        CanonicalObject::freeze(&first.document.body)
            .expect("canonical body")
            .hash(),
        &first.body_sha256
    );
    assert_eq!(first.document.manifest.body_sha256, first.body_sha256);
    let body_json = serde_json::to_string(&first.document.body).expect("render snapshot body");
    assert!(!body_json.contains("exporting_build"));
    let rendered = serde_json::to_string(&first.document).expect("render snapshot");
    assert!(!rendered.contains("active_run_id"));
    assert!(!rendered.contains("claim_fence"));
    assert!(!rendered.contains("completion_seal"));

    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("ordered save audits");
    assert_eq!(audits.len(), 3);
    assert_ne!(audits[0].attempt_id, audits[1].attempt_id);
    assert_ne!(audits[1].attempt_id, audits[2].attempt_id);
    assert_eq!(audits[0].body_sha256, first.body_sha256);
    assert_eq!(audits[0].widening_reason, None);
    assert_eq!(audits[1].body_sha256, second.body_sha256);
    assert_eq!(audits[2].body_sha256, repeated_timestamp.body_sha256);
    let (audit_total, recent_audits) = store
        .recent_work_graph_snapshot_save_audits(&project, 2)
        .expect("bounded recent save audits");
    assert_eq!(audit_total, 3);
    assert_eq!(recent_audits, audits[1..]);
    let integrity = store.verify_all().expect("integrity after save audits");
    assert_eq!(integrity.checked_graph_snapshot_audits, 3);
    assert!(integrity.invalid_graph_snapshot_audits.is_empty());
    assert_eq!(
        store
            .work_feed_head(&crate::FeedId::Project(project))
            .expect("work head after audit"),
        work_head,
        "save audit must not enter the project work feed"
    );
}

#[test]
fn unkeyed_project_scope_memory_does_not_enter_the_keyed_snapshot_section() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-unkeyed-project-memory".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .capture_note(
            &NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: None,
                prose: "shared unkeyed project observation".into(),
                visibility: NoteVisibility::Shared,
                kind: Some(MemoryKind::Episode),
                authority: Some(Authority::Soft),
                sensitivity: Some(Sensitivity::Internal),
                title: Some("Unkeyed observation".into()),
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("memory-session"),
                idempotency_key: "unkeyed-project-observation".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture unkeyed project memory");

    let snapshot = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save ignores non-keyed project memory");
    assert!(snapshot.document.body.memories.is_empty());
    assert_eq!(snapshot.document.body.summary.section_counts.memories, 0);
    assert_eq!(snapshot.document.body.summary.as_of.project_memory, 0);
}

#[test]
fn save_refuses_work_projection_that_disagrees_with_canonical_feed_history() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-work-integrity".into());
    let root = create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    let projected_bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT item_json FROM work_items WHERE work_id = ?1",
            [root.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("work projection");
    let mut projected: serde_json::Value =
        serde_json::from_slice(&projected_bytes).expect("decode work projection");
    projected["title"] = serde_json::json!("projection drift");
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&projected).expect("serialize corrupt projection"),
            ],
        )
        .expect("corrupt work projection");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("work integrity verification")
    ));
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}

#[test]
fn save_refuses_project_memory_state_position_drift() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-memory-state-integrity".into());
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "preserve the canonical memory cut".into(),
                actor: actor("memory-session"),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    store
        .connection
        .execute(
            "UPDATE project_memory_state SET change_position = change_position + 1
             WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt project-memory position");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("canonical history")
    ));
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}

#[test]
fn save_refuses_project_memory_head_projection_drift() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-memory-head-integrity".into());
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "preserve the canonical memory projection".into(),
                actor: actor("memory-session"),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    store
        .connection
        .execute(
            "UPDATE memory_heads SET title = 'projection drift'
             WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt project-memory head");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("canonical objects")
    ));
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}

#[test]
fn redactor_refusal_returns_before_the_save_audit() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-redaction".into());
    create_root(
        &mut store,
        &project,
        "reject-me snapshot root",
        "redaction-root",
    );

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &SentinelRedactor,
        ),
        Err(StoreError::RedactionRefused(_))
    ));
    let audits = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_kind = 'work_graph_snapshot_saved'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count save audits");
    assert_eq!(audits, 0);
}

#[test]
fn widened_save_records_reason_even_when_current_project_memories_are_internal() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-sensitive-memory".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "ordinary project memory".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");

    let default = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("default snapshot");
    let widening_reason = "restore  restricted planning context";
    let widened = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some(widening_reason),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("widened snapshot");

    assert_eq!(default.document.body.summary.redacted.memories, 0);
    assert_eq!(widened.document.body.summary.redacted.memories, 0);
    assert_ne!(default.body_sha256, widened.body_sha256);
    assert!(!default.document.body.summary.widened);
    assert!(widened.document.body.summary.widened);
    assert_eq!(default.document.body.summary.widening_reason, None);
    assert_eq!(
        widened.document.body.summary.widening_reason.as_deref(),
        Some(widening_reason)
    );
    assert_eq!(
        default.document.body.summary.redactor_status,
        "development no-op; no secret or PII protection"
    );
    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("sensitive snapshot audits");
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0].widening_reason, None);
    assert_eq!(audits[1].widening_reason.as_deref(), Some(widening_reason));
}

#[test]
fn restricted_memory_is_typed_redaction_while_secret_reference_is_retained() {
    let project = ProjectId("snapshot-sensitive-memory".into());
    let restricted = classified_project_memory(
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    let (default_restricted_state, default_was_redacted) =
        snapshot_active_memory(restricted.clone(), false);
    let WorkGraphSnapshotMemoryState::Active {
        body: default_restricted,
        ..
    } = default_restricted_state
    else {
        panic!("restricted fixture must be active");
    };
    assert!(default_was_redacted);
    assert_eq!(
        default_restricted,
        WorkGraphSnapshotText::Redacted {
            sensitivity: Sensitivity::Restricted
        }
    );
    let (widened_restricted_state, widened_was_redacted) = snapshot_active_memory(restricted, true);
    let WorkGraphSnapshotMemoryState::Active {
        body: widened_restricted,
        ..
    } = widened_restricted_state
    else {
        panic!("widened restricted fixture must be active");
    };
    assert!(!widened_was_redacted);
    assert_eq!(
        widened_restricted,
        WorkGraphSnapshotText::Present {
            value: "restricted planning detail".into()
        }
    );
    let secret_reference = classified_project_memory(
        &project,
        "secret-reference",
        "vault://engram/snapshot-secret",
        Sensitivity::SecretRef,
        at(2),
    );
    for widened in [false, true] {
        let (state, was_redacted) = snapshot_active_memory(secret_reference.clone(), widened);
        assert!(!was_redacted);
        let WorkGraphSnapshotMemoryState::Active { body, .. } = state else {
            panic!("secret reference fixture must be active");
        };
        assert_eq!(
            body,
            WorkGraphSnapshotText::Present {
                value: "vault://engram/snapshot-secret".into()
            }
        );
    }
}

#[test]
fn saved_snapshot_redacts_restricted_memory_and_carries_secret_reference_verbatim() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-sensitive-memory-save".into());
    insert_classified_project_memory(
        &mut store,
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    insert_classified_project_memory(
        &mut store,
        &project,
        "secret-reference",
        "writer-asserted opaque reference",
        Sensitivity::SecretRef,
        at(2),
    );

    let default = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("default snapshot");
    let widened = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("restore restricted planning context"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("widened snapshot");

    assert_eq!(default.document.body.summary.redacted.memories, 1);
    assert_eq!(widened.document.body.summary.redacted.memories, 0);
    assert!(matches!(
        &default.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Redacted {
                sensitivity: Sensitivity::Restricted
            },
            ..
        }
    ));
    assert!(matches!(
        &widened.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Present { value },
            ..
        } if value == "restricted planning detail"
    ));
    for snapshot in [&default, &widened] {
        assert!(matches!(
            &snapshot.document.body.memories[1].state,
            WorkGraphSnapshotMemoryState::Active {
                body: WorkGraphSnapshotText::Present { value },
                sensitivity: Sensitivity::SecretRef,
                ..
            } if value == "writer-asserted opaque reference"
        ));
    }
    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("sensitive snapshot audits");
    assert_eq!(audits[0].redacted.memories, 1);
    assert_eq!(audits[1].redacted.memories, 0);
}

#[test]
fn widening_reason_is_required_to_be_meaningful_before_audit() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-widening-reason".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("   "),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(message)) if message.contains("widening reason")
    ));
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}

#[test]
fn snapshot_audit_attribution_is_bounded_and_safe_for_diagnostics() {
    let directory = tempdir().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-audit-attribution".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");

    for actor_id in [
        "actor\u{202e}".to_owned(),
        "x".repeat(MAX_SNAPSHOT_AUDIT_ATTRIBUTION_TEXT_BYTES + 1),
    ] {
        let mut unsafe_actor = actor("save-session");
        unsafe_actor.actor_id = actor_id;
        assert!(matches!(
            store.save_work_graph_snapshot(
                &project,
                &unsafe_actor,
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                at(3),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(message))
                if message.contains("actor id") && message.contains("without control or format characters")
        ));
    }
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}
