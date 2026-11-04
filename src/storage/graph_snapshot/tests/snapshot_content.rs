use super::*;

// A pinned acceptance-evaluation mode is planning state: the snapshot carries
// it and load restores it verbatim, so a transfer never widens which
// evaluator may accept the task.
#[test]
fn snapshot_carries_the_pinned_evaluation_mode() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-evaluation-mode".into());
    let mut source =
        SqliteStore::open(directory.path().join("pinned-source.db")).expect("source store");
    let pinned = source
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: Some(crate::AcceptanceEvaluationMode::IndependentSession),
                external_ref: None,
                notes: Vec::new(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Pinned to an independent evaluator".into(),
                outcome: "only an independent session may accept this".into(),
                acceptance: vec!["the independent evaluator passes it".into()],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor("planner-session"),
                idempotency_key: "create-pinned".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create pinned root");
    let unpinned = create_root(&mut source, &project, "Unpinned", "create-unpinned");
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
    let exported_mode = |work_id: crate::WorkId| {
        saved
            .document
            .body
            .items
            .iter()
            .find(|item| item.work_id == work_id)
            .expect("exported item")
            .evaluation_mode
    };
    assert_eq!(
        exported_mode(pinned.work_id),
        Some(crate::AcceptanceEvaluationMode::IndependentSession)
    );
    assert_eq!(exported_mode(unpinned.work_id), None);
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("loader"),
            &snapshot_bytes(&saved.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load");
    assert_eq!(
        destination
            .get_work_item(pinned.work_id)
            .expect("restored pinned item")
            .evaluation_mode,
        Some(crate::AcceptanceEvaluationMode::IndependentSession)
    );
    assert_eq!(
        destination
            .get_work_item(unpinned.work_id)
            .expect("restored unpinned item")
            .evaluation_mode,
        None
    );
    assert!(destination.verify_all().expect("verify").is_healthy());
}

// Criteria keep the order typed: a snapshot saves and loads a list that does
// not sort, and a list an earlier build stored sorted, each unchanged. Only a
// repeated criterion is refused.
#[test]
fn snapshot_keeps_the_typed_order_of_acceptance() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-typed-acceptance".into());
    let mut source =
        SqliteStore::open(directory.path().join("typed-source.db")).expect("source store");
    let mut create = |acceptance: &[&str], key: &str| {
        let mut request = CreateWorkRequest {
            acceptance: acceptance.iter().map(|text| (*text).to_owned()).collect(),
            ..root_create_request(&project, key)
        };
        request.title = key.into();
        source
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("create root")
    };
    let typed = create(&["zeta holds", "alpha holds"], "typed-order");
    let sorted = create(&["alpha holds", "zeta holds"], "sorted-order");
    assert_eq!(typed.acceptance, vec!["zeta holds", "alpha holds"]);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("saver"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save a typed-order list");
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("loader"),
            &snapshot_bytes(&saved.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load a typed-order list");
    for original in [&typed, &sorted] {
        assert_eq!(
            destination
                .get_work_item(original.work_id)
                .expect("restored item")
                .acceptance,
            original.acceptance
        );
    }

    let mut repeated = saved.document.clone();
    let item = repeated
        .body
        .items
        .iter_mut()
        .find(|item| item.work_id == typed.work_id)
        .expect("exported item");
    item.acceptance.push("zeta holds".into());
    rebind_snapshot_body(&mut repeated);
    let refused = SqliteStore::open_in_memory()
        .expect("second destination")
        .load_work_graph_snapshot(
            &project,
            &actor("loader"),
            &snapshot_bytes(&repeated),
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect_err("a repeated criterion");
    assert!(
        refused
            .to_string()
            .contains("acceptance repeats a criterion"),
        "{refused}"
    );
}

#[test]
fn initial_note_order_survives_snapshot_recreation() {
    let mut source = SqliteStore::open_in_memory().expect("source");
    let project = ProjectId("initial-note-order".into());
    let notes: Vec<String> = (0..16)
        .map(|index| format!("Initial finding {index}"))
        .collect();
    let item = source
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: notes.clone(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Ordered initial observations".into(),
                outcome: "Preserve submission order".into(),
                acceptance: vec!["Round trip preserves notes".into()],
                kind: WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor("creator"),
                idempotency_key: "initial-note-order".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create with notes");
    let (_, observations) = source
        .work_observation_tail(item.work_id, notes.len())
        .expect("observations");
    assert!(
        observations
            .iter()
            .all(|(_, note)| note.created_at == at(1))
    );
    let mut hash_order = observations.clone();
    hash_order.sort_by(|left, right| left.0.cmp(&right.0));
    assert_ne!(
        hash_order, observations,
        "fixture must distinguish hash order from submission order"
    );
    let read_notes = |store: &SqliteStore| {
        store
            .work_read_snapshot(|store| {
                store
                    .work_record_index(
                        &project,
                        item.work_id,
                        crate::storage::WorkRecordKind::Notes,
                    )?
                    .iter()
                    .map(|entry| {
                        match store.work_record_content(&project, item.work_id, entry)? {
                            crate::storage::WorkRecordContent::Note(note) => Ok(note.summary),
                            _ => panic!("note index returned another family"),
                        }
                    })
                    .collect::<Result<Vec<_>, StoreError>>()
            })
            .expect("canonical record notes")
    };
    assert_eq!(read_notes(&source), notes);
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
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("loader"),
            &snapshot_bytes(&saved.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load");
    assert_eq!(read_notes(&destination), notes);
    assert!(destination.verify_all().expect("verify").is_healthy());
}

#[test]
fn load_uses_body_semantics_and_exact_typed_source_bytes() {
    type SourceMutation = (&'static str, fn(&mut serde_json::Value));

    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-source-validation".into());
    let mut source =
        SqliteStore::open(directory.path().join("source-validation.db")).expect("source store");
    let (imported, original_source) = create_imported_root(&mut source, &project);
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
    assert_eq!(saved.document.body.sources.len(), 1);
    assert_eq!(
        saved.document.body.sources[0].hash,
        imported.source_snapshot_id.unwrap()
    );
    assert_eq!(
        saved.document.body.sources[0].canonical_json,
        serde_json::to_value(original_source).unwrap()
    );

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
        let stored_id = source.hash.clone();
        mutate(&mut source.canonical_json);
        // The source snapshot keeps the id it was stored under, and the
        // item still names it; an id never follows content.
        assert_eq!(source.hash, stored_id);
        assert_eq!(
            document.body.items[0].source_snapshot_id.as_ref(),
            Some(&stored_id)
        );
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
