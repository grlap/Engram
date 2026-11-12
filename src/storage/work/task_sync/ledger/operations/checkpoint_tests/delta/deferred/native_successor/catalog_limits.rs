use super::*;
use crate::storage::work::search_budget::{Limits, unit_bytes_limit, with_limits};

#[test]
fn native_successor_applies_revision_and_blocker_above_the_catalog_search_limit() {
    let (mut f, mut retained, mut payload, _) = mapping_fixture(false);
    let before = state(&f.store);
    with_limits(
        Limits {
            unit_bytes: 4096,
            ..Limits::default()
        },
        || {
            let text = "x".repeat(unit_bytes_limit() + 8);
            let blocker = crate::task_sync::SharedBlocker {
                blocker_id: uuid::Uuid::new_v4().to_string(),
                kind: WorkBlockerKind::ExternalInput,
                detail: text.clone(),
                created_by: crate::task_sync::SharedActor::from_actor(&actor("foreign-planner")),
                created_at: at(12),
            };
            let document = payload
                .documents
                .get_mut(&f.selected)
                .unwrap()
                .as_mut()
                .unwrap();
            document.title = text.clone();
            document.blockers.push(blocker.clone());
            payload.output.inventory_root = payload
                .documents
                .values()
                .flatten()
                .try_fold(
                    crate::task_sync::inventory::InventoryRoot::ZERO,
                    crate::task_sync::inventory::InventoryRoot::including,
                )
                .unwrap();
            retained.request.intent.candidate.output = payload.output.clone();
            retained.request.intent.candidate.content_fingerprint =
                CanonicalObject::freeze(&payload).unwrap().key().clone();
            select_successor_effects_on(
                &f.store.connection,
                &f.input.binding.project,
                &payload,
                &mut retained,
            )
            .unwrap();
            let tx = f.store.begin_work_mutation().unwrap();
            let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
                &tx,
                &f.input.binding_id,
                &retained,
            )
            .unwrap();
            apply_successor_effects_on(&checked, &payload).unwrap();
            assert_eq!(load_work_item(&tx, f.selected).unwrap().title, text);
            let bytes: Vec<u8> = tx
                .query_row(
                    "SELECT blocker_json FROM work_blockers WHERE blocker_id=?1",
                    [&blocker.blocker_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<WorkBlocker>(&bytes).unwrap(),
                WorkBlocker {
                    blocker_id: blocker.blocker_id,
                    work_id: f.selected,
                    kind: blocker.kind,
                    detail: blocker.detail,
                    created_by: blocker.created_by.reported(),
                    created_at: blocker.created_at,
                }
            );
        },
    );
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
