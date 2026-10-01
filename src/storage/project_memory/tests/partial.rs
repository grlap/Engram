//! Partial revises: an append or one marked section, built from the revision
//! they name inside the writer transaction, with the change they made.

use super::*;
use crate::domain::ProjectMemoryEdit;

const PROJECT: &str = "partial-memory";
const SESSION: &str = "partial-memory-session";

fn store() -> SqliteStore {
    SqliteStore::open_in_memory().expect("store")
}

fn create(store: &mut SqliteStore, key: &str, body: &str) {
    store
        .remember_project_memory(
            &project_memory_request(PROJECT, SESSION, Some(key), body, 1_000),
            &DevelopmentNoopRedactor,
        )
        .expect("create memory");
}

fn revise(
    store: &mut SqliteStore,
    key: &str,
    basis: Option<u64>,
    edit: &ProjectMemoryEdit,
    text: &str,
    at_ms: i64,
) -> Result<ProjectMemoryMutationReceipt, StoreError> {
    let mut request = project_memory_request(PROJECT, SESSION, Some(key), text, at_ms);
    request.revise = true;
    request.expected_revision = basis;
    store.remember_project_memory_edit_with_admission(
        &request,
        edit,
        &DevelopmentNoopRedactor,
        admit_project_memory_full,
    )
}

fn body(store: &SqliteStore, key: &str, revision: Option<u64>) -> (u64, String) {
    let full = store
        .project_memory_full(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            key,
            revision,
        )
        .expect("full read");
    (full.current_revision, full.body)
}

fn section(name: &str) -> ProjectMemoryEdit {
    ProjectMemoryEdit::Section { name: name.into() }
}

// An append adds a paragraph to the revision it names and keeps every byte
// of it; the receipt shows only what was added. An exact retry replays, even
// after a later revision, so nothing is appended twice; a different edit at
// the same basis conflicts and writes nothing.
#[test]
fn an_append_builds_on_its_basis_and_replays_instead_of_appending_twice() {
    let mut store = store();
    create(&mut store, "notes", "First paragraph.");
    let appended = revise(
        &mut store,
        "notes",
        Some(1),
        &ProjectMemoryEdit::Append,
        "Second.",
        2_000,
    )
    .expect("append");
    assert_eq!((appended.revision, appended.duplicate), (2, false));
    assert_eq!(body(&store, "notes", None).1, "First paragraph.\n\nSecond.");
    let change = appended.change.clone().expect("change");
    assert_eq!(change.edit, "append");
    assert_eq!(
        (change.removed_bytes, change.added.as_str()),
        (0, "\n\nSecond.")
    );
    assert_eq!((change.before_bytes, change.after_bytes), (16, 25));

    let retried = revise(
        &mut store,
        "notes",
        Some(1),
        &ProjectMemoryEdit::Append,
        "Second.",
        2_500,
    )
    .expect("exact retry");
    assert_eq!((retried.revision, retried.duplicate), (2, true));
    assert_eq!(retried.change, appended.change);

    revise(
        &mut store,
        "notes",
        None,
        &ProjectMemoryEdit::Whole,
        "Rewritten.",
        3_000,
    )
    .expect("a later whole revise");
    let late_retry = revise(
        &mut store,
        "notes",
        Some(1),
        &ProjectMemoryEdit::Append,
        "Second.",
        3_500,
    )
    .expect("exact retry after a later revision");
    assert_eq!((late_retry.revision, late_retry.duplicate), (2, true));
    // Its change is read from revisions 1 and 2, not from the current head.
    assert_eq!(late_retry.change, appended.change);

    let competing = revise(
        &mut store,
        "notes",
        Some(1),
        &ProjectMemoryEdit::Append,
        "Other.",
        4_000,
    );
    assert!(
        matches!(
            competing,
            Err(StoreError::ProjectMemoryRevisionConflict {
                expected: 1,
                current: 3,
                ..
            })
        ),
        "{competing:?}"
    );
    assert_eq!(body(&store, "notes", None), (3, "Rewritten.".to_owned()));
}

