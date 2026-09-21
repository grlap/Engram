//! Word-level regression cases from the first review pair.

use super::*;
use crate::domain::FeedId;
use crate::{NextInput, NoteInput};

struct Item {
    work_ref: String,
    work_id: crate::domain::WorkId,
    run_id: crate::domain::WorkRunId,
}

fn prepare(
    verbs: &AgentVerbs,
    database: &std::path::Path,
    project: &ProjectId,
    title: &str,
    second: i64,
) -> Item {
    let added = verbs
        .add(
            AddInput {
                title: title.into(),
                acceptance: vec!["the change is verified".into()],
                ..AddInput::default()
            },
            at(second),
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
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(second + 1),
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
            at(second + 2),
        )
        .expect("gate");
    let work = SqliteStore::open(database)
        .expect("store")
        .resolve_work_ref(project, &work_ref)
        .expect("resolve work");
    Item {
        work_ref,
        work_id: work.work_id,
        run_id: work.active_run_id.expect("active run"),
    }
}

fn basis(database: &std::path::Path, item: &Item) -> i64 {
    SqliteStore::open(database)
        .expect("store")
        .work_feed_head(&FeedId::RunExecution(item.run_id))
        .expect("run feed head")
}

fn gate_hashes(database: &std::path::Path, item: &Item) -> Vec<String> {
    SqliteStore::open(database)
        .expect("store")
        .work_run_evidence(item.run_id)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect()
}

fn done_input(work_ref: &str) -> DoneInput {
    DoneInput {
        work_ref: Some(work_ref.into()),
        summary: Some("finished".into()),
        ..DoneInput::default()
    }
}

fn family(row: &serde_json::Value) -> String {
    row["family"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

// Review 2 (Medium): the locators that `show --notes --gates` prints are the
// citations `evaluate` accepts; the record keeps full hashes, and
// observations or another run's records stay refused.
#[test]
fn evaluate_accepts_the_locators_show_prints_and_refuses_observations() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-locators".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = prepare(&verbs, &database, &project, "Cited item", 0);
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(item.work_ref.clone()),
                text: "implementation finding: the boundary is covered".into(),
                refs: Vec::new(),
            },
            at(3),
        )
        .expect("holder note");
    let peer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    peer.note(
        &NoteInput {
            status: false,
            work_ref: Some(item.work_ref.clone()),
            text: "observation from a peer that does not hold the run".into(),
            refs: Vec::new(),
        },
        at(4),
    )
    .expect("peer observation");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 5);
    let other = prepare(&verbs, &database, &project, "Other item", 6);
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(other.work_ref.clone()),
                text: "a note that belongs to the other run".into(),
                refs: Vec::new(),
            },
            at(9),
        )
        .expect("other note");

    let window = verbs
        .show_records(
            &item.work_ref,
            &ShowInput {
                notes: true,
                gates: true,
                ..ShowInput::default()
            },
            at(10),
        )
        .expect("show --notes --gates");
    let rows = window.value["notes"].as_array().expect("rows").clone();
    let locator = |predicate: &dyn Fn(&serde_json::Value) -> bool| {
        rows.iter()
            .find(|row| predicate(row))
            .and_then(|row| row["locator"].as_str())
            .expect("locator")
            .to_owned()
    };
    let gate_locator = locator(&|row| family(row) == "gates");
    let note_locator = locator(&|row| family(row) == "notes" && row["non_holder"] != true);
    let observation_locator = locator(&|row| row["non_holder"] == true);
    let other_window = verbs
        .show_records(
            &other.work_ref,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(10),
        )
        .expect("show the other item's notes");
    let foreign_locator = other_window.value["notes"][0]["locator"]
        .as_str()
        .expect("foreign locator")
        .to_owned();

    // A judgment pass may rest on a gate and a note together; the record
    // keeps the full evidence ides those locators name.
    let recorded = verbs
        .evaluate(
            evaluate_input(
                &item.work_ref,
                basis(&database, &item),
                vec![verdict(
                    1,
                    "pass",
                    "judgment",
                    &[gate_locator.clone(), note_locator.clone()],
                )],
            ),
            at(11),
        )
        .expect("printed locators are accepted citations");
    assert_eq!(recorded.value["evaluation"]["passed"], 1);
    let record = SqliteStore::open(&database)
        .expect("store")
        .acceptance_evaluation_status(item.work_id, None)
        .expect("status")
        .expect("evaluation")
        .record;
    let cited = record.verdicts[0]
        .evidence
        .iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(cited.len(), 2, "{cited:?}");
    assert!(
        cited.contains(&gate_hashes(&database, &item)[0]),
        "{cited:?}"
    );
    assert!(cited.iter().all(|id| id.len() == 32), "{cited:?}");

    let head_before_refusals = basis(&database, &item);
    for (label, rejected, reason_word) in [
        ("observation", observation_locator, "observation"),
        (
            "another run",
            foreign_locator,
            "not a note/gate on this item",
        ),
        (
            "not evidence at all",
            "artifact://build/log.txt".to_owned(),
            "not the recorded evidence identity",
        ),
    ] {
        let refused = verbs
            .evaluate(
                evaluate_input(
                    &item.work_ref,
                    basis(&database, &item),
                    vec![verdict(
                        1,
                        "pass",
                        "judgment",
                        std::slice::from_ref(&rejected),
                    )],
                ),
                at(12),
            )
            .expect_err(label);
        assert!(
            matches!(
                refused.error,
                StoreError::AcceptanceEvaluationRefused { .. }
            ),
            "{label}: {refused:?}"
        );
        let message = refused.to_string();
        assert!(
            message.contains(&rejected) && message.contains(reason_word),
            "{label}: the refusal names the rejected locator and why: {message}"
        );
    }
    // A refused citation leaves the run feed untouched.
    assert_eq!(basis(&database, &item), head_before_refusals);
}

