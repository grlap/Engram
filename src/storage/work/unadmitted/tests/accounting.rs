//! Accounting under `account_if_eligible`: an eligible reported change
//! enters source-change accounting at its own run-feed position and opens the
//! selected rule set's obligations; the lifecycle keeps an ineligible one as
//! audit-only with its reason; a repeat links to the change it repeats; and
//! no reported change accounts nothing.

use super::refusals::name_root;
use super::*;
use crate::domain::{NamedRootBindingKind, SourceChangeDetection, WorkObligationState};
use crate::storage::WorkObligationCompletionAction;
use crate::storage::work::WorkObligationRecord;
use crate::storage::work::completion::load_work_obligation_records_on;
use crate::storage::work::feeds::{latest_source_mutation_on, newest_measured_sighting_on};

/// The project's current policy, named for accounting.
pub(super) fn account(fixture: &Fixture) -> ObservationPolicyBasis {
    let policy =
        SqliteStore::load_active_control_policy(&fixture.store.connection).expect("active policy");
    ObservationPolicyBasis::AccountIfEligible {
        project_policy_epoch: policy.epoch,
        policy: policy.policy_id,
        obligation_rule_set: policy.obligation_rule_set,
    }
}

/// An inter-turn change from `from` to `to` in workspace-A, sent for
/// accounting at the run's current head.
pub(super) fn accounted(
    fixture: &Fixture,
    key: &str,
    from: &str,
    to: &str,
) -> ExecutionObserveInput {
    let mut input = inter_turn_change(fixture, key);
    input.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: content_change(from, to),
    };
    input.policy_basis = account(fixture);
    input
}

/// [`accounted`], observed at `second`.
pub(super) fn observe_accounted(
    fixture: &mut Fixture,
    key: &str,
    from: &str,
    to: &str,
    second: i64,
) -> Result<ExecutionObservationReceipt, StoreError> {
    let input = accounted(fixture, key, from, to);
    observe(fixture, input, second)
}

pub(super) fn obligations(fixture: &Fixture) -> Vec<WorkObligationRecord> {
    load_work_obligation_records_on(&fixture.store.connection, fixture.claim.run_id, None)
        .expect("obligations")
}

/// The receipt names a new change anchored to the record itself.
pub(super) fn self_anchored(receipt: &ExecutionObservationReceipt) -> bool {
    receipt.accounting
        == ObservationAccounting::SourceChange {
            source_change: Some(receipt.observation.clone()),
        }
}

fn audit(reason: ObservationAuditReason) -> ObservationAccounting {
    ObservationAccounting::AuditOnly { reason }
}

#[test]
fn accounting_names_the_current_policy_or_is_refused_with_nothing_recorded() {
    let mut fixture = fixture();
    let before = footprint(&fixture.store);
    let ObservationPolicyBasis::AccountIfEligible {
        project_policy_epoch,
        policy,
        obligation_rule_set,
    } = account(&fixture)
    else {
        unreachable!("account names a policy")
    };
    for wrong in [
        ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch: ProjectPolicyEpoch(project_policy_epoch.0 + 1),
            policy: policy.clone(),
            obligation_rule_set: obligation_rule_set.clone(),
        },
        ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch,
            policy: crate::ObjectId::from_canonical_bytes(b"another policy"),
            obligation_rule_set: obligation_rule_set.clone(),
        },
        ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch,
            policy: policy.clone(),
            obligation_rule_set: crate::ObjectId::from_canonical_bytes(b"another rule set"),
        },
    ] {
        let mut input = accounted(&fixture, "account", "rev-a", "rev-b");
        input.policy_basis = wrong;
        let refused = observe(&mut fixture, input, 7).expect_err("refused");
        assert!(
            matches!(
                refused,
                StoreError::ExecutionObservationPolicyBasisMismatch(_)
            ),
            "{refused:?}"
        );
        assert_eq!(
            crate::host::store_error_code(&refused),
            "execution_observation_policy_basis_mismatch"
        );
        assert_eq!(footprint(&fixture.store), before);
    }
    // The refusals reserved nothing: the key records the current policy's
    // request.
    let receipt = observe_accounted(&mut fixture, "account", "rev-a", "rev-b", 8)
        .expect("the key was never committed");
    assert!(self_anchored(&receipt), "{receipt:?}");
}

