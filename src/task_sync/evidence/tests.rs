use serde_json::{Value, json};

use super::{EvidenceError, EvidencePack, RecordEnvelope, RecordKind, limits};
use crate::{ObjectId, ProjectId};

fn evidence(summary: &str) -> Value {
    json!({
        "schema_version": crate::schema::SCHEMA_VERSION,
        "work_id": uuid::Uuid::new_v4(),
        "run_id": uuid::Uuid::new_v4(),
        "claim_id": uuid::Uuid::new_v4(),
        "claim_fence": 1,
        "summary": summary,
        "refs": [],
        "actor": {
            "actor_id": "fixture", "actor_kind": "agent", "assurance": "asserted",
            "run_id": null, "session_id": null, "source_tool": null,
            "source_skill": null, "provenance_chain": [], "reason": "fixture"
        },
        "created_at": "2026-01-01T00:00:00Z"
    })
}

fn envelope(origin: &ObjectId, id: &ObjectId, body: &Value) -> RecordEnvelope {
    RecordEnvelope::from_canonical(
        origin.clone(),
        id.clone(),
        "work_evidence",
        serde_json_canonicalizer::to_vec(body).unwrap(),
    )
    .unwrap()
}

fn pack(records: Vec<RecordEnvelope>) -> Result<EvidencePack, EvidenceError> {
    EvidencePack::prepare(
        ObjectId::mint(),
        ProjectId("project".into()),
        ObjectId::mint(),
        records,
    )
}

#[test]
fn decoder_requires_uuid_origins_and_pack_ids_without_reinterpreting_body_fingerprints() {
    use super::witnesses::{GoverningBasis, GoverningSnapshotWitness, SnapshotAvailability};
    let origin = ObjectId::mint();
    let id = ObjectId::mint();
    let fingerprint = ObjectId::from_canonical_bytes(b"governing content comparison");
    let body = GoverningSnapshotWitness {
        basis: GoverningBasis {
            evaluation: ObjectId::mint(),
            project_id: ProjectId("project".into()),
            work_id: crate::WorkId(uuid::Uuid::new_v4()),
            work_revision: 1,
            content_fingerprint: fingerprint.clone(),
        },
        snapshot: SnapshotAvailability::GoverningSnapshotUnavailable,
    };
    let bytes = serde_json_canonicalizer::to_vec(&body).unwrap();
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        id.clone(),
        RecordKind::GoverningSnapshotWitness.as_str(),
        bytes.clone(),
    )
    .unwrap();
    assert_eq!(record.canonical_bytes(), bytes);
    let input = EvidencePack::prepare(
        ObjectId::mint(),
        ProjectId("project".into()),
        origin,
        vec![record],
    )
    .unwrap();
    let encoded = input.to_bytes().unwrap();
    assert_eq!(
        EvidencePack::read(encoded.as_slice())
            .unwrap()
            .to_bytes()
            .unwrap(),
        encoded
    );
    {
        let mut wire: Value = serde_json::from_slice(&encoded).unwrap();
        wire["records"][0]["origin"] = json!(fingerprint);
        let malformed = serde_json_canonicalizer::to_vec(&wire).unwrap();
        assert!(matches!(
            EvidencePack::read(malformed.as_slice()),
            Err(EvidenceError::InvalidPack(_))
        ));
    }
    for field in ["pack_id", "publishing_origin"] {
        let mut wire: Value = serde_json::from_slice(&encoded).unwrap();
        wire[field] = json!(fingerprint);
        let malformed = serde_json_canonicalizer::to_vec(&wire).unwrap();
        assert!(matches!(
            EvidencePack::read(malformed.as_slice()),
            Err(EvidenceError::InvalidPack(_))
        ));
    }
}

#[test]
fn historical_record_ids_and_bytes_survive_envelope_and_pack_roundtrip() {
    let origin = ObjectId::mint();
    let body = evidence("retained historical evidence");
    // Emulate an existing historical record using the old identity scheme.
    // Production capture must preserve it, never derive a replacement identity.
    let historical = crate::CanonicalObject::freeze(&body).unwrap();
    assert_eq!(historical.key().as_str().len(), 64);
    let original = envelope(&origin, historical.key(), &body);
    assert_eq!(original.canonical_bytes(), historical.bytes());
    let prepared = pack(vec![original.clone()]).unwrap();
    let encoded = prepared.to_bytes().unwrap();
    let received = EvidencePack::read(encoded.as_slice()).unwrap();
    assert_eq!(received.pack_id(), prepared.pack_id());
    assert_eq!(received.records(), [original]);
    assert_eq!(received.records()[0].object_id(), historical.key());
    assert_eq!(received.records()[0].canonical_bytes(), historical.bytes());
    assert_eq!(received.to_bytes().unwrap(), encoded);
}

