//! A completion that names no item follows the agent words' rule for a bare
//! `done`: refused, recording nothing, while the session holds a claim on
//! other work, unless it repeats a completion already admitted on the focus.

use super::*;
use crate::storage::{ImplicitFocusState, test_database_shape_snapshot};

/// Fields drop in declaration order, so the home is declared last: it is
/// removed only after the services have closed the store.
struct Fixture {
    database: std::path::PathBuf,
    service: LocalWorkService,
    _directory: crate::test_support::TempHome,
}

fn service_for(database: &std::path::Path, project: &str, session: &str) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        ProjectId(project.into()),
        "agent".into(),
        SessionId(session.into()),
        Some("protocol-test".into()),
    )
}

fn fixture(project: &str) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = service_for(&database, project, "bare-completion-session");
    Fixture {
        database,
        service,
        _directory: directory,
    }
}

fn root(service: &LocalWorkService, title: &str, now: DateTime<Utc>) -> WorkItemSummary {
    proposed_root(
        service
            .work_propose(root_input(title, &format!("{title}-root")), now)
            .expect("root"),
    )
}

/// Claims `work` by name, which also focuses it.
fn claim(service: &LocalWorkService, work: &WorkItemSummary, ttl: i64, now: DateTime<Utc>) {
    service
        .work_update_on(
            Some(&work.short_ref),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(ttl),
                recovery_reason: None,
                idempotency_key: String::new(),
            },
            now,
        )
        .expect("claim by name");
}

fn completed(result: Result<WorkCompleteResult, StoreError>) -> WorkCompletedReceipt {
    match result.expect("completion") {
        WorkCompleteResult::Completed(receipt) => receipt,
        WorkCompleteResult::Refused(refusal) => panic!("expected a completion, got {refusal:?}"),
    }
}

fn shape(database: &std::path::Path) -> impl PartialEq + std::fmt::Debug {
    let connection = rusqlite::Connection::open(database).expect("connection");
    test_database_shape_snapshot(&connection).expect("snapshot")
}

/// The attempt is refused with the bare-target ambiguity naming `focus` and
/// every held item, and the store is exactly as it was.
fn refused_as_ambiguous(
    database: &std::path::Path,
    focus: &WorkItemSummary,
    held: &[&WorkItemSummary],
    attempt: impl FnOnce() -> Result<WorkCompleteResult, StoreError>,
) {
    let before = shape(database);
    let result = attempt();
    let Err(StoreError::WorkBareTargetAmbiguous(ambiguity)) = &result else {
        panic!("expected a bare-target refusal, got {result:?}");
    };
    assert_eq!(ambiguity.operation, "work_complete");
    assert_eq!(ambiguity.focus, focus.short_ref);
    let expected: Vec<String> = held.iter().map(|item| item.short_ref.clone()).collect();
    assert_eq!(ambiguity.held, expected);
    assert_eq!(ambiguity.total, held.len());
    assert_eq!(ambiguity.more, 0);
    assert_eq!(shape(database), before, "the refusal recorded nothing");
}

/// The attempt is refused with the implicit-target conflict naming `focus`,
/// its state and the held items, and the store is exactly as it was.
fn refused_as_conflict(
    database: &std::path::Path,
    focus: &WorkItemSummary,
    state: ImplicitFocusState,
    held: &[&WorkItemSummary],
    attempt: impl FnOnce() -> Result<WorkCompleteResult, StoreError>,
) {
    let before = shape(database);
    let result = attempt();
    let Err(StoreError::WorkImplicitTargetConflict(conflict)) = &result else {
        panic!("expected an implicit-target refusal, got {result:?}");
    };
    assert_eq!(conflict.operation, "work_complete");
    assert_eq!(conflict.focus, focus.short_ref);
    assert_eq!(conflict.focus_state, state);
    let expected: Vec<String> = held.iter().map(|item| item.short_ref.clone()).collect();
    assert_eq!(conflict.held, expected);
    assert_eq!(conflict.more, 0);
    assert_eq!(shape(database), before, "the refusal recorded nothing");
}

fn sorted(mut items: Vec<&WorkItemSummary>) -> Vec<&WorkItemSummary> {
    items.sort_by(|left, right| left.short_ref.cmp(&right.short_ref));
    items
}

