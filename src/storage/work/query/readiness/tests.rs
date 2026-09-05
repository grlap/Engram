use std::cell::Cell;

use rusqlite::trace::{TraceEvent, TraceEventCodes};

use crate::storage::work::test_support::*;
use crate::storage::work::*;
use crate::verbs::{AgentVerbs, GateInput, NoteInput};
use crate::{
    DecomposeWorkRequest, DisposeWorkRequest, ReviseWorkRequest, WaiveRequiredChildRequest,
    WorkDisposition,
};

struct Fixture {
    directory: tempfile::TempDir,
    store: SqliteStore,
    root: WorkItem,
    claim: WorkClaim,
    sealed_child: CompletionSeal,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("fixture directory");
        let mut store = SqliteStore::open(directory.path().join("work.db")).expect("fixture store");
        let root = store
            .create_work(
                &root_request("advisory-readiness", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let plan = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("waived", ChildRequirement::Required, "Disposed outcome"),
                        child("sealed", ChildRequirement::Required, "Delivered outcome"),
                        child("optional", ChildRequirement::Optional, "Optional branch"),
                    ],
                    prerequisites: vec![],
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: "plan".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let root = plan.parent;
        let waived = &plan.children[0];
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: waived.work_id,
                    expected_work_revision: waived.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "explicit omission".into(),
                    actor: actor("planner"),
                    idempotency_key: "dispose".into(),
                    disposed_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose child");
        store
            .waive_required_child(
                &WaiveRequiredChildRequest {
                    parent_id: root.work_id,
                    child_id: waived.work_id,
                    expected_parent_revision: root.revision,
                    reason: "accept omission".into(),
                    actor: actor("planner"),
                    idempotency_key: "waive".into(),
                    waived_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("waive child");
        let delivered = &plan.children[1];
        let child_claim = claim(&mut store, delivered, "holder", "child-claim", 4, 3600);
        let child_evidence = evidence(
            &mut store,
            delivered,
            &child_claim,
            "holder",
            "child-evidence",
            5,
        );
        checkpoint(
            &mut store,
            delivered,
            &child_claim,
            "holder",
            "child-checkpoint",
            6,
            std::slice::from_ref(&child_evidence),
        );
        let sealed_child = complete(
            &mut store,
            delivered,
            &child_claim,
            "holder",
            &child_evidence,
            "child-complete",
            7,
        )
        .expect("complete child");
        let claim = claim(&mut store, &root, "holder", "root-claim", 8, 3600);
        // Fill the bounded evidence page before either measurement. Adding
        // the sampled note/gate must not change how many summaries fit.
        let seed_evidence = (0..8)
            .map(|index| {
                evidence(
                    &mut store,
                    &root,
                    &claim,
                    "holder",
                    &format!("seed-{index}"),
                    9 + index,
                )
            })
            .collect::<Vec<_>>();
        checkpoint(
            &mut store,
            &root,
            &claim,
            "holder",
            "seed-checkpoint",
            18,
            &seed_evidence,
        );
        Self {
            directory,
            store,
            root,
            claim,
            sealed_child,
        }
    }

    fn verbs(&self) -> AgentVerbs {
        AgentVerbs::new(
            self.directory.path().join("work.db"),
            self.root.project_id.clone(),
            "holder".into(),
            self.claim.holder.clone(),
            None,
        )
    }

    fn grow_history(&mut self, count: i64, start: i64) {
        for index in 0..count {
            self.root = self
                .store
                .revise_work(
                    &ReviseWorkRequest {
                        work_id: self.root.work_id,
                        expected_revision: self.root.revision,
                        patch: WorkRevisionPatch {
                            title: Some(format!("Planning revision {start}-{index}")),
                            ..WorkRevisionPatch::default()
                        },
                        authority: WorkPlanningAuthority::Claim {
                            run_id: self.claim.run_id,
                            holder: self.claim.holder.clone(),
                            claim_id: self.claim.claim_id,
                            claim_fence: self.claim.fence,
                        },
                        actor: actor("holder"),
                        idempotency_key: format!("revision-{start}-{index}"),
                        updated_at: at(start + index),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("grow real revision history");
        }
    }
}

fn measured<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    crate::canonical::reset_canonical_decode_count();
    let result = operation();
    let count = crate::canonical::canonical_decode_count();
    (result, count)
}

thread_local! {
    static READINESS_STATEMENTS: Cell<usize> = const { Cell::new(0) };
}

fn measured_queries<T>(
    connection: &rusqlite::Connection,
    operation: impl FnOnce() -> T,
) -> (T, usize, usize) {
    READINESS_STATEMENTS.set(0);
    // Trace only this fixture connection; count work, never elapsed time or
    // SQL text. No production connection or process-global hook is changed.
    connection.trace_v2(
        TraceEventCodes::SQLITE_TRACE_STMT,
        Some(|event| {
            if matches!(event, TraceEvent::Stmt(..)) {
                READINESS_STATEMENTS.set(READINESS_STATEMENTS.get() + 1);
            }
        }),
    );
    let (result, decodes) = measured(operation);
    connection.trace_v2(TraceEventCodes::empty(), None);
    let statements = READINESS_STATEMENTS.get();
    assert!(statements > 0, "the sample must observe its SQLite reads");
    (result, decodes, statements)
}

fn guidance_cost(fixture: &Fixture, verbs: &AgentVerbs, second: i64) -> [usize; 5] {
    let now = at(second);
    let (readiness, readiness_count, readiness_statements) =
        measured_queries(&fixture.store.connection, || {
            fixture.store.work_completion_readiness(
                fixture.root.work_id,
                &fixture.claim.holder,
                now,
            )
        });
    assert!(
        readiness.expect("readiness").0,
        "a seal plus an exact waiver account for required children; optional open work is not a barrier"
    );
    let (waivable, waivable_count, waiver_statements) =
        measured_queries(&fixture.store.connection, || {
            fixture.store.waivable_required_children(&fixture.root, 8)
        });
    assert!(waivable.expect("waivable").is_empty());
    let (shown, show_count) = measured(|| verbs.show(&fixture.root.short_ref, now));
    assert!(
        shown.expect("show").value["allowed_next"]
            .as_array()
            .expect("allowed next")
            .iter()
            .any(|value| value == "work_complete:capture" || value == "work_complete")
    );
    [
        readiness_count,
        waivable_count,
        show_count,
        readiness_statements,
        waiver_statements,
    ]
}

#[test]
fn advisory_child_guidance_stays_bounded_as_root_history_grows() {
    let mut fixture = Fixture::new();
    let verbs = fixture.verbs();
    for (index, history_count) in [32, 512].into_iter().enumerate() {
        let start = 100 + i64::try_from(index).expect("index") * 1000;
        // Revised summaries also read their adjacent planning snapshots.
        // Keep that bounded history window homogeneous in both samples.
        fixture.grow_history(32, start);
        let start = start + 32;
        let second = start + history_count + 1;
        // Derive the budget from the same current-state reads at runtime.
        // Only history grows between samples: evidence-page composition,
        // child seals and waivers stay unchanged. No wall-clock limit or
        // pinned decode ceiling substitutes for this comparison.
        let before = guidance_cost(&fixture, &verbs, second);
        fixture.grow_history(history_count, start);
        let after = guidance_cost(&fixture, &verbs, second);
        for ((before, after), label) in before.into_iter().zip(after).zip([
            "readiness decodes",
            "waivable decodes",
            "show decodes",
            "readiness statements",
            "waivable statements",
        ]) {
            assert!(
                after <= before,
                "{label} must not replay growing history: {before} -> {after}"
            );
        }
        // The holder mutations consuming this guidance must still succeed.
        verbs
            .note(
                &NoteInput {
                    work_ref: Some(fixture.root.short_ref.clone()),
                    text: format!("progress {index}"),
                    refs: vec![],
                },
                at(second),
            )
            .expect("note");
        verbs
            .gate(
                GateInput {
                    work_ref: Some(fixture.root.short_ref.clone()),
                    name: format!("check {index}"),
                    failed: vec![],
                    evidence_ref: None,
                },
                at(second),
            )
            .expect("gate");
    }
    let report = fixture
        .store
        .verify_all()
        .expect("verify populated fixture");
    assert!(report.is_healthy(), "{report:?}");
}

fn prepare_completion(fixture: &mut Fixture) -> CompleteWorkRequest {
    let evidence = evidence(
        &mut fixture.store,
        &fixture.root,
        &fixture.claim,
        "holder",
        "root-evidence",
        30,
    );
    let all_evidence = fixture
        .store
        .work_run_evidence(fixture.claim.run_id)
        .expect("complete evidence set");
    checkpoint(
        &mut fixture.store,
        &fixture.root,
        &fixture.claim,
        "holder",
        "root-checkpoint",
        31,
        &all_evidence,
    );
    assert_eq!(
        fixture
            .store
            .work_completion_readiness(fixture.root.work_id, &fixture.claim.holder, at(32))
            .expect("ready"),
        (true, true)
    );
    let mut request = completion_request(
        &fixture.root,
        &fixture.claim,
        "holder",
        &evidence,
        "child-proof",
        32,
    );
    request.evidence = all_evidence;
    request
}

#[test]
fn advisory_child_readiness_requires_a_current_seal_row() {
    let fixture = Fixture::new();
    fixture
        .store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    fixture
        .store
        .connection
        .execute(
            "DELETE FROM work_completion_seals WHERE run_id = ?1",
            [fixture.sealed_child.run_id.0.to_string()],
        )
        .expect("remove child seal projection");
    assert_eq!(
        fixture
            .store
            .work_completion_readiness(fixture.root.work_id, &fixture.claim.holder, at(32))
            .expect("missing seal readiness"),
        (false, false)
    );
    restore_savepoint(&fixture.store);
}

#[derive(Clone, Copy, Debug)]
enum ProofDamage {
    SealBytes,
    MissingWaiverEvent,
}

fn current_execution(fixture: &Fixture) -> RootExecution {
    super::super::load_root_execution(
        &fixture.store.connection,
        fixture.sealed_child.root_execution_id,
    )
    .expect("canonical-bound execution")
}

fn altered_execution(
    execution: &RootExecution,
    edit: impl FnOnce(&mut RootExecution),
) -> RootExecution {
    let mut altered = execution.clone();
    edit(&mut altered);
    altered
}

fn assert_invalid_current_waiver(fixture: &Fixture, execution: &RootExecution) {
    let error = super::current_required_child_waivers(
        &fixture.store.connection,
        fixture.root.work_id,
        execution,
    )
    .expect_err("invalid current waiver");
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(ref reason)
        if reason.contains("has an invalid current required-child waiver")),
        "{error:?}"
    );
}

#[test]
fn current_waiver_guards_reject_invalid_execution_bindings() {
    let fixture = Fixture::new();
    let execution = current_execution(&fixture);
    assert_eq!(execution.required_child_waivers.len(), 1);
    assert_eq!(
        super::current_required_child_waivers(
            &fixture.store.connection,
            fixture.root.work_id,
            &execution,
        )
        .expect("valid waiver")
        .len(),
        1
    );
    // Exercise this helper directly: corrupting only execution_json would
    // stop at the caller's canonical-binding guard before reaching it.
    for altered in [
        altered_execution(&execution, |value| {
            value
                .required_child_waivers
                .push(value.required_child_waivers[0].clone());
        }),
        altered_execution(&execution, |value| value.root_id = WorkId::new()),
        altered_execution(&execution, |value| {
            value.project_id = ProjectId("other-project".into());
        }),
        altered_execution(&execution, |value| {
            value.required_child_waivers[0].work_revision += 1;
        }),
        altered_execution(&execution, |value| {
            value.required_child_waivers[0].waived_by = " ".into();
        }),
        altered_execution(&execution, |value| {
            value.required_child_waivers[0].reason = " ".into();
        }),
    ] {
        assert_invalid_current_waiver(&fixture, &altered);
    }
}

#[test]
fn current_waiver_guards_reject_invalid_child_shapes() {
    let mut fixture = Fixture::new();
    let execution = current_execution(&fixture);
    let optional_id: String = fixture.store.connection.query_row(
        "SELECT work_id FROM work_items WHERE parent_id = ?1 AND child_requirement = 'optional'",
        [fixture.root.work_id.0.to_string()], |row| row.get(0),
    ).expect("optional child");
    let optional = super::load_work_item(
        &fixture.store.connection,
        WorkId(uuid::Uuid::parse_str(&optional_id).expect("work id")),
    )
    .expect("optional item");
    let other_root = fixture
        .store
        .create_work(
            &root_request("advisory-readiness", "parentless", 20),
            &DevelopmentNoopRedactor,
        )
        .expect("parentless work");
    // Valid canonical items isolate the parent/requirement checks from the
    // lifecycle check: both are genuinely cancelled and revision-bound.
    for item in [&optional, &other_root] {
        fixture
            .store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "shape fixture".into(),
                    actor: actor("planner"),
                    idempotency_key: format!("shape-{}", item.work_id.0),
                    disposed_at: at(21),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel shape fixture");
    }
    for id in [
        optional.work_id,
        other_root.work_id,
        fixture.sealed_child.work_id,
    ] {
        let child = super::load_work_item(&fixture.store.connection, id).expect("current child");
        let altered = altered_execution(&execution, |value| {
            value.root_id = child.root_id;
            value.required_child_waivers[0].work_id = child.work_id;
            value.required_child_waivers[0].work_revision = child.revision;
        });
        assert_invalid_current_waiver(&fixture, &altered);
    }
}

#[test]
fn advisory_child_readiness_requires_execution_seal_membership() {
    let fixture = Fixture::new();
    let execution = current_execution(&fixture);
    let seal_hash: String = fixture
        .store
        .connection
        .query_row(
            "SELECT seal_hash FROM work_completion_seals WHERE run_id = ?1",
            [fixture.sealed_child.run_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("seal row remains present");
    let altered = altered_execution(&execution, |value| {
        value
            .required_child_seals
            .retain(|hash| hash.as_str() != seal_hash);
    });
    assert_eq!(
        altered.required_child_seals.len() + 1,
        execution.required_child_seals.len()
    );
    assert!(super::required_children_ready(
        &fixture.store.connection, fixture.root.work_id, &execution,
    ).expect("bound seal"));
    assert!(
        !super::required_children_ready(&fixture.store.connection, fixture.root.work_id, &altered,)
            .expect("unbound seal is not ready")
    );
}

#[test]
fn advisory_entrypoints_refuse_projection_only_waiver_drift() {
    // Expect load_root_execution/active_root_execution_optional's existing
    // canonical-binding refusals, so only the error variant matters here.
    // The two current_waiver_guards_* tests exercise the new helper branches.
    let fixture = Fixture::new();
    for (path, value) in [
        (
            "$.required_child_waivers[0].work_revision",
            serde_json::json!(0),
        ),
        ("$.required_child_waivers[0].reason", serde_json::json!("")),
        ("$.project_id", serde_json::json!("other-project")),
    ] {
        fixture
            .store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        fixture.store.connection.execute(
            "UPDATE work_root_executions SET execution_json = CAST(json_set(execution_json, ?2, json(?3)) AS BLOB)
             WHERE root_execution_id = ?1",
            params![fixture.sealed_child.root_execution_id.0.to_string(), path, value.to_string()],
        ).expect("projection-only drift");
        let readiness_error = fixture
            .store
            .work_completion_readiness(fixture.root.work_id, &fixture.claim.holder, at(32))
            .expect_err("readiness refuses unbound execution");
        let waiver_error = fixture
            .store
            .waivable_required_children(&fixture.root, 8)
            .expect_err("waiver listing refuses unbound execution");
        for error in [readiness_error, waiver_error] {
            assert!(
                matches!(error, StoreError::InvalidWorkProjection(_)),
                "{error:?}"
            );
        }
        restore_savepoint(&fixture.store);
    }
}

#[test]
fn advisory_child_readiness_does_not_replace_completion_proof_validation() {
    for damage in [ProofDamage::SealBytes, ProofDamage::MissingWaiverEvent] {
        let mut fixture = Fixture::new();
        let request = prepare_completion(&mut fixture);
        let connection = &fixture.store.connection;
        let root_id = fixture.root.work_id.0.to_string();
        let (sql, parameter) = match damage {
            ProofDamage::SealBytes => (
                "SELECT seal_hash FROM work_completion_seals WHERE run_id = ?1",
                fixture.sealed_child.run_id.0.to_string(),
            ),
            ProofDamage::MissingWaiverEvent => (
                "SELECT entry.object_hash FROM work_feed_entries entry
                 JOIN objects object ON object.object_hash = entry.object_hash
                 WHERE entry.feed_kind = 'root_work' AND entry.feed_id = ?1
                   AND object.object_kind = 'work_event'
                   AND json_extract(object.canonical_json, '$.transition.kind') = 'required_child_waived'",
                root_id.clone(),
            ),
        };
        let hash: String = connection
            .query_row(sql, [parameter], |row| row.get(0))
            .expect("retained proof hash");
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [&hash],
                |row| row.get(0),
            )
            .expect("retain original proof bytes");
        let feed_row: Option<(i64, String, Option<String>)> =
            matches!(damage, ProofDamage::MissingWaiverEvent).then(|| {
                connection
                    .query_row(
                        "SELECT position, object_kind, work_id FROM work_feed_entries
                 WHERE feed_kind = 'root_work' AND feed_id = ?1 AND object_hash = ?2",
                        params![root_id, hash],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .expect("retain original waiver feed row")
            });
        // Commit the corruption before invoking a mutation. An outer SAVEPOINT
        // would make BEGIN IMMEDIATE fail before the proof checks under test.
        let affected = match damage {
            ProofDamage::SealBytes => connection.execute(
                "UPDATE objects SET canonical_json = CAST('{}' AS BLOB) WHERE object_hash = ?1",
                [&hash],
            ),
            ProofDamage::MissingWaiverEvent => connection.execute(
                "DELETE FROM work_feed_entries WHERE feed_kind = 'root_work' AND feed_id = ?1 AND object_hash = ?2",
                params![root_id, hash],
            ),
        }.expect("damage retained proof");
        assert_eq!(affected, 1);
        assert!(connection.is_autocommit());
        // The hint may still be offered from current projections. It must
        // never replace proof verification under the completion lock.
        assert!(
            fixture
                .store
                .work_completion_readiness(fixture.root.work_id, &fixture.claim.holder, at(32))
                .expect("advisory readiness")
                .0
        );
        // Includes all feed heads, canonical objects and work rows, not just
        // counts; even a same-count rewrite must be rolled back on refusal.
        let before =
            test_database_shape_snapshot(&fixture.store.connection).expect("before refusal");
        let error = fixture
            .store
            .complete_work(&request, &DevelopmentNoopRedactor)
            .expect_err("completion must verify the damaged proof");
        match (damage, error) {
            (ProofDamage::SealBytes, StoreError::HashMismatch { expected, actual }) => {
                assert_eq!(expected.as_str(), hash);
                assert_eq!(actual, ObjectHash::from_canonical_bytes(b"{}"));
            }
            (ProofDamage::MissingWaiverEvent, StoreError::InvalidWorkProjection(reason)) => {
                assert_eq!(
                    reason,
                    format!(
                        "root execution {:?} required-child waivers do not match canonical events",
                        fixture.sealed_child.root_execution_id
                    )
                );
            }
            (damage, error) => panic!("{damage:?} returned a non-proof error: {error:?}"),
        }
        assert_eq!(
            test_database_shape_snapshot(&fixture.store.connection).expect("after refusal"),
            before
        );
        assert!(
            !fixture
                .store
                .verify_all()
                .expect("doctor rejects corrupt proof")
                .is_healthy()
        );
        let connection = &fixture.store.connection;
        let restored = match damage {
            ProofDamage::SealBytes => connection.execute(
                "UPDATE objects SET canonical_json = ?2 WHERE object_hash = ?1",
                params![hash, bytes],
            ),
            ProofDamage::MissingWaiverEvent => {
                let (position, kind, work_id) = feed_row.expect("saved waiver feed row");
                connection.execute(
                "INSERT INTO work_feed_entries (feed_kind, feed_id, position, object_kind, object_hash, work_id)
                 VALUES ('root_work', ?1, ?2, ?3, ?4, ?5)",
                params![root_id, position, kind, hash, work_id],
                )
            },
        }.expect("restore exact retained proof");
        assert_eq!(restored, 1);
        assert!(connection.is_autocommit());
        assert!(
            fixture
                .store
                .verify_all()
                .expect("restored fixture")
                .is_healthy()
        );
        fixture
            .store
            .complete_work(&request, &DevelopmentNoopRedactor)
            .expect("the identical completion succeeds with intact proofs");
    }
}
