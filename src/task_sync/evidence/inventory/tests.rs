use super::*;
use crate::{ProjectId, task_sync::evidence::EvidencePack};
use serde_json::json;

fn actor() -> serde_json::Value {
    json!({"actor_id":"fixture","actor_kind":"agent","assurance":"asserted",
        "run_id":null,"session_id":null,"source_tool":null,"source_skill":null,
        "provenance_chain":[],"reason":"fixture"})
}

fn envelope(
    origin: &ObjectId,
    id: &ObjectId,
    kind: &str,
    body: &serde_json::Value,
) -> RecordEnvelope {
    RecordEnvelope::from_canonical(
        origin.clone(),
        id.clone(),
        kind,
        serde_json_canonicalizer::to_vec(body).unwrap(),
    )
    .unwrap()
}

fn enrollment(id: &ObjectId) -> RecordEnvelope {
    envelope(
        id,
        id,
        "task_sync_producing_origin",
        &json!({
            "project_id":"project", "predecessor_origin_id":null,
            "enrollment_operation_id":uuid::Uuid::new_v4(), "actor":actor(),
            "enrolled_at":"2026-01-01T00:00:00Z"
        }),
    )
}

fn note(origin: &ObjectId, id: &ObjectId, previous: Option<&ObjectId>) -> RecordEnvelope {
    envelope(
        origin,
        id,
        "work_evidence",
        &json!({
            "schema_version":crate::schema::SCHEMA_VERSION,
            "work_id":uuid::Uuid::new_v4(),"run_id":uuid::Uuid::new_v4(),
            "claim_id":uuid::Uuid::new_v4(),"claim_fence":1,"summary":"note",
            "refs":[ObjectId::mint().to_string()],"actor":actor(),"created_at":"2026-01-01T00:00:00Z",
            "gate":previous.map(|id| json!({"schema_version":crate::schema::SCHEMA_VERSION,
                "name":"focused","passed":false,"failed":["test"],"previous":id}))
        }),
    )
}

fn pack(origin: &ObjectId, records: Vec<RecordEnvelope>) -> EvidencePack {
    EvidencePack::prepare(
        ObjectId::mint(),
        ProjectId("project".into()),
        origin.clone(),
        records,
    )
    .unwrap()
}

#[test]
fn typed_inventory_orders_cross_pack_history_without_treating_refs_as_links() {
    let origin = ObjectId::mint();
    let earlier = ObjectId::mint();
    let later = ObjectId::mint();
    let packs = [
        pack(&origin, vec![note(&origin, &later, Some(&earlier))]),
        pack(
            &origin,
            vec![note(&origin, &earlier, None), enrollment(&origin)],
        ),
    ];
    let plan = plan_batch(&packs, |_| Ok(None)).unwrap();
    let position = |id: &ObjectId| {
        plan.components
            .iter()
            .position(|part| part.objects.contains(id))
            .unwrap()
    };
    assert!(position(&origin) < position(&earlier));
    assert!(position(&earlier) < position(&later));
    let mut all = plan
        .components
        .iter()
        .flat_map(|part| part.objects.clone())
        .collect::<Vec<_>>();
    all.sort();
    let mut expected = vec![origin, earlier, later];
    expected.sort();
    assert_eq!(all, expected);
}

#[test]
fn known_exact_member_cannot_bypass_its_missing_typed_context() {
    let origin = ObjectId::mint();
    let missing = ObjectId::mint();
    let record = note(&origin, &ObjectId::mint(), Some(&missing));
    let known = record.clone();
    let packs = [pack(&origin, vec![enrollment(&origin), record])];
    assert!(
        matches!(plan_batch(&packs, |id| Ok((id==known.object_id()).then(|| known.clone()))),
        Err(EvidenceError::Dependency { object, problem:"required original is unavailable" }) if object==missing)
    );
}

#[test]
fn typed_inventory_refuses_wrong_kind_before_returning_any_component() {
    let origin = ObjectId::mint();
    let wrong = ObjectId::mint();
    let packs = [pack(
        &origin,
        vec![
            enrollment(&origin),
            enrollment(&wrong),
            note(&origin, &ObjectId::mint(), Some(&wrong)),
        ],
    )];
    assert!(matches!(plan_batch(&packs, |_| Ok(None)),
        Err(EvidenceError::Dependency { object, problem:"required original kind differs" }) if object==wrong));
}

#[test]
fn typed_inventory_excludes_source_fingerprints_and_execution_scalars() {
    let origin = ObjectId::mint();
    let rules = ObjectId::mint();
    let fingerprint = crate::CanonicalObject::freeze(&"action contents")
        .unwrap()
        .key()
        .clone();
    let record = envelope(
        &origin,
        &ObjectId::mint(),
        "execution_observation",
        &json!({
            "schema_version":crate::schema::SCHEMA_VERSION,"project_id":"project",
            "binding":{"root_execution_id":uuid::Uuid::new_v4(),"work_id":uuid::Uuid::new_v4(),
            "run_id":uuid::Uuid::new_v4(),"work_revision":1,"claim_id":uuid::Uuid::new_v4(),"claim_fence":1},
            "session_id":"fixture-session","grant_id":"grant","observation_id":"observation",
        "action_fingerprint":fingerprint,"effect":"observe","outcome":"succeeded",
            "source_changed":false,"reported_source_change":null,"obligation_rule_set":rules,
            "source_basis":null,"observed_at":null,"actor":actor(),"recorded_at":"2026-01-01T00:00:00Z"
        }),
    );
    let links = dependencies(&record).unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.contains(&RequiredObject {
        object_id: origin,
        kinds: vec![RecordKind::ProducingOrigin]
    }));
    assert!(links.contains(&RequiredObject {
        object_id: rules,
        kinds: vec![RecordKind::ObligationRuleSet]
    }));
    assert!(!links.iter().any(|link| link.object_id == fingerprint));
}
