use super::*;

fn fixture() -> (FeedBoundary, ObjectId, OriginFeedSegment, OriginFeedCut) {
    let source = OriginFeed {
        origin: ObjectId::mint(),
        project_id: ProjectId("p".into()),
        feed: FeedId::Project(ProjectId("p".into())),
    };
    let prefix = FeedBoundary {
        source: source.clone(),
        through: 0,
        segment: None,
        endpoint: None,
    };
    let id = ObjectId::mint();
    let entries = (1..=3)
        .map(|position| FeedMember {
            position,
            object_kind: "work_event".into(),
            object_id: ObjectId::mint(),
        })
        .collect::<Vec<_>>();
    let segment = OriginFeedSegment {
        source: source.clone(),
        from_exclusive: 0,
        through: 3,
        predecessor: None,
        anchor: None,
        entries,
    };
    let cut = OriginFeedCut {
        source,
        through: 3,
        endpoint: segment.entries.last().cloned(),
        segments: vec![id.clone()],
    };
    (prefix, id, segment, cut)
}

#[test]
fn equal_overlap_replays_and_changed_membership_refuses() {
    let (prefix, id, segment, cut) = fixture();
    let segments = BTreeMap::from([(id, segment)]);
    let first = cut
        .validate_chain(&prefix, &segments, &BTreeMap::new())
        .unwrap();
    assert_eq!(first.len(), 3);
    assert_eq!(
        cut.validate_chain(&prefix, &segments, &first).unwrap(),
        first
    );
    let mut conflict = first;
    conflict.get_mut(&2).unwrap().object_id = ObjectId::mint();
    assert!(matches!(
        cut.validate_chain(&prefix, &segments, &conflict),
        Err(EvidenceError::Witness(WitnessProblem::SourceFeedConflict {
            position: 2
        }))
    ));
}

#[test]
fn omitted_interior_events_and_missing_segments_do_not_establish_a_cut() {
    let (prefix, id, mut segment, cut) = fixture();
    assert!(matches!(
        cut.validate_chain(&prefix, &BTreeMap::new(), &BTreeMap::new()),
        Err(EvidenceError::Witness(WitnessProblem::CutUnavailable))
    ));
    segment.entries.remove(1);
    assert!(matches!(
        cut.validate_chain(&prefix, &BTreeMap::from([(id, segment)]), &BTreeMap::new()),
        Err(EvidenceError::Witness(WitnessProblem::SourceFeedGap {
            position: 2
        }))
    ));
}

#[test]
fn beyond_cut_and_foreign_origin_are_refused() {
    let (prefix, id, mut segment, mut cut) = fixture();
    cut.through = 2;
    cut.endpoint = Some(segment.entries[1].clone());
    let segments = BTreeMap::from([(id.clone(), segment.clone())]);
    assert!(matches!(
        cut.validate_chain(&prefix, &segments, &BTreeMap::new()),
        Err(EvidenceError::Witness(WitnessProblem::CutUnavailable))
    ));
    segment.source.origin = ObjectId::mint();
    assert!(matches!(
        cut.validate_chain(&prefix, &BTreeMap::from([(id, segment)]), &BTreeMap::new()),
        Err(EvidenceError::Witness(WitnessProblem::SourceScopeDiffers))
    ));
}

#[test]
fn tail_requires_the_exact_predecessor_and_endpoint_anchor() {
    let (prefix, id, segment, _) = fixture();
    segment.validate(&prefix).unwrap();
    let previous = FeedBoundary {
        source: segment.source.clone(),
        through: 3,
        segment: Some(id.clone()),
        endpoint: segment.entries.last().cloned(),
    };
    let mut tail = OriginFeedSegment {
        source: segment.source,
        from_exclusive: 3,
        through: 4,
        predecessor: Some(id),
        anchor: previous.endpoint.clone(),
        entries: vec![FeedMember {
            position: 4,
            object_kind: "named_root_binding".into(),
            object_id: ObjectId::mint(),
        }],
    };
    tail.validate(&previous).unwrap();
    tail.anchor.as_mut().unwrap().object_id = ObjectId::mint();
    assert!(matches!(
        tail.validate(&previous),
        Err(EvidenceError::Witness(WitnessProblem::ContinuityDiffers))
    ));
}

