//! The host-only core operations that act on the ambient focus follow the
//! agent words' implicit-target rule: after `add` moves the focus to a new
//! item while the session holds another, a form that names no item is
//! refused with `work_implicit_target_conflict` and records nothing.

use super::*;
use crate::domain::{WorkBlockerKind, WorkRevisionPatch};
use crate::storage::{ImplicitFocusState, StoreError, test_database_shape_snapshot};

/// Fields drop in declaration order, so the home is declared last: it is
/// removed only after the service has closed the store.
struct HeldThenAdded {
    database: std::path::PathBuf,
    service: LocalWorkService,
    held: WorkItemSummary,
    added: WorkItemSummary,
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

fn claim_input() -> WorkUpdateInput {
    WorkUpdateInput::Claim {
        ttl_seconds: Some(3_600),
        recovery_reason: None,
        idempotency_key: String::new(),
    }
}

/// The session holds `held`; `added` was then added, which focuses it.
fn held_then_added(project: &str) -> HeldThenAdded {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = service_for(&database, project, "implicit-session");
    let held = proposed_root(
        service
            .work_propose(root_input("Held work", "held-root"), at(0))
            .expect("held root"),
    );
    service
        .work_update(claim_input(), at(1))
        .expect("claim the held root");
    let added = proposed_root(
        service
            .work_propose(root_input("Added work", "added-root"), at(2))
            .expect("added root"),
    );
    HeldThenAdded {
        database,
        service,
        held,
        added,
        _directory: directory,
    }
}

/// Every update form, as a host sends it without naming an item.
fn update_forms(held: &str) -> Vec<WorkUpdateInput> {
    let key = String::new;
    vec![
        claim_input(),
        WorkUpdateInput::ClaimNextReady {
            ttl_seconds: Some(3_600),
            recovery_reason: None,
            idempotency_key: key(),
        },
        WorkUpdateInput::Release {
            reason: "released".into(),
            waiver_reason: None,
            idempotency_key: key(),
        },
        WorkUpdateInput::Checkpoint {
            summary: "checkpoint".into(),
            evidence: None,
            idempotency_key: key(),
        },
        WorkUpdateInput::Evidence {
            summary: "evidence".into(),
            refs: Vec::new(),
            attach: None,
            idempotency_key: key(),
        },
        WorkUpdateInput::Block {
            blocker_kind: WorkBlockerKind::Manual,
            detail: "blocked".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Unblock {
            blocker_id: None,
            idempotency_key: key(),
        },
        WorkUpdateInput::Revise {
            patch: WorkRevisionPatch {
                title: Some("Revised".into()),
                ..WorkRevisionPatch::default()
            },
            idempotency_key: key(),
        },
        WorkUpdateInput::AddPrerequisite {
            prerequisite: held.into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::RemovePrerequisite {
            prerequisite: held.into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Reopen {
            reason: "reopened".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Cancel {
            reason: "cancelled".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Reject {
            reason: "rejected".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Supersede {
            replacement: held.into(),
            reason: "superseded".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::Detach {
            reason: "detached".into(),
            idempotency_key: key(),
        },
        WorkUpdateInput::WaiveRequiredChild {
            child: held.into(),
            reason: "waived".into(),
            idempotency_key: key(),
        },
    ]
}

fn decompose_input() -> WorkProposeInput {
    keyed_decompose_input("child", "")
}

fn keyed_decompose_input(child: &str, key: &str) -> WorkProposeInput {
    serde_json::from_value(serde_json::json!({
        "kind": "decompose",
        "children": [{
            "key": child,
            "title": format!("Child {child}"),
            "outcome": "Child outcome",
            "acceptance": ["Child accepted"],
        }],
        "idempotency_key": key,
    }))
    .expect("decomposition input")
}

/// The refusal names the operation, the focus, its state and what is held,
/// and the store is exactly as it was.
fn refused<T: std::fmt::Debug>(
    database: &std::path::Path,
    operation: &str,
    focus: &WorkItemSummary,
    held: &[&WorkItemSummary],
    state: ImplicitFocusState,
    attempt: impl FnOnce() -> Result<T, StoreError>,
) {
    let snapshot = || {
        let connection = rusqlite::Connection::open(database).expect("connection");
        test_database_shape_snapshot(&connection).expect("snapshot")
    };
    let before = snapshot();
    let result = attempt();
    let Err(StoreError::WorkImplicitTargetConflict(conflict)) = &result else {
        panic!("{operation}: expected an implicit-target refusal, got {result:?}");
    };
    assert_eq!(conflict.operation, operation);
    assert_eq!(conflict.focus, focus.short_ref, "{operation}");
    assert_eq!(conflict.focus_state, state, "{operation}");
    let expected: Vec<String> = held.iter().map(|item| item.short_ref.clone()).collect();
    assert_eq!(conflict.held, expected, "{operation}");
    assert_eq!(conflict.more, 0, "{operation}");
    assert_eq!(snapshot(), before, "{operation} recorded nothing");
}

#[test]
fn every_ambient_core_form_after_add_refuses_while_other_work_is_held() {
    let fixture = held_then_added("implicit-every-form");
    let HeldThenAdded {
        database,
        service,
        held,
        added,
        ..
    } = &fixture;
    for form in update_forms(&held.short_ref) {
        let (operation, _, _) = update_metadata(&form);
        let operation = if matches!(form, WorkUpdateInput::Reject { .. }) {
            REJECT_PROTOCOL_OPERATION.to_owned()
        } else {
            format!("work_update:{operation}")
        };
        refused(
            database,
            &operation,
            added,
            &[held],
            ImplicitFocusState::Unclaimed,
            || service.work_update(form.clone(), at(3)),
        );
        refused(
            database,
            &operation,
            added,
            &[held],
            ImplicitFocusState::Unclaimed,
            || service.work_update_on(None, form, at(3)),
        );
    }
    refused(
        database,
        crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
        added,
        &[held],
        ImplicitFocusState::Unclaimed,
        || service.work_propose(decompose_input(), at(3)),
    );
    // A root names no focus and is created as before.
    service
        .work_propose(root_input("Another root", "another-root"), at(4))
        .expect("a root is not an ambient form");
}

#[test]
fn a_named_target_a_held_focus_or_no_other_claim_acts_as_before() {
    let fixture = held_then_added("implicit-positives");
    let HeldThenAdded {
        service,
        held,
        added,
        ..
    } = &fixture;
    // Naming the item acts on it.
    service
        .work_update_on(Some(&added.short_ref), claim_input(), at(3))
        .expect("an explicit claim of the added item");
    // A focus this session holds acts, though it holds other work too.
    service
        .work_update(
            WorkUpdateInput::Checkpoint {
                summary: "progress".into(),
                evidence: None,
                idempotency_key: String::new(),
            },
            at(4),
        )
        .expect("a checkpoint of the held focus");
    service
        .work_propose(decompose_input(), at(5))
        .expect("a decomposition of the held focus");
    // Releasing both leaves no other claim: a bare form acts on the focus.
    for (item, second) in [(held, 6), (added, 7)] {
        service
            .work_update_on(
                Some(&item.short_ref),
                WorkUpdateInput::Release {
                    reason: "done for now".into(),
                    waiver_reason: Some("done for now".into()),
                    idempotency_key: String::new(),
                },
                at(second),
            )
            .expect("release");
    }
    service
        .work_update(claim_input(), at(8))
        .expect("a bare claim with no other claim held");
}

#[test]
fn a_focus_held_elsewhere_and_held_work_past_the_shown_bound_are_named() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = service_for(&database, "implicit-bound", "implicit-session");
    let mut held = Vec::new();
    for index in 0..4 {
        let item = proposed_root(
            service
                .work_propose(
                    root_input(&format!("Held {index}"), &format!("held-{index}")),
                    at(index),
                )
                .expect("held root"),
        );
        service
            .work_update_on(Some(&item.short_ref), claim_input(), at(index))
            .expect("claim the held root");
        held.push(item);
    }
    let peer = service_for(&database, "implicit-bound", "peer-session");
    let elsewhere = proposed_root(
        peer.work_propose(root_input("Peer work", "peer-root"), at(10))
            .expect("peer root"),
    );
    peer.work_update(claim_input(), at(11))
        .expect("peer claims");
    service
        .work_focus(&elsewhere.short_ref, at(12))
        .expect("focus the peer's item");
    let mut sorted: Vec<&WorkItemSummary> = held.iter().collect();
    sorted.sort_by(|left, right| left.short_ref.cmp(&right.short_ref));
    let snapshot = || {
        let connection = rusqlite::Connection::open(&database).expect("connection");
        test_database_shape_snapshot(&connection).expect("snapshot")
    };
    let before = snapshot();
    let result = service.work_update(
        WorkUpdateInput::Checkpoint {
            summary: "progress".into(),
            evidence: None,
            idempotency_key: String::new(),
        },
        at(13),
    );
    let Err(StoreError::WorkImplicitTargetConflict(conflict)) = &result else {
        panic!("expected an implicit-target refusal, got {result:?}");
    };
    assert_eq!(conflict.focus_state, ImplicitFocusState::HeldElsewhere);
    assert_eq!(
        conflict.held,
        sorted
            .iter()
            .take(crate::storage::IMPLICIT_TARGET_HELD_SHOWN)
            .map(|item| item.short_ref.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(conflict.more, 1);
    assert_eq!(snapshot(), before);
}

// A keyed call this session already began is a retry of an intent admitted
// earlier: its lost response replays though the focus has moved since.
#[test]
fn a_keyed_retry_replays_after_the_focus_moved() {
    let fixture = held_then_added("implicit-keyed-retry");
    let HeldThenAdded {
        service,
        held,
        added,
        ..
    } = &fixture;
    let checkpoint = WorkUpdateInput::Checkpoint {
        summary: "keyed progress".into(),
        evidence: None,
        idempotency_key: "keyed-checkpoint".into(),
    };
    let original = service
        .work_update_on(Some(&held.short_ref), checkpoint.clone(), at(3))
        .expect("keyed checkpoint of the held item");
    service
        .work_focus(&added.short_ref, at(4))
        .expect("focus moves to the added item");
    let replay = service
        .work_update(checkpoint, at(5))
        .expect("the keyed retry replays");
    assert_eq!(
        serde_json::to_value(&replay).expect("replay"),
        serde_json::to_value(&original).expect("original")
    );
    // The same key with another payload, and a new key, are new acts on the
    // unheld focus: neither replays.
    for (key, summary) in [
        ("keyed-checkpoint", "changed progress"),
        ("another-checkpoint", "keyed progress"),
    ] {
        let refusal = service.work_update(
            WorkUpdateInput::Checkpoint {
                summary: summary.into(),
                evidence: None,
                idempotency_key: key.into(),
            },
            at(6),
        );
        assert!(
            matches!(refusal, Err(StoreError::WorkImplicitTargetConflict(_))),
            "{key}: {refusal:?}"
        );
    }
}

// An attempt begun but never finished is not a replay: its retry on the
// unheld focus is checked like any new act.
#[test]
fn an_unfinished_keyed_attempt_does_not_bypass_the_rule() {
    let fixture = held_then_added("implicit-unfinished");
    let HeldThenAdded {
        database,
        service,
        held,
        ..
    } = &fixture;
    let checkpoint = WorkUpdateInput::Checkpoint {
        summary: "interrupted progress".into(),
        evidence: None,
        idempotency_key: "interrupted-checkpoint".into(),
    };
    let mut store = SqliteStore::open(database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, Some(held.work_id), at(3))
        .expect("basis on the held item");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &ProjectId("implicit-unfinished".into()),
            session_id: &SessionId("implicit-session".into()),
            operation: "work_update:checkpoint",
            idempotency_key: "interrupted-checkpoint",
            intent: &service.protocol_intent(&checkpoint),
            basis: &basis,
            now: at(3),
        })
        .expect("begin an attempt that never finishes");
    drop(store);
    let refusal = service.work_update(checkpoint, at(4));
    assert!(
        matches!(refusal, Err(StoreError::WorkImplicitTargetConflict(_))),
        "{refusal:?}"
    );
}

// A focus that is no longer open is named as such.
#[test]
fn a_focus_that_is_not_open_is_named_not_open() {
    let fixture = held_then_added("implicit-not-open");
    let HeldThenAdded {
        database,
        service,
        held,
        added,
        ..
    } = &fixture;
    service
        .work_update_on(
            Some(&added.short_ref),
            WorkUpdateInput::Cancel {
                reason: "no longer needed".into(),
                idempotency_key: String::new(),
            },
            at(3),
        )
        .expect("cancel the added item, which keeps the focus on it");
    refused(
        database,
        "work_update:checkpoint",
        added,
        &[held],
        ImplicitFocusState::NotOpen,
        || {
            service.work_update(
                WorkUpdateInput::Checkpoint {
                    summary: "progress".into(),
                    evidence: None,
                    idempotency_key: String::new(),
                },
                at(4),
            )
        },
    );
}

// A keyed decomposition replays after the focus moved; the same key with
// other children is a new act and is refused.
#[test]
fn a_keyed_decomposition_replays_after_the_focus_moved() {
    let fixture = held_then_added("implicit-keyed-decompose");
    let HeldThenAdded {
        service,
        held,
        added,
        ..
    } = &fixture;
    service
        .work_focus(&held.short_ref, at(3))
        .expect("focus the held item");
    let original = service
        .work_propose(keyed_decompose_input("first", "keyed-decompose"), at(4))
        .expect("decompose the held focus");
    service
        .work_focus(&added.short_ref, at(5))
        .expect("focus moves to the added item");
    let replay = service
        .work_propose(keyed_decompose_input("first", "keyed-decompose"), at(6))
        .expect("the keyed decomposition replays");
    assert_eq!(
        serde_json::to_value(&replay).expect("replay"),
        serde_json::to_value(&original).expect("original")
    );
    let refusal = service.work_propose(keyed_decompose_input("second", "keyed-decompose"), at(7));
    assert!(
        matches!(refusal, Err(StoreError::WorkImplicitTargetConflict(_))),
        "{refusal:?}"
    );
}

// A keyed release whose core write committed before its attempt could
// finish is recovered by its retry, though the released focus is no longer
// held and the session still holds other work.
#[test]
fn a_committed_unfinished_release_is_recovered_while_other_work_is_held() {
    let fixture = held_then_added("implicit-committed-release");
    let HeldThenAdded {
        database,
        service,
        added,
        ..
    } = &fixture;
    service
        .work_update_on(Some(&added.short_ref), claim_input(), at(3))
        .expect("claim the added item too");
    let input = WorkUpdateInput::Release {
        reason: "redirected".into(),
        waiver_reason: Some("redirected".into()),
        idempotency_key: "committed-release".into(),
    };
    let mut store = SqliteStore::open(database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, at(4))
        .expect("basis on the held focus");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &ProjectId("implicit-committed-release".into()),
            session_id: &SessionId("implicit-session".into()),
            operation: "work_update:release",
            idempotency_key: "committed-release",
            intent: &service.protocol_intent(&input),
            basis: &basis,
            now: at(4),
        })
        .expect("begin the release attempt");
    let work = basis.focused_work.clone().expect("focus");
    let claim = basis.claim.clone().expect("live claim");
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: SessionId("implicit-session".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "redirected".into(),
                waiver_reason: Some("redirected".into()),
                actor: service.actor("work_update", "release ambient local work"),
                idempotency_key: service
                    .core_operation_key("work_update:release", "committed-release", "release_work")
                    .expect("scoped operation key"),
                released_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("commit the core release without its protocol result");
    drop(store);
    let recovered = service
        .work_update(input.clone(), at(5))
        .expect("the retry recovers the committed release");
    assert_eq!(recovered.operation, "release");
    assert_eq!(recovered.receipt.work_id, added.work_id);
    let replayed = service
        .work_update(input, at(6))
        .expect("and then replays it");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("replay"),
        serde_json::to_vec(&recovered).expect("recovery")
    );
    // Recovery read the committed result back: the release happened once.
    let connection = rusqlite::Connection::open(database).expect("connection");
    let releases: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'project' AND entry.object_kind = 'work_event'
               AND entry.work_id = ?1
               AND json_extract(object.canonical_json, '$.transition.kind') = 'released'",
            [added.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("count release events");
    assert_eq!(releases, 1);
}
