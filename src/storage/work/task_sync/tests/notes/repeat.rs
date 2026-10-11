use super::*;
use crate::domain::{RecordRestoredWorkEvidenceRequest, RestoredWorkEvidenceInput};

#[test]
fn task_sync_notes_empty_included_profile_refuses_without_retaining_state() {
    let mut tree = candidate();
    tree.tasks.clear();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = state(&store);
    for included in [true, false, true, false] {
        tree.project.notes_included = included;
        for dry in [true, false] {
            let result = store.import_task_tree(
                &tree.project.project_id,
                &actor("repeat"),
                &tree,
                dry,
                at(30),
            );
            if included {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("notes_included requires at least one task")
                );
            } else {
                let result = result.unwrap();
                assert!(!result.changed);
                assert_eq!(result.dry_run, dry);
            }
            assert_eq!(state(&store), before);
        }
    }
    assert!(store.verify_all().unwrap().is_healthy());
    let empty_export = store
        .export_task_tree_with_notes(&tree.project.project_id, "host-A")
        .unwrap();
    assert!(
        empty_export
            .tree
            .documents()
            .unwrap_err()
            .to_string()
            .contains("notes_included requires at least one task")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_notes_zero_note_repeat_preserves_the_retained_profile() {
    for included in [false, true] {
        let mut tree = candidate();
        tree.project.notes_included = included;
        assert_ne!(tree.tasks, [] as [TaskDocument; 0]);
        assert!(tree.tasks.iter().all(|task| task.notes.is_empty()));
        tree.validate().unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        import(&mut store, &tree);
        let before = state(&store);
        let mut changed = tree.clone();
        changed.project.notes_included = !included;
        for dry in [true, false] {
            let error = store
                .import_task_tree(
                    &tree.project.project_id,
                    &actor("repeat"),
                    &changed,
                    dry,
                    at(30),
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("divergent or removed planning state")
            );
            assert_eq!(state(&store), before);
            let result = store
                .import_task_tree(
                    &tree.project.project_id,
                    &actor("repeat"),
                    &tree,
                    dry,
                    at(30),
                )
                .unwrap();
            assert!(!result.changed);
            assert_eq!(result.dry_run, dry);
            assert_eq!(state(&store), before);
        }
        assert!(store.verify_all().unwrap().is_healthy());
        assert!(
            store
                .export_task_tree_with_notes(&tree.project.project_id, "host-B")
                .unwrap()
                .tree
                .project
                .notes_included
        );
        assert!(
            !store
                .export_task_tree(&tree.project.project_id)
                .unwrap()
                .project
                .notes_included
        );
    }
}

#[test]
fn task_sync_notes_repeat_after_local_peer_or_late_note_respects_candidate_profile() {
    for included in [false, true] {
        for late in [false, true] {
            let mut tree = if included {
                note_candidate()
            } else {
                candidate()
            };
            let mut store = SqliteStore::open_in_memory().unwrap();
            import(&mut store, &tree);
            let task = tree
                .tasks
                .iter()
                .find(|task| {
                    task.lifecycle
                        == if late {
                            WorkLifecycle::Completed
                        } else {
                            WorkLifecycle::Open
                        }
                })
                .unwrap();
            let item = store.get_work_item(task.id).unwrap();
            if late {
                let mut author = actor("late-author");
                author.provenance_chain.push(crate::domain::ProvenanceLink {
                    relation: crate::domain::ProvenanceRelation::DerivedFrom,
                    source: crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
                    reference: Some(
                        crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into(),
                    ),
                });
                store
                    .record_restored_work_evidence(
                        &RecordRestoredWorkEvidenceRequest {
                            work_id: item.work_id,
                            expected_work_revision: item.revision,
                            holder: SessionId("late-author".into()),
                            input: RestoredWorkEvidenceInput::Note {
                                status: false,
                                summary: "Local late finding".into(),
                                refs: vec![],
                            },
                            actor: author,
                            idempotency_key: "local-late".into(),
                            recorded_at: at(20),
                        },
                        &crate::DevelopmentNoopRedactor,
                    )
                    .unwrap();
            } else {
                store
                    .record_work_observation(
                        &RecordWorkObservationRequest {
                            status: true,
                            project_id: item.project_id.clone(),
                            work_id: item.work_id,
                            expected_work_revision: item.revision,
                            session_id: SessionId("peer".into()),
                            summary: "Local peer progress".into(),
                            refs: vec![],
                            actor: crate::domain::non_holder_note_actor(actor("peer")),
                            idempotency_key: "local-peer".into(),
                            recorded_at: at(20),
                        },
                        &crate::DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            assert!(store.verify_all().unwrap().is_healthy());
            let before = state(&store);
            for dry in [true, false] {
                let result = store.import_task_tree(
                    &tree.project.project_id,
                    &actor("repeat"),
                    &tree,
                    dry,
                    at(30),
                );
                if included {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("divergent shared notes")
                    );
                } else {
                    let result = result.unwrap();
                    assert!(!result.changed);
                    assert_eq!(result.dry_run, dry);
                }
                assert_eq!(state(&store), before);
            }
            // Disabling notes must still refuse removal of imported shared notes.
            if included {
                tree.project.notes_included = false;
                for task in &mut tree.tasks {
                    task.notes.clear();
                }
                assert!(
                    store
                        .import_task_tree(
                            &tree.project.project_id,
                            &actor("repeat"),
                            &tree,
                            false,
                            at(30)
                        )
                        .is_err()
                );
                assert_eq!(state(&store), before);
            }
        }
    }
}
