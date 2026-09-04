use chrono::{Duration, TimeZone};
use tempfile::tempdir;

use super::*;
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

fn create_imported_root(
    store: &mut SqliteStore,
    project: &ProjectId,
) -> (crate::WorkItem, WorkSourceSnapshot) {
    let snapshot = WorkSourceSnapshot {
        schema_version: crate::schema::SCHEMA_VERSION,
        adapter_kind: "snapshot-test".into(),
        canonical_ref: "tracker:SNAP-1".into(),
        projected: crate::domain::WorkSourceProjection {
            title: Some("Imported snapshot work".into()),
            body: None,
            status: Some("open".into()),
            owner: None,
        },
        captured_at: at(1),
        source_revision: Some("revision-1".into()),
        fingerprint: "source-fingerprint".into(),
        canonical_url: Some("https://tracker.invalid/SNAP-1".into()),
        payload_hash: CanonicalObject::freeze(&serde_json::json!({"source": "SNAP-1"}))
            .expect("freeze source payload")
            .hash()
            .clone(),
        raw: std::collections::BTreeMap::new(),
    };
    let object = CanonicalObject::freeze(&snapshot).expect("freeze source snapshot");
    let transaction = store.connection.transaction().expect("source transaction");
    SqliteStore::insert_object(&transaction, "work_source_snapshot", &object)
        .expect("insert source snapshot");
    transaction.commit().expect("commit source snapshot");
    let item = store
        .create_work(
            &CreateWorkRequest {
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Imported snapshot work".into(),
                outcome: "the imported provenance survives recreation".into(),
                acceptance: vec!["the source stays exact".into()],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: vec!["snapshot".into()],
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Imported,
                source_snapshot_id: Some(object.hash().clone()),
                actor: actor("import-session"),
                idempotency_key: "imported-snapshot-root".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create imported root");
    (item, snapshot)
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
        source_snapshot: Some(SourceSnapshot {
            source_ref: RESTORED_MEMORY_SOURCE.into(),
            fingerprint: "0".repeat(64),
            observed_at: created_at,
        }),
        confidence: None,
        sensitivity,
        classification_reason: "restored project episode".into(),
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
        policy_reason: "restored project episode is active immediately".into(),
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

fn snapshot_bytes(document: &WorkGraphSnapshotDocument) -> Vec<u8> {
    serde_json::to_vec_pretty(document).expect("serialize snapshot")
}

fn rebind_snapshot_body(document: &mut WorkGraphSnapshotDocument) {
    document.manifest.summary = document.body.summary.clone();
    document.manifest.body_sha256 = CanonicalObject::freeze(&document.body)
        .expect("freeze changed snapshot body")
        .hash()
        .clone();
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
fn save_refuses_a_document_that_the_loader_would_reject_before_audit() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("snapshot-self-validation".into());
    create_root(
        &mut store,
        &project,
        "Unsafe\u{202e}title",
        "unsafe-history-shape",
    );

    let error = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect_err("save must not disclose a snapshot its loader rejects");
    assert!(
        matches!(error, StoreError::InvalidGraphSnapshot(message) if message.contains("work title"))
    );
    let (count, audits) = store
        .recent_work_graph_snapshot_save_audits(&project, 8)
        .expect("read disclosure audit");
    assert_eq!(count, 0);
    assert!(audits.is_empty());
}

#[test]
fn load_uses_body_semantics_and_exact_typed_source_bytes() {
    type SourceMutation = (&'static str, fn(&mut serde_json::Value));

    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-source-validation".into());
    let mut source =
        SqliteStore::open(directory.path().join("source-validation.db")).expect("source store");
    create_imported_root(&mut source, &project);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save imported work");

    let mut manifest_retimestamped = saved.document.clone();
    manifest_retimestamped.manifest.exported_at = at(0);
    let mut destination =
        SqliteStore::open(directory.path().join("manifest-time.db")).expect("destination store");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&manifest_retimestamped),
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("manifest time must not change body acceptance");

    let mutations: [SourceMutation; 3] = [
        ("source-top-member", |value| {
            value
                .as_object_mut()
                .expect("source object")
                .insert("unexpected".into(), serde_json::json!(true));
        }),
        ("source-nested-member", |value| {
            value["projected"]
                .as_object_mut()
                .expect("projected source object")
                .insert("unexpected".into(), serde_json::json!(true));
        }),
        ("source-scalar-spelling", |value| {
            value["captured_at"] = serde_json::json!("2026-09-04T12:00:01+00:00");
        }),
    ];
    for (name, mutate) in mutations {
        let mut document = saved.document.clone();
        let source = document
            .body
            .sources
            .first_mut()
            .expect("one source snapshot");
        mutate(&mut source.canonical_json);
        source.hash = CanonicalObject::freeze(&source.canonical_json)
            .expect("freeze mutated source")
            .hash()
            .clone();
        document.body.items[0].source_snapshot_id = Some(source.hash.clone());
        rebind_snapshot_body(&mut document);
        let mut destination =
            SqliteStore::open(directory.path().join(format!("{name}.db"))).expect("destination");
        assert!(matches!(
            destination.load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &snapshot_bytes(&document),
                false,
                at(4), &DevelopmentNoopRedactor,),
            Err(StoreError::InvalidGraphSnapshot(message))
                if message.contains("exactly preserved")
        ));
    }
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
fn restored_redacted_memory_stays_typed_when_a_later_save_is_widened() {
    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-restored-redaction".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source store");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save redacted source memory");
    assert_eq!(saved.document.body.summary.redacted.memories, 1);
    let bytes = serde_json::to_vec_pretty(&saved.document).expect("serialize snapshot");

    let mut restored =
        SqliteStore::open(directory.path().join("restored.db")).expect("restored store");
    restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load redacted memory");
    let restored_memory = restored
        .project_memory_full(
            &project,
            &crate::SessionId("reader-session".into()),
            &actor("reader-session"),
            "restricted-entry",
        )
        .expect("read restored placeholder");
    assert_eq!(restored_memory.body, REDACTED_MEMORY_PLACEHOLDER);

    let widened = restored
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("carry every available restricted field"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("save widened restored graph");
    assert!(widened.document.body.summary.widened);
    assert_eq!(widened.document.body.summary.redacted.memories, 1);
    assert!(matches!(
        &widened.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Redacted {
                sensitivity: Sensitivity::Restricted
            },
            ..
        }
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "checks restricted load disclosure, audit, and stable identity across two fresh destinations"
)]
fn widened_restricted_load_stores_only_audited_placeholders() {
    let project = ProjectId("snapshot-widened-load".into());
    let mut source = SqliteStore::open_in_memory().expect("source");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "private-plaintext-sentinel",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("human-readable recovery"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("widened save");
    assert_eq!(saved.document.body.summary.redacted.memories, 0);
    let bytes = snapshot_bytes(&saved.document);
    assert!(String::from_utf8_lossy(&bytes).contains("private-plaintext-sentinel"));
    let mut memory_ids = Vec::new();
    for _ in 0..2 {
        let mut destination = SqliteStore::open_in_memory().expect("destination");
        let preview = destination
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                true,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect("preview widened load");
        assert_eq!(preview.preview.placeholder_memories, ["restricted-entry"]);
        let loaded = destination
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                false,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect("load widened file");
        assert_eq!(loaded.preview, preview.preview);
        let memory = destination
            .project_memory_full(
                &project,
                &crate::SessionId("peer".into()),
                &actor("peer"),
                "restricted-entry",
            )
            .expect("peer reads only placeholder");
        assert_eq!(memory.body, REDACTED_MEMORY_PLACEHOLDER);
        let raw_bodies: i64 = destination
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE instr(CAST(canonical_json AS TEXT), ?1) > 0",
                ["private-plaintext-sentinel"],
                |row| row.get(0),
            )
            .expect("search all persisted canonical bytes");
        assert_eq!(raw_bodies, 0);
        let stored_id: String = destination
            .connection
            .query_row("SELECT memory_id FROM memory_heads", [], |row| row.get(0))
            .expect("restored memory identity");
        let memory_id = uuid::Uuid::parse_str(&stored_id).expect("valid UUID");
        assert_eq!(memory_id.get_version_num(), 8);
        assert_eq!(memory_id.get_variant(), uuid::Variant::RFC4122);
        memory_ids.push(memory_id);
        let (_, audits) = destination
            .recent_work_graph_snapshot_load_audits(&project, 8)
            .expect("load audits");
        let audit = &audits[0];
        assert!(audit.widened);
        assert_eq!(
            audit.widening_reason.as_deref(),
            Some("human-readable recovery")
        );
        assert_eq!(audit.redacted.memories, 1);
        let mut missing_reason = audit.clone();
        missing_reason.widening_reason = None;
        assert!(validate_loaded_event(&missing_reason, Some(&project)).is_err());
        let mut inconsistent_flag = audit.clone();
        inconsistent_flag.widened = false;
        assert!(validate_loaded_event(&inconsistent_flag, Some(&project)).is_err());
        let saved_again = destination
            .save_work_graph_snapshot(
                &project,
                &actor("save-session"),
                Some("save available text"),
                WorkGraphSnapshotDestinationKind::Stdout,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect("resave never recovers restricted plaintext");
        assert_eq!(saved_again.document.body.summary.redacted.memories, 1);
        assert!(
            !String::from_utf8_lossy(&snapshot_bytes(&saved_again.document))
                .contains("private-plaintext-sentinel")
        );
        assert!(destination.verify_all().expect("integrity").is_healthy());
    }
    assert_eq!(memory_ids[0], memory_ids[1]);
}

#[test]
fn load_redactor_refusal_leaves_every_destination_section_and_audit_absent() {
    let project = ProjectId("snapshot-load-redaction".into());
    let mut source = SqliteStore::open_in_memory().expect("source");
    create_root(&mut source, &project, "Safe work title", "source-root");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "reject-me restricted plaintext",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("human-readable recovery"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("widened source");
    let bytes = snapshot_bytes(&saved.document);
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    let object_count = |store: &SqliteStore| {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("object count")
    };
    let before = object_count(&destination);
    for dry_run in [true, false] {
        assert!(matches!(
            destination.load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                dry_run,
                at(3),
                &SentinelRedactor,
            ),
            Err(StoreError::RedactionRefused(_))
        ));
        assert_eq!(object_count(&destination), before);
        let rows: i64 = destination
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM work_items)
                  + (SELECT COUNT(*) FROM memory_heads)
                  + (SELECT COUNT(*) FROM work_restored_records)",
                [],
                |row| row.get(0),
            )
            .expect("all load sections remain empty");
        assert_eq!(rows, 0);
        assert_eq!(
            destination
                .recent_work_graph_snapshot_load_audits(&project, 8)
                .expect("load audit count")
                .0,
            0
        );
    }
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
        "x".repeat(MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES + 1),
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
    let mut long_actor = actor("save-session");
    long_actor.actor_id = "x".repeat(300);
    store
        .save_work_graph_snapshot(
            &project,
            &long_actor,
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("ordinary long attribution remains valid for save");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one round trip proves open planning relations, notes, memories, inert history, and resumed execution"
)]
fn save_load_save_recreates_inert_work_and_preserves_restored_records() {
    let directory = tempdir().expect("tempdir");
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source store");
    let project = ProjectId("snapshot-round-trip".into());
    let root = create_root(&mut source, &project, "Snapshot root", "snapshot-root");
    let prerequisite = create_root(
        &mut source,
        &project,
        "Snapshot prerequisite",
        "snapshot-prerequisite",
    );
    source
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: root.work_id,
                prerequisite_id: prerequisite.work_id,
                expected_revision: root.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner-session"),
                idempotency_key: "snapshot-prerequisite-edge".into(),
                changed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add source prerequisite");
    let claim = source
        .claim_work(
            &ClaimWorkRequest {
                work_id: prerequisite.work_id,
                expected_work_revision: prerequisite.revision,
                expected_run_id: prerequisite.active_run_id,
                holder: crate::SessionId("note-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("note-session"),
                idempotency_key: "claim-prerequisite-for-note".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim source prerequisite");
    source
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: prerequisite.work_id,
                run_id: claim.run_id,
                expected_work_revision: prerequisite.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "the open prerequisite carries its note".into(),
                refs: vec!["review:open-item".into()],
                actor: actor("note-session"),
                idempotency_key: "note-open-prerequisite".into(),
                recorded_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record source note");
    source
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "recreate the keyed project memory".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember source memory");
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save source graph");
    let bytes = serde_json::to_vec_pretty(&saved.document).expect("serialize snapshot");

    let mut restored =
        SqliteStore::open(directory.path().join("restored.db")).expect("restored store");
    let preview = restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("dry-run load");
    assert!(!preview.loaded);
    assert_eq!(preview.preview.refs.len(), 2);
    assert!(preview.preview.refs.contains(&root.short_ref));
    assert!(preview.preview.refs.contains(&prerequisite.short_ref));
    assert_eq!(preview.preview.summary.section_counts.memories, 1);
    assert!(matches!(
        restored.inspect_work(root.work_id, at(4)),
        Err(StoreError::WorkNotFound(_))
    ));
    let (audit_count, audits) = restored
        .recent_work_graph_snapshot_load_audits(&project, 8)
        .expect("dry-run audit query");
    assert_eq!(audit_count, 0);
    assert!(audits.is_empty());

    let loaded = restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("committed load");
    assert!(loaded.loaded);
    let (load_audit_count, load_audits) = restored
        .recent_work_graph_snapshot_load_audits(&project, 8)
        .expect("load audit query");
    assert_eq!(load_audit_count, 1);
    assert_eq!(load_audits.len(), 1);
    assert_eq!(load_audits[0].body_sha256, loaded.preview.body_sha256);
    let attempt_id = uuid::Uuid::parse_str(&load_audits[0].attempt_id).expect("load attempt id");
    assert_eq!(attempt_id.get_version(), Some(uuid::Version::SortRand));
    let memory = restored
        .project_memory_full(
            &project,
            &crate::SessionId("reader-session".into()),
            &actor("reader-session"),
            "snapshot-contract",
        )
        .expect("read restored project memory");
    assert_eq!(memory.body, "recreate the keyed project memory");
    let ready = restored
        .inspect_work(root.work_id, at(6))
        .expect("inspect restored work");
    assert!(ready.work.restored);
    assert!(ready.work.active_run_id.is_none());
    assert_eq!(ready.availability, crate::WorkAvailability::Blocked);
    assert_eq!(ready.blocked_by, vec![prerequisite.work_id]);
    let original_item_json = serde_json::to_vec(&ready.work).expect("serialize restored item");
    let mut tampered_item = ready.work.clone();
    tampered_item.title = "projection-only tamper".into();
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&tampered_item).expect("serialize tampered item")
            ],
        )
        .expect("tamper restored projection");
    assert!(matches!(
        restored.get_work_item(root.work_id),
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("canonical restored record")
    ));
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![root.work_id.0.to_string(), &original_item_json],
        )
        .expect("restore projected item");
    let assert_claim_refuses_projection = |store: &mut SqliteStore, key: &str| {
        assert!(matches!(
            store.claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: 1,
                    expected_run_id: None,
                    holder: crate::SessionId("tamper-session".into()),
                    ttl_seconds: 300,
                    recovery_reason: None,
                    actor: actor("tamper-session"),
                    idempotency_key: key.into(),
                    claimed_at: at(6),
                },
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWorkProjection(_))
        ));
    };
    let mut schema_tamper = ready.work.clone();
    schema_tamper.schema_version += 1;
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&schema_tamper).expect("serialize schema tamper")
            ],
        )
        .expect("tamper restored schema");
    assert_claim_refuses_projection(&mut restored, "claim-schema-tamper");
    let mut attribution_tamper = ready.work.clone();
    attribution_tamper.created_by = actor("invented-loader");
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&attribution_tamper).expect("serialize attribution tamper")
            ],
        )
        .expect("tamper restored attribution");
    assert_claim_refuses_projection(&mut restored, "claim-attribution-tamper");
    for (name, created_at, updated_at) in [
        ("created-at", at(4), ready.work.updated_at),
        ("updated-at", ready.work.created_at, at(7)),
    ] {
        let mut time_tamper = ready.work.clone();
        time_tamper.created_at = created_at;
        time_tamper.updated_at = updated_at;
        restored
            .connection
            .execute(
                "UPDATE work_items SET item_json = ?2, created_at_ms = ?3, updated_at_ms = ?4
                 WHERE work_id = ?1",
                rusqlite::params![
                    root.work_id.0.to_string(),
                    serde_json::to_vec(&time_tamper).expect("serialize timestamp tamper"),
                    created_at.timestamp_millis(),
                    updated_at.timestamp_millis()
                ],
            )
            .expect("tamper both timestamp projections consistently");
        assert_claim_refuses_projection(&mut restored, name);
    }
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2, created_at_ms = ?3, updated_at_ms = ?4
             WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                &original_item_json,
                ready.work.created_at.timestamp_millis(),
                ready.work.updated_at.timestamp_millis()
            ],
        )
        .expect("restore projected item again");
    let prerequisite_anchor = restored
        .connection
        .query_row(
            "SELECT event_hash FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
            |row| row.get::<_, String>(0),
        )
        .expect("read restored prerequisite anchor");
    restored
        .connection
        .execute(
            "DELETE FROM work_prerequisites WHERE work_id = ?1 AND prerequisite_id = ?2",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
        )
        .expect("tamper restored relation projection");
    assert_claim_refuses_projection(&mut restored, "claim-relation-tamper");
    restored
        .connection
        .execute(
            "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string(),
                prerequisite_anchor
            ],
        )
        .expect("restore relation projection");
    let restored_history = restored
        .work_restored_records(prerequisite.work_id)
        .expect("read restored prerequisite history");
    assert!(restored_history.iter().any(|record| {
        record.history.notes.iter().any(|note| {
            note.summary == "the open prerequisite carries its note"
                && note.refs == ["review:open-item"]
        })
    }));
    let (_, invalid) = restored
        .verify_work_projections()
        .expect("verify restored work");
    assert!(invalid.is_empty(), "{invalid:?}");

    let saved_again = restored
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("save restored graph");
    assert!(matches!(
        saved_again.document.body.records[0].payload,
        crate::WorkGraphSnapshotRecordPayload::Restored { .. }
    ));
    let bytes_again =
        serde_json::to_vec_pretty(&saved_again.document).expect("serialize restored snapshot");
    let mut restored_again = SqliteStore::open(directory.path().join("restored-again.db"))
        .expect("second restored store");
    restored_again
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes_again,
            false,
            at(8),
            &DevelopmentNoopRedactor,
        )
        .expect("load restored graph again");
    let (_, invalid) = restored_again
        .verify_work_projections()
        .expect("verify second restored work");
    assert!(invalid.is_empty(), "{invalid:?}");

    let claim = restored_again
        .claim_work(
            &ClaimWorkRequest {
                work_id: prerequisite.work_id,
                expected_work_revision: 1,
                expected_run_id: None,
                holder: crate::SessionId("runner-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("runner-session"),
                idempotency_key: "claim-restored-root".into(),
                claimed_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim restored work");
    assert_eq!(claim.work_id, prerequisite.work_id);
    let (_, invalid) = restored_again
        .verify_work_projections()
        .expect("verify claimed restored work");
    assert!(invalid.is_empty(), "{invalid:?}");
    let saved_after_claim = restored_again
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .expect("save claimed restored graph");
    assert_eq!(saved_after_claim.document.body.records.len(), 3);
    assert_eq!(
        saved_after_claim
            .document
            .body
            .records
            .iter()
            .filter(|record| matches!(
                record.payload,
                crate::WorkGraphSnapshotRecordPayload::Restored { .. }
            ))
            .count(),
        2
    );
    assert_eq!(
        saved_after_claim
            .document
            .body
            .records
            .iter()
            .filter(|record| matches!(
                record.payload,
                crate::WorkGraphSnapshotRecordPayload::Native { .. }
            ))
            .count(),
        1
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "two save/load generations establish independent scalar and historical-proof regressions"
)]
fn load_validates_exact_and_internal_shape_of_every_restored_generation() {
    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-restored-generation-validation".into());
    let mut source =
        SqliteStore::open(directory.path().join("generation-source.db")).expect("source store");
    let root = create_root(&mut source, &project, "Generation root", "generation-root");
    let first = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save first generation");
    let mut middle =
        SqliteStore::open(directory.path().join("generation-middle.db")).expect("middle store");
    middle
        .load_work_graph_snapshot(
            &project,
            &actor("middle-load"),
            &snapshot_bytes(&first.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load first generation");
    middle
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: 1,
                expected_run_id: None,
                holder: crate::SessionId("runner-session".into()),
                ttl_seconds: 300,
                recovery_reason: None,
                actor: actor("runner-session"),
                idempotency_key: "generation-claim".into(),
                claimed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim restored work");
    let second = middle
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("save second generation");
    let mut terminal =
        SqliteStore::open(directory.path().join("generation-terminal.db")).expect("terminal store");
    terminal
        .load_work_graph_snapshot(
            &project,
            &actor("terminal-load"),
            &snapshot_bytes(&second.document),
            false,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("load second generation");
    let carried = terminal
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("save two carried generations");
    assert_eq!(carried.document.body.records.len(), 2);
    assert!(carried.document.body.records.iter().all(|record| matches!(
        record.payload,
        crate::WorkGraphSnapshotRecordPayload::Restored { .. }
    )));

    let mut nonpreserved_scalar = carried.document.clone();
    let crate::WorkGraphSnapshotRecordPayload::Restored {
        object_hash,
        canonical_json,
    } = &mut nonpreserved_scalar.body.records[0].payload
    else {
        panic!("first generation must be restored");
    };
    let created_at = canonical_json["history"]["events"][0]["occurred_at"]
        .as_str()
        .expect("history event timestamp")
        .strip_suffix('Z')
        .expect("UTC timestamp")
        .to_owned()
        + "+00:00";
    canonical_json["history"]["events"][0]["occurred_at"] = serde_json::json!(created_at);
    *object_hash = CanonicalObject::freeze(canonical_json)
        .expect("freeze nonpreserved scalar")
        .hash()
        .clone();
    rebind_snapshot_body(&mut nonpreserved_scalar);
    let mut destination =
        SqliteStore::open(directory.path().join("generation-unknown.db")).expect("destination");
    assert!(matches!(
        destination.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&nonpreserved_scalar),
            false,
            at(8), &DevelopmentNoopRedactor,),
        Err(StoreError::InvalidGraphSnapshot(message)) if message.contains("exactly preserved")
    ));

    let mut invalid_old_lifecycle = carried.document.clone();
    let crate::WorkGraphSnapshotRecordPayload::Restored {
        object_hash,
        canonical_json,
    } = &mut invalid_old_lifecycle.body.records[0].payload
    else {
        panic!("first generation must be restored");
    };
    canonical_json["item"]["lifecycle"] = serde_json::json!("completed");
    *object_hash = CanonicalObject::freeze(canonical_json)
        .expect("freeze invalid old generation")
        .hash()
        .clone();
    rebind_snapshot_body(&mut invalid_old_lifecycle);
    let mut destination =
        SqliteStore::open(directory.path().join("generation-lifecycle.db")).expect("destination");
    assert!(matches!(
        destination.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&invalid_old_lifecycle),
            false,
            at(8), &DevelopmentNoopRedactor,),
        Err(StoreError::InvalidGraphSnapshot(message))
            if message.contains("completion proof")
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one blocked graph fixture exercises all three runless planning mutations and integrity"
)]
fn runless_restored_work_supports_blocked_planning_and_disposal() {
    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-runless-planning".into());
    let mut source =
        SqliteStore::open(directory.path().join("runless-source.db")).expect("source store");
    let empty = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(0),
            &DevelopmentNoopRedactor,
        )
        .expect("save empty graph");
    let cancel = create_root(&mut source, &project, "Cancel restored", "runless-cancel");
    let supersede = create_root(
        &mut source,
        &project,
        "Supersede restored",
        "runless-supersede",
    );
    let decompose = create_root(
        &mut source,
        &project,
        "Decompose restored",
        "runless-decompose",
    );
    let replacement = create_root(&mut source, &project, "Replacement", "runless-replacement");
    for (index, work) in [&cancel, &supersede, &decompose].into_iter().enumerate() {
        source
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: work.work_id,
                    expected_work_revision: work.revision,
                    kind: WorkBlockerKind::Manual,
                    detail: format!("restored blocker {index}"),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("runless-blocker-{index}"),
                    blocked_at: at(5 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("block source work");
    }
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .expect("save blocked roots");
    let bytes = snapshot_bytes(&saved.document);

    let mut cancelled =
        SqliteStore::open(directory.path().join("runless-cancel.db")).expect("cancel store");
    cancelled
        .load_work_graph_snapshot(
            &project,
            &actor("empty-load-session"),
            &snapshot_bytes(&empty.document),
            false,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .expect("empty load must not obstruct a later work load with an earlier clock");
    cancelled
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load cancel graph");
    let cancelled_item = cancelled
        .dispose_work(
            &DisposeWorkRequest {
                work_id: cancel.work_id,
                expected_work_revision: 1,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "cancel restored blocked work".into(),
                actor: actor("planner-session"),
                idempotency_key: "runless-cancel-dispose".into(),
                disposed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel restored blocked work");
    assert_eq!(cancelled_item.lifecycle, WorkLifecycle::Cancelled);
    assert!(
        cancelled
            .verify_all()
            .expect("verify cancellation")
            .is_healthy()
    );

    let mut superseded =
        SqliteStore::open(directory.path().join("runless-supersede.db")).expect("supersede store");
    superseded
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load supersede graph");
    let superseded_item = superseded
        .dispose_work(
            &DisposeWorkRequest {
                work_id: supersede.work_id,
                expected_work_revision: 1,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(replacement.work_id),
                reason: "replace restored blocked work".into(),
                actor: actor("planner-session"),
                idempotency_key: "runless-supersede-dispose".into(),
                disposed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("supersede restored blocked work");
    assert_eq!(superseded_item.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(superseded_item.superseded_by, Some(replacement.work_id));
    assert!(
        superseded
            .verify_all()
            .expect("verify supersession")
            .is_healthy()
    );

    let mut decomposed =
        SqliteStore::open(directory.path().join("runless-decompose.db")).expect("decompose store");
    decomposed
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load decompose graph");
    let mut request = DecomposeWorkRequest {
        parent_id: decompose.work_id,
        expected_parent_revision: 1,
        children: vec![ChildWorkDraft {
            local_key: "native-child".into(),
            child_requirement: ChildRequirement::Required,
            title: "Native child after load".into(),
            outcome: "the child is planned".into(),
            acceptance: vec!["child exists".into()],
            kind: WorkItemKind::Task,
            priority: 2,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
        }],
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor: actor("planner-session"),
        idempotency_key: "runless-decomposition".into(),
        created_at: at(12),
    };
    request.prerequisites.push(crate::ChildWorkPrerequisite {
        work_key: "native-child".into(),
        prerequisite: crate::WorkDependencyRef::Existing(decompose.work_id),
    });
    let before = decomposed
        .get_work_item(decompose.work_id)
        .expect("parent before refusal");
    let objects_before: i64 = decomposed
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("objects before refusal");
    assert!(matches!(
        decomposed.decompose_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::WorkDependencyCycle)
    ));
    assert_eq!(
        decomposed
            .get_work_item(decompose.work_id)
            .expect("parent after refusal"),
        before
    );
    let objects_after: i64 = decomposed
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("objects after refusal");
    assert_eq!(objects_after, objects_before);
    assert!(
        decomposed
            .verify_all()
            .expect("verify refused decompose")
            .is_healthy()
    );
    request.prerequisites.clear();
    let result = decomposed
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect("decompose restored blocked work");
    assert_eq!(result.children.len(), 1);
    assert!(result.parent.active_run_id.is_some());
    assert!(
        decomposed
            .verify_all()
            .expect("verify decomposition")
            .is_healthy()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one round trip creates terminal fanout above the live open-descendant envelope"
)]
fn terminal_direct_children_above_the_open_envelope_round_trip() {
    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-terminal-fanout".into());
    let mut source =
        SqliteStore::open(directory.path().join("terminal-source.db")).expect("source store");
    let root = create_root(
        &mut source,
        &project,
        "Large terminal history",
        "terminal-root",
    );
    let replacement = create_root(
        &mut source,
        &project,
        "Terminal replacement",
        "terminal-replacement",
    );
    let mut child_count = 0_usize;
    for batch in 0..9 {
        let parent = source.get_work_item(root.work_id).expect("current parent");
        let children = (0..16)
            .map(|index| ChildWorkDraft {
                local_key: format!("child-{batch:02}-{index:02}"),
                child_requirement: ChildRequirement::Optional,
                title: format!("Terminal child {batch:02}-{index:02}"),
                outcome: "Preserve terminal planning history".into(),
                acceptance: vec!["terminal history is retained".into()],
                kind: WorkItemKind::Task,
                priority: 2,
                labels: vec!["snapshot".into()],
                assigned_to: None,
                deferred_until: None,
            })
            .collect();
        let decomposition = source
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: parent.revision,
                    children,
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("terminal-batch-{batch}"),
                    created_at: at(i64::from(batch) + 2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose terminal batch");
        for child in decomposition.children {
            let superseded = child_count == 0;
            source
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: child.work_id,
                        expected_work_revision: child.revision,
                        disposition: if superseded {
                            WorkDisposition::Superseded
                        } else {
                            WorkDisposition::Cancelled
                        },
                        replacement_id: superseded.then_some(replacement.work_id),
                        reason: "retained terminal history".into(),
                        actor: actor("planner-session"),
                        idempotency_key: format!("cancel-terminal-{}", child.short_ref),
                        disposed_at: at(i64::from(batch) + 20),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("cancel terminal child");
            child_count += 1;
        }
    }
    assert!(child_count > super::super::work::MAX_OPEN_WORK_DESCENDANTS as usize);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(40),
            &DevelopmentNoopRedactor,
        )
        .expect("save terminal fanout");
    let bytes = snapshot_bytes(&saved.document);
    let mut destination = SqliteStore::open(directory.path().join("terminal-destination.db"))
        .expect("destination store");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(41),
            &DevelopmentNoopRedactor,
        )
        .expect("load terminal fanout");
    let children = destination
        .work_children(root.work_id)
        .expect("restored terminal children");
    assert_eq!(children.len(), child_count);
    assert_eq!(
        children
            .iter()
            .filter(|child| child.lifecycle == WorkLifecycle::Superseded)
            .count(),
        1
    );
    assert_eq!(
        children
            .iter()
            .filter(|child| child.lifecycle == WorkLifecycle::Cancelled)
            .count(),
        child_count - 1
    );
    assert!(children.iter().any(|child| {
        child.lifecycle == WorkLifecycle::Superseded
            && child.superseded_by == Some(replacement.work_id)
    }));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one refusal matrix proves corrupt inputs never leave partial destination state"
)]
fn load_refuses_incompatible_and_corrupt_documents_without_partial_state() {
    let directory = tempdir().expect("tempdir");
    let project = ProjectId("snapshot-load-refusals".into());
    let mut source =
        SqliteStore::open(directory.path().join("source-refusals.db")).expect("source store");
    let root = create_root(&mut source, &project, "Snapshot root", "snapshot-root");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-refusal",
        "restricted body",
        Sensitivity::Restricted,
        at(2),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save source graph");

    let refuse = |name: &str, bytes: &[u8], destination: &ProjectId| {
        let mut store = SqliteStore::open(directory.path().join(format!("{name}.db")))
            .expect("open refusal destination");
        let error = store
            .load_work_graph_snapshot(
                destination,
                &actor("load-session"),
                bytes,
                false,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect_err("load must be refused");
        let work_count = store
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count destination work");
        assert_eq!(work_count, 0, "{name} left partial work");
        error
    };

    let mut digest_mismatch = saved.document.clone();
    digest_mismatch.body.items[0].title = "Changed without rebinding the digest".into();
    assert!(matches!(
        refuse(
            "digest-mismatch",
            &snapshot_bytes(&digest_mismatch),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("digest")
    ));

    let mut dangling = saved.document.clone();
    dangling.body.items[0].prerequisites = vec![WorkId::new()];
    rebind_snapshot_body(&mut dangling);
    assert!(matches!(
        refuse("dangling", &snapshot_bytes(&dangling), &project),
        StoreError::InvalidGraphSnapshot(message) if message.contains("dangling")
    ));

    let mut false_redaction_count = saved.document.clone();
    false_redaction_count.body.summary.redacted.memories = 2;
    rebind_snapshot_body(&mut false_redaction_count);
    assert!(matches!(
        refuse(
            "redaction-count",
            &snapshot_bytes(&false_redaction_count),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("redacted counts")
    ));

    let mut unwidened_plaintext = saved.document.clone();
    let WorkGraphSnapshotMemoryState::Active {
        body, sensitivity, ..
    } = &mut unwidened_plaintext.body.memories[0].state
    else {
        panic!("restricted fixture must be active");
    };
    assert_eq!(*sensitivity, Sensitivity::Restricted);
    *body = WorkGraphSnapshotText::Present {
        value: "restricted body".into(),
    };
    unwidened_plaintext.body.summary.redacted.memories = 0;
    rebind_snapshot_body(&mut unwidened_plaintext);
    assert!(matches!(
        refuse(
            "unwidened-restricted-plaintext",
            &snapshot_bytes(&unwidened_plaintext),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("requires a widened snapshot")
    ));

    let mut invalid_gate = saved.document.clone();
    let WorkGraphSnapshotRecordPayload::Native { history } =
        &mut invalid_gate.body.records[0].payload
    else {
        panic!("source save must contain a native history layer");
    };
    history.notes.push(crate::WorkGraphSnapshotNote {
        evidence_kind: crate::WorkEvidenceKind::Generic,
        summary: "invalid unnormalized gate".into(),
        refs: Vec::new(),
        gate: Some(crate::WorkGraphSnapshotGate {
            name: " Cargo Test ".into(),
            passed: true,
            failed: Vec::new(),
            evidence_ref: None,
        }),
        actor: actor("gate-session"),
        recorded_at: at(2),
    });
    rebind_snapshot_body(&mut invalid_gate);
    assert!(matches!(
        refuse("invalid-gate", &snapshot_bytes(&invalid_gate), &project),
        StoreError::InvalidGraphSnapshot(message) if message.contains("gate fields")
    ));

    let mut unknown_event = saved.document.clone();
    let WorkGraphSnapshotRecordPayload::Native { history } =
        &mut unknown_event.body.records[0].payload
    else {
        panic!("source save must contain a native history layer");
    };
    history.events[0].kind = "future_transition".into();
    rebind_snapshot_body(&mut unknown_event);
    assert!(matches!(
        refuse(
            "unknown-event",
            &snapshot_bytes(&unknown_event),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("history event")
    ));

    let mut different_build = saved.document.clone();
    let different_fingerprint = ObjectHash::from_stored("0".repeat(64)).expect("valid hash");
    different_build.body.summary.format_fingerprint = different_fingerprint.clone();
    different_build.manifest.summary.format_fingerprint = different_fingerprint;
    assert!(matches!(
        refuse(
            "different-build",
            &snapshot_bytes(&different_build),
            &project
        ),
        StoreError::GraphDifferentBuild
    ));

    let mut future_document =
        serde_json::to_value(&different_build).expect("future snapshot JSON value");
    future_document["body"]
        .as_object_mut()
        .expect("future snapshot body")
        .insert("future_member".into(), serde_json::json!(true));
    assert!(matches!(
        refuse(
            "different-build-with-future-member",
            &serde_json::to_vec(&future_document).expect("future snapshot bytes"),
            &project
        ),
        StoreError::GraphDifferentBuild
    ));

    let mut unknown_member = serde_json::to_value(&saved.document).expect("snapshot JSON value");
    unknown_member["body"]
        .as_object_mut()
        .expect("snapshot body object")
        .insert("unexpected".into(), serde_json::json!(true));
    assert!(matches!(
        refuse(
            "unknown-member",
            &serde_json::to_vec(&unknown_member).expect("unknown-member bytes"),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("unknown field")
    ));

    let mut unknown_actor_member =
        serde_json::to_value(&saved.document).expect("snapshot JSON value");
    unknown_actor_member["body"]["records"][0]["history"]["events"][0]["actor"]
        .as_object_mut()
        .expect("history actor object")
        .insert("unexpected".into(), serde_json::json!(true));
    let changed_body = CanonicalObject::freeze(&unknown_actor_member["body"])
        .expect("freeze body with nested unknown member")
        .hash()
        .to_string();
    unknown_actor_member["manifest"]["body_sha256"] = serde_json::json!(changed_body);
    assert!(matches!(
        refuse(
            "unknown-actor-member",
            &serde_json::to_vec(&unknown_actor_member).expect("unknown actor bytes"),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("not preserved")
    ));

    let other_project = ProjectId("another-project".into());
    assert!(matches!(
        refuse(
            "project-mismatch",
            &snapshot_bytes(&saved.document),
            &other_project
        ),
        StoreError::GraphProjectMismatch { .. }
    ));

    let mut nonempty =
        SqliteStore::open(directory.path().join("nonempty.db")).expect("nonempty store");
    create_root(&mut nonempty, &project, "Existing root", "existing-root");
    assert!(matches!(
        nonempty.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&saved.document),
            false,
            at(4),
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::GraphDestinationNotEmpty)
    ));
    assert_eq!(
        nonempty
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row
                .get::<_, i64>(0))
            .expect("count preserved work"),
        1
    );
    assert!(matches!(
        nonempty.inspect_work(root.work_id, at(5)),
        Err(StoreError::WorkNotFound(_))
    ));

    let mut memory_nonempty =
        SqliteStore::open(directory.path().join("memory-nonempty.db")).expect("memory store");
    memory_nonempty
        .capture_note(
            &NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: None,
                prose: "existing unkeyed project observation".into(),
                visibility: NoteVisibility::Shared,
                kind: Some(MemoryKind::Episode),
                authority: Some(Authority::Soft),
                sensitivity: Some(Sensitivity::Internal),
                title: Some("Existing observation".into()),
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("memory-session"),
                idempotency_key: "existing-observation".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture existing memory");
    assert!(matches!(
        memory_nonempty.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&saved.document),
            false,
            at(4),
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::GraphDestinationNotEmpty)
    ));
}
