use super::*;

pub(super) fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::seconds(second)
}

pub(super) fn actor(session: &str) -> ActorContext {
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

pub(super) fn root_create_request(project: &ProjectId, key: &str) -> CreateWorkRequest {
    CreateWorkRequest {
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        external_ref: None,
        notes: Vec::new(),
        project_id: project.clone(),
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        title: key.into(),
        outcome: "the snapshot preserves planning state".into(),
        acceptance: Vec::new(),
        kind: WorkItemKind::Feature,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        origin: WorkOrigin::Local,
        source_snapshot_id: None,
        actor: actor("planner-session"),
        idempotency_key: key.into(),
        created_at: at(1),
    }
}

pub(super) fn create_root(
    store: &mut SqliteStore,
    project: &ProjectId,
    title: &str,
    key: &str,
) -> crate::WorkItem {
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

pub(super) fn create_imported_root(
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
            .key()
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
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
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
                source_snapshot_id: Some(object.key().clone()),
                actor: actor("import-session"),
                idempotency_key: "imported-snapshot-root".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create imported root");
    (item, snapshot)
}

pub(super) fn classified_project_memory(
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
        retiring_target: None,
        retiring_target_cleared: false,
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

pub(super) fn insert_classified_project_memory(
    store: &mut SqliteStore,
    project: &ProjectId,
    key: &str,
    body: &str,
    sensitivity: Sensitivity,
    created_at: DateTime<Utc>,
) {
    let version = classified_project_memory(project, key, body, sensitivity, created_at);
    insert_project_memory_version(store, project, &version);
}

pub(super) fn insert_project_memory_version(
    store: &mut SqliteStore,
    project: &ProjectId,
    version: &MemoryVersion,
) {
    let created_at = version.created_at;
    let sensitivity = version.sensitivity;
    let version_object = CanonicalObject::freeze(version).expect("freeze memory version");
    let assertion = crate::domain::MemoryAssertionEvent {
        schema_version: crate::schema::SCHEMA_VERSION,
        memory_id: version.memory_id,
        version: version_object.key().clone(),
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
                 memory_id, version_id, assertion_id, schema_version,
                 status, scope_kind, project_id, task_id, work_id, agent_id,
                 memory_kind, authority, delivery, sensitivity, title, body,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'active', 'project', ?5, NULL, NULL, NULL,
                 'episode', 'soft', 'on_demand', ?6, ?7, ?8, ?9
             )",
            rusqlite::params![
                version.memory_id.0.to_string(),
                version_object.key().as_str(),
                assertion_object.key().as_str(),
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
            "INSERT INTO object_fts (object_id, title, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![version_object.key().as_str(), version.title, version.body],
        )
        .expect("insert memory search projection");
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

pub(super) fn snapshot_bytes(document: &WorkGraphSnapshotDocument) -> Vec<u8> {
    serde_json::to_vec_pretty(document).expect("serialize snapshot")
}

pub(super) fn rebind_snapshot_body(document: &mut WorkGraphSnapshotDocument) {
    document.manifest.summary = document.body.summary.clone();
    document.manifest.body_sha256 = CanonicalObject::freeze(&document.body)
        .expect("freeze changed snapshot body")
        .key()
        .clone();
}

pub(super) fn refuse_save_before_effects(live: &crate::SessionId) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("snapshot-admit".into());
    create_root(
        &mut store,
        &project,
        "Admission root",
        "snapshot-admit-root",
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .save_work_graph_snapshot(
            &project,
            &actor(&live.0),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect_err("oversized save actor");
    assert_oversized_session_refusal(&error, live);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

pub(super) fn refuse_load_before_effects(live: &crate::SessionId) {
    let mut source = SqliteStore::open_in_memory().expect("source");
    let project = ProjectId("snapshot-admit".into());
    create_root(
        &mut source,
        &project,
        "Admission root",
        "snapshot-admit-root",
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("saver"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save");
    let bytes = snapshot_bytes(&saved.document);
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    let before =
        crate::storage::test_database_shape_snapshot(&destination.connection).expect("before");
    let error = destination
        .load_work_graph_snapshot(
            &project,
            &actor(&live.0),
            &bytes,
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect_err("oversized load actor");
    assert_oversized_session_refusal(&error, live);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&destination.connection).expect("after"),
        before
    );
}
