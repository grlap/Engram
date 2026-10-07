//! An obligation a late observation opens on a finished run is history: the
//! readers show it as stored but owe nothing for it, a replayed receipt from
//! the live run reads as history too, and the run's seal still reads back,
//! binding exactly the obligations it froze.

use super::*;
use crate::verbs::NextInput;

/// A completed item whose seal froze one waived source-change obligation and
/// whose finished run then received a late source change, which opened an
/// obligation the seal never saw. It holds the service, the item, the claim
/// it held, the keyed checkpoint taken while the first obligation was open,
/// its receipt, the completion receipt, and the home they live in.
struct Finished {
    database: std::path::PathBuf,
    project: ProjectId,
    service: LocalWorkService,
    root: WorkItemSummary,
    claim: crate::domain::WorkClaim,
    checkpoint: WorkUpdateInput,
    checkpointed: WorkUpdateResult,
    sealed: WorkCompletedReceipt,
    // Last, so every store handle above closes before the home is removed.
    _directory: crate::test_support::TempHome,
}

fn finished(project: &str) -> Finished {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId(project.into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("historical-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Finished work", "historical-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "historical-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let claim = {
        let mut store = SqliteStore::open(&database).expect("fixture store");
        store.append_source_change_fixture(root.work_id, "early-change", at(2), "early-revision");
        store.claim_fixture(root.work_id)
    };
    // A receipt taken while the obligation is owed, under an explicit key.
    let checkpoint = WorkUpdateInput::Checkpoint {
        summary: "progress while a test is owed".into(),
        evidence: None,
        idempotency_key: "historical-checkpoint".into(),
    };
    let checkpointed = service
        .work_update(checkpoint.clone(), at(3))
        .expect("checkpoint");
    let WorkCompleteResult::Completed(sealed) = service
        .work_complete(
            completion_input("delivered", "historical-completion"),
            at(4),
        )
        .expect("complete")
    else {
        panic!("completion was refused");
    };
    SqliteStore::open(&database)
        .expect("fixture store")
        .append_late_source_change_fixture(
            root.work_id,
            &claim,
            "late-change",
            at(5),
            "late-revision",
        );
    Finished {
        database,
        project,
        service,
        root,
        claim,
        checkpoint,
        checkpointed,
        sealed,
        _directory: directory,
    }
}

/// The page's late obligation, stored as open, read back as history.
fn assert_historical(page: &WorkObligationPage) {
    assert!(page.historical, "{page:?}");
    assert_eq!(page.open_total, Some(0));
    assert!(
        page.items
            .iter()
            .any(|item| item.state == WorkObligationState::Open),
        "the late row is shown as stored: {page:?}"
    );
    assert!(
        page.items
            .iter()
            .all(|item| matches!(item.guidance, WorkObligationGuidance::None)
                && item.completion_action.is_none()),
        "{page:?}"
    );
}

/// No reminder offers owed work: none carries a phrase of the obligation
/// reminder table, whichever completion action it would name.
fn assert_nothing_owed(reminders: &[String]) {
    const OWED: [&str; 6] = [
        "have not run since",
        "has not run since",
        "is still owed",
        "is open —",
        "the host records the result",
        "more obligations are open",
    ];
    assert!(
        reminders
            .iter()
            .all(|reminder| OWED.iter().all(|owed| !reminder.contains(owed))),
        "{reminders:?}"
    );
}

fn verbs(finished: &Finished) -> AgentVerbs {
    AgentVerbs::new(
        finished.database.clone(),
        finished.project.clone(),
        "agent".into(),
        SessionId("historical-session".into()),
        Some("protocol-test".into()),
    )
}

// Inspect, next, show and the reminders read the late obligation as history:
// stored as open, owing nothing, with no owed-work guidance or count. The
// run's seal and the stored obligation rows are untouched by the reads.
#[test]
fn a_late_obligation_on_a_finished_run_is_read_as_history() {
    let finished = finished("historical-reads");
    let store = SqliteStore::open(&finished.database).expect("store");
    let run = store.get_work_run(finished.claim.run_id).expect("run");
    assert_eq!(run.state, crate::domain::WorkRunState::Completed);
    let seal = run.completion_seal.clone().expect("a sealed run");
    let rows = store
        .work_run_obligations(finished.claim.run_id)
        .expect("stored obligations");
    assert!(
        rows.iter()
            .any(|row| row.state == WorkObligationState::Open),
        "the late observation opened an obligation after the seal"
    );
    drop(store);

    let inspected = finished
        .service
        .work_inspect(&finished.root.short_ref, at(6))
        .expect("inspect");
    assert_historical(&inspected.view().obligation_page);
    let inspected_json = serde_json::to_value(&inspected).expect("inspect json");
    assert_eq!(inspected_json["obligation_page"]["historical"], true);
    assert_eq!(inspected_json["obligation_page"]["open_total"], 0);
    assert!(
        !inspected_json
            .to_string()
            .contains("record_verification_then_checkpoint")
    );

    let verbs = verbs(&finished);
    let shown = verbs.show(&finished.root.short_ref, at(7)).expect("show");
    let text = shown.text();
    assert!(text.contains("historical (run completed)"), "{text}");
    assert!(!text.contains("tests have not run"), "{text}");
    assert_eq!(shown.value["historical_open_obligations"], 1);
    assert!(shown.value.get("evaluation_obligations").is_none());
    assert_nothing_owed(&shown.reminders);

    let peeked = verbs
        .next(
            &NextInput {
                limit: None,
                peek: true,
                verbose: true,
                context_generation: None,
            },
            at(8),
        )
        .expect("next");
    let focus_page = &peeked.value["focus"]["obligation_page"];
    assert_eq!(focus_page["historical"], true, "{}", peeked.value);
    assert_eq!(focus_page["open_total"], 0, "{}", peeked.value);
    assert!(
        !peeked
            .value
            .to_string()
            .contains("record_verification_then_checkpoint"),
        "{}",
        peeked.value
    );
    assert_nothing_owed(&peeked.reminders);

    let store = SqliteStore::open(&finished.database).expect("store");
    let after = store.get_work_run(finished.claim.run_id).expect("run");
    assert_eq!(after.completion_seal, Some(seal));
    assert_eq!(
        store
            .work_run_obligations(finished.claim.run_id)
            .expect("stored obligations"),
        rows,
        "reads left the stored rows as they were"
    );
}

// A receipt stored while its run was live, replayed after the run finished,
// is returned as recorded, but its obligation page reads as history: the
// obligation it once offered as owed work is owed no longer, even once the
// item is reopened on a new run.
#[test]
fn a_replayed_receipt_from_the_live_run_reads_as_history() {
    let finished = finished("historical-replay");
    let owed = &finished.checkpointed.obligation_page;
    assert!(!owed.historical);
    assert_eq!(owed.open_total, Some(1), "{owed:?}");
    assert!(
        owed.items.iter().any(|item| matches!(
            item.guidance,
            WorkObligationGuidance::RecordVerificationThenCheckpoint { .. }
        )),
        "{owed:?}"
    );
    // Reopened, the item has a new live run; the replayed page still belongs
    // to the finished one, so it stays history.
    let reopen = WorkUpdateInput::Reopen {
        reason: "more to do".into(),
        idempotency_key: "historical-reopen".into(),
    };
    let reopened = finished
        .service
        .work_update(reopen.clone(), at(6))
        .expect("reopen");
    assert!(!reopened.obligation_page.historical);
    let replayed = finished
        .service
        .work_update(finished.checkpoint.clone(), at(7))
        .expect("the checkpoint replays");
    assert_eq!(replayed.receipt, finished.checkpointed.receipt);
    assert_eq!(replayed.operation, finished.checkpointed.operation);
    let page = &replayed.obligation_page;
    assert!(page.historical, "{page:?}");
    assert_eq!(page.open_total, Some(0));
    assert_eq!(page.items.len(), owed.items.len());
    assert!(
        page.items
            .iter()
            .all(|item| matches!(item.guidance, WorkObligationGuidance::None)),
        "{page:?}"
    );
    // The reopen's own receipt, replayed, is the new live run's empty page,
    // which owes nothing and is not history.
    let reopen_replayed = finished
        .service
        .work_update(reopen, at(8))
        .expect("the reopen replays");
    assert_eq!(reopen_replayed.receipt, reopened.receipt);
    assert!(!reopen_replayed.obligation_page.historical);
}

// A later `done` on the completed item reads its seal back. The late row is
// neither bound by the seal nor allowed to break the readback: the sealed
// page lists exactly the obligation the seal froze, as it did at completion.
#[test]
fn a_seal_reads_back_after_a_late_obligation() {
    let finished = finished("historical-readback");
    let frozen = &finished.sealed.obligation_page;
    assert_eq!(frozen.items.len(), 1, "the seal froze one obligation");
    assert_eq!(frozen.items[0].state, WorkObligationState::Waived);
    let WorkCompleteResult::Completed(replayed) = finished
        .service
        .work_complete(
            completion_input("delivered", "historical-completion-read-back"),
            at(6),
        )
        .expect("the seal reads back after a late obligation")
    else {
        panic!("the readback was refused");
    };
    assert_eq!(replayed.seal, finished.sealed.seal);
    let page = &replayed.obligation_page;
    assert!(page.historical);
    assert_eq!(page.open_total, Some(0));
    let identities = |page: &WorkObligationPage| {
        page.items
            .iter()
            .map(|item| {
                (
                    item.obligation_id,
                    item.definition.clone(),
                    item.resolution.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(identities(page), identities(frozen));
}

// The readback accepts a row the seal does not bind only when it was opened
// after the seal's cut. Read against a seal whose cut reaches the late row,
// at it or past it, the same row is a pre-cut row the seal omitted: damage.
#[test]
fn a_row_the_seal_omitted_before_its_cut_is_damage() {
    let finished = finished("historical-damage");
    let store = SqliteStore::open(&finished.database).expect("store");
    let seal: CompletionSeal = store
        .get(&finished.sealed.seal)
        .expect("seal read")
        .expect("the seal");
    let late = store
        .work_run_obligations(finished.claim.run_id)
        .expect("stored obligations")
        .into_iter()
        .find(|row| row.state == WorkObligationState::Open)
        .expect("the late row")
        .obligation
        .trigger_position
        .position;
    assert!(late > seal.completion_cut.position);
    crate::work_service::projection::sealed_work_obligation_page(&store, &seal)
        .expect("the real seal reads back");
    for cut in [late, late + 1] {
        let mut reaching = seal.clone();
        reaching.completion_cut.position = cut;
        let result =
            crate::work_service::projection::sealed_work_obligation_page(&store, &reaching);
        assert!(
            matches!(result, Err(StoreError::InvalidWorkProjection(_))),
            "cut {cut}: {result:?}"
        );
    }
}

// A keyed claim of a parent's next ready child answers with the child's
// page, not the parent's. Replayed after the child completed, while the
// parent's own run is still live, that page reads as the child's history.
#[test]
fn a_replayed_child_claim_reads_the_childs_run_as_history() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("historical-child-claim".into()),
        "agent".into(),
        SessionId("historical-child-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Parent", "historical-parent"), at(0))
            .expect("root"),
    );
    let WorkProposeResult::Decomposition(decomposition) = service
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    external_ref: None,
                    notes: Vec::new(),
                    key: "child".into(),
                    title: "child".into(),
                    outcome: "child outcome".into(),
                    acceptance: vec!["child accepted".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "historical-decomposition".into(),
            },
            at(1),
        )
        .expect("decompose")
    else {
        panic!("expected a decomposition");
    };
    let child = decomposition.children[0].clone();
    service
        .work_focus(&root.work_id.0.to_string(), at(2))
        .expect("focus the parent");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "historical-parent-claim".into(),
            },
            at(3),
        )
        .expect("claim the parent");
    let claim_child = |key: &str, second: i64| {
        service.work_update(
            WorkUpdateInput::ClaimNextReady {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: key.into(),
            },
            at(second),
        )
    };
    claim_child("historical-child-claim", 4).expect("claim the child");
    SqliteStore::open(&database)
        .expect("fixture store")
        .append_source_change_fixture(child.work_id, "child-change", at(5), "child-revision");
    // Sent from the parent, the renewal answers with the child's page while
    // its obligation is owed: the stored basis names the parent.
    service
        .work_focus(&root.work_id.0.to_string(), at(6))
        .expect("refocus the parent");
    let renewed = claim_child("historical-child-renewal", 7).expect("renew the child");
    assert_eq!(renewed.receipt.work_id, child.work_id);
    assert!(!renewed.obligation_page.historical);
    assert_eq!(renewed.obligation_page.open_total, Some(1));
    service
        .work_focus(&child.work_id.0.to_string(), at(8))
        .expect("focus the child");
    // The session holds the parent too, so the completion names the child.
    match service
        .work_complete_on(
            Some(&child.short_ref),
            completion_input("child delivered", "historical-child-done"),
            at(9),
        )
        .expect("complete the child")
    {
        WorkCompleteResult::Completed(_) => {}
        WorkCompleteResult::Refused(refusal) => {
            panic!("the child's completion was refused: {refusal:?}")
        }
    }
    let parent = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(root.work_id)
        .expect("parent");
    assert!(
        parent.active_run_id.is_some(),
        "the parent's run is still live"
    );

    let replayed = claim_child("historical-child-renewal", 10).expect("the renewal replays");
    assert_eq!(replayed.receipt, renewed.receipt);
    let page = &replayed.obligation_page;
    assert!(page.historical, "{page:?}");
    assert_eq!(page.open_total, Some(0));
    assert!(
        page.items
            .iter()
            .all(|item| matches!(item.guidance, WorkObligationGuidance::None)),
        "{page:?}"
    );
}

