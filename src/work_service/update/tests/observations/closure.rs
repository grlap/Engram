use super::*;
use crate::domain::{NON_HOLDER_NOTE_REFERENCE, WorkObservationBasis, is_non_holder_note_marker};
use crate::verbs::{AgentVerbs, NoteInput};
use std::sync::Arc;

#[test]
fn phoenix_note_marker_collisions_preserve_authority_and_restored_classification() {
    for (actor, session) in [
        (NON_HOLDER_NOTE_SOURCE, "ordinary-session"),
        ("ordinary-actor", NON_HOLDER_NOTE_REFERENCE),
        (NON_HOLDER_NOTE_SOURCE, NON_HOLDER_NOTE_REFERENCE),
    ] {
        let directory = tempdir().unwrap();
        let database = directory.path().join("source.db");
        let owner = service(&database, "owner");
        let root = proposed_root(
            owner
                .work_propose(root_input("Collision", "root"), at(0))
                .unwrap(),
        );
        let reviewer = Arc::new(LocalWorkService::new(
            database.clone(),
            owner.project_id.clone(),
            actor.into(),
            SessionId(session.into()),
            None,
        ));
        let words = AgentVerbs::with_shared_service(
            reviewer.clone(),
            actor.into(),
            SessionId(session.into()),
        );
        let note = |text: &str| NoteInput {
            work_ref: Some(root.short_ref.clone()),
            text: text.into(),
            refs: Vec::new(),
        };
        let observed = words.note(&note("observation"), at(1)).unwrap();
        assert_eq!(observed.value["non_holder"], true);
        assert!(observed.text().contains("(observation, no run credit)"));
        assert_eq!(
            observed.value["receipt"]["result"],
            observed.value["evidence"]["result"]
        );
        // Later execution events would push this early note outside the
        // four-row restored-history tail, so check its classification now.
        assert_restored_note_kind(
            &reviewer,
            directory.path(),
            &root.short_ref,
            "observation",
            "non_holder_note",
            2,
        );
        claim(&reviewer, &root.short_ref, 2);
        let executed = words.note(&note("execution"), at(3)).unwrap();
        assert!(executed.value.get("non_holder").is_none());
        assert!(!executed.text().contains("no run credit"));
        assert_ne!(
            executed.value["receipt"]["result"],
            executed.value["evidence"]["result"]
        );
        let store = SqliteStore::open(&database).unwrap();
        let (_, observations) = store.work_observation_tail(root.work_id, 8).unwrap();
        assert_eq!(
            observations[0]
                .1
                .actor
                .provenance_chain
                .iter()
                .filter(|link| is_non_holder_note_marker(link))
                .count(),
            1
        );
        assert!(store.verify_all().unwrap().is_healthy());
        assert_restored_note_kind(
            &reviewer,
            directory.path(),
            &root.short_ref,
            "execution",
            "generic",
            4,
        );
    }
}

fn assert_restored_note_kind(
    reviewer: &LocalWorkService,
    directory: &std::path::Path,
    work_ref: &str,
    summary: &str,
    expected: &str,
    second: i64,
) {
    let saved = reviewer
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(second))
        .unwrap();
    let restored = service(&directory.join(format!("restored-{summary}.db")), "reader");
    restored
        .load_work_graph_snapshot(
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(second + 1),
        )
        .unwrap();
    let focus = restored.work_focus(work_ref, at(second + 2)).unwrap();
    let note = focus
        .restored_history
        .items
        .iter()
        .find(|entry| entry.summary == summary)
        .unwrap();
    assert_eq!(note.kind, expected);
}

#[test]
fn phoenix_observations_do_not_displace_selected_execution_evidence() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("work.db");
    let owner = Arc::new(service(&database, "owner"));
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Evidence priority", "root"), at(0))
            .unwrap(),
    );
    claim(&owner, &root.short_ref, 1);
    let words = AgentVerbs::with_shared_service(
        owner.clone(),
        "shared-actor".into(),
        SessionId("owner".into()),
    );
    words
        .gate(
            crate::verbs::GateInput {
                work_ref: Some(root.short_ref.clone()),
                name: "execution-check".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(2),
        )
        .unwrap();
    owner
        .work_note_on(Some(&root.short_ref), "execution evidence", &[], at(3))
        .unwrap();
    let before = owner.work_focus(&root.short_ref, at(4)).unwrap();
    assert_eq!(before.evidence_items.len(), 2);
    for index in 0..=MAX_FOCUS_RELATIONS {
        reviewer
            .work_note_on(
                Some(&root.short_ref),
                &format!("peer observation {index}"),
                &[],
                at(5),
            )
            .unwrap();
    }
    let after = owner.work_focus(&root.short_ref, at(6)).unwrap();
    for selected in &before.evidence_items {
        assert!(
            after
                .evidence_items
                .iter()
                .any(|item| item.evidence == selected.evidence)
        );
    }
    assert_eq!(after.evidence_items.len(), MAX_FOCUS_RELATIONS);
    assert_eq!(after.evidence_count, MAX_FOCUS_RELATIONS + 3);
    let shown = words.show(&root.short_ref, at(7)).unwrap();
    let notes = shown.value["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note["summary"] == "gate execution-check passed")
    );
    assert!(
        notes
            .iter()
            .any(|note| note["summary"] == "execution evidence")
    );
    assert_eq!(
        notes.last().unwrap()["summary"],
        format!("peer observation {MAX_FOCUS_RELATIONS}")
    );
    assert_eq!(shown.value["notes_omitted"], 3);
}

