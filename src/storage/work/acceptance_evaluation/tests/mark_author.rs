//! Who set a task's same-session mark, read from the item's native events in
//! order: only a Created event, or a Revised event after a native event that
//! showed the item unmarked, proves who turned the mark on.

use super::super::{MarkStep, MarkTransition, mark_author, same_session_mark_author};
use super::*;
use crate::domain::SessionId;
use crate::storage::work::{reset_work_event_decode_count, work_event_decode_count};

fn step(transition: MarkTransition, marked: bool, session: &str) -> MarkStep {
    MarkStep {
        transition,
        marked,
        session: Some(SessionId(session.into())),
    }
}

fn author(steps: Vec<MarkStep>) -> Option<String> {
    mark_author(steps).map(|session| session.0)
}

#[test]
fn the_event_that_turned_the_mark_on_names_its_author() {
    use MarkTransition::{Created, Other, Revised};
    // Marked at creation.
    assert_eq!(
        author(vec![
            step(Created, true, "peer"),
            step(Other, true, "agent")
        ]),
        Some("peer".into())
    );
    // Marked by a revision after an unmarked creation.
    assert_eq!(
        author(vec![
            step(Created, false, "agent"),
            step(Revised, true, "peer")
        ]),
        Some("peer".into())
    );
    // Revisions of other fields, and reasserting the unchanged mark, keep
    // its author.
    assert_eq!(
        author(vec![
            step(Created, true, "peer"),
            step(Revised, true, "agent"),
            step(Revised, true, "agent"),
        ]),
        Some("peer".into())
    );
    // Clearing ends the mark; setting it again authors a new one.
    assert_eq!(
        author(vec![
            step(Created, true, "agent"),
            step(Revised, false, "peer"),
            step(Revised, true, "peer"),
        ]),
        Some("peer".into())
    );
    // Cleared and not set again: no mark, no author.
    assert_eq!(
        author(vec![
            step(Created, true, "peer"),
            step(Revised, false, "agent")
        ]),
        None
    );
}

#[test]
fn a_mark_no_native_event_shows_being_turned_on_has_no_author() {
    use MarkTransition::{Created, Other, Revised};
    // Restored history: the first native event already shows the mark,
    // whether it is a revision of another field or any other event.
    assert_eq!(author(vec![step(Revised, true, "peer")]), None);
    assert_eq!(
        author(vec![
            step(Other, true, "agent"),
            step(Revised, true, "peer")
        ]),
        None
    );
    // Once cleared and set again natively, the new mark has its author.
    assert_eq!(
        author(vec![
            step(Other, true, "agent"),
            step(Revised, false, "peer"),
            step(Revised, true, "peer"),
        ]),
        Some("peer".into())
    );
    // An event that set the mark without a recorded session proves no author.
    assert_eq!(
        mark_author(vec![MarkStep {
            transition: Created,
            marked: true,
            session: None,
        }]),
        None
    );
}

/// On a real store, the lookup fetches and decodes only the item's first
/// event and its creations and revisions: the claims, renewals, notes and
/// gates recorded after the mark was set add events to the item's history
/// but none to what the lookup decodes. This counts decoded bodies, not
/// SQLite's own steps.
#[test]
fn the_mark_author_lookup_decodes_no_more_as_claims_notes_and_gates_accumulate() {
    let mut fixture = fixture("mark-author-decodes");
    let store = &mut fixture.store;
    let mut create = root_request("mark-author-decodes", "create-marked-peer", 4);
    create.evaluation_mode = Some(Mode::SameSession);
    create.actor = actor("peer");
    let marked = store
        .create_work(&create, &DevelopmentNoopRedactor)
        .expect("a peer marks it at creation");
    let held = claim(store, &marked, "runner", "claim-marked", 5, 3_600);
    let marked = store.get_work_item(marked.work_id).expect("claimed item");
    let lookup = |store: &SqliteStore, item: &WorkItem| {
        reset_work_event_decode_count();
        let author = same_session_mark_author(&store.connection, item).expect("mark author");
        (author, work_event_decode_count())
    };
    let history = |store: &SqliteStore| {
        crate::storage::work::canonical_work_events_for_item(&store.connection, marked.work_id)
            .expect("history")
            .len()
    };
    let (author, decoded) = lookup(store, &marked);
    assert_eq!(author, Some(SessionId("peer".into())));
    let events_before = history(store);
    for round in 0..6 {
        let second = 6 + round * 3;
        evidence(
            store,
            &marked,
            &held,
            "runner",
            &format!("note-{round}"),
            second,
        );
        gate(
            store,
            &marked,
            &held,
            "runner",
            &format!("gate-{round}"),
            &[],
            second + 1,
        );
        claim(
            store,
            &marked,
            "runner",
            &format!("renew-{round}"),
            second + 2,
            3_600,
        );
    }
    let marked = store
        .get_work_item(marked.work_id)
        .expect("item after the history");
    assert!(history(store) > events_before, "the history grew");
    assert_eq!(
        lookup(store, &marked),
        (Some(SessionId("peer".into())), decoded)
    );
}