// Review 5 (Medium): completion provenance distinguishes an evaluated seal
// from legacy self-assertion in `done`, completed `show`, and bounded `next`.
#[test]
fn done_show_and_next_disclose_completion_provenance() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-provenance".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let peer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let legacy = prepare(&verbs, &database, &project, "Legacy item", 0);
    let completed = verbs
        .done(done_input(&legacy.work_ref), at(3))
        .expect("legacy done");
    assert!(!completed.owed, "{}", completed.text());
    assert!(
        completed
            .text()
            .contains("acceptance: self-asserted (legacy)"),
        "{}",
        completed.text()
    );
    assert_eq!(completed.value["acceptance"]["provenance"], "self_asserted");
    let shown = verbs.show(&legacy.work_ref, at(4)).expect("show legacy");
    assert!(
        shown.text().contains("acceptance: self-asserted (legacy)"),
        "{}",
        shown.text()
    );
    // The documented exception: a legacy completed show keeps its exact JSON
    // shape; only the text line names the self-assertion.
    assert!(shown.value.get("acceptance").is_none(), "{}", shown.value);

    enable(
        &database,
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::IndependentSession,
        ],
        5,
    );
    let item = prepare(&verbs, &database, &project, "Evaluated item", 6);
    let hashes = gate_hashes(&database, &item);
    let passing = verbs
        .evaluate(
            evaluate_input(
                &item.work_ref,
                basis(&database, &item),
                vec![verdict(1, "pass", "asserted", &hashes)],
            ),
            at(9),
        )
        .expect("passing evaluation");
    let evaluation = passing.value["evaluation"]["hash"].clone();
    let oriented = verbs
        .next(
            &NextInput {
                limit: None,
                peek: true,
                verbose: false,
                context_generation: None,
            },
            at(10),
        )
        .expect("next --peek");
    assert!(
        oriented
            .text()
            .contains("evaluation: same_session 1/1 pass"),
        "{}",
        oriented.text()
    );
    let completed = verbs
        .done(done_input(&item.work_ref), at(11))
        .expect("evaluated done");
    assert!(!completed.owed, "{}", completed.text());
    assert!(
        completed
            .text()
            .contains("acceptance: evaluated (same_session, asserted) by "),
        "{}",
        completed.text()
    );
    assert!(
        !completed.text().contains("self-asserted"),
        "{}",
        completed.text()
    );
    assert_eq!(completed.value["acceptance"]["provenance"], "evaluated");
    assert_eq!(completed.value["acceptance"]["mode"], "same_session");
    assert_eq!(completed.value["acceptance"]["evaluation"], evaluation);
    let shown = verbs.show(&item.work_ref, at(12)).expect("show completed");
    assert!(
        shown
            .text()
            .contains("acceptance: evaluated (same_session, asserted) by "),
        "{}",
        shown.text()
    );
    assert_eq!(shown.value["acceptance"]["evaluation"], evaluation);

    // independent_session: a peer that never held the run evaluates; the
    // holder completes; done and show attribute the evaluation to the peer.
    let independent = prepare(&verbs, &database, &project, "Independently evaluated", 14);
    let hashes = gate_hashes(&database, &independent);
    let passing = peer
        .evaluate(
            EvaluateInput {
                mode: "independent_session".into(),
                ..evaluate_input(
                    &independent.work_ref,
                    basis(&database, &independent),
                    vec![verdict(1, "pass", "asserted", &hashes)],
                )
            },
            at(17),
        )
        .expect("the peer evaluates independently");
    let completed = verbs
        .done(done_input(&independent.work_ref), at(18))
        .expect("the holder completes on the peer's pass");
    assert!(!completed.owed, "{}", completed.text());
    assert!(
        completed
            .text()
            .contains("acceptance: evaluated (independent_session, asserted) by "),
        "{}",
        completed.text()
    );
    assert_eq!(completed.value["acceptance"]["mode"], "independent_session");
    assert_eq!(
        completed.value["acceptance"]["evaluation"],
        passing.value["evaluation"]["hash"]
    );
    // The label is the holder's display identity for the peer: opaque, never
    // the peer's session id.
    let evaluator = completed.value["acceptance"]["evaluator"]
        .as_str()
        .expect("evaluator label");
    assert!(
        evaluator.starts_with("peer-") && !evaluator.contains("agent"),
        "{evaluator}"
    );
    let shown = verbs
        .show(&independent.work_ref, at(19))
        .expect("show the independently evaluated item");
    assert!(
        shown
            .text()
            .contains("acceptance: evaluated (independent_session, asserted) by "),
        "{}",
        shown.text()
    );
    assert_eq!(shown.value["acceptance"]["mode"], "independent_session");
    assert_eq!(
        shown.value["acceptance"]["evaluation"],
        passing.value["evaluation"]["hash"]
    );
}

