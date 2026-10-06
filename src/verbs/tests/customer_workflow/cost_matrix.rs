//! Count attribution, with setup and validation excluded from capture.
use super::*;
use crate::domain::{
    ActorContext, AssuranceLevel, ChildRequirement, ChildWorkDraft, DecomposeWorkRequest,
    DisposeWorkRequest, RecordGateEvidenceRequest, RecordWorkNoteRequest,
    WaiveRequiredChildRequest, WorkDisposition, WorkPlanningAuthority,
};
use crate::memory::DevelopmentNoopRedactor;
use crate::storage::work_cost;

#[derive(Clone, Copy, Debug)]
struct Case {
    shape: &'static str,
    size: usize,
    history: usize,
    prior_waivers: usize,
    evidence_bytes: usize,
    unresolved: bool,
}

fn actor() -> ActorContext {
    ActorContext {
        actor_id: "agent".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("agent".into())),
        source_tool: Some("cost_matrix".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "attribute operation work".into(),
    }
}

fn draft(index: usize, required: bool) -> ChildWorkDraft {
    ChildWorkDraft {
        local_key: format!("child-{index}"),
        title: format!("Child {index}"),
        outcome: "Fixture child".into(),
        acceptance: vec!["delivered".into()],
        child_requirement: if required {
            ChildRequirement::Required
        } else {
            ChildRequirement::Optional
        },
        external_ref: None,
        notes: Vec::new(),
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: crate::WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
    }
}

fn claim(verbs: &AgentVerbs, work: &str, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(second),
        )
        .unwrap();
}

fn planning_authority(claim: &crate::WorkClaim) -> WorkPlanningAuthority {
    WorkPlanningAuthority::Claim {
        run_id: claim.run_id,
        holder: claim.holder.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    }
}

fn waive(verbs: &AgentVerbs, parent: &str, child: &str, second: i64) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.into()),
                action: UpdateAction::WaiveRequiredChild {
                    child: child.into(),
                    reason: "Cancelled fixture child accounted for".into(),
                },
            },
            at(second),
        )
        .unwrap();
}

struct Prepared {
    // Store and verbs are dropped before the scratch directory.
    store: SqliteStore,
    verbs: AgentVerbs,
    _directory: crate::test_support::TempHome,
    parent: String,
    children: Vec<crate::WorkItem>,
    work: crate::WorkItem,
}

fn prepare(case: Case) -> Prepared {
    prepare_with_options(case, false)
}

