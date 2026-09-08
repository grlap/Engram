use super::*;

#[test]
fn verbose_successor_rows_preserve_every_core_field_without_serializing_advisory_state() {
    let (_directory, verbs, _path, _project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Original", Some(&parent), false, 1);
    let successor = add(&verbs, "Successor", Some(&parent), false, 2);
    supersede(&verbs, &child, &successor, 3);
    let page = verbs
        .service
        .work_catalog_page(
            &crate::WorkCatalogQuery {
                child_requirement: Some(crate::ChildRequirement::Required),
                limit: 10,
                ..crate::WorkCatalogQuery::default()
            },
            Some(&parent),
            None,
            at(4),
        )
        .unwrap();
    let receipt = verbs
        .ls(
            &LsInput {
                under: Some(parent),
                required: true,
                all: true,
                verbose: true,
                limit: Some(10),
                ..LsInput::default()
            },
            at(4),
        )
        .unwrap();
    let rows = receipt.value["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(page.items.len(), 2);
    for (row, source) in rows.iter().zip(&page.items) {
        let core = serde_json::to_value(source).unwrap();
        assert!(core["work"].get("child_resolution").is_none());
        assert!(core["work"].get("required_child_successor").is_none());
        let mut rendered = row.clone();
        let overlay = rendered["work"]
            .as_object_mut()
            .unwrap()
            .remove("child_resolution");
        assert_eq!(overlay.is_some(), source.work.short_ref == child);
        assert_eq!(
            source.work.required_child_successor.is_some(),
            overlay.is_some()
        );
        assert_eq!(rendered, core);
    }
}

#[test]
fn restored_parent_without_a_run_agrees_with_successor_completion() {
    let (directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Original", Some(&parent), false, 1);
    let successor = add(&verbs, "Successor", Some(&parent), false, 2);
    let mut source = SqliteStore::open(&path).unwrap();
    let original_parent = source.resolve_work_ref(&project, &parent).unwrap();
    let actor = original_parent.created_by.clone();
    let document = source
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
        .document;
    let restored_path = directory.path().join("restored.db");
    let mut store = SqliteStore::open(&restored_path).unwrap();
    store
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).unwrap(),
            false,
            at(4),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let reader = AgentVerbs::new(
        restored_path,
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    // Bootstrap only the two children; the restored parent has no native run.
    finish(&reader, &successor, 5);
    supersede(&reader, &child, &successor, 7);
    assert!(
        store
            .latest_work_run(original_parent.work_id)
            .unwrap()
            .is_none()
    );
    assert_resolution(&reader, &parent, &child, &successor, true, 8);
    assert_eq!(
        reader.show(&parent, at(8)).unwrap().value["child_obligations"]["required_owed"]["count"],
        0
    );
    // Inspection must not create a run or grant the unclaimed parent authority.
    assert!(
        reader
            .done(
                DoneInput {
                    links: Vec::new(),
                    link_basis: None,
                    work_ref: Some(parent.clone()),
                    summary: Some("Not claimed".into()),
                    note: None
                },
                at(9)
            )
            .is_err()
    );
    assert!(
        store
            .latest_work_run(original_parent.work_id)
            .unwrap()
            .is_none()
    );
    reader
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(10),
        )
        .unwrap();
    assert_resolution(&reader, &parent, &child, &successor, true, 11);
    let completed = reader
        .done(
            DoneInput {
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(parent.clone()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(12),
        )
        .unwrap();
    assert!(!completed.owed);
    assert_eq!(
        reader.show(&parent, at(13)).unwrap().value["child_obligations"]["required_owed"]["count"],
        0
    );
    let hash = store
        .latest_work_run(original_parent.work_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let seal: crate::CompletionSeal = store.get(&hash).unwrap().unwrap();
    assert_eq!(seal.required_child_resolutions.len(), 1);
    assert!(seal.required_child_waivers.is_empty());
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn two_required_children_consolidate_into_one_successor_seal() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let first = add(&verbs, "First requirement", Some(&parent), false, 1);
    let second = add(&verbs, "Second requirement", Some(&parent), false, 2);
    let successor = add(&verbs, "Consolidated delivery", Some(&parent), false, 3);
    finish(&verbs, &successor, 4);
    supersede(&verbs, &first, &successor, 6);
    supersede(&verbs, &second, &successor, 7);
    for child in [&first, &second] {
        assert_resolution(&verbs, &parent, child, &successor, true, 8);
    }
    assert_eq!(
        verbs.show(&parent, at(8)).unwrap().value["child_obligations"]["required_owed"]["count"],
        0
    );
    finish(&verbs, &parent, 9);
    let store = SqliteStore::open(&path).unwrap();
    let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let successor_id = store
        .resolve_work_ref(&project, &successor)
        .unwrap()
        .work_id;
    let seal_hash = store
        .latest_work_run(parent_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let seal: crate::CompletionSeal = store.get(&seal_hash).unwrap().unwrap();
    let successor_hash = store
        .latest_work_run(successor_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    assert_eq!(seal.required_child_seals, vec![successor_hash.clone()]);
    assert_eq!(seal.required_child_resolutions.len(), 2);
    assert!(seal.required_child_waivers.is_empty());
    let mut originals = std::collections::HashSet::new();
    let mut supersessions = std::collections::HashSet::new();
    for resolution in &seal.required_child_resolutions {
        let crate::RequiredChildResolution::ResolvedBySuccessor {
            work_id,
            successor,
            successor_seal,
            supersession,
            ..
        } = resolution;
        assert_eq!(*successor, successor_id);
        assert_eq!(*successor_seal, successor_hash);
        assert!(originals.insert(*work_id));
        assert!(supersessions.insert(supersession.clone()));
    }
    assert_eq!(
        originals,
        [&first, &second]
            .map(|reference| store.resolve_work_ref(&project, reference).unwrap().work_id)
            .into_iter()
            .collect()
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn done_refusal_keeps_the_show_successor_reason_and_core_shape() {
    for optional in [false, true] {
        let (_directory, verbs, path, project) = fixture();
        let parent = add(&verbs, "Parent", None, false, 0);
        let child = add(&verbs, "Original", Some(&parent), false, 1);
        let successor = add(&verbs, "Successor", Some(&parent), optional, 2);
        supersede(&verbs, &child, &successor, 3);
        if optional {
            finish(&verbs, &successor, 4);
        }
        let shown = verbs.show(&child, at(6)).unwrap();
        let resolution = shown.value["status"]["work"]["child_resolution"].clone();
        let line = format!(
            "successor {} ({}): {}",
            successor,
            resolution["lifecycle"].as_str().unwrap(),
            resolution["reason"].as_str().unwrap()
        );
        assert!(shown.text().contains(&line));
        verbs
            .claim(
                ClaimInput {
                    work_ref: parent.clone(),
                    ttl_seconds: None,
                    recover: None,
                },
                at(7),
            )
            .unwrap();
        let refused = verbs
            .done(
                DoneInput {
                    links: Vec::new(),
                    link_basis: None,
                    work_ref: Some(parent.clone()),
                    summary: Some("Still owed".into()),
                    note: None,
                },
                at(8),
            )
            .unwrap();
        assert!(refused.owed);
        assert_eq!(
            refused.value["recovery"]["item"]["child_resolution"],
            resolution
        );
        assert!(refused.text().contains(&line));
        assert!(
            refused
                .reminders
                .iter()
                .any(|reminder| reminder.contains(&line))
        );
        assert_eq!(refused.value["code"], "required_child_unsealed");
        assert_eq!(refused.value["recovery"]["item"]["ref"], child);
        assert_eq!(refused.value["recovery"]["item"]["state"], "superseded");
        assert_eq!(
            refused.next,
            vec![format!(
                "engram work update {parent} --waive {child} --reason \"account for disposed required child\""
            )]
        );
        assert!(refused.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&refused.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        let service = LocalWorkService::new(
            path.clone(),
            project,
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        let core = service
            .work_complete_on(
                Some(&parent),
                crate::work_service::WorkCompleteInput {
                    links: Vec::new(),
                    link_basis: None,
                    capture: None,
                    evidence: Vec::new(),
                    acceptance: None,
                    note: None,
                    idempotency_key: String::new(),
                },
                at(9),
            )
            .unwrap();
        assert!(
            serde_json::to_value(core).unwrap()["recovery"]["item"]
                .get("child_resolution")
                .is_none()
        );
        assert!(
            SqliteStore::open(&path)
                .unwrap()
                .verify_all()
                .unwrap()
                .is_healthy()
        );
    }
}
