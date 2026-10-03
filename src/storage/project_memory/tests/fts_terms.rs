//! Memory search builds its query terms the way the full-text index split
//! the memory's text, so text holding combining marks or private-use
//! characters is found by its own words, and doctor keeps passing such a
//! correctly indexed memory.

use super::*;

/// Memory texts whose characters a letter-or-digit split would cut
/// differently from the index's tokenizer, each with the words a search for
/// it uses.
const MARKED: [(&str, &str, &str); 6] = [
    (
        "standalone U+0345",
        "mark \u{0345} here",
        "mark \u{0345} here",
    ),
    (
        "standalone U+05B0",
        "mark \u{05B0} here",
        "mark \u{05B0} here",
    ),
    (
        "standalone U+093E",
        "mark \u{093E} here",
        "mark \u{093E} here",
    ),
    (
        "a mark after a letter",
        "\u{0915}\u{093E}",
        "\u{0915}\u{093E}",
    ),
    ("decomposed accent", "word a\u{0301}b", "a\u{0301}b"),
    ("private use", "word a\u{E000}b", "a\u{E000}b"),
];

fn store_with(body: &str) -> (SqliteStore, ProjectId, SessionId) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-fts-terms".into());
    let session = SessionId("fts-terms-session".into());
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("marked"),
                body,
                1_700_000_000_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember");
    (store, project, session)
}

#[test]
fn a_memory_is_found_by_its_own_marked_words() {
    let mut missed = Vec::new();
    for (case, body, query) in MARKED {
        let (store, project, session) = store_with(body);
        let found = store
            .project_memories(&project, &session, &actor(&session.0), Some(query), None)
            .expect("search");
        if found
            .memories
            .iter()
            .map(|memory| memory.key.as_str())
            .ne(["marked"])
        {
            missed.push(case);
        }
    }
    assert!(missed.is_empty(), "missed: {missed:?}");
}

#[test]
fn doctor_passes_a_correctly_indexed_marked_memory_and_still_reports_damage() {
    for (case, body, _) in MARKED {
        let (store, _, _) = store_with(body);
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{case}: {report:?}");
        let head: String = store
            .connection
            .query_row("SELECT version_id FROM memory_heads", [], |row| row.get(0))
            .expect("head");
        store.connection.execute_batch("SAVEPOINT damage").unwrap();
        store
            .connection
            .execute("DELETE FROM object_fts WHERE object_id = ?1", [&head])
            .expect("remove the row");
        let invalid = store.verify_all().expect("doctor").invalid_objects;
        store
            .connection
            .execute_batch("ROLLBACK TO damage; RELEASE damage")
            .unwrap();
        assert!(
            invalid.contains(&format!("object_fts:{head}:projection_binding")),
            "{case}: {invalid:?}"
        );
    }
}
