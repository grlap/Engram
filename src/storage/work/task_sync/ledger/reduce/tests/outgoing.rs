use super::*;

mod delta;

fn prepared() -> (Fixture, ObjectId, Intent) {
    let mut fixture = Fixture::new(Direction::Outgoing);
    let original = fixture.legacy();
    let prepared = fixture.push(Transition::Prepared {
        intent: Box::new(original.clone()),
    });
    fixture.push(Transition::LocalCommitRecorded {
        prepared: prepared.clone(),
    });
    (fixture, prepared, original)
}

fn resolved(original: &Intent, tip: &str) -> Intent {
    let mut resolved = original.clone();
    resolved.operation = uuid::Uuid::new_v4();
    resolved.key = resolved.operation.to_string();
    resolved.candidate.operation = uuid::Uuid::new_v4();
    resolved.expected_head = Some(original.commit.oid.clone());
    resolved.commit = commit(resolved.operation, &resolved.key);
    resolved.commit.parents = vec![original.commit.oid.clone(), tip.to_owned()];
    resolved
}

fn step(fixture: &mut Fixture, prepared: &ObjectId, step: OutgoingReconciliation) -> ObjectId {
    fixture.push(Transition::OutgoingReconciled {
        prepared: prepared.clone(),
        reconciliation: Box::new(step),
    })
}

fn replay_each_cut(fixture: &Fixture, prepared: &ObjectId) {
    let start = fixture
        .chain
        .iter()
        .position(|(id, _)| id == prepared)
        .unwrap();
    for end in start + 1..=fixture.chain.len() {
        let chain = fixture.chain[..end]
            .iter()
            .map(|(id, event)| (id, event))
            .collect::<Vec<_>>();
        let complete = reduce(None, &fixture.binding, &chain).unwrap();
        let selected = operation(&fixture.binding, &chain[start..]).unwrap();
        assert_eq!(selected.head, complete.head);
        assert_eq!(selected.operations, complete.operations);
        // Recovery consumes the exact stored event bytes, retaining event ids
        // and timestamps rather than constructing a replacement transition.
        for (_, event) in &fixture.chain[..end] {
            let stored = CanonicalObject::freeze(event).unwrap();
            assert_eq!(stored.decode::<Event>().unwrap(), *event);
        }
    }
}

#[test]
fn known_ancestry_and_push_refusals_reconcile_once_and_confirm_only_the_resolved_commit() {
    for pushed in [false, true] {
        let (mut fixture, prepared, original) = prepared();
        let tip = comparison("concurrent remote tip").to_string();
        let failure = if pushed {
            let push_started = fixture.push(Transition::PushStarted {
                prepared: prepared.clone(),
            });
            fixture.push(Transition::PushRejected {
                prepared: prepared.clone(),
                push_started: push_started.clone(),
            });
            OutgoingFailure::NonFastForward {
                push_started,
                fetched_tip: tip.clone(),
            }
        } else {
            OutgoingFailure::AncestryChanged {
                fetched_tip: tip.clone(),
            }
        };
        let resolved = resolved(&original, &tip);
        let reconciliation = step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::Resolved {
                failure,
                resolved: Box::new(resolved.clone()),
            },
        );
        assert_eq!(fixture.reduced().unwrap().head.stage, Stage::Committed);
        fixture.push(Transition::PushStarted {
            prepared: prepared.clone(),
        });
        assert!(
            fixture.reduced().is_err(),
            "a retry needs its own publication start"
        );
        fixture.chain.pop();
        step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::PushStarted { reconciliation },
        );
        fixture.push(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Confirmed {
                observed_tip: original.commit.oid.clone(),
            },
        });
        assert!(
            fixture.reduced().is_err(),
            "the original commit is not the resolved publication"
        );
        fixture.chain.pop();
        let outcome = fixture.push(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Confirmed {
                observed_tip: resolved.commit.oid.clone(),
            },
        });
        let view = fixture.reduced().unwrap();
        assert_eq!(view.head.stage, Stage::Settled);
        assert_eq!(view.head.last_outgoing, Some(outcome.clone()));
        assert_eq!(
            view.operations[&original.operation.to_string()].completed,
            Some(outcome)
        );
        assert_eq!(
            view.operations.len(),
            1,
            "the retry is not a second ledger operation"
        );
        replay_each_cut(&fixture, &prepared);
    }
}

