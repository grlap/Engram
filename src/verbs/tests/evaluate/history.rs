//! `show --evaluations` and `show --evaluation RECORD_ID`: every record of a
//! run, in run-feed order, while ordinary `show` and `done` keep reading only
//! the newest record.

use super::*;
use crate::verbs::ShowInput;

/// An evaluated item with `criteria` criteria, claimed by `agent`, with one
/// gate to cite.
struct Fixture {
    database: std::path::PathBuf,
    project: ProjectId,
    verbs: AgentVerbs,
    work_ref: String,
    gate: Vec<String>,
    // Dropped last, after every handle on its files.
    _directory: crate::test_support::TempHome,
}

fn fixture(name: &str, criteria: usize) -> Fixture {
    fixture_titled(name, criteria, "Evaluated item")
}

fn fixture_titled(name: &str, criteria: usize, title: &str) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId(format!("evaluation-history-{name}"));
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 0);
    let added = verbs
        .add(
            AddInput {
                title: title.into(),
                acceptance: (1..=criteria)
                    .map(|position| format!("criterion {position} holds"))
                    .collect(),
                ..AddInput::default()
            },
            at(1),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: Some(36_000),
                recover: None,
            },
            at(2),
        )
        .expect("claim");
    verbs
        .gate(
            GateInput {
                work_ref: Some(work_ref.clone()),
                name: "cargo-test".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .expect("gate");
    let store = SqliteStore::open(&database).expect("store");
    let run_id = store
        .resolve_work_ref(&project, &work_ref)
        .expect("resolve")
        .active_run_id
        .expect("active run");
    let gate = store
        .work_run_evidence(run_id)
        .expect("evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect();
    Fixture {
        database,
        project,
        verbs,
        work_ref,
        gate,
        _directory: directory,
    }
}

impl Fixture {
    fn evidence_basis(&self) -> i64 {
        let store = SqliteStore::open(&self.database).expect("store");
        let run_id = store
            .resolve_work_ref(&self.project, &self.work_ref)
            .expect("resolve")
            .active_run_id
            .expect("active run");
        store
            .work_feed_head(&crate::domain::FeedId::RunExecution(run_id))
            .expect("run feed head")
    }

    /// Records one evaluation with these verdict words, one per criterion,
    /// and returns its record id. A correction note precedes it, since a
    /// blocking record stands until new evidence.
    fn evaluate(&self, acceptance_basis: i64, words: &[&str], second: i64) -> String {
        self.verbs
            .note(
                &NoteInput {
                    status: false,
                    work_ref: Some(self.work_ref.clone()),
                    text: format!("correction before the evaluation at {second}"),
                    refs: Vec::new(),
                },
                at(second),
            )
            .expect("correction note");
        let mut input = evaluate_input(
            &self.work_ref,
            self.evidence_basis(),
            words
                .iter()
                .enumerate()
                .map(|(index, word)| {
                    let mut verdict = verdict(index + 1, word, "judgment", &self.gate);
                    verdict.rationale = format!("{} at {second}", verdict.rationale);
                    verdict
                })
                .collect(),
        );
        input.acceptance_basis = acceptance_basis;
        let receipt = self.verbs.evaluate(input, at(second)).expect("evaluate");
        receipt.value["evaluation"]["hash"]
            .as_str()
            .expect("record id")
            .to_owned()
    }

    fn window(&self, after: Option<String>, second: i64) -> Result<Receipt, VerbError> {
        self.verbs.show_records(
            &self.work_ref,
            &ShowInput {
                evaluations: true,
                after,
                ..ShowInput::default()
            },
            at(second),
        )
    }

    fn detail(&self, record: &str, second: i64) -> Result<Receipt, VerbError> {
        self.verbs.show_records(
            &self.work_ref,
            &ShowInput {
                evaluation: Some(record.to_owned()),
                ..ShowInput::default()
            },
            at(second),
        )
    }
}

fn rows(receipt: &Receipt) -> Vec<Value> {
    receipt.value["evaluations"]
        .as_array()
        .expect("evaluation rows")
        .clone()
}

/// A failing record and then a passing one from the same evaluator session:
/// both are listed in run-feed order with the same session label, the one the
/// holder is shown with; the older one is not stale merely because a newer
/// record exists; ordinary `show` and `done` still read only the newest.
#[test]
fn a_failing_then_passing_record_from_one_session_reads_in_order() {
    let fixture = fixture("fail-then-pass", 2);
    let failed = fixture.evaluate(1, &["fail", "pass"], 4);
    let passed = fixture.evaluate(1, &["pass", "pass"], 5);
    assert_ne!(failed, passed);

    let window = fixture.window(None, 6).expect("window");
    let listed = rows(&window);
    assert_eq!(listed.len(), 2, "{}", window.text());
    assert_eq!(listed[0]["evaluation"], failed.as_str());
    assert_eq!(listed[1]["evaluation"], passed.as_str());
    assert!(
        listed[0]["run_position"].as_i64() < listed[1]["run_position"].as_i64(),
        "run-feed order"
    );
    assert_eq!(listed[0]["evaluator_session"], "you");
    assert_eq!(
        listed[0]["evaluator_session"],
        listed[1]["evaluator_session"]
    );
    assert_eq!(listed[0]["verdicts"][0]["verdict"], "fail");
    assert_eq!(listed[1]["verdicts"][0]["verdict"], "pass");
    assert_eq!(listed[0]["newest"], false);
    assert_eq!(listed[1]["newest"], true);
    assert!(listed[0]["stale"].is_null(), "{}", window.text());
    assert!(listed[1]["stale"].is_null(), "{}", window.text());
    let counts = &window.value["evaluations_window"];
    assert_eq!(counts["total"], 2);
    assert_eq!(counts["shown"], 2);
    assert_eq!(counts["omitted"], 0);
    assert!(counts["after"].is_null());

    // A peer reads the same session label for the evaluator and the holder.
    let reviewer = AgentVerbs::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "reviewer".into(),
        SessionId("reviewer".into()),
        None,
    );
    let peer_window = reviewer
        .show_records(
            &fixture.work_ref,
            &ShowInput {
                evaluations: true,
                ..ShowInput::default()
            },
            at(7),
        )
        .expect("peer window");
    let label = rows(&peer_window)[0]["evaluator_session"]
        .as_str()
        .expect("label")
        .to_owned();
    assert!(label.starts_with("peer-"), "{label}");
    assert_eq!(rows(&peer_window)[1]["evaluator_session"], label.as_str());
    let peer_show = reviewer.show(&fixture.work_ref, at(7)).expect("peer show");
    assert!(
        peer_show.text().contains(&format!("held by {label} until")),
        "{}",
        peer_show.text()
    );

    // Ordinary show keeps its newest-only record; done consumes it.
    let shown = fixture.verbs.show(&fixture.work_ref, at(8)).expect("show");
    assert!(shown.text().contains(&passed[..12]), "{}", shown.text());
    assert!(!shown.text().contains(&failed[..12]), "{}", shown.text());
    let done = fixture
        .verbs
        .done(
            DoneInput {
                work_ref: Some(fixture.work_ref.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(9),
        )
        .expect("done");
    assert!(!done.owed, "{}", done.text());
    // The completed item's latest run stays readable for auditing. Completion
    // revised the item, so its ended run's records are listed but not judged:
    // the record the seal consumed is never shown as stale.
    let after_done = fixture.window(None, 10).expect("window after done");
    let ended = rows(&after_done);
    assert_eq!(ended.len(), 2);
    assert_eq!(
        after_done.value["evaluations_window"]["stale_judged"],
        false
    );
    for row in &ended {
        assert!(row["stale"].is_null(), "{}", after_done.text());
        assert_eq!(row["stale_judged"], false);
    }
    assert_eq!(ended[1]["newest"], true);
    assert!(
        !after_done.text().contains("stale: "),
        "{}",
        after_done.text()
    );
    let sealed = fixture.detail(&passed, 11).expect("detail after done");
    assert_eq!(sealed.value["evaluation"]["stale_judged"], false);
    assert_eq!(sealed.value["evaluation"]["newest"], true);
    assert!(sealed.text().contains("not judged"), "{}", sealed.text());
}

/// A record whose criteria were revised afterwards is stale for that reason
/// of its own; the newer record judged at the new revision is fresh.
#[test]
fn an_older_record_carries_its_own_stale_reason() {
    let fixture = fixture("revised", 1);
    let passed = fixture.evaluate(1, &["pass"], 4);
    fixture
        .verbs
        .update(
            UpdateInput {
                work_ref: Some(fixture.work_ref.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: Some(vec!["criterion 1 holds, revised".into()]),
                    bindings: None,
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(5),
        )
        .expect("revise the criteria");
    let again = fixture.evaluate(2, &["pass"], 6);
    let window = fixture.window(None, 7).expect("window");
    let listed = rows(&window);
    assert_eq!(listed[0]["evaluation"], passed.as_str());
    assert_eq!(listed[0]["stale"], "revision", "{}", window.text());
    assert_eq!(listed[0]["newest"], false);
    assert_eq!(listed[1]["evaluation"], again.as_str());
    assert!(listed[1]["stale"].is_null(), "{}", window.text());
    assert!(
        window.text().contains("stale: revision"),
        "{}",
        window.text()
    );
}

/// More records than one page: exact counts, a continuation that reaches every
/// record once in order, and a cursor refused once another record changes the
/// window.
#[test]
fn the_window_pages_every_record_and_refuses_a_stale_cursor() {
    let fixture = fixture("paging", 1);
    let records = (0..20)
        .map(|index| {
            let word = if index % 2 == 0 { "fail" } else { "pass" };
            fixture.evaluate(1, &[word], 4 + index)
        })
        .collect::<Vec<_>>();
    let first = fixture.window(None, 30).expect("first page");
    let counts = &first.value["evaluations_window"];
    assert_eq!(counts["total"], 20);
    let shown = counts["shown"].as_u64().expect("shown");
    assert!(shown < 20, "{}", first.text());
    assert_eq!(counts["newer"], 0);
    assert_eq!(counts["older"], 20 - shown);
    assert_eq!(counts["omitted"], 20 - shown);
    let after = counts["after"].as_str().expect("continuation").to_owned();
    assert!(
        first.next[0].ends_with(&format!("--evaluations --after {after}")),
        "{:?}",
        first.next
    );
    let second = fixture
        .window(Some(after.clone()), 31)
        .expect("second page");
    let more = &second.value["evaluations_window"];
    assert_eq!(more["total"], 20);
    assert_eq!(more["newer"], shown);
    let mut seen = rows(&second)
        .into_iter()
        .chain(rows(&first))
        .map(|row| row["evaluation"].as_str().expect("id").to_owned())
        .collect::<Vec<_>>();
    if let Some(next) = more["after"].as_str() {
        let third = fixture
            .window(Some(next.to_owned()), 32)
            .expect("third page");
        seen = rows(&third)
            .into_iter()
            .map(|row| row["evaluation"].as_str().expect("id").to_owned())
            .chain(seen)
            .collect();
    }
    assert_eq!(seen, records, "every record once, in run-feed order");

    // A write elsewhere in the project moves neither the run's records nor
    // the item's revision: the cursor still reads its page.
    fixture
        .verbs
        .add(
            AddInput {
                title: "Unrelated item".into(),
                ..AddInput::default()
            },
            at(35),
        )
        .expect("an unrelated item");
    fixture
        .window(Some(after.clone()), 36)
        .expect("an unrelated write keeps the window");
    // A gate on the run is evidence the rows' freshness is judged from:
    // the run's head moves and the cursor is refused, though no evaluation
    // was added.
    fixture
        .verbs
        .gate(
            GateInput {
                work_ref: Some(fixture.work_ref.clone()),
                name: "cargo-clippy".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(37),
        )
        .expect("a later gate");
    assert!(matches!(
        fixture.window(Some(after.clone()), 38).unwrap_err().error,
        StoreError::WorkShowCursorInvalid { .. }
    ));
    fixture.evaluate(1, &["pass"], 40);
    let refused = fixture
        .window(Some(after), 41)
        .expect_err("a new record changes the window");
    assert!(
        matches!(refused.error, StoreError::WorkShowCursorInvalid { .. }),
        "{refused:?}"
    );
    // A cursor from another window kind is refused, not reinterpreted.
    let notes = fixture
        .verbs
        .show_records(
            &fixture.work_ref,
            &ShowInput {
                evaluations: true,
                after: Some("s1-00".into()),
                ..ShowInput::default()
            },
            at(42),
        )
        .expect_err("a notes cursor");
    assert!(
        matches!(notes.error, StoreError::WorkShowCursorInvalid { .. }),
        "{notes:?}"
    );
}

/// A row bounds its verdict words and counts the rest exactly; the complete
/// record gives every verdict with its criterion, rationale and citations.
#[test]
fn many_verdicts_are_bounded_in_the_row_and_complete_in_the_detail() {
    let bound = crate::work_service::MAX_ROW_VERDICTS;
    let criteria = bound + 4;
    let fixture = fixture("verdicts", criteria);
    let words = vec!["pass"; criteria];
    let record = fixture.evaluate(1, &words, 4);
    let window = fixture.window(None, 5).expect("window");
    let row = &rows(&window)[0];
    assert_eq!(row["verdicts"].as_array().expect("verdicts").len(), bound);
    assert_eq!(row["verdicts_total"], criteria);
    assert_eq!(row["verdicts_omitted"], criteria - bound);
    let detail_command = format!(
        "engram work show {} --evaluation {record}",
        fixture.work_ref
    );
    assert_eq!(row["detail"], detail_command.as_str());
    assert!(
        window
            .text()
            .contains(&format!("complete record: {detail_command}")),
        "{}",
        window.text()
    );
    // Suggested commands never carry a record id.
    assert!(
        window.next.iter().all(|next| !next.contains(&record)),
        "{:?}",
        window.next
    );

    let detail = fixture.detail(&record, 6).expect("detail");
    let verdicts = detail.value["evaluation"]["verdicts"]
        .as_array()
        .expect("verdicts")
        .clone();
    assert_eq!(verdicts.len(), criteria);
    assert_eq!(detail.value["evaluation"]["verdicts_omitted"], 0);
    // The stored acceptance list keeps the order typed, and verdicts follow
    // its positions.
    let contract = fixture
        .verbs
        .show_records(
            &fixture.work_ref,
            &ShowInput {
                full: true,
                ..ShowInput::default()
            },
            at(6),
        )
        .expect("full contract");
    let acceptance = contract.value["work"]["acceptance"]
        .as_array()
        .expect("acceptance")
        .clone();
    assert_eq!(acceptance.len(), criteria);
    for (index, verdict) in verdicts.iter().enumerate() {
        assert_eq!(verdict["position"], index + 1);
        assert_eq!(verdict["criterion"], acceptance[index]);
        assert!(
            verdict["rationale"]
                .as_str()
                .expect("rationale")
                .starts_with(&format!("criterion {}: pass", index + 1))
        );
        assert_eq!(verdict["citations"], json!(fixture.gate));
    }
    assert!(
        detail.text().contains("(complete record)"),
        "{}",
        detail.text()
    );
}

/// The detail route reads only this item's records; the modes stay exclusive.
#[test]
fn the_detail_refuses_another_items_record_and_the_modes_stay_exclusive() {
    let fixture = fixture("detail", 1);
    let record = fixture.evaluate(1, &["pass"], 4);
    let other = fixture
        .verbs
        .add(
            AddInput {
                title: "Another item".into(),
                ..AddInput::default()
            },
            at(5),
        )
        .expect("add another");
    let other_ref = other.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    let refused = fixture
        .verbs
        .show_records(
            &other_ref,
            &ShowInput {
                evaluation: Some(record.clone()),
                ..ShowInput::default()
            },
            at(6),
        )
        .expect_err("a record of another item");
    assert!(
        matches!(refused.error, StoreError::InvalidWork(_)),
        "{refused:?}"
    );
    let unknown = fixture
        .detail("not-a-record", 6)
        .expect_err("an invalid record id");
    assert!(
        matches!(unknown.error, StoreError::InvalidWork(_)),
        "{unknown:?}"
    );
    // An id of another kind, here the gate the verdicts cite, is no
    // evaluation: the same refusal, never a store fault naming its kind.
    let gate = fixture
        .detail(&fixture.gate[0], 6)
        .expect_err("a gate record id");
    assert!(matches!(gate.error, StoreError::InvalidWork(_)), "{gate:?}");
    for input in [
        ShowInput {
            evaluations: true,
            notes: true,
            ..ShowInput::default()
        },
        ShowInput {
            evaluations: true,
            evaluation: Some(record.clone()),
            ..ShowInput::default()
        },
        ShowInput {
            evaluation: Some(record.clone()),
            after: Some("e1-00".into()),
            ..ShowInput::default()
        },
        ShowInput {
            evaluations: true,
            full: true,
            ..ShowInput::default()
        },
    ] {
        let error = fixture
            .verbs
            .show_records(&fixture.work_ref, &input, at(7))
            .expect_err("exclusive modes");
        assert!(
            matches!(error.error, StoreError::InvalidWork(_)),
            "{error:?}"
        );
    }
    // An item with no evaluation lists none.
    let empty = fixture
        .verbs
        .show_records(
            &other_ref,
            &ShowInput {
                evaluations: true,
                ..ShowInput::default()
            },
            at(8),
        )
        .expect("empty window");
    assert_eq!(empty.value["evaluations_window"]["total"], 0);
    let observed = rows(&empty);
    assert!(observed.is_empty(), "{observed:?}");
}

/// A title longer than the agent budget is compacted in the window, with its
/// stored length and the complete read offered.
#[test]
fn a_title_longer_than_the_budget_keeps_the_window_bounded() {
    let title = "t".repeat(13_000);
    let fixture = fixture_titled("long-title", 1, &title);
    let record = fixture.evaluate(1, &["pass"], 4);
    let window = fixture.window(None, 5).expect("window");
    assert_eq!(rows(&window)[0]["evaluation"], record.as_str());
    assert_eq!(window.value["work"]["title_truncated"], true);
    assert_eq!(window.value["work"]["title_bytes"], 13_000);
    assert!(
        window.value["work"]["title"].as_str().expect("title").len() < 13_000,
        "the title is compacted"
    );
    assert!(
        window
            .next
            .contains(&format!("engram work show {} --full", fixture.work_ref)),
        "{:?}",
        window.next
    );
}

/// Stale reasons are judged under the acceptance policy, so a policy change
/// ends a window: its cursor is refused.
#[test]
fn a_policy_change_refuses_a_cursor() {
    let fixture = fixture("policy-change", 1);
    for index in 0..20 {
        fixture.evaluate(1, &["pass"], 4 + index);
    }
    let first = fixture.window(None, 30).expect("first page");
    let after = first.value["evaluations_window"]["after"]
        .as_str()
        .expect("continuation")
        .to_owned();
    SqliteStore::open(&fixture.database)
        .expect("store")
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![
                    AcceptanceEvaluationMode::SameSession,
                    AcceptanceEvaluationMode::IndependentSession,
                ],
                mechanical_basis: MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &ActorContext {
                actor_id: "policy-admin".into(),
                actor_kind: "host_operator".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: None,
                source_tool: Some("verbs_test".into()),
                source_skill: None,
                provenance_chain: Vec::<ProvenanceLink>::new(),
                reason: "widen the evaluator modes mid-window".into(),
            },
            "widen-evaluator-modes",
            None,
            at(31),
            &DevelopmentNoopRedactor,
        )
        .expect("change the policy");
    let refused = fixture
        .window(Some(after), 32)
        .expect_err("the policy changed");
    assert!(
        matches!(&refused.error, StoreError::WorkShowCursorInvalid { reason } if reason.contains("policy")),
        "{refused:?}"
    );
}

/// A new run ends the old window: its cursor is refused. A record of the
/// earlier run is still readable in full, stale for that reason.
#[test]
fn a_new_run_refuses_the_old_cursor_and_keeps_the_earlier_record_readable() {
    let fixture = fixture("new-run", 1);
    let earlier = (0..20)
        .map(|index| fixture.evaluate(1, &["pass"], 4 + index))
        .collect::<Vec<_>>();
    let first = fixture.window(None, 30).expect("first page");
    let after = first.value["evaluations_window"]["after"]
        .as_str()
        .expect("continuation")
        .to_owned();
    let run_of = || {
        SqliteStore::open(&fixture.database)
            .expect("store")
            .resolve_work_ref(&fixture.project, &fixture.work_ref)
            .expect("resolve")
            .active_run_id
    };
    let old_run = run_of().expect("active run");
    let done = fixture
        .verbs
        .done(
            DoneInput {
                work_ref: Some(fixture.work_ref.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(31),
        )
        .expect("done");
    assert!(!done.owed, "{}", done.text());
    let service = crate::work_service::LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    service
        .work_focus(&fixture.work_ref, at(32))
        .expect("focus");
    service
        .work_update(
            crate::work_service::WorkUpdateInput::Reopen {
                reason: "a new run".into(),
                idempotency_key: "reopen-for-a-new-run".into(),
            },
            at(33),
        )
        .expect("reopen");
    fixture
        .verbs
        .claim(
            ClaimInput {
                work_ref: fixture.work_ref.clone(),
                ttl_seconds: Some(36_000),
                recover: None,
            },
            at(34),
        )
        .expect("claim the reopened item");
    let new_run = run_of().expect("active run");
    assert_ne!(new_run, old_run, "the reopened item runs anew");
    let refused = fixture
        .window(Some(after), 35)
        .expect_err("the run changed");
    assert!(
        matches!(&refused.error, StoreError::WorkShowCursorInvalid { reason } if reason.contains("run changed")),
        "{refused:?}"
    );
    let fresh = fixture.window(None, 36).expect("new run's window");
    assert_eq!(fresh.value["evaluations_window"]["total"], 0);
    let detail = fixture.detail(&earlier[0], 37).expect("earlier record");
    assert_eq!(detail.value["evaluation"]["stale"], "run");
    assert_eq!(detail.value["evaluation"]["stale_judged"], true);
    assert_eq!(detail.value["evaluation"]["newest"], false);
}

/// Every show form that reads a run's records, continuations and details
/// included, succeeds for another session while the store's database and WAL
/// files cannot be written, and changes neither.
#[test]
fn every_record_window_and_detail_reads_where_the_store_files_cannot_be_written() {
    use crate::verbs::tests::customer_workflow::read_only_reads::UnwritableStoreFiles;
    let fixture = fixture("read-only", 1);
    // More records than one evaluations window, and notes too long for one
    // notes or history window, so that every window has a continuation.
    let records = (0..17)
        .map(|index| {
            let word = if index % 2 == 0 { "fail" } else { "pass" };
            fixture.evaluate(1, &[word], 4 + index)
        })
        .collect::<Vec<_>>();
    for index in 0..16 {
        fixture
            .verbs
            .note(
                &NoteInput {
                    status: false,
                    work_ref: Some(fixture.work_ref.clone()),
                    text: format!("long note {index}: {}", "x".repeat(900)),
                    refs: Vec::new(),
                },
                at(40 + index),
            )
            .expect("long note");
    }
    let reader = AgentVerbs::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let wal = std::path::PathBuf::from(format!("{}-wal", fixture.database.display()));
    assert!(wal.exists(), "a live store has its WAL file");
    let _unwritable = UnwritableStoreFiles::deny(&fixture.database);
    let database_before = std::fs::read(&fixture.database).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    let show = |form: &str, input: ShowInput| {
        let receipt = reader
            .show_records(&fixture.work_ref, &input, at(100))
            .unwrap_or_else(|error| panic!("{form}: {error}"));
        assert_eq!(
            std::fs::read(&fixture.database).unwrap(),
            database_before,
            "{form}"
        );
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before, "{form}");
        receipt
    };
    let continuation = |receipt: &Receipt, pointer: &str| {
        receipt
            .value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("a continuation at {pointer}: {}", receipt.text()))
            .to_owned()
    };

    let evaluations = show(
        "--evaluations",
        ShowInput {
            evaluations: true,
            ..ShowInput::default()
        },
    );
    let after = continuation(&evaluations, "/evaluations_window/after");
    show(
        "--evaluations --after",
        ShowInput {
            evaluations: true,
            after: Some(after),
            ..ShowInput::default()
        },
    );
    for record in [&records[0], &records[16]] {
        let detail = show(
            "--evaluation",
            ShowInput {
                evaluation: Some(record.clone()),
                ..ShowInput::default()
            },
        );
        assert!(detail.text().contains(record.as_str()), "{}", detail.text());
    }

    for gates in [false, true] {
        let notes = show(
            "--notes",
            ShowInput {
                notes: true,
                gates,
                ..ShowInput::default()
            },
        );
        let after = continuation(&notes, "/notes_window/after");
        show(
            "--notes --after",
            ShowInput {
                notes: true,
                gates,
                after: Some(after),
                ..ShowInput::default()
            },
        );
        let locator = notes.value["notes"][0]["locator"]
            .as_str()
            .expect("note locator")
            .to_owned();
        show(
            "--note",
            ShowInput {
                note: Some(locator),
                ..ShowInput::default()
            },
        );
    }

    let history = show(
        "--history",
        ShowInput {
            history: true,
            ..ShowInput::default()
        },
    );
    let after = continuation(&history, "/history/window/after");
    show(
        "--history --after",
        ShowInput {
            history: true,
            after: Some(after),
            ..ShowInput::default()
        },
    );
    show(
        "--full",
        ShowInput {
            full: true,
            ..ShowInput::default()
        },
    );
    show("show", ShowInput::default());
}