#[test]
fn phoenix_lapsed_holder_note_receipt_explicitly_has_no_run_credit() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("work.db");
    let owner = Arc::new(service(&database, "owner"));
    let root = proposed_root(
        owner
            .work_propose(root_input("Lapsed observation", "root"), at(0))
            .unwrap(),
    );
    claim(&owner, &root.short_ref, 1);
    let before = execution_inventory(&database);
    let words =
        AgentVerbs::with_shared_service(owner, "shared-actor".into(), SessionId("owner".into()));
    let noted = words
        .note(
            &NoteInput {
                work_ref: Some(root.short_ref),
                text: "finding after expiry".into(),
                refs: Vec::new(),
            },
            at(4_000),
        )
        .unwrap();
    assert_eq!(noted.value["non_holder"], true);
    assert_eq!(
        noted.value["receipt"]["result"],
        noted.value["evidence"]["result"]
    );
    assert!(noted.text().contains("(observation, no run credit)"));
    assert_eq!(execution_inventory(&database), before);
}

#[test]
fn phoenix_observation_integrity_rejects_coordinated_parent_feed_reordering() {
    for defect in ["sequence", "basis"] {
        let directory = tempdir().unwrap();
        let database = directory.path().join("work.db");
        let owner = service(&database, "owner");
        let reviewer = service(&database, "reviewer");
        let root = proposed_root(
            owner
                .work_propose(root_input("Feed order", "root"), at(0))
                .unwrap(),
        );
        reviewer
            .work_note_on(Some(&root.short_ref), "first", &[], at(1))
            .unwrap();
        reviewer
            .work_note_on(Some(&root.short_ref), "second", &[], at(1))
            .unwrap();
        let store = SqliteStore::open(&database).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        let (_, observations) = store.work_observation_tail(root.work_id, 8).unwrap();
        let first = &observations[0].0;
        let other = if defect == "sequence" {
            &observations[1].0
        } else {
            let WorkObservationBasis::NativeEvent { event } = &observations[0].1.basis else {
                panic!("native basis")
            };
            event
        };
        swap_parent_feed_positions(&database, first, other);
        let invalid = store.verify_all().unwrap().invalid_work_records;
        let expected = if defect == "sequence" {
            format!("work_observation:{}:feed_order", root.work_id.0)
        } else {
            format!("work_observation:{first}")
        };
        assert!(invalid.contains(&expected), "{defect}: {invalid:?}");
        // Save must not audit or publish a corrupt ordering cut.
        assert!(
            reviewer
                .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(2))
                .is_err()
        );
        swap_parent_feed_positions(&database, first, other);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

fn swap_parent_feed_positions(database: &std::path::Path, first: &ObjectHash, other: &ObjectHash) {
    let mut connection = rusqlite::Connection::open(database).unwrap();
    let transaction = connection.transaction().unwrap();
    for kind in ["project", "root_work"] {
        let position = |hash: &ObjectHash| {
            transaction.query_row(
            "SELECT position FROM work_feed_entries WHERE feed_kind = ?1 AND object_hash = ?2",
            rusqlite::params![kind, hash.as_str()], |row| row.get::<_, i64>(0),
        ).unwrap()
        };
        let first_position = position(first);
        let other_position = position(other);
        let temporary: i64 = transaction
            .query_row(
                "SELECT MAX(position) + 1 FROM work_feed_entries",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for (hash, position) in [
            (first, temporary),
            (other, first_position),
            (first, other_position),
        ] {
            assert_eq!(transaction.execute(
                "UPDATE work_feed_entries SET position = ?3 WHERE feed_kind = ?1 AND object_hash = ?2",
                rusqlite::params![kind, hash.as_str(), position],
            ).unwrap(), 1);
        }
    }
    transaction.commit().unwrap();
}