// Round 4 (Medium): the word recovers an exact resend after a revision change
// and after done, for explicit and keyless attempts.
#[test]
fn the_word_replays_exact_resends_after_a_revision_and_after_done() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-retries".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = prepare(&verbs, &database, &project, "Retried item", 0);
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let hashes = gate_hashes(&database, &item);
    // A replay or a refusal changes neither the run feed head nor the
    // newest evaluation; the snapshot is taken before each group.
    let snapshot = || {
        (
            basis_of(&database, &item),
            SqliteStore::open(&database)
                .expect("store")
                .acceptance_evaluation_status(item.work_id, None)
                .expect("status")
                .map(|status| status.evaluation),
        )
    };
    let submission = |basis: i64, attempt: Option<&str>| EvaluateInput {
        acceptance_basis: basis,
        attempt: attempt.map(str::to_owned),
        ..evaluate_input(
            &item.work_ref,
            basis_of(&database, &item),
            vec![verdict(1, "pass", "asserted", &hashes)],
        )
    };
    // A different payload on a given basis: the judgment basis instead of
    // the asserted one.
    let other_payload = |basis: i64| EvaluateInput {
        acceptance_basis: basis,
        ..evaluate_input(
            &item.work_ref,
            basis_of(&database, &item),
            vec![verdict(1, "pass", "judgment", &hashes)],
        )
    };
    let explicit = submission(1, Some("attempt-1"));
    let keyless = submission(1, None);
    let explicit_first = verbs
        .evaluate(explicit.clone(), at(4))
        .expect("explicit record");
    let keyless_first = verbs
        .evaluate(keyless.clone(), at(5))
        .expect("keyless record");
    verbs
        .update(
            UpdateInput {
                work_ref: Some(item.work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: Some("same_session".into()),
                },
            },
            at(6),
        )
        .expect("pin the mode, which revises the item");
    let before_pin_replays = snapshot();
    for (label, resend, first) in [
        ("explicit", &explicit, &explicit_first),
        ("keyless", &keyless, &keyless_first),
    ] {
        let replayed = verbs
            .evaluate(resend.clone(), at(7))
            .unwrap_or_else(|error| {
                panic!("{label} resend after the revision must replay: {error}")
            });
        assert_eq!(replayed.value["evaluation"]["replayed"], true, "{label}");
        assert_eq!(
            replayed.value["evaluation"]["hash"], first.value["evaluation"]["hash"],
            "{label}"
        );
    }
    assert_eq!(
        snapshot(),
        before_pin_replays,
        "word replays after the revision have no effect"
    );
    // Fresh submission on the old basis with a different payload: refused
    // through the word, nothing recorded.
    let stale = verbs
        .evaluate(other_payload(1), at(8))
        .expect_err("a fresh submission on the old basis refuses");
    assert!(
        matches!(stale.error, StoreError::WorkRevisionConflict { .. }),
        "{stale:?}"
    );
    assert_eq!(
        snapshot(),
        before_pin_replays,
        "a refused fresh submission through the word has no effect"
    );
    let shown = verbs.show(&item.work_ref, at(8)).expect("show");
    let current = shown.value["acceptance_basis"].as_i64().expect("basis");
    let fresh = EvaluateInput {
        acceptance_basis: current,
        ..evaluate_input(
            &item.work_ref,
            basis_of(&database, &item),
            vec![verdict(1, "pass", "asserted", &hashes)],
        )
    };
    let fresh_first = verbs
        .evaluate(fresh.clone(), at(9))
        .expect("fresh record on the current revision");
    let completed = verbs
        .done(done_input(&item.work_ref), at(10))
        .expect("done on the fresh pass");
    assert!(!completed.owed, "{}", completed.text());
    let before_done_replays = snapshot();
    for (label, resend, first) in [
        ("fresh", &fresh, &fresh_first),
        ("explicit", &explicit, &explicit_first),
    ] {
        let replayed = verbs
            .evaluate(resend.clone(), at(11))
            .unwrap_or_else(|error| panic!("{label} resend after done must replay: {error}"));
        assert_eq!(replayed.value["evaluation"]["replayed"], true, "{label}");
        assert_eq!(
            replayed.value["evaluation"]["hash"], first.value["evaluation"]["hash"],
            "{label}"
        );
        assert!(
            replayed.text().contains("(replayed)"),
            "{}",
            replayed.text()
        );
    }
    assert_eq!(
        snapshot(),
        before_done_replays,
        "word replays after done have no effect"
    );
    // A new payload on the completed item, at its current revision: refused
    // through the word, nothing recorded.
    let completed_revision = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(item.work_id)
        .expect("item")
        .revision;
    let refused = verbs
        .evaluate(other_payload(completed_revision), at(12))
        .expect_err("a new attempt on a completed item refuses");
    assert!(
        matches!(&refused.error, StoreError::AcceptanceEvaluationRefused { reason, .. } if reason.contains("no active run")),
        "{refused:?}"
    );
    assert_eq!(
        snapshot(),
        before_done_replays,
        "a refused new attempt on a completed item has no effect"
    );
}

