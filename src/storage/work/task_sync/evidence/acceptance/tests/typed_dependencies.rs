use super::*;
use crate::storage::work::task_sync::evidence::test_support::{
    capture_roundtrip, dependency_refusals, project_store,
};

fn imported_fixture(
    root: &std::path::Path,
    mut store: SqliteStore,
) -> (Fixture, crate::WorkSourceSnapshot, ObjectId) {
    let input = crate::domain::WorkImportInput {
        snapshot: crate::WorkSourceSnapshot {
            schema_version: SCHEMA_VERSION,
            adapter_kind: "planner".into(),
            canonical_ref: "plan/external-item".into(),
            projected: crate::WorkSourceProjection {
                title: Some("External work".into()),
                body: Some("Original external planning text".into()),
                status: None,
                owner: None,
            },
            captured_at: at(1),
            source_revision: Some("original revision".into()),
            fingerprint: "external planning content".into(),
            canonical_url: None,
            payload_hash: ObjectId::from_canonical_bytes(b"external input"),
            raw: std::collections::BTreeMap::new(),
        },
        draft: Some(crate::domain::WorkImportDraft {
            title: "Imported work".into(),
            outcome: "Retain original native history".into(),
            acceptance: vec!["The imported behavior works".into()],
        }),
    };
    let preview = store
        .preview_work_import(&project(), &input, at(1))
        .unwrap();
    let imported = store
        .apply_work_import(
            &project(),
            &input,
            &preview.preview_token,
            &actor("runner"),
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = store.get_work_item(imported.work_id).unwrap();
    let snapshot = work.source_snapshot_id.clone().unwrap();
    (
        Fixture::with_work(root, store, &work),
        input.snapshot,
        snapshot,
    )
}

#[test]
fn native_typed_dependencies_preserve_external_snapshot_and_remote_producer() {
    let root = repository();
    let home = crate::test_support::temp_home().unwrap();
    let store = project_store(home.path(), &project());
    let (mut f, external, snapshot) = imported_fixture(root.path(), store);
    f.observe(root.path(), 5);
    let producer = f.record(10);
    let verification = producer.verifications[0].record_id.clone();
    let evaluated = evaluate(&mut f, root.path(), &verification);
    let packs = capture_roundtrip(
        &mut f.store,
        &project(),
        f.binding.run_id,
        vec![evaluated.evaluation.clone()],
    );
    let records = packs
        .iter()
        .flat_map(EvidencePack::records)
        .collect::<Vec<_>>();
    let original = records
        .iter()
        .find(|record| record.object_id() == &snapshot)
        .unwrap();
    assert_eq!(original.kind(), RecordKind::WorkSourceSnapshot);
    assert_eq!(
        serde_json::from_slice::<crate::WorkSourceSnapshot>(original.canonical_bytes()).unwrap(),
        external
    );
    let remote = records
        .iter()
        .find(|record| record.object_id() == &producer.operation)
        .unwrap();
    assert_eq!(remote.kind(), RecordKind::RemoteVerificationAssociation);
    let check: VerificationEvidence = serde_json::from_slice(
        records
            .iter()
            .find(|record| record.object_id() == &verification)
            .unwrap()
            .canonical_bytes(),
    )
    .unwrap();
    assert_eq!(check.producer_observation, producer.operation);
    let governing = records
        .iter()
        .find(|record| record.kind() == RecordKind::GoverningSnapshotWitness)
        .unwrap();
    let witness: GoverningSnapshotWitness =
        serde_json::from_slice(governing.canonical_bytes()).unwrap();
    let SnapshotAvailability::Exact {
        snapshot: item,
        source_snapshot,
    } = &witness.snapshot
    else {
        panic!("native capture must carry the original governing item")
    };
    assert_eq!(item.work_id, f.binding.work_id);
    assert_eq!(
        item.as_ref(),
        &f.store.get_work_item(f.binding.work_id).unwrap()
    );
    assert_eq!(source_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(item.source_snapshot_id, *source_snapshot);
    dependency_refusals(&packs, &snapshot);
    dependency_refusals(&packs, &producer.operation);

    for replacement in [None, Some(ObjectId::mint())] {
        let mut bad = witness.clone();
        let SnapshotAvailability::Exact {
            source_snapshot, ..
        } = &mut bad.snapshot
        else {
            unreachable!()
        };
        *source_snapshot = replacement;
        assert!(bad.validate(&bad.basis).is_err());
    }
}