// A section edit replaces only the interior of the named section; the
// markers and every byte outside them stay, CRLF included.
#[test]
fn a_section_edit_replaces_only_that_section() {
    let mut store = store();
    let original = "Intro\r\n<!-- engram-section status -->\r\nold\r\n<!-- /engram-section status -->\r\nTail é";
    create(&mut store, "plan", original);
    let edited = revise(
        &mut store,
        "plan",
        Some(1),
        &section("status"),
        "new line",
        2_000,
    )
    .expect("section edit");
    assert_eq!(
        body(&store, "plan", None).1,
        "Intro\r\n<!-- engram-section status -->\r\nnew line\r\n<!-- /engram-section status -->\r\nTail é"
    );
    let change = edited.change.expect("change");
    assert_eq!(change.section.as_deref(), Some("status"));
    assert_eq!(
        (change.removed.as_str(), change.added.as_str()),
        ("old", "new line")
    );

    // A first-time section is added with an append and its markers.
    revise(
        &mut store,
        "plan",
        Some(2),
        &ProjectMemoryEdit::Append,
        "<!-- engram-section risks -->\r\nnone yet\r\n<!-- /engram-section risks -->",
        3_000,
    )
    .expect("append a section");
    revise(&mut store, "plan", Some(3), &section("risks"), "one", 4_000)
        .expect("edit the new section");
    assert!(
        body(&store, "plan", None)
            .1
            .contains("<!-- engram-section risks -->\r\none\r\n<!-- /engram-section risks -->")
    );
}

// Each refusal writes nothing: a missing section names the sections there
// are, and markers must pair, never nest, and never arrive in replacement
// text; a partial edit needs its basis, which must exist; the assembled body
// is held to the size limit; a retired key stays retired.
#[test]
fn partial_edits_refuse_before_writing() {
    let mut store = store();
    create(
        &mut store,
        "plan",
        "<!-- engram-section a -->\nx\n<!-- /engram-section a -->",
    );
    let refusals: Vec<(Result<ProjectMemoryMutationReceipt, StoreError>, &str)> = vec![
        (
            revise(&mut store, "plan", Some(1), &section("b"), "y", 2_000),
            "missing",
        ),
        (
            revise(
                &mut store,
                "plan",
                Some(1),
                &section("Bad Name"),
                "y",
                2_000,
            ),
            "name",
        ),
        (
            revise(
                &mut store,
                "plan",
                Some(1),
                &section("a"),
                "<!-- /engram-section a -->",
                2_000,
            ),
            "markers in text",
        ),
        (
            revise(
                &mut store,
                "plan",
                Some(1),
                &ProjectMemoryEdit::Append,
                "<!-- engram-section c -->",
                2_000,
            ),
            "unclosed",
        ),
        (
            revise(
                &mut store,
                "plan",
                None,
                &ProjectMemoryEdit::Append,
                "y",
                2_000,
            ),
            "no basis",
        ),
        (
            revise(
                &mut store,
                "plan",
                Some(5),
                &ProjectMemoryEdit::Append,
                "y",
                2_000,
            ),
            "no such basis",
        ),
        (
            revise(
                &mut store,
                "plan",
                Some(1),
                &ProjectMemoryEdit::Append,
                &"z".repeat(8_190),
                2_000,
            ),
            "oversize",
        ),
    ];
    for (result, case) in refusals {
        match (case, &result) {
            ("missing", Err(StoreError::ProjectMemorySectionNotFound(missing))) => {
                assert_eq!(missing.sections, vec!["a".to_owned()]);
                assert_eq!((missing.section.as_str(), missing.revision), ("b", 1));
            }
            (
                "no such basis",
                Err(StoreError::ProjectMemoryRevisionNotFound {
                    revision: 5,
                    current: 1,
                    ..
                }),
            )
            | (_, Err(StoreError::InvalidProjectMemory(_))) => {}
            _ => panic!("{case}: {result:?}"),
        }
    }
    assert_eq!(body(&store, "plan", None).0, 1, "nothing was written");

    let mut request = forget_request(PROJECT, SESSION, "plan", 3_000);
    request.actor = actor(SESSION);
    store
        .forget_project_memory(&request, &DevelopmentNoopRedactor)
        .expect("forget");
    let retired = revise(
        &mut store,
        "plan",
        Some(1),
        &ProjectMemoryEdit::Append,
        "y",
        4_000,
    );
    assert!(
        matches!(retired, Err(StoreError::ProjectMemoryRetired(_))),
        "{retired:?}"
    );
}

