use super::super::super::super::{origin, shared};
use super::*;

#[test]
fn shared_updates_note_reads_borrow_verified_members_and_decode_each_anchor_once() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut incoming = tree_with_shared();
    apply(&mut store, &incoming, None, "initial", 2);
    for task in &mut incoming.tasks {
        task.notes
            .extend((0..70).map(|index| note(&format!("Late shared note {index}"), "remote")));
        task.notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    }
    apply(&mut store, &incoming, None, "late-shared", 3);
    origin::PLANNING_DECODES.with(|count| count.set(0));
    shared::NOTE_ENUMERATIONS.with(|count| count.set(0));
    store
        .work_read_snapshot(|store| {
            let mut rendered = 0;
            for task in &incoming.tasks {
                let rows = store.work_record_index(&project(), task.id, WorkRecordKind::Notes)?;
                let late = rows
                    .into_iter()
                    .filter(|row| row.locator.contains(":shared-note-"));
                for row in late {
                    let WorkRecordContent::Note(note) =
                        store.work_record_content(&project(), task.id, &row)?
                    else {
                        panic!("shared note expected");
                    };
                    assert!(note.summary.starts_with("Late shared note"));
                    rendered += 1;
                    assert!(
                        store
                            .work_record_content(
                                &ProjectId("another-project".into()),
                                task.id,
                                &row
                            )
                            .is_err()
                    );
                    let other = incoming
                        .tasks
                        .iter()
                        .find(|other| other.id != task.id)
                        .unwrap()
                        .id;
                    assert!(store.work_record_content(&project(), other, &row).is_err());
                    let mut changed = row;
                    changed.address.hash = ObjectId::mint();
                    assert!(
                        store
                            .work_record_content(&project(), task.id, &changed)
                            .is_err()
                    );
                }
            }
            assert_eq!(rendered, 140);
            origin::PLANNING_DECODES.with(|count| assert_eq!(count.get(), 1));
            shared::NOTE_ENUMERATIONS.with(|count| assert_eq!(count.get(), incoming.tasks.len()));
            Ok(())
        })
        .unwrap();
    origin::PLANNING_DECODES.with(|count| count.set(0));
    let graph = store
        .save_work_graph_snapshot(
            &project(),
            &actor("reader"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_string(&graph.document)
            .unwrap()
            .matches("Late shared note")
            .count(),
        140
    );
    origin::PLANNING_DECODES.with(|count| assert_eq!(count.get(), 1));
}