fn prepare_with_options(case: Case, benchmark: bool) -> Prepared {
    let (directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Matrix root", None, false, 0);
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    claim(&peer, &parent, 1);
    peer.update(
        UpdateInput {
            work_ref: Some(parent.clone()),
            action: UpdateAction::Release {
                reason: Some("No contribution required".into()),
            },
        },
        at(2),
    )
    .unwrap();
    drop(peer);
    let sealed = add(&verbs, "Sealed required child", Some(&parent), false, 3);
    claim(&verbs, &sealed, 4);
    verbs
        .done(
            DoneInput {
                work_ref: Some(sealed),
                summary: Some("Delivered child".into()),
                ..DoneInput::default()
            },
            at(5),
        )
        .unwrap();
    claim(&verbs, &parent, 6);
    note(&verbs, &parent, &"s".repeat(case.evidence_bytes), 7);
    let mut store = SqliteStore::open(&path).unwrap();
    let work = store.resolve_work_ref(&project, &parent).unwrap();
    let runs = match case.shape {
        "runs" => case.size,
        "mixed" => case.size / 2,
        _ => 0,
    };
    let contributors = match case.shape {
        "contributors" => case.size,
        "mixed" => case.size - runs,
        _ => 0,
    };
    let participants = (0..contributors)
        .map(|index| SessionId(format!("synthetic-{index:08}")))
        .collect::<Vec<_>>();
    // One canonical transition for this member axis, independent of its size.
    store
        .add_root_cost_fixture(work.work_id, &participants, at(8))
        .unwrap();
    let cancelled_count = if benchmark { 22 } else { 21 };
    let mut children = (0..cancelled_count)
        .map(|index| draft(index, true))
        .collect::<Vec<_>>();
    if case.unresolved {
        children.push(draft(21, true));
    }
    children.extend((0..runs).map(|index| draft(index + 22, false)));
    let mut created = Vec::new();
    for (batch, children) in children.chunks(16).enumerate() {
        let parent_state = store.get_work_item(work.work_id).unwrap();
        let parent_claim = store.current_work_claim(work.work_id).unwrap().unwrap();
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: work.work_id,
                    expected_parent_revision: parent_state.revision,
                    children: children.to_vec(),
                    prerequisites: Vec::new(),
                    authority: planning_authority(&parent_claim),
                    actor: actor(),
                    idempotency_key: format!("fixture-children-{batch}"),
                    created_at: at(9),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        // Historical runs remain members. Close optional members as each batch
        // is built, so the scale fixture respects the live descendant budget.
        for child in &decomposition.children {
            if child.child_requirement == ChildRequirement::Optional {
                store
                    .dispose_work(
                        &DisposeWorkRequest {
                            work_id: child.work_id,
                            expected_work_revision: child.revision,
                            disposition: WorkDisposition::Cancelled,
                            replacement_id: None,
                            reason: "Fixture historical run".into(),
                            actor: actor(),
                            idempotency_key: format!("fixture-close-{}", child.work_id.0),
                            disposed_at: at(9),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
        }
        created.extend(decomposition.children);
    }
    let children = created[..cancelled_count].to_vec();
    for child in &children {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(child.short_ref.clone()),
                    action: UpdateAction::Cancel {
                        reason: "Fixture disposal".into(),
                    },
                },
                at(10),
            )
            .unwrap();
    }
    // Add synthetic history before a waiver becomes the latest parent event:
    // replaying a waiver transition would duplicate its canonical witness.
    for index in 0..case.history {
        store
            .add_root_cost_fixture(work.work_id, &[], at(20 + i64::try_from(index).unwrap()))
            .unwrap();
    }
    // All six kinds exist before any measurement; target sequence has 20 other children.
    waive(&verbs, &parent, &children[0].short_ref, 11);
    for child in children.iter().skip(1).take(case.prior_waivers) {
        waive(&verbs, &parent, &child.short_ref, 12);
    }
    let work = store.get_work_item(work.work_id).unwrap();
    let description = store.root_cost_fixture_description(work.work_id);
    for kind in [
        "Run",
        "ChildSeal",
        "ChildWaiver",
        "Contributor",
        "Contribution",
        "Waiver",
    ] {
        assert!(
            description["members"][kind]["count"].as_u64().unwrap() > 0,
            "{description}"
        );
    }
    let integrity = store.verify_all().unwrap();
    assert!(
        integrity.is_healthy(),
        "invalid matrix fixture {case:?}: {integrity:?}"
    );
    Prepared {
        store,
        verbs,
        _directory: directory,
        parent,
        children,
        work,
    }
}

fn core(
    prepared: &mut Prepared,
    operation: &str,
    child_index: usize,
    second: i64,
    work: &crate::WorkItem,
    claim: crate::WorkClaim,
) {
    match operation {
        "note" => {
            prepared
                .store
                .record_work_note(
                    &RecordWorkNoteRequest {
                        status: false,
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: claim.holder,
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary: "Measured note".into(),
                        refs: Vec::new(),
                        actor: actor(),
                        idempotency_key: format!("note-{second}"),
                        recorded_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        "gate" => {
            prepared
                .store
                .record_gate_evidence(
                    &RecordGateEvidenceRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: claim.holder,
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        name: "Measured gate".into(),
                        failed: Vec::new(),
                        evidence_ref: None,
                        actor: actor(),
                        recorded_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        "decompose" => {
            prepared
                .store
                .decompose_work(
                    &DecomposeWorkRequest {
                        parent_id: work.work_id,
                        expected_parent_revision: work.revision,
                        children: vec![draft(100_000, true)],
                        prerequisites: Vec::new(),
                        authority: planning_authority(&claim),
                        actor: actor(),
                        idempotency_key: format!("plan-{second}"),
                        created_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        "waive" => {
            prepared
                .store
                .waive_required_child(
                    &WaiveRequiredChildRequest {
                        parent_id: work.work_id,
                        child_id: prepared.children[child_index].work_id,
                        expected_parent_revision: work.revision,
                        reason: "Cancelled fixture child accounted for".into(),
                        actor: actor(),
                        idempotency_key: format!("waive-{second}"),
                        waived_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        _ => unreachable!(),
    }
}

fn word(prepared: &Prepared, operation: &str, child_index: usize, second: i64) {
    match operation {
        "note" => {
            prepared
                .verbs
                .note(
                    &NoteInput {
                        status: false,
                        work_ref: Some(prepared.parent.clone()),
                        text: "Measured note".into(),
                        refs: Vec::new(),
                    },
                    at(second),
                )
                .unwrap();
        }
        "gate" => {
            prepared
                .verbs
                .gate(
                    GateInput {
                        work_ref: Some(prepared.parent.clone()),
                        name: "Measured gate".into(),
                        failed: Vec::new(),
                        evidence_ref: None,
                    },
                    at(second),
                )
                .unwrap();
        }
        "decompose" => {
            add(
                &prepared.verbs,
                "Child 100000",
                Some(&prepared.parent),
                false,
                second,
            );
        }
        "waive" => waive(
            &prepared.verbs,
            &prepared.parent,
            &prepared.children[child_index].short_ref,
            second,
        ),
        _ => unreachable!(),
    }
}

fn measure(case: Case, operation: &str, boundary: &str) -> serde_json::Value {
    // Each boundary/operation gets an independently built equivalent baseline.
    let mut prepared = prepare(case);
    let before = prepared
        .store
        .root_cost_fixture_description(prepared.work.work_id);
    let work = prepared.store.get_work_item(prepared.work.work_id).unwrap();
    let claim = prepared
        .store
        .current_work_claim(work.work_id)
        .unwrap()
        .unwrap();
    work_cost::start();
    if boundary == "word" {
        word(&prepared, operation, case.prior_waivers + 1, 500);
    } else {
        core(
            &mut prepared,
            operation,
            case.prior_waivers + 1,
            500,
            &work,
            claim,
        );
    }
    let cost = work_cost::finish(); // before post-state reads or audit
    let after = prepared
        .store
        .root_cost_fixture_description(prepared.work.work_id);
    assert!(prepared.store.verify_all().unwrap().is_healthy());
    let result = serde_json::json!({"shape":case.shape,"size":case.size,"history_added":case.history,
        "prior_waivers":case.prior_waivers,"evidence_summary_bytes":case.evidence_bytes,
        "extra_unresolved_child":case.unresolved,"operation":operation,"boundary":boundary,
        "before":before,"after":after,"cost":cost});
    eprintln!("work_cost_matrix {result}");
    result
}

fn matrix(sizes: &[usize]) {
    for &size in sizes {
        for shape in ["contributors", "runs", "mixed"] {
            for operation in ["note", "gate", "decompose", "waive"] {
                for boundary in ["core", "word"] {
                    measure(
                        Case {
                            shape,
                            size,
                            history: 0,
                            prior_waivers: 0,
                            evidence_bytes: 32,
                            unresolved: false,
                        },
                        operation,
                        boundary,
                    );
                }
            }
        }
    }
}

#[test]
fn work_operation_count_fixture_smoke() {
    for operation in ["note", "gate", "decompose", "waive"] {
        for boundary in ["core", "word"] {
            measure(
                Case {
                    shape: "mixed",
                    size: 10,
                    history: 0,
                    prior_waivers: 0,
                    evidence_bytes: 32,
                    unresolved: false,
                },
                operation,
                boundary,
            );
        }
    }
    let prepared = prepare(Case {
        shape: "runs",
        size: 100,
        history: 0,
        prior_waivers: 19,
        evidence_bytes: 1024,
        unresolved: true,
    });
    assert!(prepared.store.verify_all().unwrap().is_healthy());
}

#[test]
#[ignore = "writes a retained synthetic seed only when explicitly requested"]
fn write_waive_benchmark_seed() {
    let output = std::env::var_os("ENGRAM_BENCHMARK_DIRECTORY").expect("benchmark directory");
    let output = PathBuf::from(output);
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join(".git").is_dir())
        .expect("main repository");
    let permitted = repository.join("target/tmp");
    let output = output.canonicalize().unwrap();
    assert!(output.starts_with(permitted.canonicalize().unwrap()));
    let mut prepared = prepare_with_options(
        Case {
            shape: "contributors",
            size: 0,
            history: 0,
            prior_waivers: 0,
            evidence_bytes: 32,
            unresolved: false,
        },
        true,
    );
    let now = chrono::Utc::now();
    prepared
        .verbs
        .claim(
            ClaimInput {
                work_ref: prepared.parent.clone(),
                ttl_seconds: Some(10800),
                recover: Some("Recover the retained synthetic benchmark seed".into()),
            },
            now,
        )
        .unwrap();
    prepared
        .verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(prepared.parent.clone()),
                text: "Retained synthetic benchmark seed".into(),
                refs: Vec::new(),
            },
            now,
        )
        .unwrap();
    prepared
        .store
        .add_root_benchmark_fixture(prepared.work.work_id, 6733, 929_647, now)
        .unwrap();
    let description = prepared
        .store
        .root_cost_fixture_description(prepared.work.work_id);
    assert_eq!(description["state_bytes"], 929_647);
    assert_eq!(
        description["members"]
            .as_object()
            .unwrap()
            .values()
            .map(|value| value["count"].as_u64().unwrap())
            .sum::<u64>(),
        6733,
    );
    assert!(prepared.store.verify_all().unwrap().is_healthy());
    let home = output.join("seed-home");
    let project = ProjectId("customer-workflow".into());
    let database = crate::project_database_path(&home, &project);
    prepared.store.backup_to(&database).unwrap();
    std::fs::write(output.join(".engram-project"), "customer-workflow\n").unwrap();
    let manifest = serde_json::json!({
        "schema_version":1,
        "classification":"new synthetic benchmark, not historical reproduction",
        "created_at":now,
        "project_id":project,
        "actor_id":"agent", "session_id":"agent",
        "parent":prepared.parent,
        "warmup_child":prepared.children[1].short_ref,
        "sample_children":prepared.children.iter().skip(2).map(|child| &child.short_ref).collect::<Vec<_>>(),
        "seed_home":home, "database":database,
        "initial_root":description,
        "byte_basis":"RFC8785 complete RootExecution canonical bytes before warmup",
        "composition":"bounded contributor identities plus attributed synthetic omission reasons; all six kinds",
        "protocol":{"warmup_waivers":1,"samples":20,"transport":"full MCP JSON-RPC stdio roundtrip","retries":0,"outliers_removed":0,"p95":"nearest rank, ceil(0.95*n)","budget_ms":1000},
        "historical_p95_ms":1032.6427,
        "historical_protocol":"lost; kept separate, not reproduced",
    });
    std::fs::write(
        output.join("seed-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    eprintln!("waive_benchmark_seed {manifest}");
}

#[test]
fn work_operation_count_matrix() {
    matrix(&[10, 100]);
    for (history, prior_waivers, evidence_bytes, unresolved) in [
        (10, 0, 32, false),
        (100, 0, 32, false),
        (0, 19, 32, false),
        (0, 19, 32, true),
        (0, 0, 1024, false),
    ] {
        for operation in ["note", "gate", "decompose", "waive"] {
            for boundary in ["core", "word"] {
                measure(
                    Case {
                        shape: "mixed",
                        size: 10,
                        history,
                        prior_waivers,
                        evidence_bytes,
                        unresolved,
                    },
                    operation,
                    boundary,
                );
            }
        }
    }
}

#[test]
fn work_operation_count_history_fixture() {
    let _prepared = prepare(Case {
        shape: "mixed",
        size: 10,
        history: 10,
        prior_waivers: 0,
        evidence_bytes: 32,
        unresolved: false,
    });
}

#[test]
#[ignore = "count matrix at 1000 members, run explicitly during attribution"]
fn work_operation_count_matrix_scale() {
    matrix(&[1000]);
}

#[test]
fn work_operation_count_matrix_twenty_distinct_waivers() {
    for unresolved in [false, true] {
        for boundary in ["core", "word"] {
            let case = Case {
                shape: "mixed",
                size: 10,
                history: 0,
                prior_waivers: 0,
                evidence_bytes: 32,
                unresolved,
            };
            let mut prepared = prepare(case);
            for index in 1..=20 {
                let before = prepared
                    .store
                    .root_cost_fixture_description(prepared.work.work_id);
                let work = prepared.store.get_work_item(prepared.work.work_id).unwrap();
                let claim = prepared
                    .store
                    .current_work_claim(work.work_id)
                    .unwrap()
                    .unwrap();
                work_cost::start();
                if boundary == "word" {
                    word(
                        &prepared,
                        "waive",
                        index,
                        500 + i64::try_from(index).unwrap(),
                    );
                } else {
                    core(
                        &mut prepared,
                        "waive",
                        index,
                        500 + i64::try_from(index).unwrap(),
                        &work,
                        claim,
                    );
                }
                let cost = work_cost::finish();
                eprintln!(
                    "work_cost_matrix {}",
                    serde_json::json!({"operation":"waive_sequence","index":index,"boundary":boundary,"extra_unresolved_child":unresolved,"before":before,"cost":cost})
                );
            }
            assert!(prepared.store.verify_all().unwrap().is_healthy());
        }
    }
}
