//! A word that moves this session's focus says so: on its receipt, or on its
//! refusal when it moved focus and then refused. An unchanged call, a peer's
//! call and a word that ends where it started say nothing, and a retry says
//! only what that retry itself did.

use std::sync::Arc;

use super::*;
use crate::storage::{FOCUS_CHANGE_RESERVE, FocusBinding, FocusChange};
use crate::verbs::focus_change::FocusDisclosure;

mod invalid_input;

struct Session {
    verbs: AgentVerbs,
    _directory: Option<crate::test_support::TempHome>,
}

fn sessions() -> (Session, Session, std::path::PathBuf) {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("focus-change".into());
    let session = |who: &str| {
        AgentVerbs::new(
            database.clone(),
            project.clone(),
            who.into(),
            SessionId(who.into()),
            None,
        )
    };
    (
        Session {
            verbs: session("agent"),
            _directory: Some(directory),
        },
        Session {
            verbs: session("peer"),
            _directory: None,
        },
        database,
    )
}

fn add(verbs: &AgentVerbs, title: &str, under: Option<&str>, second: i64) -> String {
    verbs
        .add(
            AddInput {
                title: title.into(),
                acceptance: vec![format!("{title} works")],
                under: under.map(str::to_owned),
                ..AddInput::default()
            },
            at(second),
        )
        .expect("add")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned()
}

fn claim(verbs: &AgentVerbs, work_ref: &str, second: i64) -> Result<Receipt, VerbError> {
    verbs.claim(
        ClaimInput {
            work_ref: work_ref.into(),
            ttl_seconds: Some(3_600),
            recover: None,
        },
        at(second),
    )
}

fn note(verbs: &AgentVerbs, work_ref: &str, second: i64) -> Receipt {
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work_ref.into()),
                text: format!("finding {second}"),
                refs: Vec::new(),
            },
            at(second),
        )
        .expect("note")
}

