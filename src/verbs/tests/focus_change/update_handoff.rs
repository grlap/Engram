//! `update` and `handoff` disclose the focus move they made, on their receipt
//! or beside a refusal that came after the move, and the claim they leave:
//! none after a release or a disposal, the transferred one after an accept.

use super::*;

fn update(
    verbs: &AgentVerbs,
    work_ref: &str,
    action: UpdateAction,
    second: i64,
) -> Result<Receipt, VerbError> {
    verbs.update(
        UpdateInput {
            work_ref: Some(work_ref.into()),
            action,
        },
        at(second),
    )
}

fn handoff(
    verbs: &AgentVerbs,
    work_ref: &str,
    action: HandoffAction,
    second: i64,
) -> Result<Receipt, VerbError> {
    verbs.handoff(
        HandoffInput {
            work_ref: Some(work_ref.into()),
            action,
        },
        at(second),
    )
}

/// The refusal's disclosed `(from, to)` in the projected JSON, and whether it
/// names a claim.
fn refusal_moved(verbs: &AgentVerbs, refused: &VerbError) -> Option<(Value, Value, bool)> {
    let value = verbs.project_error(refused, crate::store_error_value(&refused.error));
    let change = value.get("focus_change")?;
    Some((
        change["from"].clone(),
        change["to"].clone(),
        change.get("claim_fence").is_some(),
    ))
}

