use super::*;
use serde_json::json;

fn record(id: ObjectId, origin: ObjectId, summary: &str) -> RecordEnvelope {
    let body = json!({
        "schema_version":crate::schema::SCHEMA_VERSION,
        "work_id":uuid::Uuid::nil(), "run_id":uuid::Uuid::nil(), "claim_id":uuid::Uuid::nil(),
        "claim_fence":1,"summary":summary,"refs":[],
        "actor":{"actor_id":"fixture","actor_kind":"agent","assurance":"asserted",
            "run_id":null,"session_id":null,"source_tool":null,"source_skill":null,
            "provenance_chain":[],"reason":"fixture"},"created_at":"2026-01-01T00:00:00Z"
    });
    RecordEnvelope::from_canonical(
        origin,
        id,
        "work_evidence",
        serde_json_canonicalizer::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn reference(record: &RecordEnvelope) -> ObjectReference {
    ObjectReference {
        object_id: record.object_id().clone(),
        object_kind: record.kind(),
    }
}

#[test]
fn predecessors_precede_an_indivisible_cycle_and_no_ids_are_reminted() {
    let origin = ObjectId::mint();
    let a = record(ObjectId::mint(), origin.clone(), "a");
    let b = record(ObjectId::mint(), origin.clone(), "b");
    let c = record(ObjectId::mint(), origin, "c");
    let selected = vec![
        ClosureRecord {
            record: &a,
            dependencies: vec![reference(&b)],
        },
        ClosureRecord {
            record: &b,
            dependencies: vec![reference(&a), reference(&c)],
        },
        ClosureRecord {
            record: &c,
            dependencies: vec![],
        },
    ];
    let plan = plan_components(&selected, |_| Ok(None)).unwrap();
    assert_eq!(plan.components.len(), 2);
    assert_eq!(plan.components[0].objects, vec![c.object_id().clone()]);
    let mut cycle = vec![a.object_id().clone(), b.object_id().clone()];
    cycle.sort();
    assert_eq!(plan.components[1].objects, cycle);
    assert_eq!(plan_components(&selected, |_| Ok(None)).unwrap(), plan);
    assert_eq!(plan.batches, vec![vec![0, 1]]);
}

#[test]
fn origin_is_not_an_escape_for_conflicting_original_bytes() {
    let id = ObjectId::mint();
    let a = record(id.clone(), ObjectId::mint(), "first");
    let b = record(id.clone(), ObjectId::mint(), "different");
    let selected = [
        ClosureRecord {
            record: &a,
            dependencies: vec![],
        },
        ClosureRecord {
            record: &b,
            dependencies: vec![],
        },
    ];
    assert!(matches!(plan_components(&selected, |_| Ok(None)),
        Err(EvidenceError::RecordConflict(conflict)) if conflict==id));
    assert!(
        matches!(plan_components(&selected[..1], |_| Ok(Some(b.clone()))),
        Err(EvidenceError::RecordConflict(conflict)) if conflict==id)
    );
}

#[test]
fn known_exact_originals_need_no_new_component_and_missing_links_refuse() {
    let origin = ObjectId::mint();
    let predecessor = record(ObjectId::mint(), origin.clone(), "known");
    let next = record(ObjectId::mint(), origin, "new");
    let inputs = [ClosureRecord {
        record: &next,
        dependencies: vec![reference(&predecessor)],
    }];
    assert!(matches!(
        plan_components(&inputs, |_| Ok(None)),
        Err(EvidenceError::Dependency {
            problem: "required original is unavailable",
            ..
        })
    ));
    let mut lookups = Vec::new();
    let plan = plan_components(&inputs, |id| {
        lookups.push(id.clone());
        Ok((id == predecessor.object_id()).then(|| predecessor.clone()))
    })
    .unwrap();
    assert_eq!(
        lookups,
        vec![next.object_id().clone(), predecessor.object_id().clone()]
    );
    assert_eq!(plan.components.len(), 1);
    let replay = [ClosureRecord {
        record: &predecessor,
        dependencies: vec![],
    }];
    let plan = plan_components(&replay, |_| Ok(Some(predecessor.clone()))).unwrap();
    assert_eq!(plan.components, Vec::<AtomicComponent>::new());
    assert_eq!(plan.batches, Vec::<Vec<usize>>::new());
    assert_eq!(plan.already_known, vec![predecessor.object_id().clone()]);
}

#[test]
fn the_expected_dependency_kind_cannot_be_satisfied_by_another_kind() {
    let origin = ObjectId::mint();
    let first = record(ObjectId::mint(), origin.clone(), "first");
    let next = record(ObjectId::mint(), origin, "next");
    let wrong = ObjectReference {
        object_id: first.object_id().clone(),
        object_kind: RecordKind::CompletionSeal,
    };
    let selected = [
        ClosureRecord {
            record: &first,
            dependencies: vec![],
        },
        ClosureRecord {
            record: &next,
            dependencies: vec![wrong],
        },
    ];
    assert!(matches!(
        plan_components(&selected, |_| Ok(None)),
        Err(EvidenceError::Dependency {
            problem: "dependency kind differs",
            ..
        })
    ));
}

#[test]
fn repeated_originals_cannot_hide_a_different_reference_inventory() {
    let first = record(ObjectId::mint(), ObjectId::mint(), "first");
    let selected = [
        ClosureRecord {
            record: &first,
            dependencies: vec![],
        },
        ClosureRecord {
            record: &first,
            dependencies: vec![reference(&first)],
        },
    ];
    assert!(matches!(
        plan_components(&selected, |_| Ok(None)),
        Err(EvidenceError::Dependency {
            problem: "dependency inventory differs for the same original",
            ..
        })
    ));
}

#[test]
fn long_dependency_chain_is_iterative_and_preserves_every_original() {
    let count = 20_000;
    let edges = (0..count)
        .map(|index| if index == 0 { vec![] } else { vec![index - 1] })
        .collect::<Vec<_>>();
    let groups = strongly_connected(&edges);
    assert_eq!(groups.len(), count);
    assert!(groups.iter().all(|group| group.len() == 1));
    assert_eq!(
        groups
            .iter()
            .rev()
            .map(|group| group[0])
            .collect::<Vec<_>>(),
        (0..count).collect::<Vec<_>>()
    );
}

#[test]
fn oversized_cycle_is_refused_without_returning_half_of_it() {
    let origin = ObjectId::mint();
    let records = (0..=limits::MAX_COMPONENT_OBJECTS)
        .map(|_| record(ObjectId::mint(), origin.clone(), "cycle"))
        .collect::<Vec<_>>();
    let selected = records
        .iter()
        .enumerate()
        .map(|(index, record)| ClosureRecord {
            record,
            dependencies: vec![reference(&records[(index + 1) % records.len()])],
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(plan_components(&selected, |_|Ok(None)),Err(EvidenceError::Oversize(limits::Oversize {
        kind:limits::LimitKind::ComponentTooLarge,dimension:limits::Dimension::Objects,actual,..
    })) if actual==limits::MAX_COMPONENT_OBJECTS+1)
    );
}

#[test]
fn large_acyclic_history_splits_only_between_complete_components() {
    let origin = ObjectId::mint();
    let records = (0..=limits::MAX_BATCH_OBJECTS)
        .map(|_| record(ObjectId::mint(), origin.clone(), "independent"))
        .collect::<Vec<_>>();
    let selected = records
        .iter()
        .map(|record| ClosureRecord {
            record,
            dependencies: vec![],
        })
        .collect::<Vec<_>>();
    let plan = plan_components(&selected, |_| Ok(None)).unwrap();
    assert_eq!(plan.components.len(), records.len());
    assert_eq!(plan.batches.len(), 2);
    assert_eq!(plan.batches[0].len(), limits::MAX_BATCH_OBJECTS);
    assert_eq!(plan.batches[1].len(), 1);
    let all = plan
        .batches
        .iter()
        .flatten()
        .flat_map(|&index| plan.components[index].objects.iter().cloned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        all,
        records
            .iter()
            .map(|record| record.object_id().clone())
            .collect()
    );
}

#[test]
fn actual_retained_batch_bound_includes_the_pack_containers() {
    // Each empty pack is a real immutable retained payload with a distinct UUID.
    let pack = super::super::EvidencePack::prepare(
        ObjectId::mint(),
        crate::ProjectId("p".into()),
        ObjectId::mint(),
        vec![],
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    validate_batch_payloads([pack.as_slice()]).unwrap();
    // A reader must refuse the declared complete payload before trying to parse it.
    let oversized = vec![b' '; limits::MAX_BATCH_BYTES + 1];
    assert!(matches!(validate_batch_payloads([oversized.as_slice()]),
        Err(EvidenceError::Oversize(limits::Oversize {kind:limits::LimitKind::BatchTooLarge,
            dimension:limits::Dimension::Bytes,actual,..})) if actual==limits::MAX_BATCH_BYTES+1));
}

#[test]
fn a_cycle_over_the_decoded_byte_limit_is_not_split() {
    let origin = ObjectId::mint();
    let overhead = record(ObjectId::mint(), origin.clone(), "")
        .canonical_bytes()
        .len();
    let summary = "x".repeat(limits::MAX_RECORD_BYTES - overhead);
    let records = (0..=limits::MAX_COMPONENT_BYTES / limits::MAX_RECORD_BYTES)
        .map(|_| record(ObjectId::mint(), origin.clone(), &summary))
        .collect::<Vec<_>>();
    let selected = records
        .iter()
        .enumerate()
        .map(|(index, record)| ClosureRecord {
            record,
            dependencies: vec![reference(&records[(index + 1) % records.len()])],
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(plan_components(&selected, |_|Ok(None)),Err(EvidenceError::Oversize(limits::Oversize {
        kind:limits::LimitKind::ComponentTooLarge,dimension:limits::Dimension::Bytes,actual,..
    })) if actual==records.len()*limits::MAX_RECORD_BYTES)
    );
}

#[test]
fn actual_batch_object_limit_counts_members_across_pack_files() {
    let origin = ObjectId::mint();
    let records = (0..limits::MAX_PACK_RECORDS)
        .map(|_| record(ObjectId::mint(), origin.clone(), "member"))
        .collect::<Vec<_>>();
    let full = super::super::EvidencePack::prepare(
        ObjectId::mint(),
        crate::ProjectId("p".into()),
        origin.clone(),
        records,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    let extra = super::super::EvidencePack::prepare(
        ObjectId::mint(),
        crate::ProjectId("p".into()),
        origin.clone(),
        vec![record(ObjectId::mint(), origin, "extra")],
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    validate_batch_payloads([full.as_slice(), full.as_slice()]).unwrap();
    assert!(
        matches!(validate_batch_payloads([full.as_slice(),full.as_slice(),extra.as_slice()]),
        Err(EvidenceError::Oversize(limits::Oversize {kind:limits::LimitKind::BatchTooLarge,
            dimension:limits::Dimension::Objects,actual,..})) if actual==limits::MAX_BATCH_OBJECTS+1)
    );
}

#[test]
fn oversized_construction_metadata_is_an_explicit_refusal() {
    let origin = ObjectId::mint();
    let records = (0..24_000)
        .map(|_| record(ObjectId::mint(), origin.clone(), "independent"))
        .collect::<Vec<_>>();
    let selected = records
        .iter()
        .map(|record| ClosureRecord {
            record,
            dependencies: vec![],
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        plan_components(&selected, |_| Ok(None)),
        Err(EvidenceError::Oversize(limits::Oversize {
            kind: limits::LimitKind::PlanTooLarge,
            dimension: limits::Dimension::Bytes,
            ..
        }))
    ));
}
