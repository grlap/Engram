//! A word that names no item acts on the ambient focus, unless the session
//! holds other work and not the focus: `add` focuses the item it creates, so
//! a bare word right after one would otherwise land on that new item.

use super::*;

// Field order matters: the verbs close their store before the temporary home
// is removed.
struct Agent {
    verbs: AgentVerbs,
    database: std::path::PathBuf,
    _directory: crate::test_support::TempHome,
}

fn agent(project: &str) -> Agent {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId(project.into()),
        "agent".into(),
        SessionId("implicit-target-session".into()),
        Some("implicit-target-test".into()),
    );
    Agent {
        verbs,
        database,
        _directory: directory,
    }
}

fn add(agent: &Agent, title: &str, under: Option<&str>, second: i64) -> String {
    let receipt = agent
        .verbs
        .add(
            AddInput {
                external: None,
                notes: Vec::new(),
                title: title.into(),
                outcome: None,
                acceptance: vec![format!("{title} is delivered")],
                bindings: Vec::new(),
                under: under.map(Into::into),
                optional: false,
                priority: None,
                labels: Vec::new(),
                assignee: None,
                kind: None,
                evaluation_mode: None,
            },
            at(second),
        )
        .expect("add");
    receipt.value["work"]["short_ref"]
        .as_str()
        .expect("added ref")
        .to_owned()
}

fn claim(agent: &Agent, work_ref: &str, ttl: i64, second: i64) {
    agent
        .verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.into(),
                ttl_seconds: Some(ttl),
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

fn note(agent: &Agent, work_ref: Option<&str>, second: i64) -> Result<Receipt, VerbError> {
    agent.verbs.note(
        &NoteInput {
            status: false,
            work_ref: work_ref.map(Into::into),
            text: format!("a finding at {second}"),
            refs: Vec::new(),
        },
        at(second),
    )
}

fn gate(agent: &Agent, work_ref: Option<&str>, second: i64) -> Result<Receipt, VerbError> {
    agent.verbs.gate(
        GateInput {
            work_ref: work_ref.map(Into::into),
            name: "unit".into(),
            failed: Vec::new(),
            evidence_ref: None,
        },
        at(second),
    )
}

/// Every row of the work feed and object tables, so a refusal can be shown
/// to record nothing.
fn recorded(agent: &Agent) -> (i64, i64) {
    let connection = rusqlite::Connection::open(&agent.database).expect("store");
    let count = |table: &str| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count rows")
    };
    (count("work_feed_entries"), count("objects"))
}

/// The refusal a bare word gets, with its focus and held refs.
fn conflict(result: Result<Receipt, VerbError>) -> (VerbError, String, String, Vec<String>) {
    let error = result.expect_err("a bare word is refused");
    let StoreError::WorkImplicitTargetConflict(conflict) = &error.error else {
        panic!("expected an implicit-target refusal, got {:?}", error.error);
    };
    assert_eq!(conflict.more, 0);
    let (operation, focus, held) = (
        conflict.operation.clone(),
        conflict.focus.clone(),
        conflict.held.clone(),
    );
    (error, operation, focus, held)
}

// The reported case: hold X, add Y, then a bare note or gate. Each is refused
// naming both items, records nothing, and offers the explicit commands; the
// same words with an explicit ref act as they always have.
#[test]
fn a_bare_word_after_add_is_refused_while_another_item_is_held() {
    let agent = agent("implicit-target-after-add");
    let held = add(&agent, "Held work", None, 1);
    claim(&agent, &held, 3_600, 2);
    let added = add(&agent, "Filed follow-up", None, 3);
    let before = recorded(&agent);

    let (error, operation, focus, named) = conflict(note(&agent, None, 4));
    assert_eq!(
        (operation.as_str(), focus.as_str()),
        ("note", added.as_str())
    );
    assert_eq!(named, vec![held.clone()]);
    let guidance = error.guidance();
    assert!(
        guidance.reminders[0].contains("nothing was recorded"),
        "{guidance:?}"
    );
    assert_eq!(
        guidance.next,
        vec![
            format!("engram work note {held} \"…\""),
            format!("engram work note {added} \"…\""),
        ]
    );

    let (error, operation, _, _) = conflict(gate(&agent, None, 5));
    assert_eq!(operation, "gate");
    assert_eq!(
        error.guidance().next,
        vec![
            format!("engram work gate NAME --work-ref {held}"),
            format!("engram work claim {added}"),
        ]
    );
    // The planning flow: a bare update would revise the new, unclaimed item.
    let (error, operation, _, _) = conflict(agent.verbs.update(
        UpdateInput {
            work_ref: None,
            action: UpdateAction::Blocked {
                detail: "waiting on review".into(),
            },
        },
        at(6),
    ));
    assert_eq!(operation, "update");
    assert_eq!(
        error.guidance().next,
        vec![
            format!("engram work update {held} …"),
            format!("engram work update {added} …"),
        ]
    );
    let (_, operation, _, _) = conflict(agent.verbs.done(DoneInput::default(), at(7)));
    assert_eq!(operation, "done");
    let (error, operation, _, _) = conflict(agent.verbs.evaluate(
        EvaluateInput {
            work_ref: None,
            mode: "independent_session".into(),
            acceptance_basis: 1,
            evidence_basis: 1,
            verdicts: Vec::new(),
            attempt: None,
            source_fingerprint: None,
            model: None,
            execution_identity: None,
            parent_session: None,
            supersedes: None,
        },
        at(8),
    ));
    assert_eq!(operation, "evaluate");
    assert_eq!(
        error.guidance().next,
        vec![
            format!("engram work evaluate {held} …"),
            format!("engram work evaluate {added} …"),
        ]
    );
    assert_eq!(
        recorded(&agent),
        before,
        "a refused bare word recorded something"
    );

    // Naming the item keeps working: the holder's own note and gate, and a
    // non-holder's observation on the new item.
    note(&agent, Some(&held), 9).expect("explicit holder note");
    gate(&agent, Some(&held), 10).expect("explicit holder gate");
    let observation = note(&agent, Some(&added), 11).expect("explicit observation");
    assert_eq!(observation.value["non_holder"], json!(true));
    // A holder's note moves focus back to the held item, so a bare word
    // acts there again.
    let bare = note(&agent, None, 12).expect("bare note on the held focus");
    assert_eq!(bare.value["work"]["short_ref"], json!(held));
}

