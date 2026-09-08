use super::*;

#[test]
fn hygiene_clear_absent_external_reference_is_an_audited_revision() {
    let (_dir, verbs, path, project) = fixture();
    let created = verbs
        .add(
            AddInput {
                title: "No external linkage".into(),
                assignee: Some("agent".into()),
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = created.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = SqliteStore::open(&path).unwrap();
    let before = store.resolve_work_ref(&project, &reference).unwrap();
    assert!(before.external_ref.is_none());
    let events_before = store.work_event_tail(before.work_id, 20).unwrap();
    let receipt = verbs
        .update(
            serde_json::from_value(json!({"work_ref": reference,
            "action": {"action":"revise", "clear_external": true}}))
            .unwrap(),
            at(1),
        )
        .unwrap();
    assert!(receipt.text().contains("external reference"));
    let after = store.resolve_work_ref(&project, &reference).unwrap();
    let mut expected = before.clone();
    expected.revision += 1;
    expected.updated_at = at(1);
    assert_eq!(after, expected);
    let events_after = store.work_event_tail(before.work_id, 20).unwrap();
    assert_eq!(events_after.len(), events_before.len() + 1);
    assert_eq!(
        &events_after[..events_before.len()],
        events_before.as_slice()
    );
    let event = store
        .get::<crate::WorkEvent>(&events_after.last().unwrap().object_hash)
        .unwrap()
        .unwrap();
    assert_eq!(event.work, after);
    assert!(matches!(
        event.transition,
        crate::WorkTransition::Revised { .. }
    ));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn hygiene_status_labels_distinguish_actor_and_session_on_show_and_next() {
    let mut labels = Vec::new();
    for (actor, session) in [
        ("agent", "agent"),
        ("agent", "replacement"),
        ("private-peer-principal", "peer-session"),
    ] {
        let (_dir, reader, path, project) = fixture();
        let reference = assigned(&reader, "Assigned duty", "agent", 0);
        let expected = reader
            .service
            .display_identity()
            .author(actor, Some(&SessionId(session.into())));
        let writer = AgentVerbs::new(path, project, actor.into(), SessionId(session.into()), None);
        writer
            .claim(
                ClaimInput {
                    work_ref: reference.clone(),
                    ttl_seconds: Some(300),
                    recover: None,
                },
                at(1),
            )
            .unwrap();
        capture_status(&writer, &reference, "Waiting for review", 2);
        let shown = reader.show(&reference, at(3)).unwrap();
        assert_eq!(shown.value["current_status"]["by"], expected);
        labels.push(shown.value["current_status"]["by"].clone());
        assert!(shown.text().contains(&format!("; {expected}]")));
        assert!(
            !shown.value["current_status"]
                .to_string()
                .contains("private-peer-principal")
        );
        for verbose in [false, true] {
            let next = reader
                .next(
                    &NextInput {
                        verbose,
                        ..NextInput::default()
                    },
                    at(4),
                )
                .unwrap();
            let row = next_status_row(&next, &reference, verbose);
            assert_eq!(row["current_status"]["by"], expected);
            assert!(next.text().contains(&format!("; {expected}]")));
            assert!(
                !row["current_status"]
                    .to_string()
                    .contains("private-peer-principal")
            );
        }
    }
    assert_eq!(labels[0], "you");
    assert_ne!(labels[1], "you");
    assert_ne!(labels[2], "you");
    assert_ne!(labels[0], labels[1]);
    assert_ne!(labels[0], labels[2]);
    assert_ne!(labels[1], labels[2]);
}

#[test]
fn hygiene_external_clear_is_audited_searchable_and_snapshot_stable() {
    let (dir, verbs, path, project) = fixture();
    let reference = assigned(&verbs, "Link removal", "agent", 0);
    let revise = |external: Option<&str>, clear: bool, now| {
        verbs.update(
            serde_json::from_value(json!({"work_ref": reference,
            "action": {"action":"revise", "external": external, "clear_external": clear}}))
            .unwrap(),
            at(now),
        )
    };
    revise(Some("opaque:remove-me"), false, 1).unwrap();
    let mut store = SqliteStore::open(&path).unwrap();
    let before = store.resolve_work_ref(&project, &reference).unwrap();
    let search = |now| {
        verbs
            .ls(
                &LsInput {
                    search: Some("opaque:remove-me".into()),
                    ..LsInput::default()
                },
                at(now),
            )
            .unwrap()
    };
    assert_eq!(search(2).value["total"], 1);
    let error = revise(Some("opaque:other"), true, 3).unwrap_err();
    assert!(
        matches!(error.error, StoreError::InvalidWork(ref message) if message == "cannot set and clear the external reference together")
    );
    assert!(matches!(
        revise(Some("  "), false, 4).unwrap_err().error,
        StoreError::InvalidWork(_)
    ));
    assert_eq!(
        store.resolve_work_ref(&project, &reference).unwrap(),
        before
    );
    let cleared = revise(None, true, 5).unwrap();
    assert!(cleared.text().contains("external reference"));
    let after = store.resolve_work_ref(&project, &reference).unwrap();
    assert_eq!(after.revision, before.revision + 1);
    assert!(after.external_ref.is_none());
    assert_eq!(after.acceptance, before.acceptance);
    assert_eq!(search(6).value["total"], 0);
    let shown = verbs.show(&reference, at(6)).unwrap();
    assert!(shown.value.get("external_ref").is_none());
    assert!(!shown.text().contains("external:"));
    let next = verbs.next(&NextInput::default(), at(6)).unwrap();
    let row = next.value["assigned"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == reference)
        .unwrap();
    assert!(row.get("external_ref").is_none());
    let history = verbs
        .show_records(
            &reference,
            &ShowInput {
                history: true,
                ..ShowInput::default()
            },
            at(6),
        )
        .unwrap();
    assert!(history.text().contains("external reference"));
    let saved = store
        .save_work_graph_snapshot(
            &project,
            &after.created_by,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut restored = SqliteStore::open(dir.path().join("restored.db")).unwrap();
    restored
        .load_work_graph_snapshot(
            &project,
            &after.created_by,
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(8),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(
        restored
            .resolve_work_ref(&project, &reference)
            .unwrap()
            .external_ref
            .is_none()
    );
    assert!(restored.verify_all().unwrap().is_healthy());
    // An omitted false flag leaves canonical patch bytes unchanged.
    let empty_revision = crate::WorkRevisionPatch::default();
    let value = serde_json::to_value(&empty_revision).unwrap();
    assert!(value.get("clear_external").is_none());
    let mut explicit_false = value.clone();
    explicit_false["clear_external"] = json!(false);
    let decoded: crate::WorkRevisionPatch = serde_json::from_value(explicit_false).unwrap();
    assert_eq!(
        crate::CanonicalObject::freeze(&empty_revision)
            .unwrap()
            .bytes(),
        crate::CanonicalObject::freeze(&decoded).unwrap().bytes()
    );
}