#[test]
fn envelopes_keep_absent_and_null_bytes_distinct_through_pack_replay() {
    let origin = ObjectId::mint();
    let absent = evidence("reported, not a verification");
    let mut null = absent.clone();
    null["gate"] = Value::Null;
    let first = envelope(&origin, &ObjectId::mint(), &absent);
    let second = envelope(&origin, &ObjectId::mint(), &null);
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    let first_bytes = first.canonical_bytes().to_vec();
    let second_bytes = second.canonical_bytes().to_vec();
    let first_id = first.object_id().clone();
    let second_id = second.object_id().clone();
    let original = pack(vec![second, first]).unwrap();
    let a_to_b = original.to_bytes().unwrap();
    let retained = EvidencePack::read(a_to_b.as_slice()).unwrap();
    let b_to_a = retained.to_bytes().unwrap();
    assert_eq!(a_to_b, b_to_a);
    assert_eq!(original.pack_id(), retained.pack_id());
    for (id, bytes) in [(first_id, first_bytes), (second_id, second_bytes)] {
        let record = retained
            .records()
            .iter()
            .find(|row| row.object_id() == &id)
            .unwrap();
        assert_eq!(record.canonical_bytes(), bytes);
        assert_eq!(record.origin(), &origin);
        assert_eq!(record.kind(), RecordKind::WorkEvidence);
    }
    let value: Value = serde_json::from_slice(&a_to_b).unwrap();
    let mut canonical = serde_json_canonicalizer::to_vec(&value).unwrap();
    canonical.push(b'\n');
    assert_eq!(a_to_b, canonical);
}

#[test]
fn decoder_is_kind_dispatched_and_payload_free_on_failure() {
    let origin = ObjectId::mint();
    let id = ObjectId::mint();
    assert!(matches!(RecordKind::parse("unknown_evidence"),
        Err(EvidenceError::UnknownKind(name)) if name == "unknown_evidence"));
    let body = serde_json_canonicalizer::to_vec(&evidence("secret source object")).unwrap();
    let mismatch =
        RecordEnvelope::from_canonical(origin.clone(), id.clone(), "verification_evidence", body)
            .unwrap_err();
    assert!(matches!(mismatch, EvidenceError::Json { .. }));
    assert!(!mismatch.to_string().contains("secret source object"));
    let unsafe_kind = RecordKind::parse("source_id\nsecret")
        .unwrap_err()
        .to_string();
    assert!(!unsafe_kind.contains("secret"));
    let mut malformed = evidence("original");
    malformed["claim_fence"] = json!("{\"source_id\":\"do not echo\"}");
    let error = RecordEnvelope::from_canonical(
        origin,
        id,
        "work_evidence",
        serde_json_canonicalizer::to_vec(&malformed).unwrap(),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("do not echo"));
}

#[test]
fn decoder_rejects_noncanonical_or_duplicate_body_fields() {
    let body = evidence("original");
    let pretty = serde_json::to_vec_pretty(&body).unwrap();
    let origin = ObjectId::mint();
    let id = ObjectId::mint();
    assert!(matches!(
        RecordEnvelope::from_canonical(origin.clone(), id.clone(), "work_evidence", pretty,),
        Err(EvidenceError::NonCanonical(_))
    ));
    let mut duplicate = serde_json_canonicalizer::to_string(&body).unwrap();
    duplicate.insert_str(1, "\"summary\":\"discarded\",");
    assert!(matches!(
        RecordEnvelope::from_canonical(origin, id, "work_evidence", duplicate.into_bytes(),),
        Err(EvidenceError::NonCanonical(_))
    ));
}