// A root created with a bound criterion under an explicit key answers with
// its focus page, whose binding obligation is owed. Replayed once the run
// has completed, the proposal's page reads as that run's history.
#[test]
fn a_replayed_root_proposal_reads_its_finished_run_as_history() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database,
        ProjectId("historical-root-proposal".into()),
        "agent".into(),
        SessionId("historical-root-session".into()),
        Some("protocol-test".into()),
    );
    let mut proposal = root_input("Bound root", "historical-bound-root");
    let WorkProposeInput::Root {
        acceptance_bindings,
        ..
    } = &mut proposal
    else {
        panic!("a root proposal");
    };
    *acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let created = service
        .work_propose(proposal.clone(), at(0))
        .expect("create the bound root");
    let WorkProposeResult::Root { focus, .. } = &created else {
        panic!("expected a root");
    };
    assert!(!focus.obligation_page.historical);
    assert_eq!(
        focus.obligation_page.open_total,
        Some(1),
        "{:?}",
        focus.obligation_page
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "historical-bound-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    // Dropping the binding retires its obligation, so the run can complete.
    service
        .work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    acceptance_bindings: Some(Vec::new()),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "historical-unbind".into(),
            },
            at(2),
        )
        .expect("drop the binding");
    let WorkCompleteResult::Completed(_) = service
        .work_complete(
            completion_input("delivered", "historical-bound-done"),
            at(3),
        )
        .expect("complete")
    else {
        panic!("completion was refused");
    };

    let replayed = service
        .work_propose(proposal, at(4))
        .expect("the proposal replays");
    let WorkProposeResult::Root { focus, .. } = &replayed else {
        panic!("expected a root");
    };
    assert!(
        focus.run.is_some(),
        "the focus names its run for a trimmed page"
    );
    let page = &focus.obligation_page;
    assert!(page.historical, "{page:?}");
    assert_eq!(page.open_total, Some(0));
    assert!(
        page.items
            .iter()
            .all(|item| matches!(item.guidance, WorkObligationGuidance::None)),
        "{page:?}"
    );
}

