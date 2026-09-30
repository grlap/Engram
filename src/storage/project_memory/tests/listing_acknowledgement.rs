//! What records a context generation for a session: only a memories listing
//! that carries it. An advancing `next` records the memory position and
//! never a generation.

use super::*;

const PROJECT: &str = "project-memory-listing";
const SESSION: &str = "listing-session";

fn project() -> ProjectId {
    ProjectId(PROJECT.into())
}

fn session() -> SessionId {
    SessionId(SESSION.into())
}

fn store_with_one_memory() -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().expect("store");
    seed_project_memory(&mut store, PROJECT, "seed-session", "first");
    store
}

fn candidate(store: &SqliteStore, generation: Option<&str>) -> ProjectMemoryAdvertisement {
    candidate_for(store, &project(), &session(), generation)
}

fn candidate_for(
    store: &SqliteStore,
    project: &ProjectId,
    session: &SessionId,
    generation: Option<&str>,
) -> ProjectMemoryAdvertisement {
    store
        .project_memory_advertisement_candidate(project, session, generation)
        .expect("advertisement candidate")
}

/// Lists from the start with the generation and records the listing, as the
/// `memories` word does when it carries one.
fn list(store: &mut SqliteStore, generation: &str) {
    let cut = listing_cut(store);
    store
        .acknowledge_project_memory_listing(&project(), &session(), cut, generation)
        .expect("record the listing");
}

fn listing_cut(store: &SqliteStore) -> ProjectMemoryListingCut {
    store
        .project_memories_at_cut(&project(), &session(), &actor(SESSION), None, None, true)
        .expect("list memories")
        .1
        .expect("the listing that records reads the position")
}

fn advance(store: &mut SqliteStore, generation: Option<&str>) {
    let advertisement = candidate(store, generation);
    store
        .acknowledge_project_memory_advertisement(&project(), &session(), &advertisement)
        .expect("advancing next acknowledgement");
}

