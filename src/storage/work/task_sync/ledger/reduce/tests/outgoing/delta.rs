use super::*;
use crate::task_sync::delta::{DELTA_PAYLOAD_FORMAT, DeltaCandidatePayload};

fn prepared_delta() -> (Fixture, ObjectId, RetainedCommit) {
    let mut fixture = Fixture::new(Direction::Outgoing);
    let prepared = fixture.prepare();
    let selected = commit(fixture.intent.operation, "original delta commit");
    fixture.push(Transition::DeltaLocalCommitRecorded {
        prepared: prepared.clone(),
        commit: selected.clone(),
    });
    (fixture, prepared, selected)
}

fn failure(fixture: &mut Fixture, prepared: &ObjectId, pushed: bool, tip: &str) -> OutgoingFailure {
    if pushed {
        let push_started = fixture.push(Transition::PushStarted {
            prepared: prepared.clone(),
        });
        fixture.push(Transition::PushRejected {
            prepared: prepared.clone(),
            push_started: push_started.clone(),
        });
        OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip.into(),
        }
    } else {
        OutgoingFailure::AncestryChanged {
            fetched_tip: tip.into(),
        }
    }
}

fn resolution(
    fixture: &Fixture,
    prepared: &ObjectId,
    selected: &RetainedCommit,
    failure: OutgoingFailure,
) -> OutgoingReconciliation {
    let mut resolved = fixture.intent.clone();
    resolved.operation = uuid::Uuid::new_v4();
    resolved.key = resolved.operation.to_string();
    resolved.expected_head = Some(selected.oid.clone());
    let DeltaBasis::Outgoing { export_before, .. } = &mut resolved.basis else {
        unreachable!()
    };
    export_before.anchor = PreparedExportAnchor::Prepared {
        event: prepared.clone(),
    };
    export_before.membership = fixture.intent.candidate.output.clone();
    export_before.cut = fixture.intent.local_cut.clone();
    let mut task = crate::storage::work::task_sync::tests::candidate()
        .tasks
        .into_iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap();
    task.external_ref = None;
    task.prerequisites.clear();
    task.blockers.clear();
    task.superseded_by = None;
    task.notes.clear();
    task.activity.clear();
    resolved.candidate.operation = resolved.operation;
    resolved.candidate.directory = resolved
        .candidate
        .directory
        .with_file_name(resolved.operation.to_string());
    resolved.candidate.changed = BTreeSet::from([task.id]);
    resolved.candidate.output.inventory_root = InventoryRoot::ZERO.including(&task).unwrap();
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: fixture.binding.project.clone(),
        operation: resolved.operation,
        output: resolved.candidate.output.clone(),
        documents: BTreeMap::from([(task.id, Some(task))]),
    };
    resolved.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    let mut commit = commit(resolved.operation, "resolved delta commit");
    commit.parents = vec![selected.oid.clone(), failure.fetched_tip().into()];
    OutgoingReconciliation::DeltaResolved {
        failure,
        resolved: Box::new(resolved),
        payload: Box::new(payload),
        commit: Box::new(commit),
    }
}

#[test]
fn delta_resolution_keeps_native_preparation_separate_from_merged_git_membership() {
    for pushed in [false, true] {
        let (mut f, prepared, selected) = prepared_delta();
        let original = f.intent.clone();
        let known = failure(&mut f, &prepared, pushed, &comparison("remote").to_string());
        let recipe = resolution(&f, &prepared, &selected, known);
        let OutgoingReconciliation::DeltaResolved {
            commit, resolved, ..
        } = &recipe
        else {
            unreachable!()
        };
        let merged_commit = commit.clone();
        assert_ne!(resolved.candidate.output, original.candidate.output);
        let retry = step(&mut f, &prepared, recipe);
        assert_eq!(f.reduced().unwrap().head.pending, Some(prepared.clone()));
        step(
            &mut f,
            &prepared,
            OutgoingReconciliation::PushStarted {
                reconciliation: retry,
            },
        );
        f.push(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Confirmed {
                observed_tip: selected.oid,
            },
        });
        assert!(
            f.reduced().is_err(),
            "original commit cannot confirm the merged bytes"
        );
        f.chain.pop();
        let outcome = f.push(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Confirmed {
                observed_tip: merged_commit.oid,
            },
        });
        let view = f.reduced().unwrap();
        assert_eq!(view.operations.len(), 1);
        assert_eq!(
            view.operations[&original.operation.to_string()].completed,
            Some(outcome.clone())
        );
        assert_eq!(view.head.last_outgoing, Some(outcome));
        assert_eq!(
            f.chain[1].1.transition.retained_delta().unwrap().0,
            &original
        );
        replay_each_cut(&f, &prepared);
    }
}

