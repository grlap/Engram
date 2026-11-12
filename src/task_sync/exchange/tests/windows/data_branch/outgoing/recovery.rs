use super::*;
use crate::task_sync::ledger::{OutgoingFailure, OutgoingReconciliation, RetainedCommit};

pub(super) struct Recovery {
    pub outcome: exchange::PublicationAttempt,
    pub input: exchange::InspectedDeltaResolution,
    pub candidate: PathBuf,
    pub commit: RetainedCommit,
}

pub(super) fn arm(repos: &Repositories) -> RetainedCommit {
    let _phase = super::super::timing::phase("outgoing-arm-race");
    super::super::workflow::stage(&repos.right, &super::super::workflow::empty_tree());
    let data = Repositories::data(&repos.right);
    let peer = Repositories::commit(
        &repos.right,
        uuid::Uuid::new_v4(),
        Vec::new(),
        "Advance concurrent data before strict publication",
    );
    Repositories::data(&repos.right)
        .advance_retained(&peer, None)
        .unwrap();
    fs::write(
        repos.right.parent().unwrap().join("race.json"),
        serde_json::to_vec(&serde_json::json!({
            "repository": data.checkout, "commit": peer.oid,
        }))
        .unwrap(),
    )
    .unwrap();
    peer
}

pub(super) async fn finish(
    store: &mut SqliteStore,
    repos: &Repositories,
    original: &exchange::DeltaOutgoingSelection<'_>,
    peer: &RetainedCommit,
    attempt: exchange::PublicationAttempt,
) -> Recovery {
    let _phase = super::super::timing::phase("outgoing-recovery");
    let (input, candidate, commit) = Box::pin(resolve(store, repos, original, peer, attempt)).await;
    let outcome = Box::pin(exchange::publish_delta_retry(
        store,
        &input,
        super::publication_limits(),
        &Cancellation::default(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        exchange::PublicationAttempt::Confirmed { .. }
    ));
    let preparations = store
        .task_sync_exchange_events(original.binding_id)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| matches!(event.transition, Transition::DeltaPrepared { .. }))
        .count();
    assert_eq!(
        preparations, 1,
        "resolved retry must stay under one original preparation"
    );
    Recovery {
        outcome,
        input,
        candidate,
        commit,
    }
}

/// Record the agent's resolution of an actual non-fast-forward refusal and
/// advance the data checkout to its merged commit, ready for its one retry.
pub(super) async fn resolve(
    store: &mut SqliteStore,
    repos: &Repositories,
    original: &exchange::DeltaOutgoingSelection<'_>,
    peer: &RetainedCommit,
    attempt: exchange::PublicationAttempt,
) -> (exchange::InspectedDeltaResolution, PathBuf, RetainedCommit) {
    let exchange::PublicationAttempt::ReconciliationRequired {
        failure,
        fetched_ref,
    } = attempt
    else {
        panic!("actual rejected strict delta push must request reconciliation: {attempt:?}")
    };
    assert!(matches!(failure, OutgoingFailure::NonFastForward { .. }));
    assert_eq!(failure.fetched_tip(), peer.oid);
    let data = Repositories::data(&repos.left);
    assert_eq!(
        fixture::line(&data.checkout, &["rev-parse", &fetched_ref]),
        peer.oid
    );
    let mut resolved = original.intent.clone();
    resolved.operation = uuid::Uuid::new_v4();
    resolved.key = resolved.operation.to_string();
    resolved.expected_head = Some(original.commit.commit.oid.clone());
    resolved.candidate.operation = resolved.operation;
    resolved.candidate.directory =
        task_sync::retention::delta::candidate_directory(&data.git_directory, resolved.operation);
    resolved.candidate.changed.clear();
    let DeltaBasis::Outgoing { export_before, .. } = &mut resolved.basis else {
        unreachable!()
    };
    export_before.anchor = PreparedExportAnchor::Prepared {
        event: original.commit.prepared.clone(),
    };
    export_before.membership = original.intent.candidate.output.clone();
    export_before.cut = original.intent.local_cut.clone();
    // The competing commit carries the same empty data snapshot as the prior
    // remote tip. The original selected task stays, so resolving changes no
    // task document relative to the original native preparation.
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: original.binding.project.clone(),
        operation: resolved.operation,
        output: resolved.candidate.output.clone(),
        documents: BTreeMap::new(),
    };
    resolved.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    task_sync::retention::delta::retain_candidate(
        original.binding_id,
        original.binding,
        &resolved,
        &payload,
    )
    .unwrap();
    let commit = Repositories::commit(
        &repos.left,
        resolved.operation,
        vec![original.commit.commit.oid.clone(), peer.oid.clone()],
        "Retain exact strict delta reconciliation",
    );
    let candidate = resolved.candidate.directory.clone();
    let recipe = OutgoingReconciliation::DeltaResolved {
        failure,
        resolved: Box::new(resolved),
        payload: Box::new(payload),
        commit: Box::new(commit.clone()),
    };
    let request = exchange::DeltaResolutionRequest {
        binding_id: original.binding_id,
        binding: original.binding,
        original: original.intent,
        selection: original.commit,
        recipe: &recipe,
    };
    let input = exchange::inspect_delta_resolution(
        store,
        &request,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let recorded = exchange::record_delta_resolution(
        store,
        &input,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        exchange::record_delta_resolution(
            store,
            &input,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap(),
        recorded
    );
    data.advance_retained(&commit, Some(&original.commit.commit.oid))
        .unwrap();
    (input, candidate, commit)
}