#[test]
fn conflicting_original_identity_refuses_before_a_pack_is_exposed() {
    let origin = ObjectId::mint();
    let id = ObjectId::mint();
    let first = evidence("first result");
    let mut changed = first.clone();
    changed["summary"] = json!("changed result");
    assert!(
        matches!(pack(vec![envelope(&origin, &id, &first), envelope(&origin, &id, &changed)]),
        Err(EvidenceError::RecordConflict(conflict)) if conflict == id)
    );
    let a = envelope(&origin, &ObjectId::mint(), &first);
    let b = envelope(&origin, &ObjectId::mint(), &first);
    assert_eq!(
        pack(vec![a, b]).unwrap().records().len(),
        2,
        "equal bodies under independently minted ids remain separate originals"
    );
}

#[test]
fn record_bound_accepts_exact_boundary_and_refuses_the_next_byte() {
    let origin = ObjectId::mint();
    let id = ObjectId::mint();
    let mut body = evidence("");
    let fixed = serde_json_canonicalizer::to_vec(&body).unwrap().len();
    body["summary"] = json!("x".repeat(limits::MAX_RECORD_BYTES - fixed));
    let bytes = serde_json_canonicalizer::to_vec(&body).unwrap();
    assert_eq!(bytes.len(), limits::MAX_RECORD_BYTES);
    let record =
        RecordEnvelope::from_canonical(origin.clone(), id.clone(), "work_evidence", bytes).unwrap();
    assert_eq!(record.canonical_bytes().len(), limits::MAX_RECORD_BYTES);
    let error = RecordEnvelope::from_canonical(
        origin,
        id.clone(),
        "work_evidence",
        vec![b'x'; limits::MAX_RECORD_BYTES + 1],
    )
    .unwrap_err();
    assert!(matches!(error, EvidenceError::Oversize(limits::Oversize {
        kind: limits::LimitKind::RecordTooLarge, actual, subject: Some(ref subject), ..
    }) if actual == limits::MAX_RECORD_BYTES + 1 && subject == &id));
}

#[test]
fn encoded_pack_bound_counts_json_escaping_and_record_count() {
    let origin = ObjectId::mint();
    let body = evidence("\n".repeat(3 * 1024 * 1024).as_str());
    let record = envelope(&origin, &ObjectId::mint(), &body);
    let other = envelope(&origin, &ObjectId::mint(), &body);
    assert!(record.canonical_bytes().len() < limits::MAX_RECORD_BYTES);
    assert!(matches!(
        pack(vec![record, other]),
        Err(EvidenceError::Oversize(limits::Oversize {
            kind: limits::LimitKind::PackTooLarge,
            dimension: limits::Dimension::Bytes,
            ..
        }))
    ));
    let small = envelope(&origin, &ObjectId::mint(), &evidence("small"));
    assert!(matches!(
        pack(vec![small; limits::MAX_PACK_RECORDS + 1]),
        Err(EvidenceError::Oversize(limits::Oversize {
            kind: limits::LimitKind::PackTooLarge,
            dimension: limits::Dimension::Objects,
            ..
        }))
    ));
}

#[test]
fn bounded_reader_stops_after_one_sentinel_byte_and_never_accepts_truncation() {
    let mut input = std::io::Cursor::new(vec![b' '; limits::MAX_PACK_BYTES + 32]);
    assert!(matches!(
        EvidencePack::read(&mut input),
        Err(EvidenceError::Oversize(_))
    ));
    assert_eq!(input.position(), (limits::MAX_PACK_BYTES + 1) as u64);
    let original = pack(vec![]).unwrap().to_bytes().unwrap();
    assert!(EvidencePack::read(&original[..original.len() - 2]).is_err());
    let mut unsafe_header: Value = serde_json::from_slice(&original).unwrap();
    unsafe_header["format"] = json!({"source_id": "secret"});
    let error =
        EvidencePack::read(serde_json::to_vec(&unsafe_header).unwrap().as_slice()).unwrap_err();
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn staging_reservation_is_atomic_at_the_budget_boundary() {
    let mut budget = limits::StagingBudget::default();
    budget.reserve(limits::MAX_STAGING_BYTES).unwrap();
    assert_eq!(budget.retained_bytes(), limits::MAX_STAGING_BYTES);
    let refusal = budget.reserve(1).unwrap_err();
    assert_eq!(refusal.kind, limits::LimitKind::StagingBudgetExceeded);
    assert_eq!(refusal.actual, limits::MAX_STAGING_BYTES + 1);
    assert_eq!(budget.retained_bytes(), limits::MAX_STAGING_BYTES);
}