fn basis_of(database: &std::path::Path, item: &Item) -> i64 {
    basis(database, item)
}

// Round 6 (Low): a supplied blank mode is refused before effects; the
// explicit clear (mode omitted) and a valid set keep working.
#[test]
fn a_blank_evaluation_mode_is_refused_before_effects() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-blank-mode".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = prepare(&verbs, &database, &project, "Pinned item", 0);
    let pin = |mode: Option<&str>, second: i64| {
        verbs.update(
            UpdateInput {
                work_ref: Some(item.work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: mode.map(str::to_owned),
                },
            },
            at(second),
        )
    };
    pin(Some("independent_session"), 3).expect("pin the mode");
    let state = || {
        let store = SqliteStore::open(&database).expect("store");
        let work = store.get_work_item(item.work_id).expect("item");
        (work.evaluation_mode, work.revision, basis(&database, &item))
    };
    let pinned = state();
    assert_eq!(pinned.0, Some(AcceptanceEvaluationMode::IndependentSession));
    for blank in ["", "   ", "\t"] {
        let refused = pin(Some(blank), 4).expect_err("a blank mode is refused");
        assert!(
            matches!(&refused.error, StoreError::InvalidWork(reason) if reason.contains("blank")),
            "{blank:?}: {refused:?}"
        );
        assert_eq!(state(), pinned, "{blank:?} must leave the pin untouched");
    }
    let shown = verbs.show(&item.work_ref, at(5)).expect("show");
    assert!(
        shown
            .text()
            .contains("evaluation mode: independent_session"),
        "{}",
        shown.text()
    );
    pin(Some("same_session"), 6).expect("a valid mode sets");
    assert_eq!(state().0, Some(AcceptanceEvaluationMode::SameSession));
    pin(None, 7).expect("the explicit clear clears");
    assert_eq!(state().0, None);
}

