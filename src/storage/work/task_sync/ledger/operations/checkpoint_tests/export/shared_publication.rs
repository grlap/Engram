use super::*;
use crate::storage::work::test_support::{actor, claim};
use crate::task_sync::{CapturedTask, activity::ActivityState, progress::ExportProfile};

fn planning(f: &ExportFixture, id: crate::WorkId) -> crate::task_sync::TaskDocument {
    let capture = f
        .store
        .capture_task_documents(
            &f.binding.project,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
            [id],
        )
        .unwrap();
    let CapturedTask::Present(task) = &capture.documents[&id] else {
        panic!("native task expected");
    };
    task.as_ref().clone()
}

#[test]
fn shared_updates_publication_witness_requires_confirmation_and_resolves_unchanged_predecessor_member()
 {
    let mut f = ExportFixture::with_alias(1, Some("host-A"));
    let id = *f.payload.documents.keys().next().unwrap();
    let item = f.store.get_work_item(id).unwrap();
    let live = claim(&mut f.store, &item, "local", "claim", 36, 100);
    let captured = f
        .store
        .capture_task_documents(&f.binding.project, &f.binding.profile, [id])
        .unwrap();
    let CapturedTask::Present(native) = &captured.documents[&id] else {
        panic!("native task expected");
    };
    let old = f.payload.documents[&id].as_ref().unwrap();
    f.payload.output.inventory_root = f
        .payload
        .output
        .inventory_root
        .excluding(old)
        .unwrap()
        .including(native)
        .unwrap();
    f.payload
        .documents
        .insert(id, Some(native.as_ref().clone()));
    f.intent.candidate.output = f.payload.output.clone();
    f.intent.candidate.content_fingerprint =
        CanonicalObject::freeze(&f.payload).unwrap().key().clone();
    let cut = producing_origin::cut_on(&f.store.connection, &f.binding.project).unwrap();
    f.intent.local_cut.position.position = cut.position;
    f.intent.local_cut.anchor = cut.anchor;
    assert!(planning(&f, id).activity.is_empty());
    let prepared = f.event(Transition::DeltaPrepared {
        intent: Box::new(f.intent.clone()),
        payload: Box::new(f.payload.clone()),
    });
    assert!(
        planning(&f, id).activity.is_empty(),
        "preparation is not a publication witness"
    );
    let first = f.confirm_prepared(prepared);
    assert_eq!(planning(&f, id).activity, native.activity);
    let item = f.store.get_work_item(id).unwrap();
    f.store
        .release_work(
            &crate::ReleaseWorkRequest {
                work_id: id,
                run_id: live.run_id,
                expected_work_revision: item.revision,
                holder: live.holder.clone(),
                claim_id: live.claim_id,
                claim_fence: live.fence,
                reason: "End episode".into(),
                waiver_reason: Some("No contribution".into()),
                actor: actor("local"),
                idempotency_key: "release".into(),
                released_at: at(41),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(
        planning(&f, id).activity["host-A"]
            .values()
            .all(|notice| notice.state == ActivityState::Released)
    );
    let item = f.store.get_work_item(id).unwrap();
    claim(&mut f.store, &item, "next", "new-private-episode", 42, 100);
    assert_eq!(
        planning(&f, id).activity["host-A"].len(),
        1,
        "planning-only does not disclose a new episode"
    );
    // The latest confirmed delta has no changed task. Its complete output
    // still includes the original member, selected through the prior anchor.
    let (intent, payload) = f.next_intent(&first);
    f.intent = intent;
    f.payload = payload;
    f.confirmed();
    let exported = planning(&f, id);
    assert_eq!(exported.activity["host-A"].len(), 1);
    assert!(
        exported.activity["host-A"]
            .values()
            .all(|notice| notice.state == ActivityState::Released)
    );
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
