use super::*;
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope};
use crate::task_sync::origin::Enrollment;

fn retain_foreign_event(f: &mut Fixture, foreign: &CanonicalObject) {
    retain_foreign_record(f, foreign, "work_event");
}

fn retain_foreign_record(f: &mut Fixture, foreign: &CanonicalObject, foreign_kind: &str) {
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
        (foreign_kind, foreign),
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
fn shared_updates_foreign_originals_corroborate_prose_and_activity_without_local_authority() {
    let mut tree = candidate();
    tree.project.notes_included = true;
    let mut f = claimed_fixture_with_setup(&tree, |_, _| {});
    let item = f.store.get_work_item(f.selected).unwrap();
    let records = crate::storage::work::query::canonical_work_event_records_for_item(
        &f.store.connection,
        f.selected,
    )
    .unwrap();
    let mut foreign_start = records
        .iter()
        .find(|(_, event)| matches!(event.transition, crate::WorkTransition::Claimed { .. }))
        .unwrap()
        .1
        .clone();
    foreign_start.revision += 10;
    let foreign_start = CanonicalObject::mint(&foreign_start).unwrap();
    retain_foreign_event(&mut f, &foreign_start);
    let original_note = f
        .store
        .record_work_observation(
            &crate::domain::RecordWorkObservationRequest {
                status: false,
                project_id: item.project_id.clone(),
                work_id: item.work_id,
                expected_work_revision: item.revision,
                session_id: crate::SessionId("peer".into()),
                summary: "Foreign retained prose".into(),
                refs: Vec::new(),
                actor: crate::domain::non_holder_note_actor(actor("peer")),
                idempotency_key: "foreign-prose-fixture".into(),
                recorded_at: at(33),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut foreign_note: crate::domain::WorkObservation =
        f.store.get(&original_note.evidence).unwrap().unwrap();
    foreign_note.sequence += 10;
    let foreign = CanonicalObject::mint(&foreign_note).unwrap();
    retain_foreign_record(&mut f, &foreign, "work_observation");
    let profile = f.input.binding.profile.clone();
    let exported = f
        .store
        .export_task_sync_folder(&item.project_id, &profile)
        .unwrap();
    let mut incoming = exported.tree;
    let task = incoming
        .tasks
        .iter_mut()
        .find(|task| task.id == item.work_id)
        .unwrap();
    let start: crate::WorkEvent = foreign_start.decode().unwrap();
    task.activity.entry("remote".into()).or_default().insert(
        foreign_start.key().clone(),
        crate::task_sync::activity::ActivityNotice {
            activity_id: foreign_start.key().clone(),
            task_id: item.work_id,
            origin_machine: "remote".into(),
            actor: crate::task_sync::SharedActor::from_actor(&start.actor),
            state: crate::task_sync::activity::ActivityState::Started,
            state_revision: 1,
            event_id: foreign_start.key().clone(),
        },
    );
    task.notes.push(crate::task_sync::notes::SharedNote {
        id: foreign.key().clone(),
        kind: crate::task_sync::notes::SharedNoteKind::Generic,
        body: foreign_note.summary,
        author: crate::task_sync::SharedActor::from_actor(&foreign_note.actor),
        recorded_at: foreign_note.created_at,
        origin_machine: "remote".into(),
    });
    task.notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let before = f.store.get_work_item(item.work_id).unwrap();
    let mut bad = incoming.clone();
    let bad_task = bad
        .tasks
        .iter_mut()
        .find(|task| task.id == item.work_id)
        .unwrap();
    let entries = bad_task.activity.get_mut("remote").unwrap();
    let mut notice = entries.remove(foreign_start.key()).unwrap();
    notice.activity_id = foreign.key().clone();
    notice.event_id = foreign.key().clone();
    entries.insert(foreign.key().clone(), notice);
    let refused = f
        .store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &item.project_id,
                actor: &actor("importer"),
                tree: &bad,
                export_basis: &exported.export_basis,
                profile: &profile,
                complete: true,
                now: at(34),
            },
            "foreign-wrong-kind",
        )
        .unwrap();
    let refusal = refused
        .tasks
        .iter()
        .find(|task| task.task == item.work_id)
        .unwrap();
    assert_eq!(refusal.status, crate::storage::TaskSyncTaskStatus::Refused);
    assert_eq!(refusal.reason.as_deref(), Some("id_collision"));
    assert_eq!(f.store.get_work_item(item.work_id).unwrap(), before);
    let result = f
        .store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &item.project_id,
                actor: &actor("importer"),
                tree: &incoming,
                export_basis: &exported.export_basis,
                profile: &profile,
                complete: true,
                now: at(35),
            },
            "foreign-shared-update",
        )
        .unwrap();
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == crate::storage::TaskSyncTaskStatus::Applied),
        "{result:?}"
    );
    assert_eq!(f.store.get_work_item(item.work_id).unwrap(), before);
    assert_eq!(
        f.store
            .export_task_sync_folder(&item.project_id, &profile)
            .unwrap()
            .tree,
        incoming
    );
    assert!(matches!(
        f.store
            .get_typed_object::<crate::WorkEvent>(foreign_start.key(), "work_event"),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    assert!(matches!(
        f.store
            .get_typed_object::<crate::domain::WorkObservation>(foreign.key(), "work_observation"),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
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