fn gate(verbs: &AgentVerbs, work_ref: &str, second: i64) -> Receipt {
    verbs
        .gate(
            GateInput {
                work_ref: Some(work_ref.into()),
                name: "cargo-test".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(second),
        )
        .expect("gate")
}

fn done(verbs: &AgentVerbs, work_ref: &str, second: i64) -> Receipt {
    verbs
        .done(
            DoneInput {
                work_ref: Some(work_ref.into()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(second),
        )
        .expect("done")
}

/// The disclosed change's `(from, to)`, and whether it names a claim.
fn moved(receipt: &Receipt) -> Option<(Value, Value, bool)> {
    let change = receipt.value.get("focus_change")?;
    Some((
        change["from"].clone(),
        change["to"].clone(),
        change.get("claim_fence").is_some(),
    ))
}

fn text_has_move(receipt: &Receipt, from: &str, to: &str) -> bool {
    receipt
        .text()
        .contains(&format!("focus moved from {from} to {to}; "))
}

// Claim moves focus from the item `add` focused to the claimed one and names
// the claim it took; claiming it again leaves focus where it is and says
// nothing. A session with no focus moves from none.
#[test]
fn claim_discloses_the_move_and_the_claim_it_took() {
    let (agent, peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);

    let claimed = claim(&agent.verbs, &first, 3).expect("claim");
    assert_eq!(
        moved(&claimed),
        Some((json!(second), json!(first), true)),
        "{}",
        claimed.value
    );
    assert!(
        text_has_move(&claimed, &second, &first),
        "{}",
        claimed.text()
    );
    assert!(
        claimed.text().contains(&format!(
            "the host binds {first}'s claim from its next turn, not this one"
        )),
        "{}",
        claimed.text()
    );
    assert_eq!(claimed.value["focus_change"]["claim_fence"], json!(1));
    assert!(!claimed.text().contains("rebound"), "{}", claimed.text());
    assert!(
        !claimed.text().contains("fence"),
        "shell text carries no fence"
    );

    let renewed = claim(&agent.verbs, &first, 4).expect("renew");
    assert_eq!(moved(&renewed), None, "an unchanged focus says nothing");

    let initial = claim(&peer.verbs, &second, 5).expect("peer claims");
    assert_eq!(moved(&initial), Some((Value::Null, json!(second), true)));
    assert!(
        initial
            .text()
            .contains(&format!("focus moved from no focus to {second}; "))
    );
}

// A holder's note, gate or done on another item it holds moves focus there
// and says so; a repeat on the focused item does not. A peer's note on an
// item it does not hold leaves its focus and says nothing. Completing the
// item ends the claim, so done names no claim to bind.
#[test]
fn holder_words_on_another_held_item_disclose_the_move() {
    let (agent, peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");

    let noted = note(&agent.verbs, &first, 5);
    assert_eq!(moved(&noted), Some((json!(second), json!(first), true)));
    assert_eq!(moved(&note(&agent.verbs, &first, 6)), None);

    let gated = gate(&agent.verbs, &second, 7);
    assert_eq!(moved(&gated), Some((json!(first), json!(second), true)));

    let observed = note(&peer.verbs, &first, 8);
    assert_eq!(moved(&observed), None, "a peer's note does not move focus");

    let completed = done(&agent.verbs, &first, 9);
    assert_eq!(
        moved(&completed),
        Some((json!(second), json!(first), false)),
        "{}",
        completed.value
    );
    assert!(
        completed
            .text()
            .contains(&format!("{first} has no live claim to bind")),
        "{}",
        completed.text()
    );

    // A replay of the same done says only what the replay itself did: focus
    // moved back from the other item, and nothing when it is already there.
    note(&agent.verbs, &second, 10);
    let replayed = done(&agent.verbs, &first, 11);
    assert_eq!(moved(&replayed), Some((json!(second), json!(first), false)));
    assert_eq!(moved(&done(&agent.verbs, &first, 12)), None);
}

// A holder's evaluation of another item it holds moves focus there and names
// the claim; an evaluation of the focused item says nothing.
#[test]
fn a_holder_evaluation_of_another_held_item_discloses_the_move() {
    let (agent, _peer, database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    gate(&agent.verbs, &first, 4);
    claim(&agent.verbs, &second, 5).expect("claim second");
    super::evaluate::enable(
        &database,
        &[crate::domain::AcceptanceEvaluationMode::SameSession],
        6,
    );
    let store = SqliteStore::open(&database).expect("store");
    let run = store
        .resolve_work_ref(&ProjectId("focus-change".into()), &first)
        .expect("first")
        .active_run_id
        .expect("run");
    let citations = store
        .work_run_evidence(run)
        .expect("evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    let basis = store
        .work_feed_head(&crate::domain::FeedId::RunExecution(run))
        .expect("head");
    drop(store);
    let evaluate = |second: i64| {
        agent.verbs.evaluate(
            EvaluateInput {
                supersedes: None,
                work_ref: Some(first.clone()),
                mode: "same_session".into(),
                acceptance_basis: 1,
                evidence_basis: basis,
                verdicts: vec![crate::WorkCriterionVerdictInput {
                    criterion: 1,
                    verdict: "pass".into(),
                    basis: "asserted".into(),
                    rationale: "the gate passed".into(),
                    evidence: citations.clone(),
                }],
                attempt: Some("focus-change-evaluation".into()),
                source_fingerprint: None,
                model: None,
                execution_identity: None,
                parent_session: None,
            },
            at(second),
        )
    };
    let evaluated = evaluate(7).expect("evaluate");
    assert_eq!(moved(&evaluated), Some((json!(second), json!(first), true)));
    let replayed = evaluate(8).expect("exact resend");
    assert_eq!(moved(&replayed), None, "the resend moved nothing");
}

// A word that moves focus and then refuses says so beside its refusal, in
// JSON and in text; a refusal that moved nothing carries no change.
#[test]
fn a_refusal_after_a_move_discloses_it() {
    let (agent, peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let held = add(&peer.verbs, "Held by the peer", None, 2);
    claim(&peer.verbs, &held, 3).expect("peer claims");
    claim(&agent.verbs, &first, 4).expect("agent claims");

    let refused = claim(&agent.verbs, &held, 5).expect_err("held elsewhere");
    assert!(matches!(refused.error, StoreError::WorkClaimHeld { .. }));
    let value = agent
        .verbs
        .project_error(&refused, crate::store_error_value(&refused.error));
    assert_eq!(value["focus_change"]["from"], json!(first));
    assert_eq!(value["focus_change"]["to"], json!(held));
    assert!(value["focus_change"].get("claim_fence").is_none());
    assert_eq!(
        refused.focus_change_line(),
        Some(
            format!("focus moved from {first} to {held}; {held} has no live claim to bind")
                .as_str()
        )
    );

    let again = claim(&agent.verbs, &held, 6).expect_err("still held");
    assert_eq!(again.focus_change_line(), None, "focus was already there");
    let value = agent
        .verbs
        .project_error(&again, crate::store_error_value(&again.error));
    assert!(value.get("focus_change").is_none());
}

// Claiming under a parent focuses the parent and then the child. The first
// claim discloses the net move to the child; renewing the child it already
// holds ends where it started and says nothing.
#[test]
fn claim_under_reports_the_net_move_only() {
    let (agent, _peer, _database) = sessions();
    let parent = add(&agent.verbs, "Parent", None, 1);
    let child = add(&agent.verbs, "Child", Some(&parent), 2);
    let other = add(&agent.verbs, "Other", None, 3);

    let claimed = agent
        .verbs
        .claim_under(
            ClaimUnderInput {
                under: parent.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(4),
        )
        .expect("claim under");
    assert_eq!(moved(&claimed), Some((json!(other), json!(child), true)));

    let renewed = agent
        .verbs
        .claim_under(
            ClaimUnderInput {
                under: parent,
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(5),
        )
        .expect("renew under");
    assert_eq!(moved(&renewed), None, "child to parent to child is no move");
}

// Two words of one session run at once on two threads over one shared
// service, as the MCP server runs them; each receipt carries only a move its
// own word made.
#[test]
fn concurrent_words_on_one_shared_service_disclose_only_their_own_moves() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("focus-change-shared".into());
    let session = SessionId("agent".into());
    let service = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        session.clone(),
        None,
    ));
    let verbs =
        || AgentVerbs::with_shared_service(Arc::clone(&service), "agent".into(), session.clone());
    let items = (1..=4)
        .map(|index| add(&verbs(), &format!("Item {index}"), None, index))
        .collect::<Vec<_>>();
    for (index, item) in items.iter().enumerate() {
        claim(&verbs(), item, 10 + i64::try_from(index).unwrap()).expect("claim");
    }
    let handles = items
        .iter()
        .cycle()
        .take(16)
        .enumerate()
        .map(|(index, item)| {
            let words = verbs();
            let item = item.clone();
            std::thread::spawn(move || {
                let receipt = note(&words, &item, 20 + i64::try_from(index).unwrap());
                (item, receipt)
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        let (item, receipt) = handle.join().expect("word thread");
        if let Some((from, to, _)) = moved(&receipt) {
            assert_eq!(
                to,
                json!(item),
                "a receipt names only its own word's target"
            );
            assert_ne!(from, json!(item));
        }
    }
}

// The largest disclosure fits the reserve each fitted word leaves for it, in
// JSON and as text; a workspace name too long to show is named by its length.
#[test]
fn the_largest_disclosure_fits_its_reserve() {
    let fits = crate::storage::MAX_DISCLOSED_WORKSPACE_JSON_BYTES - 2;
    let largest = FocusChange {
        from: Some("w-ffffffffffff".into()),
        to: Some("w-eeeeeeeeeeee".into()),
        binding: Some(FocusBinding {
            claim_id: crate::domain::WorkClaimId::new(),
            claim_fence: i64::MIN,
            workspace_id: Some("w".repeat(fits)),
            generation: Some(i64::MIN),
        }),
    };
    let disclosure = FocusDisclosure::of(&largest);
    assert_eq!(disclosure.value["workspace_id"], json!("w".repeat(fits)));
    let json_increment = serde_json::to_vec(&json!({ "focus_change": disclosure.value }))
        .expect("encode")
        .len();
    assert!(json_increment < FOCUS_CHANGE_RESERVE, "{json_increment}");
    assert!(
        disclosure.line.len() + 1 < FOCUS_CHANGE_RESERVE,
        "{}",
        disclosure.line.len()
    );

    // JSON escapes ASCII controls and terminal text escapes C1 controls and
    // private-use characters: the shown name stays within the bound on both
    // surfaces, so the largest one escaped for the shell still fits.
    let with_name = |name: &str| {
        let mut change = largest.clone();
        change.binding.as_mut().expect("binding").workspace_id = Some(name.to_owned());
        FocusDisclosure::of(&change)
    };
    let shell_escaped = "\u{80}".repeat(32);
    let disclosure = with_name(&shell_escaped);
    assert_eq!(disclosure.value["workspace_id"], json!(shell_escaped));
    assert!(
        disclosure.line.len() + 1 < FOCUS_CHANGE_RESERVE,
        "{}",
        disclosure.line.len()
    );

    // Past the bound on either surface the name is named by its UTF-8
    // length, never shown clipped.
    for (name, bytes) in [
        ("\u{1}".repeat(64), 64),
        ("\u{80}".repeat(95), 190),
        ("\u{e000}".repeat(64), 192),
    ] {
        let disclosure = with_name(&name);
        assert!(disclosure.value.get("workspace_id").is_none(), "{name:?}");
        assert_eq!(disclosure.value["workspace_id_omitted_bytes"], json!(bytes));
        assert!(
            disclosure
                .line
                .contains(&format!("{bytes}-byte workspace name is not shown")),
            "{}",
            disclosure.line
        );
        assert!(disclosure.line.len() + 1 < FOCUS_CHANGE_RESERVE);
    }
}