#[test]
fn an_eligible_change_accounts_opens_its_obligations_and_moves_freshness_without_authority() {
    let mut fixture = fixture();
    let head = run_head(&fixture);
    let receipt =
        observe_accounted(&mut fixture, "change", "rev-a", "rev-b", 7).expect("accounted");
    assert!(self_anchored(&receipt), "{receipt:?}");
    assert_eq!(receipt.position.position, head + 1);
    // The stored record anchors to itself by leaving the id out.
    assert_eq!(
        stored(&fixture.store, &receipt.observation).accounting,
        ObservationAccounting::SourceChange {
            source_change: None
        }
    );
    // The stock rule set opens one test obligation, triggered by the record
    // at its own position.
    let records = obligations(&fixture);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        receipt.opened_obligations,
        vec![records[0].definition_id.clone()]
    );
    let opened = &records[0].obligation;
    assert_eq!(opened.triggering_observation, receipt.observation);
    assert_eq!(opened.trigger_position, receipt.position);
    assert_eq!(opened.rule.rule_id, crate::control::SOURCE_CHANGE_RULE_ID);
    assert_eq!(opened.opened_at, at(7));
    assert_eq!(opened.work_revision, fixture.binding.work_revision);
    assert_eq!(records[0].state, WorkObligationState::Open);
    // Freshness moves: the run's latest source change is the record.
    let (position, latest) =
        latest_source_mutation_on(&fixture.store.connection, fixture.claim.run_id, i64::MAX)
            .expect("latest change")
            .expect("a change");
    assert_eq!(position, receipt.position.position);
    assert_eq!(latest.record, receipt.observation);
    assert!(!latest.admitted);
    assert_eq!(latest.label, format!("unadmitted:{}", receipt.observation));
    assert_eq!(
        latest.reported_source_change,
        Some(SourceChangeDetection::ContentComparison)
    );
    // It enters foreign-change classification: without a named root it is
    // the run's own change, which completion records as untested.
    let actions = fixture
        .store
        .work_obligation_completion_actions(&[opened])
        .expect("actions");
    assert_eq!(actions, [WorkObligationCompletionAction::DoneWaives]);
    // No grant, and the claim is neither renewed nor changed.
    assert_eq!(
        count(&fixture.store, "SELECT COUNT(*) FROM control_turn_grants"),
        0
    );
    let claim_after = load_work_claim_optional(&fixture.store.connection, fixture.claim.run_id)
        .expect("claim")
        .expect("a claim");
    assert_eq!(claim_after.expires_at, fixture.claim.expires_at);
    assert_eq!(claim_after.fence, fixture.claim.fence);
    assert_eq!(claim_after.revision, fixture.claim.revision);
}

#[test]
fn a_change_outside_the_named_root_is_classified_against_the_root() {
    let mut fixture = fixture();
    let bound = name_root(&mut fixture, 1, NamedRootBindingKind::Bound, 4, 4);
    let mut input = accounted(&fixture, "foreign", "rev-a", "rev-b");
    input.root_basis.state = NamedRootState::Bound {
        workspace_id: "workspace-A".into(),
        generation: 1,
        named_at: at(4),
    };
    input.root_basis.latest_event = Some(bound.event);
    // A sighting in another workspace states no generation of this root.
    let ObservedOccurrence::InterTurnChange { source_change } = &mut input.occurrence else {
        unreachable!()
    };
    *source_change = ObservedSourceChange::ContentComparison {
        workspace_id: "workspace-B".into(),
        baseline: MeasuredBaseline {
            workspace_id: "workspace-B".into(),
            source_revision: "rev-a".into(),
            observed_at: at(4),
        },
        sighting: MeasuredSighting {
            source_basis: ExecutionSourceBasis {
                workspace_id: "workspace-B".into(),
                source_revision: "rev-b".into(),
                source_root_generation: None,
                source_root_state: None,
            },
            observed_at: at(5),
        },
    };
    let receipt = observe(&mut fixture, input, 7).expect("accounted");
    assert!(self_anchored(&receipt), "{receipt:?}");
    let records = obligations(&fixture);
    let actions = fixture
        .store
        .work_obligation_completion_actions(&[&records[0].obligation])
        .expect("actions");
    // The root holds it as a foreign change its binding displaces, never as
    // the run's own untested change.
    assert_eq!(actions, [WorkObligationCompletionAction::DoneDisplaces]);
}