// Round 8 (Medium): a pin or clear of the evaluation mode is a planning
// revision that history and peers must disclose by name; clearing an item
// that carries no pin is the genuine no-op control.
#[test]
fn evaluation_mode_revisions_are_disclosed_in_history_and_peer_next() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-mode-revision-disclosure".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = prepare(&verbs, &database, &project, "Disclosed item", 0);
    let pin = |mode: Option<&str>, second: i64| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(item.work_ref.clone()),
                    action: UpdateAction::EvaluationMode {
                        mode: mode.map(str::to_owned),
                    },
                },
                at(second),
            )
            .expect("evaluation mode update")
    };
    pin(Some("same_session"), 3);
    pin(None, 4);
    // The genuine no-op: clearing an item that carries no pin.
    pin(None, 5);
    let shown = verbs.show(&item.work_ref, at(6)).expect("show");
    let revised = shown.value["history"]["items"]
        .as_array()
        .expect("history")
        .iter()
        .filter(|row| row["kind"] == "revised")
        .map(|row| row["summary"].as_str().expect("summary").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        revised
            .iter()
            .filter(|summary| summary.starts_with("evaluation mode:"))
            .count(),
        2,
        "{revised:?}"
    );
    assert_eq!(
        revised
            .iter()
            .filter(|summary| summary.starts_with("no planning change"))
            .count(),
        1,
        "{revised:?}"
    );
    let peer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer-session".into()),
        None,
    );
    let delivered = peer
        .next(
            &crate::NextInput {
                limit: Some(50),
                peek: true,
                verbose: false,
                context_generation: None,
            },
            at(7),
        )
        .expect("peer next");
    let changes = delivered.value["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|change| change.as_str().expect("change line").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        changes
            .iter()
            .filter(|line| line.contains("revised by") && line.contains("evaluation mode:"))
            .count(),
        2,
        "{changes:#?}"
    );
    assert!(
        changes
            .iter()
            .any(|line| line.contains("revised by") && line.contains("no planning change")),
        "{changes:#?}"
    );
}

// Round 8 (Low, both reviewers): creation shares update's mode parsing, so a
// supplied blank refuses roots and children before any effect, while genuine
// omission and a valid selection create as before.
#[test]
fn a_blank_evaluation_mode_is_refused_at_creation() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-blank-mode-creation".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let parent = prepare(&verbs, &database, &project, "Parent item", 0);
    let add = |title: &str, under: Option<&str>, mode: Option<&str>, second: i64| {
        verbs.add(
            AddInput {
                title: title.into(),
                under: under.map(str::to_owned),
                evaluation_mode: mode.map(str::to_owned),
                ..AddInput::default()
            },
            at(second),
        )
    };
    let state = || {
        let store = SqliteStore::open(&database).expect("store");
        let focus = verbs
            .next(
                &crate::NextInput {
                    limit: None,
                    peek: true,
                    verbose: false,
                    context_generation: None,
                },
                at(2),
            )
            .expect("peek")
            .value["focus"]
            .clone();
        (
            store
                .work_feed_head(&FeedId::Project(project.clone()))
                .expect("project feed head"),
            store
                .work_event_count(parent.work_id)
                .expect("parent event count"),
            focus,
        )
    };
    let before = state();
    for (label, under) in [("root", None), ("child", Some(parent.work_ref.as_str()))] {
        for blank in ["", "   ", "\t"] {
            let refused = add("Never created", under, Some(blank), 3)
                .expect_err("a blank mode is refused at creation");
            assert!(
                matches!(&refused.error, StoreError::InvalidWork(reason) if reason.contains("blank")),
                "{label} {blank:?}: {refused:?}"
            );
            assert_eq!(state(), before, "{label} {blank:?} must leave no effect");
        }
    }
    // Genuine omission creates without a pin; a valid word pins.
    let omitted_root = add("Unpinned root", None, None, 4).expect("omitted mode creates a root");
    let omitted_ref = omitted_root.value["work"]["short_ref"]
        .as_str()
        .expect("root ref")
        .to_owned();
    let pinned_child = add("Pinned child", Some(&parent.work_ref), Some("sub_agent"), 5)
        .expect("a valid mode creates a pinned child");
    let pinned_ref = pinned_child.value["work"]["short_ref"]
        .as_str()
        .expect("child ref")
        .to_owned();
    let store = SqliteStore::open(&database).expect("store");
    assert_eq!(
        store
            .resolve_work_ref(&project, &omitted_ref)
            .expect("unpinned root")
            .evaluation_mode,
        None
    );
    assert_eq!(
        store
            .resolve_work_ref(&project, &pinned_ref)
            .expect("pinned child")
            .evaluation_mode,
        Some(AcceptanceEvaluationMode::SubAgent)
    );
}