fn stored_row(store: &SqliteStore) -> Option<(i64, Option<String>)> {
    store
        .connection
        .query_row(
            "SELECT memory_position, context_generation_digest
             FROM project_memory_advertisements
             WHERE project_id = ?1 AND session_id = ?2",
            params![PROJECT, SESSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("read the session's row")
}

#[test]
fn a_supplied_generation_is_unlisted_until_a_listing_carries_it() {
    let mut store = store_with_one_memory();
    // No row: a supplied generation is unlisted, a call without one is not.
    let first = candidate(&store, Some("termal-1"));
    assert!(first.generation_unlisted && first.changed);
    let plain = candidate(&store, None);
    assert!(!plain.generation_unlisted && plain.changed);

    list(&mut store, "termal-1");

    let listed = candidate(&store, Some("termal-1"));
    assert!(!listed.generation_unlisted && !listed.changed);
    let plain = candidate(&store, None);
    assert!(!plain.generation_unlisted && !plain.changed);
}

#[test]
fn a_changed_generation_is_unlisted_until_a_listing_carries_it() {
    let mut store = store_with_one_memory();
    list(&mut store, "termal-1");

    let changed = candidate(&store, Some("termal-2"));
    assert!(changed.generation_unlisted && changed.changed);

    list(&mut store, "termal-2");

    let listed = candidate(&store, Some("termal-2"));
    assert!(!listed.generation_unlisted && !listed.changed);
    assert!(
        candidate(&store, Some("termal-1")).generation_unlisted,
        "the earlier generation is no longer the recorded one"
    );
}

#[test]
fn an_advancing_next_records_the_position_and_never_a_generation() {
    let mut store = store_with_one_memory();
    // With no row it creates one, as before, without a generation.
    advance(&mut store, Some("termal-1"));
    let (position, digest) = stored_row(&store).expect("row");
    assert_eq!(digest, None);
    assert!(!candidate(&store, None).changed);
    assert!(candidate(&store, Some("termal-1")).generation_unlisted);

    // With a recorded generation it keeps that one and moves the position.
    list(&mut store, "termal-1");
    let (_, listed_digest) = stored_row(&store).expect("row");
    seed_project_memory(&mut store, PROJECT, "peer-session", "second");
    let moved = candidate(&store, Some("termal-2"));
    assert!(moved.changed && moved.generation_unlisted);
    advance(&mut store, Some("termal-2"));
    let (moved_position, kept_digest) = stored_row(&store).expect("row");
    assert!(moved_position > position);
    assert_eq!(kept_digest, listed_digest);
    assert!(candidate(&store, Some("termal-2")).generation_unlisted);
    assert!(!candidate(&store, Some("termal-1")).generation_unlisted);
    assert!(!candidate(&store, None).changed);
}

#[test]
fn a_generation_supplied_to_an_advancing_next_keeps_the_signal_changed_until_listed() {
    let mut store = store_with_one_memory();
    for _ in 0..2 {
        advance(&mut store, Some("termal-1"));
        assert!(candidate(&store, Some("termal-1")).changed);
    }
    list(&mut store, "termal-1");
    assert!(!candidate(&store, Some("termal-1")).changed);
}

#[test]
fn the_generation_is_stored_as_a_digest() {
    let mut store = store_with_one_memory();
    list(&mut store, "termal-1");
    let digest = stored_row(&store)
        .expect("row")
        .1
        .expect("stored generation");
    assert_eq!(digest.len(), 64);
    assert_ne!(digest, "termal-1");
}

#[test]
fn a_listing_that_repeats_what_is_recorded_writes_nothing() {
    let mut store = store_with_one_memory();
    list(&mut store, "termal-1");
    let before = store.connection.total_changes();
    list(&mut store, "termal-1");
    assert_eq!(
        store.connection.total_changes(),
        before,
        "a repeated listing must not acquire the SQLite write lock"
    );
    list(&mut store, "termal-2");
    assert!(store.connection.total_changes() > before);
}

#[test]
fn a_listing_records_the_position_its_own_snapshot_read() {
    let mut store = store_with_one_memory();
    let cut = listing_cut(&store);
    // A peer writes between the listing's snapshot and its record.
    seed_project_memory(&mut store, PROJECT, "peer-session", "second");
    store
        .acknowledge_project_memory_listing(&project(), &session(), cut, "termal-1")
        .expect("record the listing");

    let after = candidate(&store, Some("termal-1"));
    assert!(
        after.changed,
        "the memory the listing did not show is announced again"
    );
    assert!(!after.generation_unlisted);
}

#[test]
fn a_recorded_listing_belongs_to_one_project_and_session() {
    let mut store = store_with_one_memory();
    seed_project_memory(&mut store, "another-project", "seed-session", "first");
    list(&mut store, "termal-1");

    let other_session = SessionId("another-session".into());
    assert!(
        candidate_for(&store, &project(), &other_session, Some("termal-1")).generation_unlisted
    );
    let other_project = ProjectId("another-project".into());
    assert!(
        candidate_for(&store, &other_project, &session(), Some("termal-1")).generation_unlisted
    );
    assert!(!candidate(&store, Some("termal-1")).generation_unlisted);
}

#[test]
fn a_generation_is_a_plain_token() {
    for valid in [
        "termal-7",
        "a.b_c-9",
        "X",
        &"x".repeat(MAX_CONTEXT_GENERATION_BYTES),
    ] {
        validate_context_generation(Some(valid)).expect("a plain token");
    }
    validate_context_generation(None).expect("no generation");
    // Anything a shell or the terminal rendering could change is refused, so
    // the command a peek prints always carries the supplied value exactly.
    for invalid in [
        "",
        "-leading",
        "two words",
        "it's",
        "quote\"d",
        "$(x)",
        "semi;colon",
        "back\\slash",
        "g\u{e000}",
        "g\u{202e}",
        "é",
        "line\nbreak",
        &"x".repeat(MAX_CONTEXT_GENERATION_BYTES + 1),
    ] {
        assert!(
            matches!(
                validate_context_generation(Some(invalid)),
                Err(StoreError::InvalidProjectMemory(message))
                    if message.contains("context_generation")
            ),
            "{invalid:?}"
        );
    }
}

#[test]
fn a_listing_record_refuses_an_invalid_generation_before_effects() {
    let mut store = store_with_one_memory();
    let cut = listing_cut(&store);
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    for invalid in ["", "two words", "g\u{e000}"] {
        assert!(matches!(
            store.acknowledge_project_memory_listing(&project(), &session(), cut, invalid),
            Err(StoreError::InvalidProjectMemory(message))
                if message.contains("context_generation")
        ));
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

#[test]
fn a_listing_record_refuses_an_oversized_session_before_effects() {
    for live in [ascii65_session(), utf8_oversized_session()] {
        let mut store = store_with_one_memory();
        let cut = listing_cut(&store);
        let before =
            crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
        let error = store
            .acknowledge_project_memory_listing(&project(), &live, cut, "termal-1")
            .expect_err("oversized listing session");
        assert_oversized_session_refusal(&error, &live);
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
            before
        );
    }
}

/// Deletes the project's memory position, the drift a repair rebuilds.
fn drop_memory_position(connection: &rusqlite::Connection) {
    connection
        .execute(
            "DELETE FROM project_memory_state WHERE project_id = ?1",
            [PROJECT],
        )
        .expect("delete the memory position");
}

// While the project's memory position is missing, a listing and a search
// answer from the memory rows as they always did and read no position; the
// listing that would record one answers too and records nothing.
#[test]
fn every_listing_answers_while_the_memory_position_is_missing() {
    let store = store_with_one_memory();
    drop_memory_position(&store.connection);
    let listed = |query: Option<&str>| {
        store
            .project_memories(&project(), &session(), &actor(SESSION), query, None)
            .expect("the listing answers")
            .memories
            .len()
    };
    assert_eq!(listed(None), 1);
    assert_eq!(listed(Some("first")), 1);
    let (list, listing) = store
        .project_memories_at_cut(&project(), &session(), &actor(SESSION), None, None, true)
        .expect("the recording form answers too");
    assert_eq!(list.memories.len(), 1);
    assert!(listing.is_none(), "no position to record");
    assert_eq!(stored_row(&store), None);
}

// The word: `memories`, `memories QUERY` and `memories --context-generation
// G` all return the row in that drift state, and nothing is recorded.
#[test]
fn the_memories_word_answers_while_the_memory_position_is_missing() {
    use crate::verbs::{AgentVerbs, MemoriesInput, RememberInput};
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let verbs = AgentVerbs::new(database.clone(), project(), "agent".into(), session(), None);
    verbs
        .remember(
            RememberInput {
                revise: false,
                expected_revision: None,
                retires_with: None,
                clear_retires_with: false,
                text: "a retained note".into(),
                key: Some("retained".into()),
            },
            chrono::Utc::now(),
        )
        .expect("remember");
    let store = SqliteStore::open(&database).expect("store");
    drop_memory_position(&store.connection);
    for (query, context_generation) in [
        (None, None),
        (Some("retained"), None),
        (None, Some("fresh-context")),
    ] {
        let receipt = verbs
            .memories(
                &MemoriesInput {
                    revision: None,
                    query: query.map(Into::into),
                    after: None,
                    full: false,
                    context_generation: context_generation.map(Into::into),
                },
                chrono::Utc::now(),
            )
            .unwrap_or_else(|error| panic!("{query:?} {context_generation:?}: {error}"));
        assert_eq!(
            receipt.value["memories"].as_array().map(Vec::len),
            Some(1),
            "{query:?} {context_generation:?}: {}",
            receipt.value
        );
    }
    assert_eq!(stored_row(&store), None, "nothing is recorded");
}