#[test]
fn delta_resolution_shares_one_retry_across_both_known_failure_paths() {
    for first_push in [false, true] {
        for second_push in [false, true] {
            let (mut f, prepared, selected) = prepared_delta();
            let first = failure(
                &mut f,
                &prepared,
                first_push,
                &comparison("first tip").to_string(),
            );
            let recipe = resolution(&f, &prepared, &selected, first);
            let retry = step(&mut f, &prepared, recipe.clone());
            let second_tip = comparison("second tip").to_string();
            let second = if second_push {
                let push_started = step(
                    &mut f,
                    &prepared,
                    OutgoingReconciliation::PushStarted {
                        reconciliation: retry,
                    },
                );
                step(
                    &mut f,
                    &prepared,
                    OutgoingReconciliation::Stopped {
                        failure: OutgoingFailure::NonFastForward {
                            push_started: push_started.clone(),
                            fetched_tip: second_tip.clone(),
                        },
                    },
                );
                assert!(
                    f.reduced().is_err(),
                    "an unrecorded retry rejection cannot stop the exchange"
                );
                f.chain.pop();
                f.push(Transition::PushRejected {
                    prepared: prepared.clone(),
                    push_started: push_started.clone(),
                });
                OutgoingFailure::NonFastForward {
                    push_started,
                    fetched_tip: second_tip,
                }
            } else {
                OutgoingFailure::AncestryChanged {
                    fetched_tip: second_tip,
                }
            };
            step(&mut f, &prepared, recipe);
            assert!(
                f.reduced().is_err(),
                "second resolution is forbidden across either failure path"
            );
            f.chain.pop();
            step(
                &mut f,
                &prepared,
                OutgoingReconciliation::Stopped { failure: second },
            );
            let view = f.reduced().unwrap();
            assert_eq!(view.head.stage, Stage::Settled);
            assert!(view.head.last_outgoing.is_none());
            assert!(
                view.operations[&f.intent.operation.to_string()]
                    .completed
                    .is_none()
            );
            replay_each_cut(&f, &prepared);
        }
    }
}

#[test]
fn delta_resolution_refuses_changed_native_basis_attribution_payload_and_parent_selection() {
    let (mut f, prepared, selected) = prepared_delta();
    let known = failure(&mut f, &prepared, false, &comparison("remote").to_string());
    let base = resolution(&f, &prepared, &selected, known);
    for changed in [
        "operation",
        "key",
        "actor",
        "producer",
        "cut",
        "common",
        "anchor",
        "membership",
        "basis_cut",
        "confirmed",
        "directory",
        "payload",
        "expected_head",
        "parents",
        "retention",
        "width",
    ] {
        let mut recipe = base.clone();
        let OutgoingReconciliation::DeltaResolved {
            resolved,
            payload,
            commit,
            ..
        } = &mut recipe
        else {
            unreachable!()
        };
        let DeltaBasis::Outgoing {
            export_before,
            confirmed_before,
        } = &mut resolved.basis
        else {
            unreachable!()
        };
        match changed {
            "operation" => resolved.operation = f.intent.operation,
            "key" => resolved.key.clone_from(&f.intent.key),
            "actor" => resolved.actor.actor_id.push_str(" changed"),
            "producer" => resolved.producer = comparison("different producer"),
            "cut" => resolved.local_cut.position.position += 1,
            "common" => resolved.common_commit = Some(selected.clone()),
            "anchor" => {
                export_before.anchor = PreparedExportAnchor::Prepared {
                    event: ObjectId::mint(),
                };
            }
            "membership" => export_before.membership = resolved.candidate.output.clone(),
            "basis_cut" => export_before.cut.position.position += 1,
            "confirmed" => {
                *confirmed_before = Some(Box::new(crate::task_sync::delta::ConfirmedExportBasis {
                    outcome: ObjectId::mint(),
                    prepared: prepared.clone(),
                    snapshot: MembershipSnapshot {
                        commit: selected.clone(),
                        membership: f.intent.candidate.output.clone(),
                    },
                }));
            }
            "directory" => resolved
                .candidate
                .directory
                .clone_from(&f.intent.candidate.directory),
            "payload" => payload.documents.clear(),
            "expected_head" => resolved.expected_head = None,
            "parents" => commit.parents.swap(0, 1),
            "retention" => commit.retention_ref.clone_from(&selected.retention_ref),
            "width" => {
                commit.oid.truncate(40);
                commit.tree.truncate(40);
                commit.parents.iter_mut().for_each(|oid| oid.truncate(40));
            }
            _ => unreachable!(),
        }
        step(&mut f, &prepared, recipe);
        assert!(f.reduced().is_err(), "changed {changed}");
        f.chain.pop();
    }
    let unknown_start = f.push(Transition::PushStarted {
        prepared: prepared.clone(),
    });
    f.push(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Unknown { observed_tip: None },
    });
    step(&mut f, &prepared, base);
    assert!(
        f.reduced().is_err(),
        "unknown push cannot authorize a new recipe; witness {unknown_start}"
    );
}

