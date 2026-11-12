use super::*;
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope};
use crate::task_sync::origin::Enrollment;

fn retain_foreign_event(f: &mut Fixture, foreign: &CanonicalObject) {
    let origin = ObjectId::mint();
    let enrolled = CanonicalObject::identified(
        &origin,
        &Enrollment {
            project_id: f.input.binding.project.clone(),
            predecessor_origin_id: None,
            enrollment_operation_id: uuid::Uuid::new_v4(),
            actor: actor("foreign"),
            enrolled_at: at(32),
        },
    )
    .unwrap();
    let mut records = BTreeMap::new();
    for (kind, object) in [
        ("task_sync_producing_origin", &enrolled),
        ("work_event", foreign),
    ] {
        records.insert(
            object.key().clone(),
            RecordEnvelope::from_canonical(
                origin.clone(),
                object.key().clone(),
                kind,
                object.bytes().to_vec(),
            )
            .unwrap(),
        );
    }
    let mut pending = vec![foreign.key().clone()];
    while let Some(id) = pending.pop() {
        let record = &records[&id];
        for dependency in crate::task_sync::evidence::inventory::dependencies(record).unwrap() {
            if records.contains_key(&dependency.object_id) {
                continue;
            }
            let (kind, bytes): (String, Vec<u8>) = f
                .store
                .connection
                .query_row(
                    "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
                    [dependency.object_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let record = RecordEnvelope::from_canonical(
                origin.clone(),
                dependency.object_id.clone(),
                &kind,
                bytes,
            )
            .unwrap();
            pending.push(dependency.object_id.clone());
            records.insert(dependency.object_id, record);
        }
    }
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        f.input.binding.project.clone(),
        origin,
        records.into_values().collect(),
    )
    .unwrap();
    f.store
        .retain_evidence_pack(&f.input.binding.project, &pack, &ObjectId::mint())
        .unwrap();
}

#[test]
fn retained_foreign_claim_event_does_not_replace_native_deferred_planning_proof() {
    let mut f = claimed_fixture();
    let item = f.store.get_work_item(f.selected).unwrap();
    let claim = f.store.claim_fixture(f.selected);
    let native: String = f.store.connection.query_row(
        "SELECT object_id FROM objects WHERE object_kind='work_event' AND json_extract(canonical_json,'$.claim.claim_id')=?1 ORDER BY json_extract(canonical_json,'$.revision') DESC LIMIT 1",
        [claim.claim_id.0.to_string()], |row| row.get(0),
    ).unwrap();
    let bytes: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [&native],
            |row| row.get(0),
        )
        .unwrap();
    let mut event: crate::WorkEvent = serde_json::from_slice(&bytes).unwrap();
    event.revision += 10;
    let foreign = CanonicalObject::mint(&event).unwrap();
    retain_foreign_event(&mut f, &foreign);
    let original = f.apply().unwrap();
    let Transition::PlanningDeferred {
        blocking_claims, ..
    } = &original.event.transition
    else {
        panic!("native active claim must defer incoming planning");
    };
    assert_eq!(blocking_claims, &[ObjectId::from_stored(native).unwrap()]);
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), item);
    assert_eq!(f.apply().unwrap(), original);
    let retained: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [foreign.key().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, foreign.bytes());
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
