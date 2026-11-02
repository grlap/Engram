use super::*;
use crate::task_sync::{
    progress::{PreparedSelection, read_prepared},
    retention::{read_retained, retain_prepared},
};

#[test]
fn exchange_candidate_survives_four_preparations_and_exact_retry() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"source route only").unwrap();
    let git = home.path().join("git-admin");
    fs::create_dir(&git).unwrap();
    let progress = home.path().join("prepare");
    let mut original = None;
    let exchange = uuid::Uuid::new_v4();
    for generation in 0..4 {
        let tree = tree(generation + 1);
        let destination = home.path().join(format!("output-{generation}"));
        let receipt = prepare_with_producer(
            &destination,
            &progress,
            &store,
            &tree.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| Ok(capture(tree.clone())),
        )
        .unwrap();
        let selection = PreparedSelection {
            operation: receipt.operation,
            destination,
            project: tree.project.project_id,
            profile: profile(),
        };
        if generation == 0 {
            let mut prepared = read_prepared(&progress, &selection).unwrap();
            // Raw retained input may carry formatting a semantic re-export loses.
            prepared
                .documents
                .get_mut("project.json")
                .unwrap()
                .push(b'\n');
            let retained = retain_prepared(&git, exchange, &prepared).unwrap();
            original = Some((selection, prepared, retained));
        }
    }
    let (selection, prepared, retained) = original.unwrap();
    assert!(
        read_prepared(&progress, &selection).is_err(),
        "old preparation was not pruned"
    );
    let before = listing(&git);
    assert_eq!(
        read_retained(&git, exchange, &retained).unwrap(),
        prepared.tree
    );
    assert_eq!(
        crate::task_sync::retention::read_retained_documents(&git, exchange, &retained).unwrap(),
        prepared.documents
    );
    assert_eq!(
        retain_prepared(&git, exchange, &prepared).unwrap(),
        retained
    );
    assert_eq!(listing(&git), before, "retry rewrote retained input");
    assert_eq!(
        crate::task_sync::retention::inventory(&git)
            .unwrap()
            .candidates,
        [exchange].into_iter().collect()
    );
    assert_eq!(
        fs::read(retained.directory.join("project.json")).unwrap(),
        prepared.documents["project.json"]
    );
    let mut other = prepared;
    other.provenance.producer = ObjectId::from_canonical_bytes(b"different producer");
    assert!(retain_prepared(&git, exchange, &other).is_err());
    assert_eq!(listing(&git), before);
}

#[test]
fn missing_or_partial_exchange_candidate_is_not_recaptured() {
    let home = crate::test_support::temp_home().unwrap();
    let git = home.path().join("git-admin");
    fs::create_dir(&git).unwrap();
    let tree = tree(1);
    let prepared = PreparedCandidate {
        documents: tree.documents().unwrap(),
        tree: tree.clone(),
        provenance: PreparedProvenance {
            sequence: 1,
            operation: uuid::Uuid::new_v4(),
            predecessor: None,
            store: home.path().join("store.db"),
            destination: home.path().join("output"),
            project: tree.project.project_id,
            profile: profile(),
            producer: producer(),
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
            omissions: NoteOmissions::default(),
            activity_omissions: NoteOmissions::default(),
        },
    };
    let exchange = uuid::Uuid::new_v4();
    let retained = retain_prepared(&git, exchange, &prepared).unwrap();
    fs::remove_file(retained.directory.join("project.json")).unwrap();
    let before = listing(&git);
    assert!(read_retained(&git, exchange, &retained).is_err());
    assert!(retain_prepared(&git, exchange, &prepared).is_err());
    assert!(
        crate::task_sync::retention::inventory(&git).is_err(),
        "partial artifact was classified as absent"
    );
    assert_eq!(listing(&git), before);
    let partial = uuid::Uuid::new_v4();
    fs::create_dir(
        git.join("engram-task-sync-exchange")
            .join(partial.to_string()),
    )
    .unwrap();
    let before = listing(&git);
    assert!(retain_prepared(&git, partial, &prepared).is_err());
    assert_eq!(listing(&git), before);
}