#[test]
fn a_change_after_the_claim_expired_still_accounts_and_renews_nothing() {
    let mut fixture = fixture();
    let input = accounted(&fixture, "late", "rev-a", "rev-b");
    let receipt = observe(&mut fixture, input, 5_000).expect("accounted after expiry");
    assert!(self_anchored(&receipt), "{receipt:?}");
    assert_eq!(obligations(&fixture).len(), 1);
    let claim_after = load_work_claim_optional(&fixture.store.connection, fixture.claim.run_id)
        .expect("claim")
        .expect("a claim");
    assert_eq!(claim_after.expires_at, fixture.claim.expires_at);
    assert_eq!(claim_after.fence, fixture.claim.fence);
}

#[test]
fn a_newer_claim_or_a_released_binding_keeps_the_report_as_historical() {
    // A newer claim and fence on the run.
    let mut fixture = fixture();
    let input = accounted(&fixture, "historical", "rev-a", "rev-b");
    claim(
        &mut fixture.store,
        &fixture.work,
        "runner-2",
        "reclaim",
        400,
        300,
    );
    let receipt = observe(&mut fixture, input, 401).expect("recorded");
    assert_eq!(
        receipt.accounting,
        audit(ObservationAuditReason::HistoricalBinding)
    );
    assert!(
        receipt.opened_obligations.is_empty(),
        "{:?}",
        receipt.opened_obligations
    );
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );

    // A released binding.
    let mut fixture = super::fixture();
    let input = accounted(&fixture, "released", "rev-a", "rev-b");
    fixture
        .store
        .release_work(
            &crate::domain::ReleaseWorkRequest {
                work_id: fixture.work.work_id,
                run_id: fixture.claim.run_id,
                expected_work_revision: fixture.claim.accepted_work_revision,
                holder: fixture.claim.holder.clone(),
                claim_id: fixture.claim.claim_id,
                claim_fence: fixture.claim.fence,
                reason: "handing the work back".into(),
                waiver_reason: Some("nothing was left to test".into()),
                actor: actor("runner"),
                idempotency_key: "release".into(),
                released_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release");
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    assert_eq!(
        receipt.accounting,
        audit(ObservationAuditReason::HistoricalBinding)
    );
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );
}

