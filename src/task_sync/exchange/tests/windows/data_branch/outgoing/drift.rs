use super::*;
use crate::task_sync::exchange::{
    attempt::GitIo,
    delta::{
        InspectedDelta,
        attempt::{DeltaAttemptIo, test_git_io},
    },
};

pub(in super::super) async fn refuses_captured_head_drift(root: &Path) {
    let repos = Repositories::new(root);
    let prepared = Box::pin(super::prepare::prepare(&repos)).await;
    let before = prepared
        .store
        .task_sync_exchange_events(&prepared.id)
        .unwrap();
    let payload = task_sync::retention::delta::read_candidate(
        &prepared.id,
        &prepared.binding,
        &prepared.intent,
    )
    .unwrap()
    .unwrap();
    let input = InspectedDelta {
        binding_id: prepared.id,
        binding: prepared.binding,
        intent: prepared.intent,
        payload,
    };
    let cancel = Cancellation::default();
    let mut io = test_git_io(GitIo {
        target: prepared.target.clone(),
        data: prepared.data.clone(),
        limits: RunLimits::default(),
        started: std::time::Instant::now(),
        cancel: &cancel,
        operation: input.intent.operation,
    });
    // The new HEAD is a valid selected outgoing commit, but it differs from
    // the exact worktree captured by this invocation. Selection admission alone
    // must not turn this drift into permission to record progress or push.
    prepared
        .data
        .advance_retained(&prepared.selection.commit, None)
        .unwrap();
    let requests = fs::read(root.join("requests")).unwrap_or_default();
    let error = io
        .revalidate(&input, &prepared.selection)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("data worktree changed since observation")
    );
    assert_eq!(
        prepared
            .store
            .task_sync_exchange_events(&input.binding_id)
            .unwrap(),
        before
    );
    assert_eq!(
        fs::read(root.join("requests")).unwrap_or_default(),
        requests
    );
    // A fresh invocation captures that same allowed HEAD and is admitted.
    let mut fresh = test_git_io(GitIo {
        target: prepared.target,
        data: Repositories::data(&repos.left),
        limits: RunLimits::default(),
        started: std::time::Instant::now(),
        cancel: &cancel,
        operation: input.intent.operation,
    });
    fresh.revalidate(&input, &prepared.selection).await.unwrap();
    println!("data-branch-workflow complete");
}
