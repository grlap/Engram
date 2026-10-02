//! A check accounts for an obligation an unadmitted change triggered only
//! when its producer and its record follow the change and it completed no
//! earlier than the change was recorded. Without a named root the newest
//! measured sighting, not the change's own revision, decides the revision
//! the check must carry: a late report of an old revision asks for a fresh
//! check without pinning the old revision.

use super::accounting::{accounted, obligations, self_anchored};
use super::refusals::{name_root, sighting_under};
use super::*;
use crate::domain::{NamedRootBindingKind, SourceRootState, WorkObligationState};

fn basis(revision: &str) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: "workspace-A".into(),
        source_revision: revision.into(),
        source_root_generation: None,
        source_root_state: None,
    }
}

/// A passing test of `basis`, its producer, environment record and
/// verification appended now and completed at `second`.
fn fresh_check(fixture: &mut Fixture, key: &str, second: i64, basis: ExecutionSourceBasis) {
    host_verification_from_basis(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        key,
        VerificationKind::Test,
        VerificationResult::Passed,
        second,
        basis,
    );
}

/// Only the producer of a check that ran at `second` on `basis`: a quiet
/// sighting of that source.
fn producer_only(
    fixture: &mut Fixture,
    key: &str,
    second: i64,
    basis: ExecutionSourceBasis,
) -> crate::ObjectId {
    let transaction = fixture
        .store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("transaction");
    let producer = host_check_producer(
        &transaction,
        &fixture.work,
        &fixture.claim,
        "runner",
        key,
        second,
        basis,
    );
    transaction.commit().expect("commit");
    producer
}

/// The passing verification of a stored `producer`, completed at
/// `completed` and recorded now with the time `recorded`.
fn late_record(
    fixture: &mut Fixture,
    key: &str,
    (completed, recorded): (i64, i64),
    basis: ExecutionSourceBasis,
    producer: crate::ObjectId,
) {
    let transaction = fixture
        .store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("transaction");
    host_verification_of_producer(
        &transaction,
        &fixture.work,
        &fixture.claim,
        "runner",
        key,
        completed,
        recorded,
        basis,
        producer,
    );
    transaction.commit().expect("commit");
}

/// The state of the one obligation the run holds.
fn state(fixture: &Fixture) -> WorkObligationState {
    let records = obligations(fixture);
    assert_eq!(records.len(), 1, "{records:?}");
    records[0].state
}

fn accounted_change(fixture: &mut Fixture, key: &str, to: &str, second: i64) {
    let input = accounted(fixture, key, "rev-0", to);
    let receipt = observe(fixture, input, second).expect("accounted");
    assert!(self_anchored(&receipt), "{receipt:?}");
}

