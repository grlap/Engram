use super::*;

fn linked_packs(large: bool) -> [EvidencePack; 2] {
    let (id, origin) = origin(None);
    let text = if large {
        "x".repeat(5 * 1024 * 1024)
    } else {
        "reported fact".into()
    };
    let first = evidence(&id, &text);
    let second = evidence(&id, &text);
    let prior = ObjectId::mint();
    let later = ObjectId::mint();
    let prefix = OriginFeedSegment {
        source: source(&id),
        from_exclusive: 0,
        through: 1,
        predecessor: None,
        anchor: None,
        entries: vec![member(1, &first)],
    };
    let suffix = OriginFeedSegment {
        source: source(&id),
        from_exclusive: 1,
        through: 2,
        predecessor: Some(prior.clone()),
        anchor: Some(member(1, &first)),
        entries: vec![member(2, &second)],
    };
    let cut = OriginFeedCut {
        source: source(&id),
        through: 2,
        endpoint: Some(member(2, &second)),
        segments: vec![prior.clone(), later.clone()],
    };
    [
        pack(
            &id,
            vec![
                first,
                evidence(&id, &text),
                envelope(&id, &later, RecordKind::OriginFeedSegment, &suffix),
                envelope(&id, &ObjectId::mint(), RecordKind::OriginFeedCut, &cut),
            ],
        ),
        pack(
            &id,
            vec![
                enrollment(&id, &origin),
                second,
                evidence(&id, &text),
                envelope(&id, &prior, RecordKind::OriginFeedSegment, &prefix),
            ],
        ),
    ]
}

#[test]
fn cross_pack_context_exceeding_one_pack_is_atomic_in_both_orders() {
    let packs = linked_packs(true);
    let encoded = packs
        .iter()
        .map(|pack| pack.to_bytes().unwrap())
        .collect::<Vec<_>>();
    let total = encoded.iter().map(Vec::len).sum::<usize>();
    assert!(total > crate::task_sync::evidence::limits::MAX_PACK_BYTES);
    assert!(total < crate::task_sync::evidence::limits::MAX_BATCH_BYTES);
    for reverse in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("batch.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let before = snapshot(&store);
        for pack in &packs {
            assert!(
                store
                    .retain_evidence_pack(&project(), pack, &ObjectId::mint())
                    .is_err()
            );
            assert_eq!(snapshot(&store), before);
        }
        let ids = [ObjectId::mint(), ObjectId::mint()];
        let selected = if reverse {
            [(&packs[1], &ids[1]), (&packs[0], &ids[0])]
        } else {
            [(&packs[0], &ids[0]), (&packs[1], &ids[1])]
        };
        let retained = store.retain_evidence_batch(&project(), &selected).unwrap();
        for ((pack, admission), result) in selected.iter().zip(&retained) {
            assert_eq!(&result.pack_id, pack.pack_id());
            assert_eq!(&result.admission, *admission);
            assert!(!result.replayed);
        }
        let after = snapshot(&store);
        let replay = store.retain_evidence_batch(&project(), &selected).unwrap();
        assert!(
            replay
                .iter()
                .all(|row| row.replayed && row.imported_objects == 0)
        );
        assert_eq!(snapshot(&store), after);
        healthy(&store);
        let before_objects = originals(&store);
        drop(store);
        assert!(
            SqliteStore::repair_rebuildable_projections(&database)
                .unwrap()
                .is_healthy()
        );
        let store = SqliteStore::open(&database).unwrap();
        assert_eq!(originals(&store), before_objects);
        let exported = store
            .export_evidence_packs(&store.capture_evidence_packs(&project()).unwrap().unwrap())
            .unwrap();
        assert_eq!(exported.packs.len(), 2);
        for pack in &exported.packs {
            let position = packs
                .iter()
                .position(|input| input.pack_id() == pack.pack_id())
                .unwrap();
            assert_eq!(pack.to_bytes().unwrap(), encoded[position]);
        }
        healthy(&store);
    }
}

#[test]
fn a_late_cross_pack_cut_failure_rolls_back_the_entire_outer_writer() {
    let mut packs = linked_packs(false);
    let cut = packs[0]
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::OriginFeedCut)
        .unwrap();
    let mut body: OriginFeedCut = serde_json::from_slice(cut.canonical_bytes()).unwrap();
    body.through += 1;
    let records = packs[0]
        .records()
        .iter()
        .map(|record| {
            if record.object_id() == cut.object_id() {
                envelope(record.origin(), record.object_id(), record.kind(), &body)
            } else {
                record.clone()
            }
        })
        .collect();
    packs[0] = pack(packs[0].publishing_origin(), records);
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = snapshot(&store);
    let ids = [ObjectId::mint(), ObjectId::mint()];
    let mut writer = store.connection.transaction().unwrap();
    assert!(
        retain_batch_on(
            &mut writer,
            &project(),
            &[(&packs[0], &ids[0]), (&packs[1], &ids[1])]
        )
        .is_err()
    );
    writer.commit().unwrap();
    assert_eq!(snapshot(&store), before);
    healthy(&store);
}

#[test]
fn cross_pack_collisions_and_prepared_identity_aliases_leave_no_rows() {
    let (id, origin) = origin(None);
    let first = evidence(&id, "first original");
    let mut different: serde_json::Value = serde_json::from_slice(first.canonical_bytes()).unwrap();
    different["summary"] = json!("same id, other bytes");
    let a = pack(&id, vec![enrollment(&id, &origin), first.clone()]);
    let b = pack(
        &id,
        vec![envelope(&id, first.object_id(), first.kind(), &different)],
    );
    let alias = pack(&id, vec![first.clone()]);
    let ids = [ObjectId::mint(), ObjectId::mint()];
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = snapshot(&store);
    for selected in [
        [(&a, &ids[0]), (&b, &ids[1])],
        [(&b, &ids[1]), (&a, &ids[0])],
        [(&a, &ids[0]), (&alias, &ids[0])],
        [(&a, first.object_id()), (&alias, &ids[1])],
    ] {
        let mut writer = store.connection.transaction().unwrap();
        assert!(retain_batch_on(&mut writer, &project(), &selected).is_err());
        writer.commit().unwrap();
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn a_selected_batch_over_the_object_bound_refuses_before_retaining_any_pack() {
    use crate::task_sync::evidence::limits::{Dimension, LimitKind, Oversize};
    let (id, origin) = origin(None);
    let mut records = (0..3000)
        .map(|_| evidence(&id, "bounded selection"))
        .collect::<Vec<_>>();
    records.push(enrollment(&id, &origin));
    let packs = (0..3)
        .map(|_| pack(&id, records.clone()))
        .collect::<Vec<_>>();
    let ids = (0..3).map(|_| ObjectId::mint()).collect::<Vec<_>>();
    let selected = packs.iter().zip(&ids).collect::<Vec<_>>();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = snapshot(&store);
    let mut writer = store.connection.transaction().unwrap();
    assert!(matches!(
        retain_batch_on(&mut writer, &project(), &selected),
        Err(PersistenceError::Evidence(EvidenceError::Oversize(
            Oversize {
                kind: LimitKind::BatchTooLarge,
                dimension: Dimension::Objects,
                ..
            }
        )))
    ));
    writer.commit().unwrap();
    assert_eq!(snapshot(&store), before);
}