// A stored page whose rows were all trimmed away, with a positive count or
// with no count and rows left out, names no run of its own: the run the receipt names decides it,
// even once the item was reopened onto another run. Without a named run,
// the page is returned as recorded.
#[test]
fn a_trimmed_page_with_an_owed_count_follows_the_receipts_run() {
    let finished = finished("historical-trimmed");
    finished
        .service
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "more to do".into(),
                idempotency_key: "historical-trimmed-reopen".into(),
            },
            at(6),
        )
        .expect("reopen onto a new run");
    let store = SqliteStore::open(&finished.database).expect("store");
    let trimmed = || WorkObligationPage {
        omitted_count: 1,
        open_total: Some(1),
        ..WorkObligationPage::default()
    };
    let mut named = trimmed();
    crate::work_service::projection::replayed_obligation_page(
        &store,
        Some(finished.claim.run_id),
        &mut named,
    )
    .expect("the finished run decides");
    assert!(named.historical, "{named:?}");
    assert_eq!(named.open_total, Some(0));
    let mut unnamed = trimmed();
    crate::work_service::projection::replayed_obligation_page(&store, None, &mut unnamed)
        .expect("no run named");
    assert!(!unnamed.historical);
    assert_eq!(unnamed.open_total, Some(1));
    // A page stored before counts existed, trimmed to no rows, may hide open
    // ones: it too follows the named run, so it reminds of nothing owed.
    let countless = || WorkObligationPage {
        omitted_count: 1,
        open_total: None,
        ..WorkObligationPage::default()
    };
    let mut named = countless();
    crate::work_service::projection::replayed_obligation_page(
        &store,
        Some(finished.claim.run_id),
        &mut named,
    )
    .expect("the finished run decides");
    assert!(named.historical, "{named:?}");
    assert_eq!(named.open_total, Some(0));
    let mut unnamed = countless();
    crate::work_service::projection::replayed_obligation_page(&store, None, &mut unnamed)
        .expect("no run named");
    assert!(!unnamed.historical);
    assert_eq!(unnamed.open_total, None);
}