// The word's last resort keeps the verdict-independent provenance and is
// bounded by construction: every variable part is an identifier, a number,
// or a bounded session handle.
#[test]
fn the_minimal_evaluate_receipt_is_bounded() {
    let projection = crate::work_service::WorkEvaluationProjection {
        mode: AcceptanceEvaluationMode::IndependentSession,
        work_revision: i64::MAX,
        run_id: crate::domain::WorkRunId::new(),
        evaluated_cut: i64::MAX,
        verdicts_total: usize::MAX,
        verdicts_omitted: usize::MAX,
        verdicts: Vec::new(),
        passed: usize::MAX,
        blocking: None,
        source_fingerprint: None,
        attempt_key: String::new(),
        full_detail: String::new(),
    };
    let receipt = crate::verbs::handlers::minimal_evaluate_receipt(
        "w-000000000000",
        i64::MAX,
        &projection,
        &crate::ObjectId::from_canonical_bytes(b"minimal"),
        true,
    )
    .with_effective_session_id(&SessionId("\u{0001}".repeat(crate::MAX_SESSION_ID_BYTES)));
    let json_bytes =
        crate::verbs::receipts::compact_receipt_json_bytes(&receipt.value).expect("bytes");
    let text_bytes = format!("{}\n", receipt.text()).len();
    assert!(
        json_bytes < 1024 && text_bytes < 1024,
        "{json_bytes} / {text_bytes}"
    );
    assert!(
        crate::verbs::receipts::agent_receipt_fits(&receipt, MAX_AGENT_WORK_RESPONSE_BYTES)
            .expect("fit")
    );
    assert_eq!(receipt.value["evaluation"]["replayed"], true);
    assert_eq!(receipt.value["evaluation"]["mode"], "independent_session");
}

// Round 4 (Low): the reserve is derived from admitted limits, pinned by
// construction: a real receipt's skeleton with every word-only variable
// field set to its maximum at worst-case escaping and the shared projection
// removed must fit the reserve; the real receipt must obey each assumed
// bound.
#[test]
fn the_reserve_covers_the_derived_word_envelope() {
    use crate::work_service::{EVALUATE_WORD_RESERVE, MAX_SUMMARY_BYTES};
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-reserve-derivation".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    )
    .with_fitted_effective_session(SessionId("p".repeat(crate::MAX_SESSION_ID_BYTES)));
    // A stored title well past the summary bound proves the summary title
    // is compacted before it reaches the envelope.
    let added = verbs
        .add(
            AddInput {
                title: "\"".repeat(MAX_SUMMARY_BYTES * 2),
                acceptance: vec!["the change is verified".into()],
                ..AddInput::default()
            },
            at(0),
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
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(1),
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
            at(2),
        )
        .expect("gate");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let work = SqliteStore::open(&database)
        .expect("store")
        .resolve_work_ref(&project, &work_ref)
        .expect("resolve");
    let item = Item {
        work_ref: work_ref.clone(),
        work_id: work.work_id,
        run_id: work.active_run_id.expect("active run"),
    };
    let hashes = gate_hashes(&database, &item);
    let receipt = verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                basis(&database, &item),
                vec![verdict(1, "pass", "asserted", &hashes)],
            ),
            at(4),
        )
        .expect("evaluate");
    // The real receipt obeys every bound the derivation assumes.
    let title = receipt.value["work"]["title"].as_str().expect("title");
    assert!(title.len() <= MAX_SUMMARY_BYTES, "{}", title.len());
    let holder = receipt.value["claim"]["holder"].as_str().expect("holder");
    assert!(holder.len() <= crate::MAX_SESSION_ID_BYTES, "{holder}");
    assert!(
        receipt.value["omissions"]
            .as_array()
            .is_none_or(|omissions| omissions.len() <= 7)
    );
    assert!(receipt.value["full_detail"].as_str().expect("detail").len() <= 64);
    for key in [
        "operation",
        "work",
        "obligations",
        "full_detail",
        "reminders",
        "next",
        "evaluation",
        "effective_session_id",
    ] {
        assert!(
            receipt.value.get(key).is_some(),
            "{key} missing from the envelope"
        );
    }

    // The maximal word-only envelope, built from the real skeleton. A stored
    // title is only trimmed and its summary compaction keeps interior bytes,
    // and a session id is only length-checked, so both may carry interior
    // control characters: one byte each, six bytes escaped.
    let control = |bytes: usize| "\u{0001}".repeat(bytes);
    assert_eq!(control(1).len(), 1);
    assert_eq!(serde_json::to_string(&control(1)).expect("json").len(), 8);
    let mut envelope = receipt.value.clone();
    envelope["work"] = serde_json::json!({
        "short_ref": "w-000000000000",
        "title": control(MAX_SUMMARY_BYTES),
        "lifecycle": "completed",
        "revision": i64::MAX,
    });
    envelope["claim"] = serde_json::json!({
        "holder": "p".repeat(crate::MAX_SESSION_ID_BYTES),
        "held_until": "2026-09-17T11:26:45.919316200Z",
    });
    envelope["obligations"] = serde_json::json!({ "open": usize::MAX, "omitted": usize::MAX });
    envelope["omissions"] = serde_json::json!(
        (0..7)
            .map(|_| serde_json::json!({
                "section": "participated",
                "reason": "satisfied_prerequisite_count_limit",
                "omitted_count": usize::MAX,
            }))
            .collect::<Vec<_>>()
    );
    envelope["full_detail"] =
        serde_json::json!("engram work show 'w-000000000000' --notes --gates");
    // Reminders and extra next commands are shed before the reserve is
    // relied upon; one navigation command stays.
    envelope["reminders"] = serde_json::json!([]);
    envelope["next"] = serde_json::json!(["engram work show 'w-000000000000' --notes --gates"]);
    envelope["effective_session_id"] = serde_json::json!(control(crate::MAX_SESSION_ID_BYTES));
    // Only the word's extras on the evaluation block count: the projection
    // itself is inside the service envelope.
    envelope["evaluation"] = serde_json::json!({ "hash": "0".repeat(64), "replayed": true });
    let derived =
        crate::verbs::receipts::compact_receipt_json_bytes(&envelope).expect("compact bytes");
    assert!(
        derived <= EVALUATE_WORD_RESERVE,
        "derived word-only envelope {derived} bytes exceeds the reserve {EVALUATE_WORD_RESERVE}"
    );
    // The text form carries no verdict rows: one bounded summary line, the
    // full-detail line, and guidance.
    assert!(
        format!("{}\n", receipt.text()).len() < MAX_AGENT_WORK_RESPONSE_BYTES / 4,
        "{}",
        receipt.text()
    );
}