#[test]
fn a_bare_completion_beside_another_live_claim_is_refused_recording_nothing() {
    let Fixture {
        database, service, ..
    } = &fixture("bare-completion-two-claims");
    let first = root(service, "First", at(0));
    claim(service, &first, 3_600, at(1));
    let second = root(service, "Second", at(2));
    claim(service, &second, 3_600, at(3));
    let held = sorted(vec![&first, &second]);
    for key in ["", "keyed-bare-completion"] {
        refused_as_ambiguous(database, &second, &held, || {
            service.work_complete_on(None, completion_input("delivered", key), at(4))
        });
        refused_as_ambiguous(database, &second, &held, || {
            service.work_complete(completion_input("delivered", key), at(4))
        });
    }
    // Naming the item completes it while the other claim stays live.
    let receipt = completed(service.work_complete_on(
        Some(&second.short_ref),
        completion_input("delivered", "named"),
        at(5),
    ));
    assert_eq!(receipt.work_id, second.work_id);
    // With one claim left, a bare completion of the held focus completes it.
    service.work_focus(&first.short_ref, at(6)).expect("focus");
    let receipt = completed(service.work_complete(completion_input("delivered", ""), at(7)));
    assert_eq!(receipt.work_id, first.work_id);
}

#[test]
fn a_bare_completion_on_a_focus_it_does_not_hold_is_refused_while_other_work_is_held() {
    let Fixture {
        database, service, ..
    } = &fixture("bare-completion-unheld-focus");
    let held = root(service, "Held", at(0));
    claim(service, &held, 3_600, at(1));
    let added = root(service, "Added", at(2));
    refused_as_conflict(
        database,
        &added,
        ImplicitFocusState::Unclaimed,
        &[&held],
        || service.work_complete(completion_input("delivered", ""), at(3)),
    );
    // A focus another session holds names that state.
    let peer = service_for(database, "bare-completion-unheld-focus", "peer-session");
    claim(&peer, &added, 3_600, at(4));
    refused_as_conflict(
        database,
        &added,
        ImplicitFocusState::HeldElsewhere,
        &[&held],
        || service.work_complete(completion_input("delivered", "keyed"), at(5)),
    );
}

#[test]
fn only_this_session_s_live_claims_in_this_project_count() {
    let Fixture {
        database, service, ..
    } = &fixture("bare-completion-live-claims");
    let short = root(service, "Short", at(0));
    claim(service, &short, 10, at(1));
    let focus = root(service, "Focus", at(2));
    claim(service, &focus, 3_600, at(3));
    // Another session's claim and this session's claim in another project
    // are not this session's claims here.
    let peer = service_for(database, "bare-completion-live-claims", "peer-session");
    let peer_work = root(&peer, "Peer", at(4));
    claim(&peer, &peer_work, 3_600, at(5));
    let elsewhere = service_for(database, "another-project", "bare-completion-session");
    let elsewhere_work = root(&elsewhere, "Elsewhere", at(6));
    claim(&elsewhere, &elsewhere_work, 3_600, at(7));
    // The short claim is live until the second it expires.
    refused_as_ambiguous(database, &focus, &sorted(vec![&short, &focus]), || {
        service.work_complete(completion_input("delivered", ""), at(10))
    });
    let receipt = completed(service.work_complete(completion_input("delivered", ""), at(11)));
    assert_eq!(receipt.work_id, focus.work_id);
}

#[test]
fn a_finished_bare_completion_replays_on_its_restored_focus_beside_another_claim() {
    for key in ["finished-completion", ""] {
        let Fixture {
            database, service, ..
        } = &fixture(&format!("bare-completion-finished-{key}"));
        let original = root(service, "Original", at(0));
        claim(service, &original, 3_600, at(1));
        let input = completion_input("original delivered", key);
        let first = completed(service.work_complete(input.clone(), at(2)));
        let other = root(service, "Other", at(3));
        claim(service, &other, 3_600, at(4));
        // Under its key, a different focus is still the replay's target
        // conflict; a keyless request on another focus is another act.
        if !key.is_empty() {
            assert!(matches!(
                service.work_complete(input.clone(), at(5)),
                Err(StoreError::WorkOperationIdempotencyConflict { .. })
            ));
        }
        service
            .work_focus(&original.short_ref, at(6))
            .expect("restore the original focus");
        let replay = completed(service.work_complete(input.clone(), at(7)));
        assert_eq!(replay.seal, first.seal, "{key}");
        assert_eq!(replay.work_id, original.work_id, "{key}");
        // Naming the original target replays too, with the other claim held.
        let named =
            completed(service.work_complete_on(Some(&original.short_ref), input.clone(), at(8)));
        assert_eq!(named.seal, first.seal, "{key}");
        // Another intent cannot borrow the admitted act's exemption.
        service
            .work_focus(&original.short_ref, at(9))
            .expect("restore the original focus");
        refused_as_conflict(
            database,
            &original,
            ImplicitFocusState::NotOpen,
            &[&other],
            || service.work_complete(completion_input("another intent", key), at(10)),
        );
    }
}