// A revised item moves the binding's work revision on: the report keeps the
// old binding as history.
#[test]
fn a_revised_item_keeps_the_report_as_historical() {
    let mut fixture = fixture();
    let input = accounted(&fixture, "before-revision", "rev-a", "rev-b");
    let patch = crate::domain::WorkRevisionPatch {
        acceptance_bindings: None,
        external_ref: None,
        clear_external: false,
        title: Some("Ship local work, revised".into()),
        outcome: None,
        acceptance: None,
        kind: None,
        priority: None,
        labels: None,
        add_labels: Vec::new(),
        remove_labels: Vec::new(),
        assigned_to: None,
        clear_assignment: false,
        deferred_until: None,
        clear_deferral: false,
        evaluation_mode: None,
        clear_evaluation_mode: false,
    };
    fixture
        .store
        .revise_work(
            &crate::domain::ReviseWorkRequest {
                work_id: fixture.work.work_id,
                expected_revision: fixture.work.revision,
                patch,
                authority: crate::domain::WorkPlanningAuthority::Claim {
                    run_id: fixture.claim.run_id,
                    holder: fixture.claim.holder.clone(),
                    claim_id: fixture.claim.claim_id,
                    claim_fence: fixture.claim.fence,
                },
                actor: actor("runner"),
                idempotency_key: "revise".into(),
                updated_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("revise");
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    assert_eq!(
        receipt.accounting,
        audit(ObservationAuditReason::HistoricalBinding)
    );
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );
    assert!(fixture.store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_root_named_since_the_capture_keeps_the_report_as_moved() {
    let mut fixture = fixture();
    let input = accounted(&fixture, "moved", "rev-a", "rev-b");
    name_root(&mut fixture, 1, NamedRootBindingKind::Bound, 5, 6);
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    assert_eq!(
        receipt.accounting,
        audit(ObservationAuditReason::RootBasisMoved)
    );
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );
}

#[test]
fn a_report_after_completion_is_finished_run_and_leaves_the_seal_unchanged() {
    let mut fixture = fixture();
    let input = accounted(&fixture, "after-done", "rev-a", "rev-b");
    let note = evidence(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "done-evidence",
        8,
    );
    checkpoint(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "done-checkpoint",
        8,
        std::slice::from_ref(&note),
    );
    let seal = complete(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        &note,
        "done",
        9,
    )
    .expect("complete");
    let seal_bytes = |store: &SqliteStore| -> Vec<Vec<u8>> {
        store
            .connection
            .prepare(
                "SELECT canonical_json FROM objects WHERE object_kind = 'completion_seal'
                 ORDER BY object_id",
            )
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("seals")
    };
    let before = seal_bytes(&fixture.store);
    assert_eq!(before.len(), 1);
    let receipt = observe(&mut fixture, input, 10).expect("recorded");
    assert_eq!(
        receipt.accounting,
        audit(ObservationAuditReason::FinishedRun)
    );
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );
    assert_eq!(seal_bytes(&fixture.store), before);
    assert_eq!(seal.run_id, fixture.claim.run_id);
}

#[test]
fn an_equal_revision_repeats_its_change_and_a_return_is_a_new_change() {
    let mut fixture = fixture();
    let first = observe_accounted(&mut fixture, "to-b", "rev-a", "rev-b", 7).expect("a change");
    assert!(self_anchored(&first));
    let repeat = observe_accounted(&mut fixture, "b-again", "rev-x", "rev-b", 8).expect("a repeat");
    assert_eq!(
        repeat.accounting,
        ObservationAccounting::Repeat {
            source_change: first.observation.clone()
        }
    );
    assert!(
        repeat.opened_obligations.is_empty(),
        "{:?}",
        repeat.opened_obligations
    );
    assert_eq!(obligations(&fixture).len(), 1);
    // B -> C -> B: the return to B is a new change, not a repeat.
    let to_c = observe_accounted(&mut fixture, "to-c", "rev-b", "rev-c", 9).expect("a change");
    assert!(self_anchored(&to_c));
    let back =
        observe_accounted(&mut fixture, "back-to-b", "rev-c", "rev-b", 10).expect("a change");
    assert!(self_anchored(&back), "{back:?}");
    assert_eq!(obligations(&fixture).len(), 3);
}

#[test]
fn a_watcher_only_change_is_never_deduplicated() {
    let mut fixture = fixture();
    let watcher = |fixture: &Fixture, key: &str| {
        let mut input = accounted(fixture, key, "rev-a", "rev-b");
        input.occurrence = ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::WatcherOnly {
                workspace_id: "workspace-A".into(),
                observed_at: at(5),
            },
        };
        input
    };
    for (key, second) in [("watch-1", 7), ("watch-2", 8)] {
        let input = watcher(&fixture, key);
        let receipt = observe(&mut fixture, input, second).expect("a change");
        assert!(self_anchored(&receipt), "{receipt:?}");
    }
    assert_eq!(obligations(&fixture).len(), 2);
    let (_, latest) =
        latest_source_mutation_on(&fixture.store.connection, fixture.claim.run_id, i64::MAX)
            .expect("latest")
            .expect("a change");
    assert_eq!(latest.source_basis, None);
    assert_eq!(
        latest.reported_source_change,
        Some(SourceChangeDetection::WatcherOnly)
    );
}

#[test]
fn no_reported_change_accounts_nothing_and_nested_checks_are_no_sightings() {
    let mut fixture = fixture();
    let nested = ExecutionSourceBasis {
        workspace_id: "workspace-A".into(),
        source_revision: "rev-nested".into(),
        source_root_generation: None,
        source_root_state: None,
    };
    let mut turn = accounted(&fixture, "quiet-turn", "rev-a", "rev-b");
    let mut check = passed_check("cargo-test");
    check.source_basis = Some(nested.clone());
    turn.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-1".into(),
        source_change: None,
        observed_checks: vec![check.clone()],
    };
    let receipt = observe(&mut fixture, turn, 7).expect("recorded");
    assert_eq!(receipt.accounting, ObservationAccounting::NoSourceChange {});
    let mut standalone = accounted(&fixture, "standalone", "rev-a", "rev-b");
    standalone.occurrence = ObservedOccurrence::ObservedCheck {
        host_turn_ref: "turn-2".into(),
        check,
    };
    let receipt = observe(&mut fixture, standalone, 8).expect("recorded");
    assert_eq!(receipt.accounting, ObservationAccounting::NoSourceChange {});
    assert!(
        obligations(&fixture).is_empty(),
        "{:?}",
        obligations(&fixture)
    );
    assert!(
        latest_source_mutation_on(&fixture.store.connection, fixture.claim.run_id, i64::MAX)
            .expect("latest")
            .is_none()
    );
    assert!(
        newest_measured_sighting_on(
            &fixture.store.connection,
            fixture.claim.run_id,
            "workspace-A",
            i64::MAX
        )
        .expect("sighting")
        .is_none(),
        "a nested check's source is no sighting"
    );
}

