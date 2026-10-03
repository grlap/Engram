use super::*;

fn item(short_ref: &str) -> (WorkId, String) {
    (WorkId::new(), short_ref.into())
}

fn binding(fence: i64) -> FocusBinding {
    FocusBinding {
        claim_id: WorkClaimId::new(),
        claim_fence: fence,
        workspace_id: Some("workspace-A".into()),
        generation: Some(3),
    }
}

fn project() -> ProjectId {
    ProjectId("focus-journal".into())
}

fn session() -> SessionId {
    SessionId("focus-session".into())
}

// The net change is the first origin and the last target, with the binding
// last captured for that target; a claim's later binding replaces the one
// the move saw, and an ended claim leaves none.
#[test]
fn the_net_change_keeps_the_first_origin_and_the_last_capture_for_the_target() {
    let (a, b) = (item("w-a"), item("w-b"));
    let claimed = binding(2);
    let journal = FocusJournal::begin(&project(), &session());
    record_move(&project(), &session(), Some(a.clone()), b.clone(), None);
    record_binding(&project(), &session(), b.0, Some(claimed.clone()));
    let change = journal.finish().expect("a move");
    assert_eq!(change.from.as_deref(), Some("w-a"));
    assert_eq!(change.to.as_deref(), Some("w-b"));
    assert_eq!(change.binding, Some(claimed));

    let journal = FocusJournal::begin(&project(), &session());
    record_move(&project(), &session(), None, b.clone(), Some(binding(1)));
    record_ended(&session(), b.0);
    let change = journal.finish().expect("a move");
    assert_eq!(change.from, None);
    assert_eq!(change.binding, None, "an ended claim promises no binding");

    // A binding captured for another item does not attach to the target.
    let journal = FocusJournal::begin(&project(), &session());
    record_move(&project(), &session(), Some(a.clone()), b.clone(), None);
    record_binding(&project(), &session(), a.0, Some(binding(4)));
    assert_eq!(journal.finish().expect("a move").binding, None);
}

// A word that ends where it started reports nothing, and so does one that
// never moved focus.
#[test]
fn a_round_trip_or_no_move_reports_nothing() {
    let (a, b) = (item("w-a"), item("w-b"));
    let journal = FocusJournal::begin(&project(), &session());
    record_move(&project(), &session(), Some(a.clone()), b.clone(), None);
    record_move(&project(), &session(), Some(b), a, None);
    assert_eq!(journal.finish(), None);

    let journal = FocusJournal::begin(&project(), &session());
    assert_eq!(journal.finish(), None);
}

// Events of another session or project, or with no journal installed, are
// ignored; a nested guard shares the outer journal, and dropping the owner
// clears it.
#[test]
fn only_the_installing_session_records_and_the_owner_clears_on_drop() {
    let (a, b) = (item("w-a"), item("w-b"));
    record_move(&project(), &session(), Some(a.clone()), b.clone(), None);
    assert!(!recording());

    let journal = FocusJournal::begin(&project(), &session());
    let nested = FocusJournal::begin(&project(), &session());
    record_move(
        &project(),
        &SessionId("another-session".into()),
        Some(a.clone()),
        b.clone(),
        None,
    );
    record_move(
        &ProjectId("another-project".into()),
        &session(),
        Some(a.clone()),
        b.clone(),
        None,
    );
    assert_eq!(nested.finish(), None, "a nested guard takes nothing");
    assert!(recording(), "the outer journal is still installed");
    record_move(&project(), &session(), Some(a), b, None);
    drop(journal);
    assert!(!recording(), "dropping the owner clears the journal");

    let unwound = std::panic::catch_unwind(|| {
        let _journal = FocusJournal::begin(&project(), &session());
        panic!("a word panics with its journal installed");
    });
    assert!(unwound.is_err());
    assert!(!recording(), "a panic clears the journal too");
}
