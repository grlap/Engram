//! Real canonical/private storage with scripted transport facts, never a remote.

use super::super::super::attempt::{Io, PushOutcome};
use super::*;
use crate::task_sync::exchange::{
    Cancellation, PublicationAttempt, RunLimits, delta::retry::publish_with, publish_delta_retry,
};

#[tokio::test(flavor = "current_thread")]
async fn delta_retry_dispatch_confirms_once_and_replays_offline_without_retained_files() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let mut io = Io::new(PushOutcome::Confirm);
    let outcome = publish_with(&mut f.store, &input, &mut io).await.unwrap();
    assert!(matches!(outcome, PublicationAttempt::Confirmed { .. }));
    assert_eq!(io.pushes, 1);
    let before = state(&f.store);
    let OutgoingReconciliation::DeltaResolved { resolved, .. } = &input.recipe else {
        unreachable!()
    };
    std::fs::remove_file(resolved.candidate.directory.join("payload.json")).unwrap();
    assert_eq!(
        Box::pin(publish_delta_retry(
            &mut f.store,
            &input,
            RunLimits::default(),
            &Cancellation::default()
        ))
        .await
        .unwrap(),
        outcome
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(io.pushes, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn delta_retry_dispatch_unknown_observes_without_repeating_push_then_confirms_exact_tip() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let mut io = Io::new(PushOutcome::Unknown);
    assert!(matches!(
        publish_with(&mut f.store, &input, &mut io).await.unwrap(),
        PublicationAttempt::Unknown { .. }
    ));
    assert_eq!(io.pushes, 1);
    assert!(matches!(
        publish_with(&mut f.store, &input, &mut io).await.unwrap(),
        PublicationAttempt::Unknown { .. }
    ));
    assert_eq!(io.pushes, 1);
    io.tip = Some(selected_commit(&input).oid.clone());
    assert!(matches!(
        publish_with(&mut f.store, &input, &mut io).await.unwrap(),
        PublicationAttempt::Confirmed { .. }
    ));
    assert_eq!(io.pushes, 1);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[tokio::test(flavor = "current_thread")]
async fn delta_retry_dispatch_stops_after_second_known_failure_without_another_recipe_or_push() {
    for ancestry in [false, true] {
        let mut f = ExportFixture::new(2);
        let input = admitted(&mut f);
        let mut io = Io::new(PushOutcome::NonFastForward);
        if ancestry {
            io.tip = Some(
                CanonicalObject::freeze(&"second ancestry tip")
                    .unwrap()
                    .key()
                    .to_string(),
            );
            io.ancestor = false;
        }
        let outcome = publish_with(&mut f.store, &input, &mut io).await.unwrap();
        assert!(matches!(outcome, PublicationAttempt::Stopped { .. }));
        assert_eq!(io.pushes, usize::from(!ancestry));
        let polls = io.observations;
        io.refuse = true;
        assert_eq!(
            publish_with(&mut f.store, &input, &mut io).await.unwrap(),
            outcome
        );
        assert_eq!(io.observations, polls);
        assert_eq!(io.pushes, usize::from(!ancestry));
        let completed:Option<String>=f.store.connection.query_row("SELECT last_outgoing_completed_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",[f.id.as_str()],|row|row.get(0)).unwrap();
        assert_eq!(completed, None);
        assert!(f.store.verify_all().unwrap().is_healthy());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_retry_dispatch_existing_start_never_dispatches_and_selection_drift_refuses_before_start()
 {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let mut io = Io::new(PushOutcome::Confirm);
    io.refuse = true;
    let before = state(&f.store);
    assert!(publish_with(&mut f.store, &input, &mut io).await.is_err());
    assert_eq!(io.pushes, 0);
    assert_eq!(state(&f.store), before);
    io.refuse = false;
    f.store
        .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
        .unwrap();
    assert!(matches!(
        publish_with(&mut f.store, &input, &mut io).await.unwrap(),
        PublicationAttempt::Unknown { .. }
    ));
    assert_eq!(io.pushes, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn delta_retry_refusal_with_a_lost_fetch_still_settles_with_its_one_stop() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let mut io = Io::new(PushOutcome::NonFastForward);
    io.fetch_failures = 1;
    assert!(publish_with(&mut f.store, &input, &mut io).await.is_err());
    assert_eq!(io.pushes, 1);
    assert!(f.store.verify_all().unwrap().is_healthy());
    let outcome = publish_with(&mut f.store, &input, &mut io).await.unwrap();
    assert!(matches!(outcome, PublicationAttempt::Stopped { .. }));
    assert_eq!(io.pushes, 1);
    assert_eq!(
        publish_with(&mut f.store, &input, &mut io).await.unwrap(),
        outcome
    );
    assert_eq!(io.pushes, 1);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