// Round 4 (Low): the word's own envelope (lines, guidance, holder suffix,
// process-default session metadata) stays within the reserve the service
// preflight leaves free, so the row-free word receipt is bounded by
// construction; the finished receipt is what the shared strict rule measures.
#[test]
fn the_word_envelope_stays_within_its_reserve() {
    use crate::work_service::EVALUATE_WORD_RESERVE;
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-reserve".into());
    // A fitted process-default handle at the session-id bound makes the
    // finished receipt carry the largest `effective_session_id` the word
    // can produce.
    let effective = SessionId("p".repeat(crate::MAX_SESSION_ID_BYTES));
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    )
    .with_fitted_effective_session(effective.clone());
    let added = verbs
        .add(
            AddInput {
                title: "T".repeat(240),
                acceptance: (1..=8)
                    .map(|index| format!("criterion {index} {}", "c".repeat(120)))
                    .collect(),
                ..AddInput::default()
            },
            at(0),
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
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(1),
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
            at(2),
        )
        .expect("gate");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let work = SqliteStore::open(&database)
        .expect("store")
        .resolve_work_ref(&project, &work_ref)
        .expect("resolve");
    let item = Item {
        work_ref: work_ref.clone(),
        work_id: work.work_id,
        run_id: work.active_run_id.expect("active run"),
    };
    let hashes = gate_hashes(&database, &item);
    let input = evaluate_input(
        &work_ref,
        basis(&database, &item),
        (1..=8)
            .map(|position| verdict(position, "pass", "asserted", &hashes))
            .collect(),
    );
    let receipt = verbs.evaluate(input.clone(), at(4)).expect("evaluate");
    assert_eq!(receipt.value["effective_session_id"], effective.0);
    let service = verbs
        .service
        .work_evaluate_on(
            &crate::WorkEvaluateInput {
                work_ref: Some(work_ref.clone()),
                mode: input.mode.clone(),
                acceptance_basis: input.acceptance_basis,
                evidence_basis: input.evidence_basis,
                verdicts: input.verdicts.clone(),
                attempt: None,
                source_fingerprint: None,
                model: None,
                execution_identity: None,
                parent_session: None,
            },
            at(5),
        )
        .expect("the service replays the same attempt");
    assert!(service.replayed);
    let word_bytes =
        crate::verbs::receipts::compact_receipt_json_bytes(&receipt.value).expect("compact bytes");
    let service_bytes = serde_json::to_vec(&service).expect("service bytes").len();
    assert!(
        word_bytes < service_bytes + EVALUATE_WORD_RESERVE,
        "word {word_bytes} bytes, service {service_bytes} bytes, reserve {EVALUATE_WORD_RESERVE}"
    );
    assert!(
        format!("{}\n", receipt.text()).len() < MAX_AGENT_WORK_RESPONSE_BYTES,
        "{}",
        receipt.text()
    );
}

