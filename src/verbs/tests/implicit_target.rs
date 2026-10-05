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

// A note by a session that does not hold the item is an observation: its
// receipt offers a read first and never nudges the writer to claim. On
// unclaimed work the claim follows last, named as the way to execute the
// item; on work another session holds, no claim is offered; a holder's note
// keeps its own guidance. Text and JSON say the same.
#[test]
fn an_observation_note_offers_a_read_first_and_never_a_claim_nudge() {
    let agent = agent("observation-guidance");
    let nudge = "unclaimed: claim it before execution";
    let label = "observation recorded; to execute this item rather than observe it, claim it (the last next command)";
    let unclaimed = add(&agent, "Unclaimed work", None, 1);
    // The item's detail read is the receipt's full-detail line; the first next
    // command is the orientation read.
    let read = |_: &str| "engram work next --peek".to_owned();
    let detail = |work_ref: &str| format!("engram work show '{work_ref}' --notes");

    let observed = note(&agent, Some(&unclaimed), 2).expect("observation");
    assert_eq!(observed.value["non_holder"], json!(true));
    assert_eq!(observed.next[0], read(&unclaimed), "{:?}", observed.next);
    assert_eq!(observed.value["next"][0], json!(read(&unclaimed)));
    assert_eq!(observed.value["full_detail"], json!(detail(&unclaimed)));
    assert_eq!(
        observed.next.last(),
        Some(&format!("engram work claim {unclaimed}"))
    );
    assert!(
        !observed.reminders.iter().any(|line| line == nudge),
        "{:?}",
        observed.reminders
    );
    assert!(
        observed.reminders.iter().any(|line| line == label),
        "{:?}",
        observed.reminders
    );
    assert!(!observed.text().contains(nudge), "{}", observed.text());
    assert!(observed.text().contains(label), "{}", observed.text());

    // On work another session holds, the observer is offered no claim at all.
    let held = add(&agent, "Held elsewhere", None, 3);
    let holder = AgentVerbs::new(
        agent.database.clone(),
        ProjectId("observation-guidance".into()),
        "agent".into(),
        SessionId("other-holder".into()),
        None,
    );
    holder
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(4),
        )
        .expect("the other session claims");
    let watched = note(&agent, Some(&held), 5).expect("observation on held work");
    assert_eq!(watched.value["non_holder"], json!(true));
    assert_eq!(watched.next[0], read(&held), "{:?}", watched.next);
    assert!(
        !watched
            .next
            .iter()
            .any(|command| command.starts_with("engram work claim")),
        "{:?}",
        watched.next
    );
    assert!(
        !watched
            .reminders
            .iter()
            .any(|line| line == nudge || line == label),
        "{:?}",
        watched.reminders
    );
    drop(holder);

    // A holder's note keeps the holder's guidance.
    claim(&agent, &unclaimed, 3_600, 6);
    let own = note(&agent, Some(&unclaimed), 7).expect("holder note");
    assert_ne!(own.value["non_holder"], json!(true));
    assert_ne!(own.next[0], read(&unclaimed), "{:?}", own.next);
    assert!(
        !own.reminders.iter().any(|line| line == label),
        "{:?}",
        own.reminders
    );
}