#[test]
fn an_accounted_retry_returns_the_original_receipt_and_opens_nothing_more() {
    let mut fixture = fixture();
    let input = accounted(&fixture, "retry", "rev-a", "rev-b");
    let first = observe(&mut fixture, input.clone(), 7).expect("accounted");
    let before = footprint(&fixture.store);
    let again = observe(&mut fixture, input, 30).expect("replayed");
    assert_eq!(again, first);
    assert_eq!(footprint(&fixture.store), before);
    assert_eq!(first.opened_obligations.len(), 1);
}

// The repeat scope is the report's workspace: a different revision seen in
// another workspace in between leaves an equal revision a repeat, while one
// seen in the same workspace makes it a new change.
#[test]
fn a_repeat_is_scoped_to_the_reports_workspace() {
    let mut fixture = fixture();
    let first = observe_accounted(&mut fixture, "a-in-w1", "rev-0", "rev-a", 7).expect("a change");
    assert!(self_anchored(&first));
    let sight = |fixture: &mut Fixture, key: &str, workspace: &str, revision: &str| {
        let transaction = fixture
            .store
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .expect("transaction");
        host_check_producer(
            &transaction,
            &fixture.work,
            &fixture.claim,
            "runner",
            key,
            8,
            ExecutionSourceBasis {
                workspace_id: workspace.into(),
                source_revision: revision.into(),
                source_root_generation: None,
                source_root_state: None,
            },
        );
        transaction.commit().expect("commit");
    };
    sight(&mut fixture, "b-in-w2", "workspace-B", "rev-b");
    let repeat = observe_accounted(&mut fixture, "a-again", "rev-x", "rev-a", 9).expect("a repeat");
    assert_eq!(
        repeat.accounting,
        ObservationAccounting::Repeat {
            source_change: first.observation.clone()
        }
    );
    assert!(
        repeat.opened_obligations.is_empty(),
        "{:?}",
        repeat.opened_obligations
    );
    sight(&mut fixture, "b-in-w1", "workspace-A", "rev-b");
    let after =
        observe_accounted(&mut fixture, "a-after-b", "rev-b", "rev-a", 10).expect("a change");
    assert!(self_anchored(&after), "{after:?}");
    assert!(fixture.store.verify_all().expect("doctor").is_healthy());
}

// A watcher-only change carries no located source, so nothing shows it was
// in another workspace: after it, an equal revision is a new change, not a
// repeat, and a fresh check of that revision satisfies its obligation.
#[test]
fn a_watcher_only_change_breaks_a_workspace_scoped_repeat() {
    let mut fixture = fixture();
    let first = observe_accounted(&mut fixture, "a-first", "rev-0", "rev-a", 7).expect("a change");
    assert!(self_anchored(&first));
    let mut watcher = accounted(&fixture, "watcher", "rev-a", "rev-b");
    watcher.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: ObservedSourceChange::WatcherOnly {
            workspace_id: "workspace-A".into(),
            observed_at: at(5),
        },
    };
    let unlocated = observe(&mut fixture, watcher, 8).expect("a watcher-only change");
    assert!(self_anchored(&unlocated));
    let again = observe_accounted(&mut fixture, "a-again", "rev-x", "rev-a", 9).expect("a change");
    assert!(self_anchored(&again), "{again:?}");
    host_verification_from_basis(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "fresh-a",
        VerificationKind::Test,
        VerificationResult::Passed,
        10,
        ExecutionSourceBasis {
            workspace_id: "workspace-A".into(),
            source_revision: "rev-a".into(),
            source_root_generation: None,
            source_root_state: None,
        },
    );
    let satisfied = obligations(&fixture)
        .iter()
        .find(|record| record.obligation.triggering_observation == again.observation)
        .expect("the new change's obligation")
        .state;
    assert_eq!(satisfied, WorkObligationState::Satisfied);
    assert!(fixture.store.verify_all().expect("doctor").is_healthy());
}