#[test]
fn root_and_run_feeds_keep_their_own_namespaces() {
    for feed in [
        FeedId::RootWork(crate::WorkId(uuid::Uuid::new_v4())),
        FeedId::RunExecution(crate::WorkRunId(uuid::Uuid::new_v4())),
    ] {
        let (mut prefix, _, mut segment, _) = fixture();
        prefix.source.feed = feed.clone();
        segment.source.feed = feed;
        segment.validate(&prefix).unwrap();
        let bytes = serde_json_canonicalizer::to_vec(&segment).unwrap();
        let round: OriginFeedSegment = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(round, segment);
    }
}

#[test]
fn empty_cut_cannot_claim_a_malformed_retained_prefix() {
    let (mut prefix, _, _, mut cut) = fixture();
    prefix.through = 2;
    cut.through = 2;
    cut.endpoint = None;
    cut.segments.clear();
    assert!(matches!(
        cut.validate_chain(&prefix, &BTreeMap::new(), &BTreeMap::new()),
        Err(EvidenceError::Witness(WitnessProblem::ContinuityDiffers))
    ));
}

fn work() -> crate::WorkItem {
    let id = uuid::Uuid::new_v4();
    serde_json::from_value(serde_json::json!({
        "schema_version": crate::schema::SCHEMA_VERSION,
        "project_id":"p", "work_id":id, "short_ref":"fixture", "root_id":id,
        "parent_id":null, "child_requirement":"required", "title":"snapshot",
        "outcome":"all governing content", "acceptance":["criterion"],
        "kind":"task", "priority":1, "labels":[], "assigned_to":null,
        "deferred_until":null, "origin":"local", "source_snapshot_id":null,
        "lifecycle":"open", "revision":2, "active_run_id":null,
        "created_by": {"actor_id":"fixture", "actor_kind":"agent", "assurance":"asserted",
            "run_id":null, "session_id":null, "source_tool":null,"source_skill":null,
            "provenance_chain":[], "reason":"fixture"},
        "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

#[test]
fn governing_fingerprint_compares_the_complete_snapshot_not_only_criteria() {
    let snapshot = work();
    let basis = GoverningBasis {
        evaluation: ObjectId::mint(),
        project_id: snapshot.project_id.clone(),
        work_id: snapshot.work_id,
        work_revision: snapshot.revision,
        content_fingerprint: crate::CanonicalObject::freeze(&snapshot)
            .unwrap()
            .key()
            .clone(),
    };
    let mut witness = GoverningSnapshotWitness {
        basis: basis.clone(),
        snapshot: SnapshotAvailability::Exact {
            snapshot: Box::new(snapshot.clone()),
            source_snapshot: snapshot.source_snapshot_id.clone(),
        },
    };
    assert_eq!(witness.validate(&basis).unwrap(), Some(&snapshot));
    if let SnapshotAvailability::Exact { snapshot, .. } = &mut witness.snapshot {
        snapshot.title = "same criteria, different governing task".into();
    }
    assert!(matches!(
        witness.validate(&basis),
        Err(EvidenceError::Witness(
            WitnessProblem::GoverningSnapshotDiffers
        ))
    ));
    assert_eq!(basis.content_fingerprint.as_str().len(), 64);
    assert_eq!(basis.evaluation.as_str().len(), 32);
}

#[test]
fn unavailable_governing_snapshot_survives_pack_relay_without_invented_content() {
    let snapshot = work();
    let basis = GoverningBasis {
        evaluation: ObjectId::mint(),
        project_id: snapshot.project_id,
        work_id: snapshot.work_id,
        work_revision: snapshot.revision,
        content_fingerprint: ObjectId::from_canonical_bytes(b"compared original input"),
    };
    let witness = GoverningSnapshotWitness {
        basis: basis.clone(),
        snapshot: SnapshotAvailability::GoverningSnapshotUnavailable,
    };
    assert!(witness.validate(&basis).unwrap().is_none());
    let bytes = serde_json_canonicalizer::to_vec(&witness).unwrap();
    let origin = ObjectId::mint();
    let record = super::super::RecordEnvelope::from_canonical(
        origin.clone(),
        ObjectId::mint(),
        "evidence_governing_snapshot",
        bytes.clone(),
    )
    .unwrap();
    let pack = super::super::EvidencePack::prepare(
        ObjectId::mint(),
        ProjectId("p".into()),
        origin,
        vec![record],
    )
    .unwrap();
    let encoded = pack.to_bytes().unwrap();
    let retained = super::super::EvidencePack::read(encoded.as_slice()).unwrap();
    assert_eq!(retained.records()[0].canonical_bytes(), bytes);
    let retained: GoverningSnapshotWitness =
        serde_json::from_slice(retained.records()[0].canonical_bytes()).unwrap();
    assert!(retained.validate(&basis).unwrap().is_none());
}