// A valid update refused after it moved focus says so beside the refusal; a
// repeat that found focus already there says nothing.
#[test]
fn an_update_refused_after_its_move_discloses_it() {
    let (agent, peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let held = add(&peer.verbs, "Held by the peer", None, 2);
    claim(&peer.verbs, &held, 3).expect("peer claims");
    claim(&agent.verbs, &first, 4).expect("agent claims");

    let cancel = || UpdateAction::Cancel {
        reason: "not needed".into(),
    };
    let refused = update(&agent.verbs, &held, cancel(), 5).expect_err("held elsewhere");
    assert_eq!(
        refusal_moved(&agent.verbs, &refused),
        Some((json!(first), json!(held), false)),
        "{:?}",
        refused.error
    );
    assert_eq!(
        refused.focus_change_line(),
        Some(
            format!("focus moved from {first} to {held}; {held} has no live claim to bind")
                .as_str()
        )
    );

    let again = update(&agent.verbs, &held, cancel(), 6).expect_err("still held");
    assert_eq!(again.focus_change_line(), None, "focus was already there");
    assert_eq!(refusal_moved(&agent.verbs, &again), None);
}

// A valid handoff refused after it moved focus says so beside the refusal.
#[test]
fn a_handoff_refused_after_its_move_discloses_it() {
    let (agent, _peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");

    let refused =
        handoff(&agent.verbs, &second, HandoffAction::Accept, 4).expect_err("no offer to accept");
    assert_eq!(
        refusal_moved(&agent.verbs, &refused),
        Some((json!(first), json!(second), false)),
        "{:?}",
        refused.error
    );
    assert!(refused.focus_change_line().is_some());

    let again =
        handoff(&agent.verbs, &second, HandoffAction::Accept, 5).expect_err("still no offer");
    assert_eq!(again.focus_change_line(), None, "focus was already there");
}

// Releasing another held item moves focus there and ends the claim, so the
// receipt names no claim to bind; an update on the focused item says nothing.
#[test]
fn a_release_after_its_move_names_no_claim() {
    let (agent, _peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");

    let released = update(
        &agent.verbs,
        &first,
        UpdateAction::Release {
            reason: Some("pausing".into()),
        },
        5,
    )
    .expect("release");
    assert_eq!(
        moved(&released),
        Some((json!(second), json!(first), false)),
        "{}",
        released.value
    );
    assert!(
        released
            .text()
            .contains(&format!("{first} has no live claim to bind")),
        "{}",
        released.text()
    );

    let revised = update(
        &agent.verbs,
        &first,
        UpdateAction::Revise {
            external: None,
            clear_external: false,
            title: Some("First, revised".into()),
            outcome: None,
            acceptance: None,
            bindings: None,
            assignee: None,
            priority: None,
            defer: None,
            kind: None,
            labels: Vec::new(),
            unlabels: Vec::new(),
        },
        6,
    )
    .expect("revise the focused item");
    assert_eq!(moved(&revised), None, "an unchanged focus says nothing");
}

// Cancelling another held item disposes of it; the claim it had binds no more.
#[test]
fn a_cancel_after_its_move_names_no_claim() {
    let (agent, _peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");

    let cancelled = update(
        &agent.verbs,
        &first,
        UpdateAction::Cancel {
            reason: "not needed".into(),
        },
        5,
    )
    .expect("cancel");
    assert_eq!(
        moved(&cancelled),
        Some((json!(second), json!(first), false)),
        "{}",
        cancelled.value
    );
}

// Accepting an offer moves focus to the offered item and names the claim the
// accept transferred to this session.
#[test]
fn an_accept_after_its_move_names_the_transferred_claim() {
    let (agent, peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let offered = add(&peer.verbs, "Offered", None, 2);
    claim(&peer.verbs, &offered, 3).expect("peer claims");
    handoff(
        &peer.verbs,
        &offered,
        HandoffAction::Offer {
            to: "agent".into(),
            summary: Some("over to you".into()),
            ttl_seconds: None,
        },
        4,
    )
    .expect("offer");
    claim(&agent.verbs, &first, 5).expect("agent claims");

    let accepted = handoff(&agent.verbs, &offered, HandoffAction::Accept, 6).expect("accept");
    assert_eq!(
        moved(&accepted),
        Some((json!(first), json!(offered), true)),
        "{}",
        accepted.value
    );
    assert!(
        accepted
            .text()
            .contains(&format!("the host binds {offered}'s claim")),
        "{}",
        accepted.text()
    );
}

// Rejecting a required child cancels it, so the claim on it binds no more.
#[test]
fn a_reject_after_its_move_names_no_claim() {
    let (agent, _peer, _database) = sessions();
    let parent = add(&agent.verbs, "Parent", None, 1);
    let child = add(&agent.verbs, "Child", Some(&parent), 2);
    let other = add(&agent.verbs, "Other", None, 3);
    claim(&agent.verbs, &child, 4).expect("claim child");
    claim(&agent.verbs, &other, 5).expect("claim other");

    let rejected = update(
        &agent.verbs,
        &child,
        UpdateAction::Reject {
            reason: "not needed after all".into(),
        },
        6,
    )
    .expect("reject");
    assert_eq!(
        moved(&rejected),
        Some((json!(other), json!(child), false)),
        "{}",
        rejected.value
    );
}

// Offering another held item moves focus there; the pending offer keeps the
// claim from binding until it is settled, so the receipt names none.
// Cancelling the offer from elsewhere moves focus back and names the claim
// the cancel left free to bind again.
#[test]
fn an_offer_names_no_claim_and_its_cancel_names_the_retained_one() {
    let (agent, _peer, _database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");

    let offered = handoff(
        &agent.verbs,
        &first,
        HandoffAction::Offer {
            to: "peer".into(),
            summary: Some("over to you".into()),
            ttl_seconds: None,
        },
        5,
    )
    .expect("offer");
    assert_eq!(
        moved(&offered),
        Some((json!(second), json!(first), false)),
        "{}",
        offered.value
    );

    note(&agent.verbs, &second, 6);
    let cancelled = handoff(
        &agent.verbs,
        &first,
        HandoffAction::Cancel {
            reason: "kept after all".into(),
        },
        7,
    )
    .expect("cancel");
    assert_eq!(
        moved(&cancelled),
        Some((json!(second), json!(first), true)),
        "{}",
        cancelled.value
    );
}
