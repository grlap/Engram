use super::*;
use crate::task_sync::evidence::RecordEnvelope;

fn admitted<T: serde::Serialize>(f: &Fixture, kind: &str, value: &T, notes: bool) -> bool {
    let object = crate::CanonicalObject::freeze(value).unwrap();
    let record = RecordEnvelope::from_canonical(
        f.request.origin.clone(),
        ObjectId::mint(),
        kind,
        object.bytes().to_vec(),
    )
    .unwrap();
    let mut profile = f.request.profile.clone();
    profile.include_notes = notes;
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let result = super::super::disclosure::validate(
        &f.store.connection,
        &profile,
        std::iter::once(&record),
        &std::collections::BTreeMap::from([(f.work, vec![])]),
    );
    assert_eq!(record.canonical_bytes(), object.bytes());
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    if let Err(error) = &result {
        assert!(error.to_string().contains("disclosure refused"), "{error}");
    }
    result.is_ok()
}

#[test]
fn gate_and_blocker_text_obey_the_same_whole_record_disclosure() {
    let f = fixture();
    let mut gate: crate::WorkEvidence = serde_json::from_slice(&f.bytes).unwrap();
    gate.gate = Some(crate::domain::GateEvidenceRecord {
        schema_version: crate::domain::SCHEMA_VERSION,
        name: "checks".into(),
        passed: false,
        failed: vec!["lint".into()],
        previous: None,
    });
    assert!(admitted(&f, "work_evidence", &gate, true));
    assert!(!admitted(&f, "work_evidence", &gate, false));
    gate.gate.as_mut().unwrap().failed = vec!["See src/private.rs".into()];
    assert!(!admitted(&f, "work_evidence", &gate, true));
    gate.gate.as_mut().unwrap().failed.clear();
    gate.actor.reason = "See src/private.rs".into();
    assert!(!admitted(&f, "work_evidence", &gate, true));

    let mut blocker = crate::domain::WorkBlocker {
        blocker_id: "waiting".into(),
        work_id: f.work,
        kind: crate::domain::WorkBlockerKind::Manual,
        detail: "Waiting for input".into(),
        created_by: actor("runner"),
        created_at: at(5),
    };
    assert!(admitted(&f, "work_blocker", &blocker, true));
    assert!(!admitted(&f, "work_blocker", &blocker, false));
    blocker.detail = "See src/private.rs".into();
    assert!(!admitted(&f, "work_blocker", &blocker, true));
    blocker.detail = "Waiting for input".into();
    blocker.created_by.actor_id = "src/private.rs".into();
    assert!(!admitted(&f, "work_blocker", &blocker, true));
}

#[test]
fn source_payload_keys_and_nested_text_are_checked_without_rewriting() {
    let f = fixture();
    let mut source = crate::domain::WorkSourceSnapshot {
        schema_version: crate::domain::SCHEMA_VERSION,
        adapter_kind: "external".into(),
        canonical_ref: "item-one".into(),
        projected: crate::domain::WorkSourceProjection {
            title: Some("Shared task".into()),
            body: Some("Shared description".into()),
            status: Some("open".into()),
            owner: Some("team/runner".into()),
        },
        captured_at: at(5),
        source_revision: None,
        fingerprint: crate::CanonicalObject::freeze(&"observed source")
            .unwrap()
            .key()
            .as_str()
            .to_owned(),
        canonical_url: None,
        payload_hash: ObjectId::mint(),
        raw: std::collections::BTreeMap::from([(
            "fields".into(),
            serde_json::json!([{"body":"Shared detail"}]),
        )]),
    };
    assert!(admitted(&f, "work_source_snapshot", &source, true));
    assert!(!admitted(&f, "work_source_snapshot", &source, false));
    source.raw.insert(
        "fields".into(),
        serde_json::json!([{"body":"src/private.rs"}]),
    );
    assert!(!admitted(&f, "work_source_snapshot", &source, true));
    source.raw.clear();
    source
        .raw
        .insert("src/private.rs".into(), serde_json::json!(true));
    assert!(!admitted(&f, "work_source_snapshot", &source, true));
}

#[test]
fn planning_and_safe_attribution_remain_available_with_notes_off() {
    let f = fixture();
    let mut work = f.store.get_work_item(f.work).unwrap();
    work.created_by
        .provenance_chain
        .push(crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::DerivedFrom,
            source: crate::domain::DEFAULTED_OS_USER_ACTOR_SOURCE.into(),
            reference: Some(crate::domain::DEFAULTED_ACTOR_REFERENCE.into()),
        });
    assert!(admitted(&f, "work_item_revision", &work, false));
    work.created_by.source_tool = Some("src/private.rs".into());
    assert!(!admitted(&f, "work_item_revision", &work, false));
    work.created_by.source_tool = None;
    work.created_by
        .provenance_chain
        .last_mut()
        .unwrap()
        .reference = Some("src/private.rs".into());
    assert!(!admitted(&f, "work_item_revision", &work, false));
}