fn enable_with_source_freshness(database: &std::path::Path, second: i64) {
    SqliteStore::open(database)
        .expect("store")
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::SameSession],
                mechanical_basis: MechanicalBasis::Asserted,
                require_source_freshness: true,
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
                reason: "require source freshness for the verbs test".into(),
            },
            "enable-source-freshness",
            None,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("activate the policy");
}

// Review 6 (Medium): without a measurement the source is unknown, not stale;
// `done` names the fingerprint remedy, and the fingerprint is checked at done.
#[test]
fn source_freshness_is_checked_at_done_with_an_actionable_remedy() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-source".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = prepare(&verbs, &database, &project, "Source item", 0);
    enable_with_source_freshness(&database, 3);
    let hashes = gate_hashes(&database, &item);
    verbs
        .evaluate(
            EvaluateInput {
                source_fingerprint: Some("sha256:worktree-a".into()),
                ..evaluate_input(
                    &item.work_ref,
                    basis(&database, &item),
                    vec![verdict(1, "pass", "asserted", &hashes)],
                )
            },
            at(4),
        )
        .expect("passing evaluation with a fingerprint");
    let shown = verbs.show(&item.work_ref, at(5)).expect("show");
    assert!(
        !shown.text().contains("stale: source"),
        "an unmeasured read is not a mismatch: {}",
        shown.text()
    );
    assert!(
        shown.text().contains("source fingerprint"),
        "{}",
        shown.text()
    );
    let unmeasured = verbs
        .done(done_input(&item.work_ref), at(6))
        .expect("done returns a recovery receipt");
    assert!(unmeasured.owed);
    assert_eq!(unmeasured.value["code"], "acceptance_evaluation_stale");
    assert!(
        unmeasured.text().contains("--source-fingerprint"),
        "{}",
        unmeasured.text()
    );
    // A changed fingerprint is stale source; the matching one seals.
    let changed = verbs
        .done(
            DoneInput {
                source_fingerprint: Some("sha256:worktree-b".into()),
                ..done_input(&item.work_ref)
            },
            at(7),
        )
        .expect("done returns a recovery receipt");
    assert!(changed.owed);
    assert_eq!(changed.value["code"], "acceptance_evaluation_stale");
    assert!(
        changed.text().contains("stale (source)"),
        "{}",
        changed.text()
    );
    assert_eq!(
        verbs.show(&item.work_ref, at(8)).expect("show").value["status"]["work"]["lifecycle"],
        "open"
    );
    let sealed = verbs
        .done(
            DoneInput {
                source_fingerprint: Some("sha256:worktree-a".into()),
                ..done_input(&item.work_ref)
            },
            at(9),
        )
        .expect("the matching fingerprint seals");
    assert!(!sealed.owed, "{}", sealed.text());
    assert!(
        sealed
            .text()
            .contains("acceptance: evaluated (same_session, asserted) by "),
        "{}",
        sealed.text()
    );

    // An unmeasured source hides no independent stale reason: a revision
    // change after the record is reported as such on the same read path.
    let second = prepare(&verbs, &database, &project, "Second source item", 20);
    let hashes = gate_hashes(&database, &second);
    verbs
        .evaluate(
            EvaluateInput {
                source_fingerprint: Some("sha256:worktree-a".into()),
                ..evaluate_input(
                    &second.work_ref,
                    basis(&database, &second),
                    vec![verdict(1, "pass", "asserted", &hashes)],
                )
            },
            at(23),
        )
        .expect("passing evaluation with a fingerprint");
    verbs
        .update(
            UpdateInput {
                work_ref: Some(second.work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: Some("same_session".into()),
                },
            },
            at(24),
        )
        .expect("pin the mode, which revises the item");
    let revised = verbs.show(&second.work_ref, at(25)).expect("show");
    assert!(
        revised.text().contains("stale: revision"),
        "{}",
        revised.text()
    );
}