// The observation guidance holds on the paths around it: a lapsed claim's
// recovery command moves last with the execution reminder; a blocked item
// and an exact replay keep it; and the same note repeated after another
// session claims the item, a new note since a keyless note's replay key
// includes its claim basis, keeps it too. JSON says what the text says.
#[test]
fn observation_guidance_holds_for_recover_blocked_replay_and_a_later_claim() {
    let agent = agent("observation-edges");
    let nudge = "unclaimed: claim it before execution";
    let label = "observation recorded; to execute this item rather than observe it, claim it (the last next command)";
    let peek = "engram work next --peek";
    let holder = AgentVerbs::new(
        agent.database.clone(),
        ProjectId("observation-edges".into()),
        "agent".into(),
        SessionId("other-holder".into()),
        None,
    );
    let observation = |receipt: &Receipt| {
        assert_eq!(
            receipt.value["non_holder"],
            json!(true),
            "{}",
            receipt.text()
        );
        assert_eq!(receipt.next[0], peek, "{:?}", receipt.next);
        assert_eq!(receipt.value["next"][0], json!(peek));
        assert!(
            !receipt.reminders.iter().any(|line| line == nudge),
            "{:?}",
            receipt.reminders
        );
        let reminders = receipt.value["reminders"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!reminders.contains(&json!(nudge)), "{reminders:?}");
    };

    // A claim that lapsed: the recovery claim moves last, with the reminder.
    let lapsed = add(&agent, "Lapsed work", None, 1);
    holder
        .claim(
            ClaimInput {
                work_ref: lapsed.clone(),
                ttl_seconds: Some(60),
                recover: None,
            },
            at(2),
        )
        .expect("a short claim");
    let after_lapse = note(&agent, Some(&lapsed), 200).expect("observation after the lapse");
    observation(&after_lapse);
    let recover = after_lapse.next.last().expect("a last next command");
    assert!(
        recover.starts_with(&format!("engram work claim {lapsed} --recover")),
        "{:?}",
        after_lapse.next
    );
    assert_eq!(
        after_lapse.value["next"]
            .as_array()
            .and_then(|next| next.last()),
        Some(&json!(recover))
    );
    assert!(
        after_lapse.reminders.iter().any(|line| line == label),
        "{:?}",
        after_lapse.reminders
    );
    assert!(
        after_lapse.value["reminders"]
            .as_array()
            .is_some_and(|reminders| reminders.contains(&json!(label)))
    );

    // A blocked open item keeps the observation guidance.
    let blocked = add(&agent, "Blocked work", None, 201);
    agent
        .verbs
        .update(
            UpdateInput {
                work_ref: Some(blocked.clone()),
                action: UpdateAction::Blocked {
                    detail: "waiting on review".into(),
                },
            },
            at(202),
        )
        .expect("block");
    observation(&note(&agent, Some(&blocked), 203).expect("observation on blocked work"));

    // An exact replay of an observation keeps its guidance: the same note
    // again records nothing new and returns the same evidence.
    let open = add(&agent, "Open work", None, 204);
    let same_note = NoteInput {
        status: false,
        work_ref: Some(open.clone()),
        text: "one observed finding".into(),
        refs: Vec::new(),
    };
    let first = agent.verbs.note(&same_note, at(205)).expect("observation");
    let replay = agent
        .verbs
        .note(&same_note, at(206))
        .expect("the same observation again");
    assert_eq!(
        replay.value["evidence"], first.value["evidence"],
        "a replay"
    );
    observation(&first);
    observation(&replay);
    assert_eq!(replay.next, first.next);
    assert_eq!(replay.reminders, first.reminders);

    // The same note after another session claims the item: still an
    // observation, now with no claim to offer.
    holder
        .claim(
            ClaimInput {
                work_ref: open.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(207),
        )
        .expect("the other session claims");
    let later = agent
        .verbs
        .note(&same_note, at(208))
        .expect("the same note after the claim");
    // A new note, not a replay: its key includes the claim basis.
    assert_ne!(
        later.value["evidence"], first.value["evidence"],
        "a new note"
    );
    observation(&later);
    assert!(
        !later
            .next
            .iter()
            .any(|command| command.starts_with("engram work claim")),
        "{:?}",
        later.next
    );
    assert!(
        !later.reminders.iter().any(|line| line == label),
        "{:?}",
        later.reminders
    );
    drop(holder);
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
// never a claim of finished work or of work another session holds, and a
// note or late gate only where the core admits it: completed work, never
// cancelled, superseded or proposed work.
#[test]
fn the_focus_command_fits_the_focus_state() {
    use crate::domain::WorkLifecycle as Life;
    use crate::storage::ImplicitFocusState as State;
    use crate::verbs::receipts::implicit_focus_command as command;
    let focus = "w-focus";
    let show = "engram work show w-focus";
    let open = [
        ("note", State::Unclaimed, "engram work note w-focus \"…\""),
        (
            "note",
            State::HeldElsewhere,
            "engram work note w-focus \"…\"",
        ),
        ("gate", State::Unclaimed, "engram work claim w-focus"),
        ("gate", State::HeldElsewhere, show),
        ("done", State::Unclaimed, "engram work claim w-focus"),
        ("done", State::HeldElsewhere, show),
        ("update", State::Unclaimed, "engram work update w-focus …"),
        ("update", State::HeldElsewhere, show),
        (
            "evaluate",
            State::Unclaimed,
            "engram work evaluate w-focus …",
        ),
        (
            "evaluate",
            State::HeldElsewhere,
            "engram work evaluate w-focus …",
        ),
    ];
    for (word, state, expected) in open {
        assert_eq!(
            command(word, focus, state, Life::Open),
            expected,
            "{word} {state:?}"
        );
    }
    let completed = [
        ("note", "engram work note w-focus \"…\""),
        ("gate", "engram work gate NAME --work-ref w-focus"),
        ("done", show),
        ("update", show),
        ("evaluate", show),
    ];
    for (word, expected) in completed {
        assert_eq!(
            command(word, focus, State::NotOpen, Life::Completed),
            expected,
            "{word} completed"
        );
    }
    for lifecycle in [Life::Cancelled, Life::Superseded, Life::Proposed] {
        for word in ["note", "gate", "done", "update", "evaluate"] {
            assert_eq!(
                command(word, focus, State::NotOpen, lifecycle),
                show,
                "{word} {lifecycle:?}"
            );
        }
    }
}

// The reported flow: hold X, add Y, end Y, then a bare note and a bare gate.
// Every command offered for the focus is one the core admits there: on
// cancelled or superseded work only the read, on completed work the explicit
// note and the late gate, and each one runs.
#[test]
fn every_command_offered_for_an_ended_focus_is_admitted() {
    for ending in ["cancelled", "superseded", "completed"] {
        let agent = agent(&format!("implicit-ended-{ending}"));
        let held = add(&agent, "Held work", None, 1);
        claim(&agent, &held, 600, 2);
        let replacement = add(&agent, "Replacement", None, 3);
        let ended = add(&agent, "Ended focus", None, 4);
        match ending {
            "cancelled" | "superseded" => {
                let action = if ending == "cancelled" {
                    UpdateAction::Cancel {
                        reason: "not needed".into(),
                    }
                } else {
                    UpdateAction::Supersede {
                        replacement: replacement.clone(),
                        reason: "replaced".into(),
                    }
                };
                agent
                    .verbs
                    .update(
                        UpdateInput {
                            work_ref: Some(ended.clone()),
                            action,
                        },
                        at(5),
                    )
                    .expect("end the focus");
            }
            _ => {
                claim(&agent, &ended, 600, 5);
                agent
                    .verbs
                    .done(
                        DoneInput {
                            work_ref: Some(ended.clone()),
                            summary: Some("delivered".into()),
                            ..DoneInput::default()
                        },
                        at(6),
                    )
                    .expect("complete the focus");
            }
        }
        for (word, second) in [("note", 10), ("gate", 20)] {
            let refused = if word == "note" {
                note(&agent, None, second)
            } else {
                gate(&agent, None, second)
            };
            let (error, _, focus, _) = conflict(refused);
            assert_eq!(focus, ended, "{ending} {word}");
            let next = error.guidance().next;
            let for_focus: Vec<&String> = next
                .iter()
                .filter(|command| command.contains(ended.as_str()))
                .collect();
            assert_eq!(for_focus.len(), 1, "{ending} {word}: {next:?}");
            let command = for_focus[0].as_str();
            match (ending, word) {
                ("completed", "note") => {
                    assert_eq!(command, format!("engram work note {ended} \"…\""));
                    note(&agent, Some(&ended), second + 1).expect("the offered late note runs");
                }
                ("completed", "gate") => {
                    assert_eq!(command, format!("engram work gate NAME --work-ref {ended}"));
                    gate(&agent, Some(&ended), second + 1).expect("the offered late gate runs");
                }
                _ => {
                    assert_eq!(
                        command,
                        format!("engram work show {ended}"),
                        "{ending} {word}"
                    );
                    agent
                        .verbs
                        .show(&ended, at(second + 1))
                        .expect("the offered read runs");
                    // The commands the core refuses on this focus are not offered.
                    assert!(note(&agent, Some(&ended), second + 2).is_err(), "{ending}");
                    assert!(gate(&agent, Some(&ended), second + 3).is_err(), "{ending}");
                }
            }
            // The held item is offered as well.
            assert!(
                next.iter().any(|command| command.contains(held.as_str())),
                "{ending} {word}: {next:?}"
            );
        }
    }
}

fn bare_evaluate(agent: &Agent, work_ref: Option<&str>, second: i64) -> Result<Receipt, VerbError> {
    agent.verbs.evaluate(
        EvaluateInput {
            work_ref: work_ref.map(Into::into),
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
        at(second),
    )
}

/// The refusal a bare done or evaluate gets while several claims are live.
fn ambiguity(result: Result<Receipt, VerbError>) -> (VerbError, String, String, Vec<String>) {
    let error = result.expect_err("a bare word is refused");
    let StoreError::WorkBareTargetAmbiguous(ambiguity) = &error.error else {
        panic!("expected a bare-target refusal, got {:?}", error.error);
    };
    let (operation, focus, held) = (
        ambiguity.operation.clone(),
        ambiguity.focus.clone(),
        ambiguity.held.clone(),
    );
    (error, operation, focus, held)
}

// Two live claims, the focus one of them: a bare done or evaluate is refused
// with every held ref and the explicit commands, and records nothing. Bare
// note, gate and handoff act on the held focus as before, and a focus the
// session does not hold keeps the older refusal.
#[test]
fn bare_done_and_evaluate_refuse_while_several_claims_are_live() {
    let agent = agent("bare-target-several-claims");
    let first = add(&agent, "First held work", None, 1);
    let second = add(&agent, "Second held work", None, 2);
    claim(&agent, &first, 3_600, 3);
    claim(&agent, &second, 3_600, 4);
    let mut held = vec![first.clone(), second.clone()];
    held.sort();
    let before = recorded(&agent);

    let (error, operation, focus, named) = ambiguity(agent.verbs.done(DoneInput::default(), at(5)));
    assert_eq!(
        (operation.as_str(), focus.as_str()),
        ("done", second.as_str())
    );
    assert_eq!(named, held, "every held ref, in ref order");
    let guidance = error.guidance();
    assert!(
        guidance.reminders[0].contains("holds 2 live claims")
            && guidance.reminders[0].contains("nothing was recorded"),
        "{guidance:?}"
    );
    assert_eq!(
        guidance.next,
        held.iter()
            .map(|work_ref| format!("engram work done {work_ref} \"…\""))
            .collect::<Vec<_>>()
    );
    let wire = crate::store_error_value(&error.error);
    assert_eq!(wire["error"]["code"], "work_bare_target_ambiguous");
    assert_eq!(wire["error"]["details"]["held_refs"], json!(held), "{wire}");

    let (error, operation, _, named) = ambiguity(bare_evaluate(&agent, None, 6));
    assert_eq!(operation, "evaluate");
    assert_eq!(named, held);
    assert_eq!(
        error.guidance().next,
        held.iter()
            .map(|work_ref| format!("engram work evaluate {work_ref} …"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        recorded(&agent),
        before,
        "a refused bare word recorded something"
    );

    // Note and gate stay bare-capable on the held focus.
    let bare = note(&agent, None, 7).expect("bare note on the held focus");
    assert_eq!(bare.value["work"]["short_ref"], json!(second));
    let gated = gate(&agent, None, 8).expect("bare gate on the held focus");
    assert_eq!(gated.value["work"]["short_ref"], json!(second));

    // A focus this session does not hold keeps the older refusal, which
    // offers the focus, rather than the count's.
    let unheld = add(&agent, "Filed follow-up", None, 9);
    let (error, operation, focus, named) = conflict(agent.verbs.done(DoneInput::default(), at(10)));
    assert_eq!(
        (operation.as_str(), focus.as_str()),
        ("done", unheld.as_str())
    );
    assert_eq!(named, held);
    assert!(
        error
            .guidance()
            .next
            .contains(&format!("engram work claim {unheld}")),
        "{:?}",
        error.guidance()
    );
    claim(&agent, &second, 3_600, 11);
    handoff_round_trip(&agent, &second, 12);
}

/// A bare handoff offer and its cancel both act on the held focus.
fn handoff_round_trip(agent: &Agent, focus: &str, second: i64) {
    for (action, at_second) in [
        (
            HandoffAction::Offer {
                to: "another-session".into(),
                summary: Some("taking over the focused item".into()),
                ttl_seconds: Some(600),
            },
            second,
        ),
        (
            HandoffAction::Cancel {
                reason: "kept it".into(),
            },
            second + 1,
        ),
    ] {
        let receipt = agent
            .verbs
            .handoff(
                HandoffInput {
                    work_ref: None,
                    action,
                },
                at(at_second),
            )
            .expect("bare handoff on the held focus");
        assert!(receipt.text().contains(focus), "{}", receipt.text());
    }
}

/// A passing same-session evaluation of `work_ref`'s one criterion, citing
/// its run evidence through the current run-feed head; `named` decides
/// whether the input names the item or leaves it to the focus.
fn passing_evaluation(agent: &Agent, project: &str, work_ref: &str, named: bool) -> EvaluateInput {
    let store = SqliteStore::open(&agent.database).expect("store");
    let run = store
        .resolve_work_ref(&ProjectId(project.into()), work_ref)
        .expect("resolve")
        .active_run_id
        .expect("active run");
    let citations = store
        .work_run_evidence(run)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(!citations.is_empty(), "a gate to cite");
    let basis = store
        .work_feed_head(&crate::domain::FeedId::RunExecution(run))
        .expect("run feed head");
    EvaluateInput {
        supersedes: None,
        work_ref: named.then(|| work_ref.to_owned()),
        mode: "same_session".into(),
        acceptance_basis: 1,
        evidence_basis: basis,
        verdicts: vec![crate::WorkCriterionVerdictInput {
            criterion: 1,
            verdict: "pass".into(),
            basis: "asserted".into(),
            rationale: "the gate passed".into(),
            evidence: citations,
        }],
        attempt: Some(format!("evaluate-{work_ref}")),
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
    }
}

// With two live claims, done and evaluate naming the item act as they always
// have: an evaluation records and its exact resend replays, and done
// completes the named item. Once one live claim remains, a bare gate,
// evaluate and done act on it.
#[test]
fn explicit_and_single_claim_done_and_evaluate_act_as_before() {
    let project = "bare-target-explicit-then-one";
    let agent = agent(project);
    let first = add(&agent, "First held work", None, 1);
    let second = add(&agent, "Second held work", None, 2);
    claim(&agent, &first, 3_600, 3);
    claim(&agent, &second, 3_600, 4);
    super::evaluate::enable(
        &agent.database,
        &[crate::domain::AcceptanceEvaluationMode::SameSession],
        5,
    );
    gate(&agent, Some(&second), 6).expect("explicit gate");
    let evaluation = passing_evaluation(&agent, project, &second, true);
    let evaluated = agent
        .verbs
        .evaluate(evaluation.clone(), at(7))
        .expect("an explicit evaluate records while two claims are live");
    let replayed = agent
        .verbs
        .evaluate(evaluation, at(8))
        .expect("its exact resend replays");
    assert_eq!(evaluated.value["evaluation"]["replayed"], false);
    assert_eq!(replayed.value["evaluation"]["replayed"], true);
    assert_eq!(
        replayed.value["evaluation"]["hash"], evaluated.value["evaluation"]["hash"],
        "{}",
        replayed.value
    );
    let done = agent
        .verbs
        .done(
            DoneInput {
                work_ref: Some(second.clone()),
                summary: Some("Delivered the second item".into()),
                ..DoneInput::default()
            },
            at(9),
        )
        .expect("an explicit done completes the named item");
    assert_eq!(done.value["work"]["short_ref"], json!(second));

    claim(&agent, &first, 3_600, 10);
    let gated = gate(&agent, None, 11).expect("a bare gate with one live claim");
    assert_eq!(gated.value["work"]["short_ref"], json!(first));
    agent
        .verbs
        .evaluate(passing_evaluation(&agent, project, &first, false), at(12))
        .expect("a bare evaluate with one live claim");
    let completed = agent
        .verbs
        .done(
            DoneInput {
                summary: Some("Delivered the first item".into()),
                ..DoneInput::default()
            },
            at(13),
        )
        .expect("a bare done with one live claim");
    assert_eq!(completed.value["work"]["short_ref"], json!(first));
}