#[test]
fn an_interrupted_bare_completion_replays_its_seal_beside_another_claim() {
    let Fixture {
        database, service, ..
    } = &fixture("bare-completion-interrupted");
    let original = root(service, "Original", at(0));
    claim(service, &original, 3_600, at(1));
    let input = completion_input("original delivered", "interrupted-completion");
    let seal = commit_completion_core_without_finishing(service, &input, at(2));
    let seal_id = service.store().expect("store").stored_seal_id(&seal);
    let other = root(service, "Other", at(3));
    claim(service, &other, 3_600, at(4));
    assert!(matches!(
        service.work_complete(input.clone(), at(5)),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    service
        .work_focus(&original.short_ref, at(6))
        .expect("restore the original focus");
    // Another intent under the same key is refused as a new act.
    refused_as_conflict(
        database,
        &original,
        ImplicitFocusState::NotOpen,
        &[&other],
        || {
            service.work_complete(
                completion_input("another intent", "interrupted-completion"),
                at(7),
            )
        },
    );
    let replay = completed(service.work_complete(input.clone(), at(8)));
    assert_eq!(replay.work_id, original.work_id);
    assert_eq!(replay.run_id, seal.run_id);
    assert_eq!(replay.seal, seal_id);
    assert_eq!(replay.completed_at, seal.completed_at);
    let finished = completed(service.work_complete(input, at(9)));
    assert_eq!(finished.seal, seal_id);
}

/// Records the durable attempt of `input` on the focus, and with `capture`
/// also its evidence and checkpoint, without sealing anything.
fn stage_unsealed_attempt(
    service: &LocalWorkService,
    input: &WorkCompleteInput,
    capture: bool,
    now: DateTime<Utc>,
) {
    let mut store = service.store().expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, now)
        .expect("completion basis");
    let intent = service.protocol_intent(input);
    let raw_key = service
        .effective_idempotency_key(
            &input.idempotency_key,
            "work_complete",
            &basis,
            &intent,
            now,
        )
        .expect("completion key");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_complete",
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })
        .expect("pending completion attempt");
    if capture {
        let work = basis.focused_work.clone().expect("focused work");
        let claim = service
            .live_protocol_claim(&basis, &work, now)
            .expect("completion claim");
        let evidence = LocalWorkService::completion_evidence_basis(&store, &claim, &input.evidence)
            .expect("completion evidence basis");
        service
            .prepare_completion_evidence(
                &mut store,
                CompletionEvidencePlan {
                    work: &work,
                    claim: &claim,
                    capture: input.capture.as_ref(),
                    evidence,
                    base_key: &raw_key,
                    now,
                },
            )
            .expect("capture and checkpoint");
    }
}

#[test]
fn an_unsealed_attempt_is_not_an_admitted_completion() {
    for capture in [false, true] {
        let project = format!("bare-completion-unsealed-{capture}");
        let Fixture {
            database, service, ..
        } = &fixture(&project);
        let original = root(service, "Original", at(0));
        claim(service, &original, 3_600, at(1));
        let input = completion_input("original delivered", "unsealed-completion");
        stage_unsealed_attempt(service, &input, capture, at(2));
        let other = root(service, "Other", at(3));
        claim(service, &other, 3_600, at(4));
        service
            .work_focus(&original.short_ref, at(5))
            .expect("restore the original focus");
        refused_as_ambiguous(
            database,
            &original,
            &sorted(vec![&original, &other]),
            || service.work_complete(input.clone(), at(6)),
        );
        // Released, the other claim no longer stands beside it: the pending
        // attempt resumes and seals.
        service
            .work_update_on(
                Some(&other.short_ref),
                WorkUpdateInput::Release {
                    reason: "done with it".into(),
                    waiver_reason: Some("nothing was done on it".into()),
                    idempotency_key: String::new(),
                },
                at(7),
            )
            .expect("release the other claim");
        service
            .work_focus(&original.short_ref, at(8))
            .expect("restore the original focus");
        let receipt = completed(service.work_complete(input, at(9)));
        assert_eq!(receipt.work_id, original.work_id, "capture {capture}");
    }
}
