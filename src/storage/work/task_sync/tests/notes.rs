use super::*;
use crate::domain::{RecordWorkNoteRequest, RecordWorkObservationRequest, SessionId};
use crate::task_sync::notes::{SharedNote, SharedNoteKind};

mod exclusions;
mod historical;
mod limits;
mod paths;
mod profiles;
mod repeat;

fn shared_note(body: &str) -> SharedNote {
    SharedNote {
        id: CanonicalObject::mint(&body).unwrap().key().clone(),
        kind: SharedNoteKind::Status,
        body: body.into(),
        author: SharedActor::from_actor(&actor("greg/codex")),
        recorded_at: at(1),
        origin_machine: "host-A".into(),
    }
}

fn note_candidate() -> TaskTree {
    let mut tree = candidate();
    tree.project.notes_included = true;
    tree.tasks[0].notes = vec![shared_note("Reported progress")];
    tree
}

#[test]
fn task_sync_notes_native_holder_peer_and_status_round_trip_without_authority() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("native-notes", "notes", 0);
    request.notes = vec![
        "Same body Ω\r\nnext line".into(),
        "Same body Ω\r\nnext line".into(),
    ];
    let work = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let peer = source
        .record_work_observation(
            &RecordWorkObservationRequest {
                status: true,
                project_id: work.project_id.clone(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                session_id: SessionId("peer".into()),
                summary: "Peer status".into(),
                refs: vec![],
                actor: crate::domain::non_holder_note_actor(actor("peer")),
                idempotency_key: "peer-note".into(),
                recorded_at: at(1),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = claim(&mut source, &work, "holder", "claim", 2, 100);
    let work = source.get_work_item(work.work_id).unwrap();
    let holder = source
        .record_work_note(
            &RecordWorkNoteRequest {
                status: true,
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "Holder status".into(),
                refs: vec![],
                actor: actor("holder"),
                idempotency_key: "holder-note".into(),
                recorded_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let planning = source
        .export_task_tree(&work.project_id)
        .unwrap()
        .documents()
        .unwrap();
    for bytes in planning.values() {
        let document: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(document.get("notes").is_none());
        assert!(document.get("notes_included").is_none());
    }
    let before = state(&source);
    origin::ENUMERATIONS.with(|count| count.set(0));
    let exported = source
        .export_task_tree_with_notes(&work.project_id, "host-A")
        .unwrap();
    assert_eq!(state(&source), before);
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert!(exported.omissions.counts.is_empty());
    let notes = &exported.tree.tasks[0].notes;
    assert_eq!(notes.len(), 4);
    for note in notes {
        let encoded_body = serde_json::to_string(&note.body).unwrap();
        for bytes in planning.values() {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains(&encoded_body));
            assert!(!text.contains(note.id.as_str()));
        }
    }
    assert_eq!(
        notes.iter().filter(|n| n.body == request.notes[0]).count(),
        2
    );
    for (id, author, time) in [
        (&peer.evidence, "peer", at(1)),
        (&holder.evidence, "holder", at(3)),
    ] {
        let note = notes.iter().find(|n| &n.id == id).unwrap();
        assert_eq!(note.kind, SharedNoteKind::Status);
        assert_eq!(note.author.actor_id, author);
        assert_eq!(note.recorded_at, time);
    }
    let bytes = exported.tree.documents().unwrap();
    let decoded = TaskTree::from_documents(&bytes).unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &decoded);
    let reexport = target
        .export_task_tree_with_notes(&work.project_id, "host-B")
        .unwrap();
    assert_eq!(reexport.tree.documents().unwrap(), bytes);
    assert_eq!(reexport.omissions.counts.len(), 0);
    assert_eq!(
        target
            .export_task_tree(&work.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        planning
    );
    let before = state(&target);
    assert!(!import(&mut target, &decoded).changed);
    assert_eq!(state(&target), before);
    let history = query::restored_records_for_item(&target.connection, work.work_id).unwrap();
    assert_eq!(history[0].history.notes.len(), 4);
    for note in &history[0].history.notes {
        assert!(note.actor.is_task_sync_reported());
        assert!(crate::domain::status_note_role(&note.actor).is_none());
        assert!(note.actor.run_id.is_none() && note.actor.provenance_chain.is_empty());
        assert!(note.gate.is_none() && note.refs.is_empty());
    }
    for table in [
        "work_run_evidence",
        "work_restored_evidence",
        "work_observations",
        "work_runs",
        "work_claims",
        "work_session_state",
        "control_sessions",
    ] {
        assert_eq!(
            target
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert!(target.verify_all().unwrap().is_healthy());
    target
        .save_work_graph_snapshot(
            &work.project_id,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn task_sync_notes_known_id_changes_and_disabled_removal_refuse_without_writes() {
    let tree = note_candidate();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &tree);
    for field in [
        "body", "kind", "author", "time", "machine", "id", "omitted", "disabled",
    ] {
        let mut changed = tree.clone();
        let note = &mut changed.tasks[0].notes[0];
        match field {
            "body" => note.body = "Different report".into(),
            "kind" => note.kind = SharedNoteKind::Generic,
            "author" => note.author.actor_id = "other-author".into(),
            "time" => note.recorded_at = at(100),
            "machine" => note.origin_machine = "host-B".into(),
            "id" => note.id = shared_note("other").id,
            "omitted" => changed.tasks[0].notes.clear(),
            "disabled" => {
                changed.project.notes_included = false;
                changed.tasks[0].notes.clear();
            }
            _ => unreachable!(),
        }
        for dry_run in [false, true] {
            let before = state(&target);
            assert!(
                target
                    .import_task_tree(
                        &tree.project.project_id,
                        &actor("importer"),
                        &changed,
                        dry_run,
                        at(100)
                    )
                    .is_err(),
                "{field}"
            );
            assert_eq!(state(&target), before, "{field}");
        }
    }
    let mut foreign = tree.clone();
    foreign.project.project_id = ProjectId("other-project".into());
    for task in &mut foreign.tasks {
        task.notes.clear();
    }
    foreign.tasks[0].notes = tree.tasks[0].notes.clone();
    // Fresh work ids avoid the existing task collision, isolating note identity.
    foreign.tasks = vec![
        foreign
            .tasks
            .iter()
            .find(|t| t.lifecycle == WorkLifecycle::Proposed)
            .unwrap()
            .clone(),
    ];
    foreign.tasks[0].id = WorkId::new();
    foreign.tasks[0].notes = tree.tasks[0].notes.clone();
    let before = state(&target);
    assert!(
        target
            .import_task_tree(
                &foreign.project.project_id,
                &actor("importer"),
                &foreign,
                false,
                at(100)
            )
            .unwrap_err()
            .to_string()
            .contains("known shared note id")
    );
    assert_eq!(state(&target), before);
}

#[test]
fn task_sync_notes_repair_migration_and_legacy_flattening_keep_identity_contract() {
    let fixture = crate::test_support::temp_home().unwrap();
    let database = fixture.path().join("notes.db");
    let tree = note_candidate();
    let mut store = SqliteStore::open(&database).unwrap();
    import(&mut store, &tree);
    let original = tree.documents().unwrap();
    let before_objects = objects(&store);
    let before_items = item_bytes(&store);
    let before_restored = store.connection.prepare("SELECT work_id, generation_index, record_id FROM work_restored_records ORDER BY work_id")
        .unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,String>(2)?))).unwrap()
        .collect::<Result<Vec<_>,_>>().unwrap();
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let mut store = SqliteStore::open(&database).unwrap();
    assert_eq!(objects(&store), before_objects);
    assert_eq!(item_bytes(&store), before_items);
    assert_eq!(store.connection.prepare("SELECT work_id, generation_index, record_id FROM work_restored_records ORDER BY work_id")
        .unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,String>(2)?))).unwrap()
        .collect::<Result<Vec<_>,_>>().unwrap(), before_restored);
    assert_eq!(
        store
            .export_task_tree_with_notes(&tree.project.project_id, "host-B")
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        original
    );
    let snapshot = store
        .save_work_graph_snapshot(
            &tree.project.project_id,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(20),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut flattened = SqliteStore::open_in_memory().unwrap();
    flattened
        .load_work_graph_snapshot(
            &tree.project.project_id,
            &actor("loader"),
            &serde_json::to_vec(&snapshot.document).unwrap(),
            false,
            at(21),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let export = flattened
        .export_task_tree_with_notes(&tree.project.project_id, "host-C")
        .unwrap();
    assert_eq!(
        export.omissions.counts["legacy note without original id"],
        1
    );
    assert!(export.tree.tasks.iter().all(|t| t.notes.is_empty()));
    let migration = fixture.path().join("rows.json");
    crate::storage::migration::export_json(&database, &migration).unwrap();
    let target = fixture.path().join("migrated.db");
    crate::storage::migration::import_json(&migration, &target).unwrap();
    let migrated = SqliteStore::open(&target).unwrap();
    assert_eq!(
        migrated
            .export_task_tree_with_notes(&tree.project.project_id, "host-C")
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        original
    );
    assert!(migrated.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_notes_late_findings_keep_original_ids_and_exclude_references() {
    use crate::domain::{RecordRestoredWorkEvidenceRequest, RestoredWorkEvidenceInput};
    let tree = note_candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let completed = tree
        .tasks
        .iter()
        .find(|t| t.lifecycle == WorkLifecycle::Completed)
        .unwrap();
    let mut original = None;
    let mut late_actor = actor("late-author");
    late_actor
        .provenance_chain
        .push(crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::DerivedFrom,
            source: crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
            reference: Some(crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
        });
    for (index, refs) in [vec![], vec!["local-evidence".into()]]
        .into_iter()
        .enumerate()
    {
        let item = store.get_work_item(completed.id).unwrap();
        let id = store
            .record_restored_work_evidence(
                &RecordRestoredWorkEvidenceRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    holder: SessionId("late-author".into()),
                    input: RestoredWorkEvidenceInput::Note {
                        status: false,
                        summary: "Late finding".into(),
                        refs,
                    },
                    actor: late_actor.clone(),
                    idempotency_key: format!("late-note-{index}"),
                    recorded_at: at(20 + i64::try_from(index).unwrap()),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        if index == 0 {
            original = Some(id);
        }
    }
    let exported = store
        .export_task_tree_with_notes(&tree.project.project_id, "host-B")
        .unwrap();
    assert_eq!(exported.omissions.counts["references"], 1);
    let note = &exported
        .tree
        .tasks
        .iter()
        .find(|t| t.id == completed.id)
        .unwrap()
        .notes
        .iter()
        .find(|note| Some(&note.id) == original.as_ref())
        .unwrap();
    assert_eq!(Some(&note.id), original.as_ref());
    assert_eq!(note.body, "Late finding");
    assert_eq!(note.origin_machine, "host-B");
    assert_eq!(note.recorded_at, at(20));
    let retained = exported
        .tree
        .tasks
        .iter()
        .flat_map(|t| &t.notes)
        .find(|n| n.id == tree.tasks[0].notes[0].id)
        .unwrap();
    assert_eq!(retained.origin_machine, "host-A");
    assert!(store.verify_all().unwrap().is_healthy());
}