// A session that holds nothing, or whose claim has expired, keeps the bare
// observation on the focus: nothing else could be meant.
#[test]
fn a_session_holding_nothing_keeps_the_bare_observation() {
    let agent = agent("implicit-target-nothing-held");
    let added = add(&agent, "Proposed work", None, 1);
    let observed = note(&agent, None, 2).expect("bare observation");
    assert_eq!(observed.value["work"]["short_ref"], json!(added));
    assert_eq!(observed.value["non_holder"], json!(true));
    // The planning flow with no claim held: a bare update revises the focus,
    // as it always has.
    let planned = agent
        .verbs
        .update(
            UpdateInput {
                work_ref: None,
                action: UpdateAction::Blocked {
                    detail: "waiting on review".into(),
                },
            },
            at(2),
        )
        .expect("bare planning update with no claim held");
    assert!(planned.text().contains(&added), "{}", planned.text());

    let expiring = add(&agent, "Briefly held", None, 3);
    claim(&agent, &expiring, 60, 4);
    let later = add(&agent, "Added after", None, 5);
    conflict(note(&agent, None, 6));
    // At and after the claim's expiry the session holds nothing.
    let observed = note(&agent, None, 64).expect("bare observation once the claim expired");
    assert_eq!(observed.value["work"]["short_ref"], json!(later));
}

// A focus that is one of several held items is unaffected; a child added
// under the held parent takes the focus, so a bare word names both.
#[test]
fn a_held_focus_is_unaffected_and_a_new_child_is_named() {
    let agent = agent("implicit-target-several-held");
    let first = add(&agent, "First held", None, 1);
    claim(&agent, &first, 3_600, 2);
    let second = add(&agent, "Second held", None, 3);
    claim(&agent, &second, 3_600, 4);
    let on_focus = note(&agent, None, 5).expect("bare note on a held focus");
    assert_eq!(on_focus.value["work"]["short_ref"], json!(second));
    gate(&agent, None, 6).expect("bare gate on a held focus");

    let child = add(&agent, "Child of the held parent", Some(&second), 7);
    let (_, _, focus, mut held) = conflict(note(&agent, None, 8));
    assert_eq!(focus, child);
    held.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(held, expected);
    note(&agent, Some(&second), 9).expect("explicit note on the held parent");
}

// The command offered for the unheld focus fits what the word can do there:
// never a claim of finished work or of work another session holds.
#[test]
fn the_focus_command_fits_the_focus_state() {
    use crate::storage::ImplicitFocusState as State;
    use crate::verbs::receipts::implicit_focus_command as command;
    let focus = "w-focus";
    assert_eq!(
        command("note", focus, State::Unclaimed),
        "engram work note w-focus \"…\""
    );
    assert_eq!(
        command("note", focus, State::NotOpen),
        "engram work note w-focus \"…\""
    );
    assert_eq!(
        command("note", focus, State::HeldElsewhere),
        "engram work note w-focus \"…\""
    );
    assert_eq!(
        command("gate", focus, State::Unclaimed),
        "engram work claim w-focus"
    );
    assert_eq!(
        command("gate", focus, State::NotOpen),
        "engram work gate NAME --work-ref w-focus"
    );
    assert_eq!(
        command("gate", focus, State::HeldElsewhere),
        "engram work show w-focus"
    );
    assert_eq!(
        command("done", focus, State::Unclaimed),
        "engram work claim w-focus"
    );
    assert_eq!(
        command("done", focus, State::NotOpen),
        "engram work show w-focus"
    );
    assert_eq!(
        command("done", focus, State::HeldElsewhere),
        "engram work show w-focus"
    );
    assert_eq!(
        command("update", focus, State::Unclaimed),
        "engram work update w-focus …"
    );
    assert_eq!(
        command("update", focus, State::NotOpen),
        "engram work show w-focus"
    );
    assert_eq!(
        command("update", focus, State::HeldElsewhere),
        "engram work show w-focus"
    );
    assert_eq!(
        command("evaluate", focus, State::Unclaimed),
        "engram work evaluate w-focus …"
    );
    assert_eq!(
        command("evaluate", focus, State::HeldElsewhere),
        "engram work evaluate w-focus …"
    );
    assert_eq!(
        command("evaluate", focus, State::NotOpen),
        "engram work show w-focus"
    );
}