// Marking a replayed root's page as history adds a few bytes. A receipt that
// fitted the budget only just sheds recoverable focus context to fit again,
// and its page stays history: the recorded, owed page is never restored.
#[test]
fn a_replayed_root_at_the_budget_sheds_context_and_stays_history() {
    let finished = finished("historical-budget");
    let inspected = finished
        .service
        .work_inspect(&finished.root.short_ref, at(6))
        .expect("inspect");
    let mut focus = inspected.view().clone();
    let sample = focus
        .history
        .items
        .first()
        .cloned()
        .expect("the finished item has history");
    // A page trimmed to no rows that still counts one owed obligation.
    focus.obligation_page = WorkObligationPage {
        omitted_count: 1,
        open_total: Some(1),
        ..WorkObligationPage::default()
    };
    let mut replay = WorkProposeResult::Root {
        work: finished.root.clone(),
        focus: Box::new(focus),
    };
    let limit = crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES;
    let size = |replay: &WorkProposeResult| serde_json::to_vec(replay).expect("bytes").len();
    // Grow recoverable history, then pad the title, to a few bytes under.
    while size(&replay) + 2 * serde_json::to_vec(&sample).expect("bytes").len() < limit {
        let WorkProposeResult::Root { focus, .. } = &mut replay else {
            unreachable!()
        };
        focus.history.items.push(sample.clone());
    }
    let padding = limit - 4 - size(&replay);
    let WorkProposeResult::Root { work, .. } = &mut replay else {
        unreachable!()
    };
    work.title.push_str(&"t".repeat(padding));
    assert_eq!(size(&replay), limit - 4);

    // Historical stored proposals omitted an unbound focus's key. Reading
    // those bytes keeps the binding absent in memory, then emits explicit null.
    let mut stored = serde_json::to_value(&replay).expect("stored proposal");
    stored["focus"]
        .as_object_mut()
        .unwrap()
        .remove("control_binding");
    replay = serde_json::from_value(stored).expect("read historical proposal");
    assert_eq!(size(&replay), limit - 4);
    assert_eq!(
        serde_json::to_value(&replay).unwrap()["focus"].get("control_binding"),
        Some(&serde_json::Value::Null)
    );

    let store = SqliteStore::open(&finished.database).expect("store");
    let WorkProposeResult::Root { focus, .. } = &mut replay else {
        unreachable!()
    };
    crate::work_service::projection::replayed_obligation_page(
        &store,
        Some(finished.claim.run_id),
        &mut focus.obligation_page,
    )
    .expect("the finished run decides");
    assert!(size(&replay) > limit, "the mark pushes the receipt over");
    crate::work_service::propose::fit_replayed_root(&mut replay).expect("the replay fits");
    assert!(size(&replay) <= limit);
    let WorkProposeResult::Root { focus, .. } = &replay else {
        unreachable!()
    };
    assert!(focus.obligation_page.historical);
    assert_eq!(focus.obligation_page.open_total, Some(0));
}