#[test]
fn retry_exhaustion_stops_without_publication_or_cursor_credit() {
    for pushed in [false, true] {
        let (mut fixture, prepared, original) = prepared();
        let tip = comparison("first competing tip").to_string();
        let resolved = resolved(&original, &tip);
        let reconciliation = step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::Resolved {
                failure: OutgoingFailure::AncestryChanged {
                    fetched_tip: tip.clone(),
                },
                resolved: Box::new(resolved.clone()),
            },
        );
        let failure = if pushed {
            let push_started = step(
                &mut fixture,
                &prepared,
                OutgoingReconciliation::PushStarted { reconciliation },
            );
            fixture.push(Transition::PushRejected {
                prepared: prepared.clone(),
                push_started: push_started.clone(),
            });
            OutgoingFailure::NonFastForward {
                push_started,
                fetched_tip: comparison("second tip").to_string(),
            }
        } else {
            OutgoingFailure::AncestryChanged {
                fetched_tip: comparison("second tip").to_string(),
            }
        };
        step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::Resolved {
                failure: failure.clone(),
                resolved: Box::new(resolved.clone()),
            },
        );
        assert!(
            fixture.reduced().is_err(),
            "a second reconciliation cannot reset the budget"
        );
        fixture.chain.pop();
        step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::Stopped { failure },
        );
        let view = fixture.reduced().unwrap();
        assert_eq!(view.head.stage, Stage::Settled);
        assert!(view.head.pending.is_none());
        assert!(view.head.last_outgoing.is_none());
        assert!(
            view.operations[&original.operation.to_string()]
                .completed
                .is_none()
        );
        replay_each_cut(&fixture, &prepared);
        // A reported stop frees the pending slot, but creates no receipt for the
        // old request. A later independent exchange has a fresh operation/key.
        let mut next = fixture.legacy();
        next.key = "independent request after reported stop".into();
        fixture.push(Transition::Prepared {
            intent: Box::new(next),
        });
        assert!(fixture.reduced().is_ok());
    }
}

#[test]
fn unknown_publication_cannot_be_reconciled_or_reported_as_a_known_stop() {
    for retry in [false, true] {
        let (mut fixture, prepared, original) = prepared();
        let tip = comparison("remote tip").to_string();
        let resolved = resolved(&original, &tip);
        let push_started = if retry {
            let reconciliation = step(
                &mut fixture,
                &prepared,
                OutgoingReconciliation::Resolved {
                    failure: OutgoingFailure::AncestryChanged {
                        fetched_tip: tip.clone(),
                    },
                    resolved: Box::new(resolved.clone()),
                },
            );
            step(
                &mut fixture,
                &prepared,
                OutgoingReconciliation::PushStarted { reconciliation },
            )
        } else {
            fixture.push(Transition::PushStarted {
                prepared: prepared.clone(),
            })
        };
        fixture.push(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Unknown { observed_tip: None },
        });
        let view = fixture.reduced().unwrap();
        assert_eq!(view.head.stage, Stage::Unknown);
        assert_eq!(view.head.pending, Some(prepared.clone()));
        replay_each_cut(&fixture, &prepared);
        let failure = OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip,
        };
        for invalid in [
            OutgoingReconciliation::Resolved {
                failure: failure.clone(),
                resolved: Box::new(resolved.clone()),
            },
            OutgoingReconciliation::Stopped { failure },
        ] {
            step(&mut fixture, &prepared, invalid);
            assert!(fixture.reduced().is_err());
            fixture.chain.pop();
        }
    }
}

#[test]
fn reconciliation_refuses_wrong_witness_parent_scope_and_attribution() {
    let (mut fixture, prepared, original) = prepared();
    let tip = comparison("remote tip").to_string();
    let base = resolved(&original, &tip);
    for change in 0..5 {
        let mut candidate = base.clone();
        match change {
            0 => {
                candidate.commit.parents.pop().unwrap();
            }
            1 => candidate.commit.parents.reverse(),
            2 => candidate.operation = original.operation,
            3 => candidate.actor.actor_id.push_str(" other actor"),
            4 => candidate.commit.retention_ref = original.commit.retention_ref.clone(),
            _ => unreachable!(),
        }
        step(
            &mut fixture,
            &prepared,
            OutgoingReconciliation::Resolved {
                failure: OutgoingFailure::AncestryChanged {
                    fetched_tip: tip.clone(),
                },
                resolved: Box::new(candidate),
            },
        );
        assert!(fixture.reduced().is_err(), "invalid selection {change}");
        fixture.chain.pop();
    }
    fixture.push(Transition::PushStarted {
        prepared: prepared.clone(),
    });
    step(
        &mut fixture,
        &prepared,
        OutgoingReconciliation::Resolved {
            failure: OutgoingFailure::NonFastForward {
                push_started: ObjectId::mint(),
                fetched_tip: tip,
            },
            resolved: Box::new(base),
        },
    );
    assert!(
        fixture.reduced().is_err(),
        "a caller cannot substitute the first push witness"
    );
}

#[test]
fn delta_retry_refuses_a_legacy_full_tree_recipe() {
    let mut fixture = Fixture::new(Direction::Outgoing);
    let prepared = fixture.prepare();
    let selected = commit(fixture.intent.operation, "original delta commit");
    fixture.push(Transition::DeltaLocalCommitRecorded {
        prepared: prepared.clone(),
        commit: selected.clone(),
    });
    let mut original = fixture.legacy();
    original.commit = selected;
    let tip = comparison("delta remote tip").to_string();
    let resolved = resolved(&original, &tip);
    step(
        &mut fixture,
        &prepared,
        OutgoingReconciliation::Resolved {
            failure: OutgoingFailure::AncestryChanged { fetched_tip: tip },
            resolved: Box::new(resolved),
        },
    );
    assert!(fixture.reduced().is_err());
}
