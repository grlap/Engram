use super::*;
use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};

#[test]
fn outgoing_delta_still_refuses_changed_selected_native_work() {
    let mut f = ExportFixture::new(2);
    let work = *f.intent.candidate.changed.first().unwrap();
    let item = f.store.get_work_item(work).unwrap();
    f.store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: work,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Changed selected export".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: f.intent.actor.clone(),
                idempotency_key: "intervening-export-edit".into(),
                updated_at: at(36),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&f.store);
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert!(!f.intent.candidate.directory.exists());
}

/// An interrupted preparation retained its validated candidate, but its SQL
/// commit failed. A later edit of a selected native task must not turn the
/// exact retry into a permanent refusal that also holds custody from every
/// other exchange: the retry completes with the retained bytes, a changed
/// request under that operation still refuses, and exchange continues.
#[test]
fn retained_outgoing_preparation_recovers_after_sql_failure_and_selected_native_edit() {
    let mut f = ExportFixture::new(2);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_retained_export BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.transition')='delta_prepared'
        BEGIN SELECT RAISE(ABORT,'injected export preparation failure'); END;").unwrap();
    let error = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected export preparation failure"),
        "{error}"
    );
    let retained = retention::delta::read_candidate(&f.id, &f.binding, &f.intent)
        .unwrap()
        .unwrap();
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_retained_export")
        .unwrap();
    let work = *f.intent.candidate.changed.first().unwrap();
    let item = f.store.get_work_item(work).unwrap();
    f.store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: work,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Edited after the interrupted preparation".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: f.intent.actor.clone(),
                idempotency_key: "edit-after-interrupted-export".into(),
                updated_at: at(41),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut changed = f.intent.clone();
    changed.key.push_str(" changed");
    let before = state(&f.store);
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &changed, at(42))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(43))
        .unwrap();
    let event = f
        .store
        .task_sync_delta_outgoing_events(&f.id, &f.intent)
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == &prepared)
        .unwrap()
        .1;
    let crate::task_sync::ledger::Transition::DeltaPrepared { payload, .. } = event.transition
    else {
        panic!("the retry records the outgoing preparation");
    };
    assert_eq!(*payload, retained);
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(retained)
    );
    let (input, selection) = commit::selected(&mut f);
    assert_eq!(selection.prepared, prepared);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(44))
        .unwrap();
    for (progress, second) in [
        (OutgoingProgress::PushStarted, 45),
        (
            OutgoingProgress::Observed(PublicationObservation::Confirmed {
                observed_tip: selection.commit.oid.clone(),
            }),
            46,
        ),
    ] {
        f.store
            .record_task_sync_delta_progress_once(&input, &selection, progress, at(second))
            .unwrap();
    }
    super::super::audit_healthy(&mut f.store, &f.binding, &f.id);
}

#[test]
fn retained_outgoing_preparation_recovers_after_sql_failure_and_unrelated_work() {
    let mut f = ExportFixture::new(2);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_retained_export BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.transition')='delta_prepared'
        BEGIN SELECT RAISE(ABORT,'injected export preparation failure'); END;").unwrap();
    let before = state(&f.store);
    let error = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected export preparation failure"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    let retained = retention::delta::read_candidate(&f.id, &f.binding, &f.intent)
        .unwrap()
        .unwrap();
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_retained_export")
        .unwrap();
    super::super::unrelated(&mut f.store, &f.binding.project);
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(81))
        .unwrap();
    assert_eq!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(82))
            .unwrap(),
        prepared
    );
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(retained)
    );
    let (input, selection) = commit::selected(&mut f);
    assert_eq!(selection.prepared, prepared);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(83))
        .unwrap();
    f.store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(84),
        )
        .unwrap();
    f.store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::Observed(PublicationObservation::Confirmed {
                observed_tip: selection.commit.oid.clone(),
            }),
            at(85),
        )
        .unwrap();
    super::super::audit_healthy(&mut f.store, &f.binding, &f.id);
}