// A whole-body revise keeps working as before, and its receipt now shows the
// change too.
#[test]
fn a_whole_revise_is_unchanged_and_shows_its_change() {
    let mut store = store();
    create(&mut store, "plain", "alpha one");
    let revised = revise(
        &mut store,
        "plain",
        None,
        &ProjectMemoryEdit::Whole,
        "alpha two",
        2_000,
    )
    .expect("whole revise");
    assert_eq!(revised.revision, 2);
    assert_eq!(body(&store, "plain", None).1, "alpha two");
    // The shortest span that differs, between the common prefix and suffix.
    let change = revised.change.expect("change");
    assert_eq!(change.edit, "whole");
    assert_eq!(
        (
            change.span_start,
            change.removed.as_str(),
            change.added.as_str()
        ),
        (6, "one", "two")
    );
}

// Section names are 1 to 64 bytes, and a refusal never echoes a longer one;
// an edit that cannot be built on a stale basis is a conflict; an append is
// not blocked by an unpaired marker the basis only quotes; and a section can
// be cleared.
#[test]
fn partial_edits_bound_names_report_stale_bases_and_clear_sections() {
    let mut store = store();
    create(
        &mut store,
        "plan",
        "<!-- engram-section a -->\nx\n<!-- /engram-section a -->",
    );
    let longest = "n".repeat(64);
    let at_limit = revise(&mut store, "plan", Some(1), &section(&longest), "y", 2_000);
    assert!(
        matches!(&at_limit, Err(StoreError::ProjectMemorySectionNotFound(missing)) if missing.section == longest),
        "a 64-byte name is a name: {at_limit:?}"
    );
    for name in ["n".repeat(65), "n".repeat(20_000)] {
        let refused = revise(&mut store, "plan", Some(1), &section(&name), "y", 2_000)
            .expect_err("an over-long name");
        let message = refused.to_string();
        assert!(
            matches!(refused, StoreError::InvalidProjectMemory(_)),
            "{message}"
        );
        assert!(message.len() < 200 && !message.contains(&name), "{message}");
    }

    // Revision 2 moves the head; an edit on revision 1 that cannot be built
    // there is reported as the stale edit it is.
    revise(&mut store, "plan", Some(1), &section("a"), "z", 3_000).expect("edit a");
    let stale = revise(&mut store, "plan", Some(1), &section("missing"), "y", 4_000);
    assert!(
        matches!(
            stale,
            Err(StoreError::ProjectMemoryRevisionConflict {
                expected: 1,
                current: 2,
                ..
            })
        ),
        "{stale:?}"
    );

    // Empty text clears the section and keeps its markers.
    revise(&mut store, "plan", Some(2), &section("a"), "", 5_000).expect("clear a");
    assert_eq!(
        body(&store, "plan", None).1,
        "<!-- engram-section a -->\n<!-- /engram-section a -->"
    );

    // A basis that quotes an opening marker without its pair still takes an
    // append; the appended text must pair its own markers.
    create(
        &mut store,
        "doc",
        "Example:\n<!-- engram-section example -->\nend of quote",
    );
    revise(
        &mut store,
        "doc",
        Some(1),
        &ProjectMemoryEdit::Append,
        "More prose.",
        6_000,
    )
    .expect("append to a basis that quotes a marker");
    let unpaired = revise(
        &mut store,
        "doc",
        Some(2),
        &ProjectMemoryEdit::Append,
        "<!-- engram-section new -->",
        7_000,
    );
    assert!(
        matches!(unpaired, Err(StoreError::InvalidProjectMemory(_))),
        "{unpaired:?}"
    );
}
