//! Caller-facing publication and retry observations read the remote themselves.

use super::*;
use crate::task_sync::exchange::{
    attempt::GitIo,
    delta::attempt::{DeltaAttemptIo, test_git_io},
};
use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};

fn last_observation(store: &SqliteStore, binding: &ObjectId) -> PublicationObservation {
    let events = store.task_sync_exchange_events(binding).unwrap();
    let Some((_, event)) = events.last() else {
        panic!("the binding has recorded events")
    };
    let Transition::PushObserved { observation, .. } = &event.transition else {
        panic!("the last event is an observation: {:?}", event.transition)
    };
    observation.clone()
}

fn confirmed(commit: &str) -> OutgoingProgress {
    OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: commit.to_owned(),
    })
}

fn unknown() -> OutgoingProgress {
    OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None })
}

/// A caller's confirmation of a commit the selected remote does not hold
/// refuses with nothing recorded; an observation records the actual remote
/// tip as unknown; once the remote holds the exact retained commit the owner's
/// own read confirms it. The same holds for the one resolved retry.
pub(in super::super) async fn observes_the_selected_remote_tip(root: &Path) {
    let repos = Repositories::new(root);
    let super::prepare::Prepared {
        mut store,
        data,
        target,
        id,
        binding,
        intent,
        selection,
        ..
    } = Box::pin(super::prepare::prepare(&repos)).await;
    data.advance_retained(&selection.commit, None).unwrap();
    let cancel = Cancellation::default();
    let record = |progress| exchange::DeltaPublicationRequest {
        binding_id: &id,
        binding: &binding,
        intent: &intent,
        selection: &selection,
        progress,
    };
    let (_, fresh) = exchange::record_delta_publication(
        &mut store,
        &record(OutgoingProgress::PushStarted),
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap();
    assert!(fresh);
    let before = store.task_sync_exchange_events(&id).unwrap();
    let error = exchange::record_delta_publication(
        &mut store,
        &record(confirmed(&selection.commit.oid)),
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("not the exact retained commit"),
        "{error}"
    );
    assert_eq!(store.task_sync_exchange_events(&id).unwrap(), before);
    exchange::record_delta_publication(
        &mut store,
        &record(unknown()),
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap();
    let PublicationObservation::Unknown { observed_tip } = last_observation(&store, &id) else {
        panic!("an absent or different remote tip stays unknown")
    };
    assert_ne!(observed_tip.as_deref(), Some(selection.commit.oid.as_str()));
    // Publish the exact commit through the confined transport, as the caller
    // that owns this start would, then observe it.
    let mut io = test_git_io(GitIo {
        target: target.clone(),
        data: Repositories::data(&repos.left),
        limits: RunLimits::default(),
        started: std::time::Instant::now(),
        cancel: &cancel,
        operation: intent.operation,
    });
    io.push(&selection.commit.oid).await.unwrap();
    exchange::record_delta_publication(
        &mut store,
        &record(unknown()),
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(
        last_observation(&store, &id),
        PublicationObservation::Confirmed {
            observed_tip: selection.commit.oid.clone(),
        }
    );
    assert!(store.verify_all().unwrap().is_healthy());
    println!("data-branch-workflow complete");
}

/// The one resolved retry of the actual refused publication, resumed from
/// its handoff, observes the remote itself against its merged commit, exactly
/// as the original publication does.
pub(in super::super) async fn observes_the_retry_remote_tip(root: &Path) {
    let mut resumed = super::handoff::resume(root);
    let super::handoff::Resumed {
        repos,
        store,
        binding_id: id,
        binding,
        intent,
        selection,
        peer,
        attempt,
        ..
    } = &mut resumed;
    let outgoing = exchange::DeltaOutgoingSelection {
        binding_id: &*id,
        binding: &*binding,
        intent: &*intent,
        commit: &*selection,
    };
    let data = Repositories::data(&repos.left);
    // The same selected target the first phase's binding captured.
    let target = ExchangeTarget::capture(
        &TargetSelection {
            repository: data.checkout.clone(),
            remote: "origin".into(),
            reference: fixture::BRANCH.into(),
        },
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        target.fingerprint().unwrap(),
        binding.target.configuration_fingerprint
    );
    let cancel = Cancellation::default();
    let (input, _, merged) = Box::pin(super::recovery::resolve(
        store,
        repos,
        &outgoing,
        peer,
        attempt.clone(),
    ))
    .await;
    let (_, fresh) = exchange::record_delta_retry_progress(
        store,
        &input,
        &OutgoingProgress::PushStarted,
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap();
    assert!(fresh);
    let before = store.task_sync_exchange_events(id).unwrap();
    let error = exchange::record_delta_retry_progress(
        store,
        &input,
        &confirmed(&merged.oid),
        RunLimits::default(),
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("not the exact retained commit"),
        "{error}"
    );
    assert_eq!(store.task_sync_exchange_events(id).unwrap(), before);
    exchange::record_delta_retry_progress(store, &input, &unknown(), RunLimits::default(), &cancel)
        .await
        .unwrap();
    let PublicationObservation::Unknown { observed_tip } = last_observation(store, id) else {
        panic!("the competing remote tip stays unknown")
    };
    assert_eq!(observed_tip.as_deref(), Some(peer.oid.as_str()));
    let mut io = test_git_io(GitIo {
        target,
        data: Repositories::data(&repos.left),
        limits: RunLimits::default(),
        started: std::time::Instant::now(),
        cancel: &cancel,
        operation: intent.operation,
    });
    io.push(&merged.oid).await.unwrap();
    exchange::record_delta_retry_progress(store, &input, &unknown(), RunLimits::default(), &cancel)
        .await
        .unwrap();
    assert_eq!(
        last_observation(store, id),
        PublicationObservation::Confirmed {
            observed_tip: merged.oid.clone(),
        }
    );
    resumed.finish(root);
}
