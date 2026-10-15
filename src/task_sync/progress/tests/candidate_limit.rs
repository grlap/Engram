//! Preparation refuses a candidate its writer would refuse before it prunes
//! or stages a generation.

use super::*;
use crate::task_sync::{
    SharedActor,
    notes::{SharedNote, SharedNoteKind},
};

fn notes_profile() -> ExportProfile {
    ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    }
}

#[test]
fn task_prepare_refuses_an_oversized_candidate_before_pruning_or_staging() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let output = home.path().join("candidate");
    let mut snapshot = tree(2);
    snapshot.project.notes_included = true;
    let prepare = |destination: &Path, captured: ExportCapture| {
        prepare_with_producer(
            destination,
            &progress,
            &store,
            &snapshot.project.project_id,
            &notes_profile(),
            false,
            &producer(),
            |_| Ok(captured),
        )
    };
    let read = |operation| {
        read_prepared(
            &progress,
            &PreparedSelection {
                operation,
                destination: output.clone(),
                project: snapshot.project.project_id.clone(),
                profile: notes_profile(),
            },
        )
        .map(|prepared| prepared.documents)
    };

    // Two completed generations, so a further preparation would prune the
    // older one before publishing its own.
    let retained: Vec<_> = (0..2)
        .map(|_| {
            prepare(&output, capture(snapshot.clone()))
                .unwrap()
                .operation
        })
        .collect();
    let documents = snapshot.documents().unwrap();
    for operation in &retained {
        assert_eq!(read(*operation).unwrap(), documents);
    }

    // Each note fits the native note limit; their inline total in one task
    // file exceeds the candidate writer's per-file limit.
    let body = "Progress note line.\n".repeat(2500).trim_end().to_owned();
    let mut notes: Vec<_> = (0..24)
        .map(|_| SharedNote {
            id: ObjectId::mint(),
            kind: SharedNoteKind::Generic,
            body: body.clone(),
            author: SharedActor {
                actor_id: "writer".into(),
                actor_kind: "human".into(),
            },
            recorded_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            origin_machine: "host-a".into(),
        })
        .collect();
    notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let mut oversized = snapshot.clone();
    oversized.tasks[0].notes = notes;
    let task_file = format!("tasks/{}.json", oversized.tasks[0].id.0);
    assert!(oversized.documents().unwrap()[&task_file].len() > 1024 * 1024);

    // A fresh destination, so the existing output's overwrite refusal cannot
    // answer first.
    let fresh = home.path().join("fresh");
    let before = listing(home.path());
    let error = prepare(&fresh, capture(oversized)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "local work input is invalid: export exceeds the supported task candidate limits"
    );
    // Compared before anything loads the journal, which would remove a
    // leftover staging directory.
    assert_eq!(
        before,
        listing(home.path()),
        "the refusal pruned a generation, staged or wrote an entry"
    );
    assert!(!fresh.exists());
    for operation in &retained {
        assert_eq!(read(*operation).unwrap(), documents);
    }
}
