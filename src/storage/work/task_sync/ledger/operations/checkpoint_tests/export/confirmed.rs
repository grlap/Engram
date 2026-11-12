//! Historical canonical fixtures; these do not admit a merged graph or claim
//! that a Git commit or remote push actually ran.

use super::*;
use crate::task_sync::ledger::{
    OutgoingFailure, OutgoingReconciliation, PublicationObservation, RetainedCommit,
};
use rusqlite::params;

mod budget;

struct History {
    basis: ConfirmedExportBasis,
    recipe: ObjectId,
    start: ObjectId,
    local: RetainedCommit,
    unknown: ObjectId,
}

fn comparison(label: &str) -> ObjectId {
    CanonicalObject::freeze(&label).unwrap().key().clone()
}

fn commit(operation: uuid::Uuid, label: &str) -> RetainedCommit {
    RetainedCommit {
        oid: comparison(label).to_string(),
        tree: comparison(&format!("tree {label}")).to_string(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: comparison(&format!("raw {label}")),
    }
}

fn history(f: &mut ExportFixture, pushed: bool) -> History {
    let prepared = f.event(Transition::DeltaPrepared {
        intent: Box::new(f.intent.clone()),
        payload: Box::new(f.payload.clone()),
    });
    let local = commit(f.intent.operation, "original local output");
    f.event(Transition::DeltaLocalCommitRecorded {
        prepared: prepared.clone(),
        commit: local.clone(),
    });
    let tip = comparison("concurrent fetched data").to_string();
    let failure = if pushed {
        let push_started = f.event(Transition::PushStarted {
            prepared: prepared.clone(),
        });
        f.event(Transition::PushRejected {
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
    let mut task = f
        .payload
        .documents
        .values()
        .find_map(Option::as_ref)
        .unwrap()
        .clone();
    task.id = crate::WorkId::new();
    task.title = "Concurrent remote planning".into();
    task.outcome = "Historical planning body.".repeat(8192);
    task.activity.clear();
    task.notes.clear();
    let mut resolved = f.intent.clone();
    resolved.operation = uuid::Uuid::new_v4();
    resolved.key = resolved.operation.to_string();
    resolved.expected_head = Some(local.oid.clone());
    resolved.candidate.operation = resolved.operation;
    resolved.candidate.directory = retention::delta::candidate_directory(
        &f.binding.repository.git_directory,
        resolved.operation,
    );
    resolved.candidate.changed = BTreeSet::from([task.id]);
    resolved.candidate.output.inventory_root = f
        .intent
        .candidate
        .output
        .inventory_root
        .including(&task)
        .unwrap();
    let DeltaBasis::Outgoing { export_before, .. } = &mut resolved.basis else {
        unreachable!()
    };
    export_before.anchor = PreparedExportAnchor::Prepared {
        event: prepared.clone(),
    };
    export_before.cut = f.intent.local_cut.clone();
    export_before.membership = f.intent.candidate.output.clone();
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: f.binding.project.clone(),
        operation: resolved.operation,
        output: resolved.candidate.output.clone(),
        documents: BTreeMap::from([(task.id, Some(task))]),
    };
    resolved.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    let mut selected = commit(resolved.operation, "resolved Git output");
    selected.parents = vec![local.oid.clone(), tip];
    let recipe = f.event(Transition::OutgoingReconciled {
        prepared: prepared.clone(),
        reconciliation: Box::new(OutgoingReconciliation::DeltaResolved {
            failure,
            resolved: Box::new(resolved.clone()),
            payload: Box::new(payload),
            commit: Box::new(selected.clone()),
        }),
    });
    let start = f.event(Transition::OutgoingReconciled {
        prepared: prepared.clone(),
        reconciliation: Box::new(OutgoingReconciliation::PushStarted {
            reconciliation: recipe.clone(),
        }),
    });
    let unknown = f.event(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Unknown { observed_tip: None },
    });
    let outcome = f.event(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Confirmed {
            observed_tip: selected.oid.clone(),
        },
    });
    History {
        basis: ConfirmedExportBasis {
            prepared,
            outcome,
            snapshot: MembershipSnapshot {
                commit: selected,
                membership: resolved.candidate.output,
            },
        },
        recipe,
        start,
        local,
        unknown,
    }
}

fn next(f: &ExportFixture, basis: &ConfirmedExportBasis) -> (DeltaIntent, DeltaCandidatePayload) {
    let (mut intent, payload) = f.next_intent(basis);
    let DeltaBasis::Outgoing { export_before, .. } = &mut intent.basis else {
        unreachable!()
    };
    // The native export basis is the original preparation, not merged Git text.
    export_before.membership = f.intent.candidate.output.clone();
    (intent, payload)
}

#[test]
fn outgoing_resolved_confirmation_selects_merged_membership_without_recapturing_it_as_native() {
    for pushed in [false, true] {
        let mut f = ExportFixture::new(32);
        let h = history(&mut f, pushed);
        let (intent, payload) = next(&f, &h.basis);
        assert_ne!(h.basis.snapshot.membership, f.intent.candidate.output);
        let before = state(&f.store);
        assert_eq!(
            f.store
                .inspect_task_sync_delta_export(&f.id, &intent)
                .unwrap(),
            payload
        );
        assert_eq!(state(&f.store), before);
        assert!(!f.intent.candidate.directory.exists());
        let sql = crate::storage::work::task_sync::closure::membership::head::resolution_sql();
        let metadata: String = f
            .store
            .connection
            .query_row(&sql, [h.recipe.as_str()], |row| row.get(0))
            .unwrap();
        assert!(metadata.len() < 8192);
        let value: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert!(value.get("payload").is_none());
        assert!(value.get("documents").is_none());
        let plan = f
            .store
            .connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([h.recipe.as_str()], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" ");
        assert!(
            plan.contains("objects_task_sync_membership_checkpoint"),
            "{plan}"
        );
        assert!(!plan.contains("SCAN"), "{plan}");
        let record_bytes: i64 = f
            .store
            .connection
            .query_row(
                "SELECT length(canonical_json) FROM objects WHERE object_id=?1",
                [h.recipe.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(record_bytes > 128 * 1024);
        assert!(metadata.len() < usize::try_from(record_bytes).unwrap() / 16);
    }
}

#[test]
fn outgoing_resolved_confirmation_survives_a_newer_preparation_and_projection_rebuild() {
    let mut f = ExportFixture::new(1);
    let h = history(&mut f, true);
    let (intent, payload) = next(&f, &h.basis);
    f.intent = intent;
    f.payload = payload;
    let prepared = f.event(Transition::DeltaPrepared {
        intent: Box::new(f.intent.clone()),
        payload: Box::new(f.payload.clone()),
    });
    assert!(f.store.verify_all().unwrap().is_healthy());
    drop(f.store);
    SqliteStore::repair_rebuildable_projections(&f.binding.store.database).unwrap();
    let store = SqliteStore::open(&f.binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let selected = crate::storage::work::task_sync::ledger::view::read::operation_on(
        &store.connection,
        &f.id,
        &f.binding,
        &prepared,
        &mut crate::storage::work::task_sync::ledger::view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(selected.prepared, prepared);
    assert!(matches!(selected.intent,
        crate::storage::work::task_sync::ledger::view::read::CheckedRequest::Delta(intent)
        if intent.as_ref() == &f.intent));
}

#[test]
fn outgoing_resolved_confirmation_refuses_original_membership_commit_and_unknown_outcome() {
    let mut f = ExportFixture::new(2);
    let h = history(&mut f, true);
    let before = state(&f.store);
    for scenario in [
        "native_membership",
        "original_commit",
        "unknown",
        "prepared",
        "commit_bytes",
    ] {
        let mut basis = h.basis.clone();
        match scenario {
            "native_membership" => basis.snapshot.membership = f.intent.candidate.output.clone(),
            "original_commit" => basis.snapshot.commit = h.local.clone(),
            "unknown" => basis.outcome.clone_from(&h.unknown),
            "prepared" => basis.prepared = ObjectId::mint(),
            "commit_bytes" => {
                basis.snapshot.commit.raw_bytes_fingerprint =
                    comparison("different retained bytes");
            }
            _ => unreachable!(),
        }
        let (intent, _) = next(&f, &basis);
        assert!(
            f.store
                .inspect_task_sync_delta_export(&f.id, &intent)
                .is_err(),
            "{scenario}"
        );
        assert_eq!(state(&f.store), before);
    }
}

#[test]
fn outgoing_resolved_confirmation_refuses_missing_links_and_forked_canonical_recipes() {
    for scenario in [
        "recipe",
        "start",
        "fork",
        "recipe_parent",
        "retry_recipe",
        "retry_actor",
    ] {
        let mut f = ExportFixture::new(1);
        let h = history(&mut f, true);
        if scenario == "fork" {
            let event: crate::task_sync::ledger::Event =
                crate::storage::work::task_sync::ledger::view::read::object(
                    &f.store.connection,
                    &h.recipe,
                    crate::task_sync::ledger::EVENT_KIND,
                    &mut crate::storage::work::task_sync::ledger::view::read::ReadBudget::default(),
                )
                .unwrap();
            let object = CanonicalObject::mint(&event).unwrap();
            f.store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
                params![object.key().as_str(), crate::task_sync::ledger::EVENT_KIND, object.bytes()]).unwrap();
        } else if matches!(scenario, "recipe_parent" | "retry_recipe" | "retry_actor") {
            let (id, path) = match scenario {
                "recipe_parent" => (&h.recipe, "$.predecessor"),
                "retry_recipe" => (&h.start, "$.transition.reconciliation.reconciliation"),
                "retry_actor" => (&h.start, "$.actor.reason"),
                _ => unreachable!(),
            };
            f.store.connection.execute(
                "UPDATE objects SET canonical_json=CAST(json_set(canonical_json,?2,?3) AS BLOB) WHERE object_id=?1",
                params![id.as_str(), path, ObjectId::mint().as_str()],
            ).unwrap();
        } else {
            let id = if scenario == "recipe" {
                &h.recipe
            } else {
                &h.start
            };
            f.store
                .connection
                .execute("DELETE FROM objects WHERE object_id=?1", [id.as_str()])
                .unwrap();
        }
        let before = state(&f.store);
        let (intent, _) = next(&f, &h.basis);
        assert!(
            f.store
                .inspect_task_sync_delta_export(&f.id, &intent)
                .is_err(),
            "{scenario}"
        );
        assert_eq!(state(&f.store), before);
    }
}