// Old A, an intervening sighting of B, then the late report of A: a fresh
// passing check of B, the source as it is, satisfies without the source
// moving back to A.
#[test]
fn a_late_report_of_an_old_revision_is_satisfied_by_a_fresh_check_of_the_current_one() {
    let mut fixture = fixture();
    producer_only(&mut fixture, "sight-b", 6, basis("rev-b"));
    accounted_change(&mut fixture, "late-a", "rev-a", 7);
    assert_eq!(state(&fixture), WorkObligationState::Open);
    fresh_check(&mut fixture, "fresh-b", 8, basis("rev-b"));
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

// A report of a revision that is truly the newest is held the same way: a
// fresh check of it satisfies.
#[test]
fn a_truly_newer_revision_is_satisfied_by_a_fresh_check_of_it() {
    let mut fixture = fixture();
    accounted_change(&mut fixture, "new-a", "rev-a", 7);
    fresh_check(&mut fixture, "fresh-a", 8, basis("rev-a"));
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

// A check completed at the instant the change was recorded follows it.
#[test]
fn a_check_completed_when_the_change_was_recorded_satisfies() {
    let mut fixture = fixture();
    accounted_change(&mut fixture, "same-time", "rev-a", 7);
    fresh_check(&mut fixture, "same-time-check", 7, basis("rev-a"));
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

// A producer recorded before the change, whose verification arrives after
// it, did not run after the change.
#[test]
fn an_old_producer_with_a_late_record_does_not_satisfy() {
    let mut fixture = fixture();
    let producer = producer_only(&mut fixture, "early", 8, basis("rev-a"));
    accounted_change(&mut fixture, "after-producer", "rev-a", 7);
    late_record(&mut fixture, "early", (8, 9), basis("rev-a"), producer);
    assert_eq!(state(&fixture), WorkObligationState::Open);
}

// A cached check completed before the change was recorded, whose producer
// and record both arrive after it, does not satisfy either.
#[test]
fn a_cached_check_completed_before_the_record_does_not_satisfy() {
    let mut fixture = fixture();
    accounted_change(&mut fixture, "cached", "rev-a", 7);
    let producer = producer_only(&mut fixture, "cached-check", 6, basis("rev-a"));
    late_record(
        &mut fixture,
        "cached-check",
        (6, 8),
        basis("rev-a"),
        producer,
    );
    assert_eq!(state(&fixture), WorkObligationState::Open);
}

// A later quiet sighting of another revision before the check is recorded
// leaves that check behind; a check of the newer source satisfies.
#[test]
fn a_later_different_sighting_invalidates_a_check_recorded_after_it() {
    let mut fixture = fixture();
    accounted_change(&mut fixture, "then-move", "rev-a", 7);
    let producer = producer_only(&mut fixture, "check-b", 8, basis("rev-b"));
    producer_only(&mut fixture, "sight-c", 9, basis("rev-c"));
    late_record(&mut fixture, "check-b", (8, 10), basis("rev-b"), producer);
    assert_eq!(state(&fixture), WorkObligationState::Open);
    fresh_check(&mut fixture, "check-c", 11, basis("rev-c"));
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

// The obligation belongs to the run: a check under the successor claim
// resolves it.
#[test]
fn a_successor_claim_resolves_the_runs_obligation() {
    let mut fixture = fixture();
    accounted_change(&mut fixture, "before-reclaim", "rev-a", 7);
    let successor = claim(
        &mut fixture.store,
        &fixture.work,
        "runner-2",
        "reclaim",
        400,
        300,
    );
    host_verification_from_basis(
        &mut fixture.store,
        &fixture.work,
        &successor,
        "runner-2",
        "successor-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        401,
        basis("rev-a"),
    );
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

/// An accounted change sighted in the claim's named root, generation 1.
fn named_root_change(fixture: &mut Fixture) {
    let bound = name_root(fixture, 1, NamedRootBindingKind::Bound, 4, 4);
    let mut input = sighting_under(
        fixture,
        "in-root",
        NamedRootState::Bound {
            workspace_id: "workspace-A".into(),
            generation: 1,
            named_at: at(4),
        },
        bound.event,
        (1, SourceRootState::Named),
    );
    input.policy_basis = super::accounting::account(fixture);
    let receipt = observe(fixture, input, 7).expect("accounted in the root");
    assert!(self_anchored(&receipt), "{receipt:?}");
}

fn in_root(revision: &str) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        source_root_generation: Some(1),
        source_root_state: Some(SourceRootState::Named),
        ..basis(revision)
    }
}

// Under a named root the root's newest sighting decides the revision as
// before, and the time floor still holds.
#[test]
fn under_a_named_root_the_floors_hold_and_a_fresh_check_in_the_root_satisfies() {
    let mut fixture = fixture();
    named_root_change(&mut fixture);
    let producer = producer_only(&mut fixture, "cached-in-root", 6, in_root("rev-b"));
    late_record(
        &mut fixture,
        "cached-in-root",
        (6, 8),
        in_root("rev-b"),
        producer,
    );
    assert_eq!(state(&fixture), WorkObligationState::Open);
    fresh_check(&mut fixture, "fresh-in-root", 9, in_root("rev-b"));
    assert_eq!(state(&fixture), WorkObligationState::Satisfied);
}

// A later admitted change becomes the run's latest change, but the
// obligation the unadmitted change triggered keeps its own floor: a check
// whose producer came before that change satisfies only the later change's
// obligation.
#[test]
fn a_later_admitted_change_does_not_lift_the_unadmitted_triggers_floor() {
    let mut fixture = fixture();
    let producer = producer_only(&mut fixture, "before-both", 9, basis("rev-c"));
    accounted_change(&mut fixture, "unadmitted", "rev-a", 7);
    let admitted = source_mutation_from_basis(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "admitted",
        8,
        Some(basis("rev-c")),
        None,
    );
    late_record(
        &mut fixture,
        "before-both",
        (9, 10),
        basis("rev-c"),
        producer,
    );
    let states: Vec<(bool, WorkObligationState)> = obligations(&fixture)
        .iter()
        .map(|record| {
            (
                record.obligation.triggering_observation == admitted,
                record.state,
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            (false, WorkObligationState::Open),
            (true, WorkObligationState::Satisfied)
        ]
    );
}

// A criterion bound to a test is carried by a check of the source after the
// run's latest change. After an unadmitted change the binding's obligation,
// which no source change triggered, holds a check to the same floors.
#[test]
fn a_bound_criterion_holds_its_check_to_the_floors_of_an_unadmitted_change() {
    let mut request = root_request("project-a", "bound-work", 1);
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let binding_state = |fixture: &Fixture| {
        obligations(fixture)
            .iter()
            .find(|record| {
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some()
            })
            .expect("the binding's obligation")
            .state
    };
    for (key, check, recorded, satisfied) in [
        // An old producer whose record arrives after the change.
        ("old-producer", "early", None, false),
        // A cached check completed before the change was recorded.
        ("cached", "cached-check", Some((6, 8)), false),
        // A fresh check after it.
        ("fresh", "fresh-check", Some((8, 8)), true),
    ] {
        let mut fixture = fixture_of(SqliteStore::open_in_memory().expect("store"), &request);
        let producer = match recorded {
            None => Some(producer_only(&mut fixture, check, 8, basis("rev-a"))),
            Some(_) => None,
        };
        accounted_change(&mut fixture, key, "rev-a", 7);
        match (producer, recorded) {
            (Some(producer), _) => {
                late_record(&mut fixture, check, (8, 9), basis("rev-a"), producer);
            }
            (None, Some((completed, at_record))) => {
                let producer = producer_only(&mut fixture, check, completed, basis("rev-a"));
                late_record(
                    &mut fixture,
                    check,
                    (completed, at_record),
                    basis("rev-a"),
                    producer,
                );
            }
            (None, None) => unreachable!(),
        }
        assert_eq!(
            binding_state(&fixture) == WorkObligationState::Satisfied,
            satisfied,
            "{key}"
        );
    }
}

// The binding's obligation names no source change, so a later admitted
// change does not lift an earlier unadmitted change's floors from it: a
// check whose producer came before the unadmitted change, recorded late and
// completed before that change was recorded, carries no bound criterion,
// while a fresh check does.
#[test]
fn a_bound_criterion_keeps_an_earlier_unadmitted_changes_floors_after_a_later_admitted_change() {
    let mut request = root_request("project-a", "bound-after-admitted", 1);
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut fixture = fixture_of(SqliteStore::open_in_memory().expect("store"), &request);
    let binding_state = |fixture: &Fixture| {
        obligations(fixture)
            .iter()
            .find(|record| {
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some()
            })
            .expect("the binding's obligation")
            .state
    };
    let producer = producer_only(&mut fixture, "before-unadmitted", 9, basis("rev-c"));
    let input = accounted(&fixture, "unadmitted", "rev-0", "rev-a");
    let receipt = observe(&mut fixture, input, 10).expect("accounted");
    assert!(self_anchored(&receipt), "{receipt:?}");
    source_mutation_from_basis(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "admitted",
        8,
        Some(basis("rev-c")),
        None,
    );
    late_record(
        &mut fixture,
        "before-unadmitted",
        (9, 11),
        basis("rev-c"),
        producer,
    );
    assert_eq!(binding_state(&fixture), WorkObligationState::Open);
    fresh_check(&mut fixture, "fresh", 12, basis("rev-c"));
    assert_eq!(binding_state(&fixture), WorkObligationState::Satisfied);
}

// Completion holds the newest check of a bound kind to every unadmitted
// change's floors, not only the latest change's: the binding was satisfied
// earlier, but the newest check did not follow the unadmitted change.
#[test]
fn completion_refuses_a_bound_check_that_did_not_follow_an_earlier_unadmitted_change() {
    let mut request = root_request("project-a", "bound-at-completion", 1);
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut fixture = fixture_of(SqliteStore::open_in_memory().expect("store"), &request);
    fresh_check(&mut fixture, "first", 4, basis("rev-0"));
    let producer = producer_only(&mut fixture, "before-unadmitted", 9, basis("rev-c"));
    let input = accounted(&fixture, "unadmitted", "rev-0", "rev-a");
    observe(&mut fixture, input, 10).expect("accounted");
    source_mutation_from_basis(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "admitted",
        8,
        Some(basis("rev-c")),
        None,
    );
    late_record(
        &mut fixture,
        "before-unadmitted",
        (9, 11),
        basis("rev-c"),
        producer,
    );
    let note = evidence(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "done-note",
        12,
    );
    checkpoint(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "done-checkpoint",
        12,
        std::slice::from_ref(&note),
    );
    let refused = complete(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        &note,
        "done",
        13,
    )
    .expect_err("the newest bound check did not follow the unadmitted change");
    assert!(
        matches!(
            &refused,
            StoreError::WorkBoundVerificationRefused { cause, .. }
                if cause.mismatch == crate::domain::VerificationEvidenceMismatch::NotAfterMutation
        ),
        "{refused:?}"
    );
}

// A watcher-only unadmitted change carries no revision, so its floors are
// the only guard: completion refuses a newest bound check whose producer
// came before the change, and one that completed before it was recorded.
#[test]
fn completion_holds_a_bound_check_to_a_watcher_only_unadmitted_change() {
    for (key, producer_first, completed) in [("old-producer", true, 9), ("cached", false, 9)] {
        let mut request = root_request("project-a", &format!("bound-watcher-{key}"), 1);
        request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
            criterion: 1,
            requirement: crate::domain::VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
        }];
        let mut fixture = fixture_of(SqliteStore::open_in_memory().expect("store"), &request);
        fresh_check(&mut fixture, "first", 4, basis("rev-0"));
        let early = producer_first
            .then(|| producer_only(&mut fixture, "late-check", completed, basis("rev-0")));
        let mut input = accounted(&fixture, "watcher", "rev-0", "rev-a");
        input.occurrence = ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::WatcherOnly {
                workspace_id: "workspace-A".into(),
                observed_at: at(5),
            },
        };
        let receipt = observe(&mut fixture, input, 10).expect("accounted");
        assert!(self_anchored(&receipt), "{receipt:?}");
        let producer = early.unwrap_or_else(|| {
            producer_only(&mut fixture, "late-check", completed, basis("rev-0"))
        });
        late_record(
            &mut fixture,
            "late-check",
            (completed, 11),
            basis("rev-0"),
            producer,
        );
        let note = evidence(
            &mut fixture.store,
            &fixture.work,
            &fixture.claim,
            "runner",
            "done-note",
            12,
        );
        checkpoint(
            &mut fixture.store,
            &fixture.work,
            &fixture.claim,
            "runner",
            "done-checkpoint",
            12,
            std::slice::from_ref(&note),
        );
        let refused = complete(
            &mut fixture.store,
            &fixture.work,
            &fixture.claim,
            "runner",
            &note,
            "done",
            13,
        )
        .expect_err("the newest bound check did not follow the watcher-only change");
        assert!(
            matches!(&refused, StoreError::WorkBoundVerificationRefused { .. }),
            "{key}: {refused:?}"
        );
    }
}