#[test]
fn delta_resolution_artifact_aliases_use_keyed_canonical_reads_and_one_operation_projection() {
    let (mut f, prepared, selected) = prepared_delta();
    let recipe = resolution(
        &f,
        &prepared,
        &selected,
        OutgoingFailure::AncestryChanged {
            fetched_tip: comparison("remote").to_string(),
        },
    );
    let OutgoingReconciliation::DeltaResolved { resolved, .. } = &recipe else {
        unreachable!()
    };
    let artifact = resolved.clone();
    let retry = step(&mut f, &prepared, recipe);
    step(
        &mut f,
        &prepared,
        OutgoingReconciliation::PushStarted {
            reconciliation: retry,
        },
    );
    let store = crate::SqliteStore::open_in_memory().unwrap();
    let mut ledger = Ledger::default();
    let object = CanonicalObject::identified(&f.id, &f.binding).unwrap();
    store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
        rusqlite::params![f.id.as_str(), crate::task_sync::ledger::BINDING_KIND, object.bytes()]).unwrap();
    ledger.bindings.insert(f.id.clone(), f.binding.clone());
    for (id, event) in &f.chain {
        let object = CanonicalObject::identified(id, event).unwrap();
        store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
            rusqlite::params![id.as_str(), crate::task_sync::ledger::EVENT_KIND, object.bytes()]).unwrap();
        ledger.events.insert(id.clone(), event.clone());
        view::record(&store.connection, &ledger, event).unwrap();
    }
    assert_eq!(
        view::read::reconciliation_parent_for_operation(
            &store.connection,
            &f.id,
            artifact.operation
        )
        .unwrap(),
        Some(prepared.clone())
    );
    assert!(
        view::read::request_by(&store.connection, &f.id, artifact.operation, &artifact.key)
            .is_err()
    );
    assert!(
        view::read::request_by(
            &store.connection,
            &f.id,
            uuid::Uuid::new_v4(),
            &artifact.key
        )
        .is_err()
    );
    assert!(
        view::read::request_by(
            &store.connection,
            &f.id,
            artifact.operation,
            "unrelated key"
        )
        .is_err()
    );
    let mut budget = view::read::ReadBudget::default();
    let actual =
        view::read::operation_on(&store.connection, &f.id, &f.binding, &prepared, &mut budget)
            .unwrap();
    assert_eq!(actual.prepared, prepared);
    assert_eq!(actual.events, f.chain[1..]);
    let count: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_exchange_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    // Re-deriving selected rows does not turn the artifact into another request.
    store
        .connection
        .execute("DELETE FROM work_task_sync_exchange_operations", [])
        .unwrap();
    view::record(&store.connection, &ledger, &f.chain.last().unwrap().1).unwrap();
    view::read::operation_on(
        &store.connection,
        &f.id,
        &f.binding,
        &prepared,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
}
