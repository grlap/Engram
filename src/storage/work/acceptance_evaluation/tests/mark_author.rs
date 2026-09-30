//! Who set a task's same-session mark, read from the item's native events in
//! order: only a Created event, or a Revised event after a native event that
//! showed the item unmarked, proves who turned the mark on.

use super::super::{MarkStep, MarkTransition, mark_author};
use crate::domain::SessionId;

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
