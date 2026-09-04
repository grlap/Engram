use chrono::{TimeZone, Utc};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

use super::*;
use crate::storage::{
    AssuranceLevel, MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS,
};
use crate::*;
use crate::{ProjectId, domain::ProvenanceLink};

fn admit_project_memory_full(full: &ProjectMemoryFull) -> Result<(), StoreError> {
    crate::work_service::project_memory_full_response(full.clone()).map(drop)
}

fn actor(session: &str) -> ActorContext {
    ActorContext {
        actor_id: "project-memory-agent".into(),
        actor_kind: "agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId(session.into())),
        source_tool: Some("remember".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "project memory test".into(),
    }
}

fn project_context_revision(store: &SqliteStore, project: &ProjectId) -> i64 {
    store
        .connection
        .query_row(
            "SELECT COALESCE((
                 SELECT revision FROM project_context_revisions WHERE project_id = ?1
             ), 0)",
            [project.0.as_str()],
            |row| row.get(0),
        )
        .expect("project context revision")
}

fn project_memory_request(
    project: &str,
    session: &str,
    key: Option<&str>,
    body: &str,
    at_ms: i64,
) -> RememberProjectMemoryRequest {
    RememberProjectMemoryRequest {
        project_id: ProjectId(project.into()),
        session_id: SessionId(session.into()),
        key: key.map(str::to_owned),
        body: body.into(),
        actor: actor(session),
        created_at: Utc.timestamp_millis_opt(at_ms).unwrap(),
    }
}

struct RejectingRedactor;

impl Redactor for RejectingRedactor {
    fn inspect(&self, _prose: &str) -> Result<(), String> {
        Err("project-memory test refusal".into())
    }

    fn description(&self) -> &'static str {
        "project-memory rejecting test redactor"
    }
}

struct MatchingRedactor(&'static str);

impl Redactor for MatchingRedactor {
    fn inspect(&self, prose: &str) -> Result<(), String> {
        if prose == self.0 {
            Err("project-memory attribution refusal".into())
        } else {
            Ok(())
        }
    }

    fn description(&self) -> &'static str {
        "project-memory matching test redactor"
    }
}

fn assert_project_memory_advertisement_contract(
    store: &mut SqliteStore,
    project: &ProjectId,
    session: &SessionId,
) {
    let (count, changed) = store
        .project_memory_advertisement(project, session, None)
        .expect("first signal");
    assert_eq!(count, 1);
    assert!(changed);
    let changes_before_stable_signal = store.connection.total_changes();
    assert!(
        !store
            .project_memory_advertisement(project, session, None)
            .expect("stable signal")
            .1
    );
    assert_eq!(
        store.connection.total_changes(),
        changes_before_stable_signal,
        "an unchanged signal must not acquire the SQLite write lock"
    );
    SqliteStore::bump_project_context_revision_on(&store.connection, project)
        .expect("unrelated project-context change");
    assert!(
        !store
            .project_memory_advertisement(project, session, None)
            .expect("project-memory signal ignores unrelated context changes")
            .1
    );
    let omitted = store
        .project_memory_advertisement_candidate(project, session, Some("fresh-context"))
        .expect("unacknowledged fresh context signal");
    assert!(omitted.changed);
    assert!(
        store
            .project_memory_advertisement_candidate(project, session, Some("fresh-context"))
            .expect("omitted signal reannounces")
            .changed
    );
    store
        .acknowledge_project_memory_advertisement(project, session, &omitted)
        .expect("acknowledge delivered signal");
    let stored_generation_digest = store
        .connection
        .query_row(
            "SELECT context_generation_digest FROM project_memory_advertisements
             WHERE project_id = ?1 AND session_id = ?2",
            params![project.0, session.0],
            |row| row.get::<_, String>(0),
        )
        .expect("stored context-generation digest");
    assert_eq!(stored_generation_digest.len(), 64);
    assert_ne!(stored_generation_digest, "fresh-context");
    assert!(
        !store
            .project_memory_advertisement_candidate(project, session, Some("fresh-context"))
            .expect("acknowledged signal stays quiet")
            .changed
    );
}

#[test]
fn project_memory_advertisement_bookkeeping_is_bounded_per_project() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-advertisement-bound".into());
    let current_session = SessionId("current-session".into());
    let current_advertisement = ProjectMemoryAdvertisement {
        count: 0,
        changed: true,
        change_position: 0,
        context_generation_digest: None,
    };
    store
        .acknowledge_project_memory_advertisement(
            &project,
            &current_session,
            &current_advertisement,
        )
        .expect("seed established session acknowledgement");
    store
        .connection
        .execute(
            "WITH RECURSIVE sessions(value) AS (
                 VALUES(0)
                 UNION ALL
                 SELECT value + 1 FROM sessions
                 WHERE value < ?2
             )
             INSERT INTO project_memory_advertisements (
                 project_id, session_id, context_generation_digest, memory_position
             )
             SELECT ?1, printf('session-%04d', value), NULL, 0 FROM sessions",
            params![project.0, MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT + 8],
        )
        .expect("seed stale advertisement rows");
    store
        .acknowledge_project_memory_advertisement(
            &project,
            &current_session,
            &current_advertisement,
        )
        .expect("refresh established session and prune");
    let retained = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM project_memory_advertisements WHERE project_id = ?1",
            [project.0.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count bounded advertisements");
    assert_eq!(retained, MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT);
    let current_is_newest = store
        .connection
        .query_row(
            "SELECT rowid = (
                 SELECT MAX(rowid) FROM project_memory_advertisements WHERE project_id = ?1
             )
             FROM project_memory_advertisements
             WHERE project_id = ?1 AND session_id = ?2",
            params![project.0, current_session.0],
            |row| row.get::<_, bool>(0),
        )
        .expect("refreshed session receives the newest acknowledgement position");
    assert!(current_is_newest);
    assert!(
        !store
            .project_memory_advertisement_candidate(&project, &current_session, None)
            .expect("refreshed established session stays acknowledged")
            .changed
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle scenario pins create, replay, search visibility, forget, and advisory behavior"
)]
fn project_memory_create_refuse_read_forget_and_advertise_are_typed() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-lifecycle".into());
    let session = SessionId("memory-session".into());
    let request = project_memory_request(
        &project.0,
        &session.0,
        None,
        "Alpha beta\nfull project observation",
        1_700_000_000_000,
    );
    let context_revision_before_remember = project_context_revision(&store, &project);
    let created = store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .expect("remember");
    assert_eq!(created.key, "alpha-beta-full-project-observation");
    assert!(!created.duplicate);
    assert_eq!(
        project_context_revision(&store, &project),
        context_revision_before_remember,
        "dedicated project memories do not invalidate generic context packets"
    );
    assert!(
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .expect("exact replay")
            .duplicate
    );
    let mut changed_context = request.clone();
    changed_context.actor.provenance_chain.push(ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: "model=changed-after-lost-response".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    assert!(
        store
            .remember_project_memory(&changed_context, &DevelopmentNoopRedactor)
            .expect("attribution-only context change replays the original memory")
            .duplicate
    );

    let collision = project_memory_request(
        &project.0,
        &session.0,
        Some(&created.key),
        "different body",
        1_700_000_001_000,
    );
    assert!(matches!(
        store.remember_project_memory(&collision, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryExists(key)) if key == created.key
    ));
    let full = store
        .project_memory_full(&project, &session, &actor(&session.0), &created.key)
        .expect("full read");
    assert_eq!(full.body, request.body);
    let list = store
        .project_memories(&project, &session, &actor(&session.0), None, None)
        .expect("list");
    assert_eq!(list.memories.len(), 1);
    assert_eq!(list.memories[0].first_line, "Alpha beta");
    assert!(
        store
            .search_memories(
                &project,
                None,
                None,
                &session,
                "project-memory-agent",
                Some("observation"),
                20,
            )
            .expect("generic search excludes dedicated project memories")
            .is_empty()
    );
    let context = store
        .build_context(
            &project,
            None,
            &session,
            "project-memory-agent",
            Utc.timestamp_millis_opt(1_700_000_001_500).unwrap(),
        )
        .expect("project memories stay out of generic context packets");
    assert!(context.pinned.is_empty());
    assert!(context.index.is_empty());
    assert!(context.omissions.is_empty());
    assert!(context.omission_summaries.is_empty());

    assert_project_memory_advertisement_contract(&mut store, &project, &session);
    let context_revision_before_forget = project_context_revision(&store, &project);

    assert!(matches!(
        store.forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: session.clone(),
                key: created.key.clone(),
                actor: actor(&session.0),
                created_at: Utc.timestamp_millis_opt(1_699_999_999_999).unwrap(),
            },
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidProjectMemory(message))
            if message.contains("precedes the remembered timestamp")
    ));
    assert_eq!(
        store
            .project_memory_full(&project, &session, &actor(&session.0), &created.key)
            .expect("clock-skew refusal leaves memory active")
            .body,
        request.body
    );

    let forgotten = store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: session.clone(),
                key: created.key.clone(),
                actor: actor(&session.0),
                created_at: Utc.timestamp_millis_opt(1_700_000_002_000).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("forget");
    assert!(!forgotten.duplicate);
    assert!(
        store
            .forget_project_memory(
                &ForgetProjectMemoryRequest {
                    project_id: project.clone(),
                    session_id: session.clone(),
                    key: created.key.clone(),
                    actor: actor(&session.0),
                    created_at: Utc.timestamp_millis_opt(1_700_000_003_000).unwrap(),
                },
                &DevelopmentNoopRedactor
            )
            .expect("forget replay")
            .duplicate
    );
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), &created.key),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
    assert!(matches!(
        store.remember_project_memory(&request, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
    assert!(
        store
            .search_memories(
                &project,
                None,
                None,
                &session,
                "project-memory-agent",
                Some("observation"),
                20,
            )
            .expect("forgotten project memory is excluded from generic search")
            .is_empty()
    );
    assert!(
        store
            .project_memories(&project, &session, &actor(&session.0), None, None)
            .expect("forgotten project memory is excluded from dedicated listing")
            .memories
            .is_empty()
    );
    assert_eq!(
        project_context_revision(&store, &project),
        context_revision_before_forget,
        "forget does not invalidate generic context packets"
    );
    assert_eq!(
        store
            .project_memory_advertisement(&project, &session, None)
            .expect("forget signal"),
        (0, true)
    );
}

#[test]
fn keyed_project_memories_refuse_contradiction_lifecycle_transitions() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-contradiction".into());
    let session = SessionId("memory-contradiction-session".into());
    for (key, body, at_ms) in [
        ("first-key", "first retained statement", 1_700_000_000_000),
        ("second-key", "second retained statement", 1_700_000_000_001),
    ] {
        store
            .remember_project_memory(
                &project_memory_request(&project.0, &session.0, Some(key), body, at_ms),
                &DevelopmentNoopRedactor,
            )
            .expect("remember contradiction fixture");
    }
    let first = lookup_project_memory_on(&store.connection, &project, "first-key")
        .expect("lookup first")
        .expect("first exists");
    let second = lookup_project_memory_on(&store.connection, &project, "second-key")
        .expect("lookup second")
        .expect("second exists");
    assert!(matches!(
        store.record_memory_contradiction(
            &project,
            None,
            None,
            &session,
            "project-memory-agent",
            &first.version_hash,
            &second.version_hash,
            "these statements conflict",
            "project-memory-contradiction",
            actor(&session.0),
            Utc.timestamp_millis_opt(1_700_000_000_002).unwrap(),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidContradiction(detail))
            if detail.contains("cannot be contradiction endpoints")
    ));
    let listed = store
        .project_memories(&project, &session, &actor(&session.0), None, None)
        .expect("list after refused contradiction");
    assert_eq!(listed.memories.len(), 2);
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: project,
                session_id: session.clone(),
                key: "first-key".into(),
                actor: actor(&session.0),
                created_at: Utc.timestamp_millis_opt(1_700_000_000_003).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("forget remains available");
}

#[test]
fn project_memory_listing_continues_by_safe_key_and_search_is_bounded() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-listing".into());
    let session = SessionId("memory-list-session".into());
    for index in 0..25 {
        let request = project_memory_request(
            &project.0,
            &session.0,
            Some(&format!("memory-{index:02}")),
            &format!("needle observation {index}"),
            1_700_000_000_000 + index,
        );
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .expect("remember fixture");
    }
    let first = store
        .project_memories(&project, &session, &actor(&session.0), None, None)
        .expect("first page");
    assert_eq!(first.memories.len(), PROJECT_MEMORY_LIST_LIMIT);
    assert_eq!(first.next_after.as_deref(), Some("memory-19"));
    let second = store
        .project_memories(
            &project,
            &session,
            &actor(&session.0),
            None,
            first.next_after.as_deref(),
        )
        .expect("second page");
    assert_eq!(second.memories.len(), 5);
    assert!(second.exhausted);
    assert!(second.next_after.is_none());

    let filtered = store
        .project_memories(&project, &session, &actor(&session.0), Some("needle"), None)
        .expect("filtered");
    assert_eq!(filtered.memories.len(), PROJECT_MEMORY_LIST_LIMIT);
    assert!(filtered.next_after.is_none());
    assert_eq!(filtered.omitted_count, 5);
    assert!(!filtered.exhausted);

    let complete = store
        .project_memories(
            &project,
            &session,
            &actor(&session.0),
            Some("observation 24"),
            None,
        )
        .expect("complete filtered result");
    assert_eq!(complete.memories.len(), 1);
    assert_eq!(complete.omitted_count, 0);
    assert!(complete.exhausted);

    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("my_key"),
                "ranking exact key",
                1_700_000_000_100,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember exact ranking key");
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("aaa-my_key"),
                "ranking prefix key",
                1_700_000_000_101,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember competing ranking key");
    let ranked = store
        .project_memories(&project, &session, &actor(&session.0), Some("my_key"), None)
        .expect("rank exact underscore key");
    assert_eq!(ranked.memories[0].key, "my_key");
    assert!(matches!(
        store.project_memories(
            &project,
            &session,
            &actor(&session.0),
            Some("needle"),
            Some("memory-00"),
        ),
        Err(StoreError::InvalidProjectMemory(message)) if message.contains("does not accept --after")
    ));
}

#[test]
fn project_memory_filtered_search_ranks_relevance_before_key_order() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-relevance".into());
    let session = SessionId("memory-relevance-session".into());
    for index in 0..PROJECT_MEMORY_LIST_LIMIT {
        store
            .remember_project_memory(
                &project_memory_request(
                    &project.0,
                    &session.0,
                    Some(&format!("aaa-{index:02}")),
                    "needle observation",
                    1_700_000_000_000 + i64::try_from(index).expect("fixture index"),
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("remember alphabetical fixture");
    }
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("zzz-most-relevant"),
                "needle needle needle needle needle",
                1_700_000_000_100,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember relevance fixture");

    let ranked = store
        .project_memories(&project, &session, &actor(&session.0), Some("needle"), None)
        .expect("ranked filtered result");
    assert_eq!(ranked.memories.len(), PROJECT_MEMORY_LIST_LIMIT);
    assert_eq!(ranked.memories[0].key, "zzz-most-relevant");
    assert_eq!(ranked.omitted_count, 1);
}

#[test]
fn project_memory_search_uses_unicode_case_folding() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-unicode-search".into());
    let session = SessionId("unicode-search-session".into());
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("unicode-memory"),
                "ÖSTERREICH project observation",
                1_700_000_000_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember Unicode search fixture");
    let unicode = store
        .project_memories(
            &project,
            &session,
            &actor(&session.0),
            Some("österreich"),
            None,
        )
        .expect("Unicode case-insensitive memory search");
    assert_eq!(unicode.memories.len(), 1);
    assert_eq!(unicode.memories[0].key, "unicode-memory");
    assert!(unicode.exhausted);
}

#[test]
fn project_memory_search_query_is_bounded_before_fts() {
    let store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-query-bounds".into());
    let session = SessionId("memory-query-session".into());
    assert!(
        store
            .project_memories(
                &project,
                &session,
                &actor(&session.0),
                Some(&"x".repeat(MAX_PROJECT_MEMORY_QUERY_BYTES)),
                None,
            )
            .is_ok()
    );
    assert!(matches!(
        store.project_memories(
            &project,
            &session,
            &actor(&session.0),
            Some(&"x".repeat(MAX_PROJECT_MEMORY_QUERY_BYTES + 1)),
            None,
        ),
        Err(StoreError::InvalidProjectMemory(message)) if message.contains("UTF-8 bytes")
    ));
    let allowed_tokens = (0..MAX_PROJECT_MEMORY_QUERY_TOKENS)
        .map(|index| format!("t{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        store
            .project_memories(
                &project,
                &session,
                &actor(&session.0),
                Some(&allowed_tokens),
                None,
            )
            .is_ok()
    );
    let too_many_tokens = format!("{allowed_tokens} overflow");
    assert!(matches!(
        store.project_memories(
            &project,
            &session,
            &actor(&session.0),
            Some(&too_many_tokens),
            None,
        ),
        Err(StoreError::InvalidProjectMemory(message)) if message.contains("search tokens")
    ));
}

#[test]
fn terminal_project_memory_tombstone_dominates_projection_replay_order() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-terminal-rebuild".into());
    let session = SessionId("terminal-rebuild-session".into());
    let created_at = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let key = "terminal-rebuild";
    let request = project_memory_request(
        &project.0,
        &session.0,
        Some(key),
        "terminal rebuild observation",
        created_at.timestamp_millis(),
    );
    let prepared = prepare_project_memory(&request, key).expect("prepare memory");
    let tombstone = MemoryAssertionEvent {
        schema_version: SCHEMA_VERSION,
        memory_id: prepared.version.memory_id,
        version: prepared.version_object.hash().clone(),
        status: MemoryStatus::Tombstoned,
        policy_reason: "explicit project-memory forget".into(),
        actor: request.actor.clone(),
        created_at,
    };
    let tombstone_object = CanonicalObject::freeze(&tombstone).expect("freeze tombstone");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin fixture transaction");
    SqliteStore::insert_object(&transaction, "memory_version", &prepared.version_object)
        .expect("insert version");
    SqliteStore::insert_object(
        &transaction,
        "memory_assertion_event",
        &prepared.assertion_object,
    )
    .expect("insert active assertion");
    SqliteStore::insert_object(&transaction, "memory_assertion_event", &tombstone_object)
        .expect("insert tombstone assertion");
    SqliteStore::apply_memory_projection(
        &transaction,
        prepared.version_object.hash(),
        tombstone_object.hash(),
        &prepared.version,
        &tombstone,
        MemoryProjectionMode::Replay,
    )
    .expect("apply tombstone first");
    SqliteStore::apply_memory_projection(
        &transaction,
        prepared.version_object.hash(),
        prepared.assertion_object.hash(),
        &prepared.version,
        &prepared.assertion,
        MemoryProjectionMode::Replay,
    )
    .expect("later live replay cannot replace tombstone");
    transaction.commit().expect("commit fixture objects");
    let live_transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin refused live projection");
    assert!(matches!(
        SqliteStore::apply_memory_projection(
            &live_transaction,
            prepared.version_object.hash(),
            prepared.assertion_object.hash(),
            &prepared.version,
            &prepared.assertion,
            MemoryProjectionMode::Live,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("cannot replace terminal memory head")
    ));
    drop(live_transaction);
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), key),
        Err(StoreError::ProjectMemoryRetired(retired)) if retired == key
    ));
    assert!(
        store
            .search_memories(
                &project,
                None,
                None,
                &session,
                "project-memory-agent",
                Some("terminal rebuild"),
                20,
            )
            .expect("tombstoned memory stays hidden")
            .is_empty()
    );
}

#[test]
fn project_memory_size_and_binding_refuse_before_persistence() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let accepted = project_memory_request(
        "project-memory-bounds",
        "memory-bound-session",
        Some("plain-boundary"),
        &"x".repeat(MAX_PROJECT_MEMORY_BODY_BYTES),
        1_700_000_000_000,
    );
    store
        .remember_project_memory_with_admission(
            &accepted,
            &DevelopmentNoopRedactor,
            admit_project_memory_full,
        )
        .expect("plain 8 KiB body fits the envelope");
    let escaped = project_memory_request(
        "project-memory-bounds",
        "memory-bound-session",
        Some("escaped-boundary"),
        &"\"".repeat(MAX_PROJECT_MEMORY_BODY_BYTES),
        1_700_000_001_000,
    );
    assert!(matches!(
        store.remember_project_memory_with_admission(
            &escaped,
            &DevelopmentNoopRedactor,
            admit_project_memory_full,
        ),
        Err(StoreError::InvalidProjectMemory(message)) if message.contains("serialized full memory response")
    ));
    for key in [
        "Uppercase",
        "-leading",
        &"x".repeat(MAX_PROJECT_MEMORY_KEY_BYTES + 1),
    ] {
        let request = project_memory_request(
            "project-memory-refusals",
            "memory-refusal-session",
            Some(key),
            "body",
            1_700_000_003_000,
        );
        assert!(matches!(
            store.remember_project_memory(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidProjectMemory(message)) if message.contains("memory key")
        ));
    }
    let unsluggable = project_memory_request(
        "project-memory-refusals",
        "memory-refusal-session",
        None,
        "--- ...",
        1_700_000_004_000,
    );
    assert!(matches!(
        store.remember_project_memory(&unsluggable, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(message)) if message.contains("cannot produce a safe key")
    ));
    let rejected = project_memory_request(
        "project-memory-refusals",
        "memory-refusal-session",
        Some("redactor-refusal"),
        "body",
        1_700_000_005_000,
    );
    assert!(matches!(
        store.remember_project_memory(&rejected, &RejectingRedactor),
        Err(StoreError::RedactionRefused(message)) if message == "project-memory test refusal"
    ));
    let missing_project = ProjectId("project-memory-missing".into());
    let missing_session = SessionId("memory-missing-session".into());
    assert!(matches!(
        store.project_memory_full(
            &missing_project,
            &missing_session,
            &actor(&missing_session.0),
            "never-used",
        ),
        Err(StoreError::ProjectMemoryNotFound(key)) if key == "never-used"
    ));
}

#[test]
fn project_memory_binding_and_context_generation_refusals_are_bounded() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut unauthorized = project_memory_request(
        "project-memory-bounds",
        "memory-bound-session",
        Some("unauthorized"),
        "body",
        1_700_000_002_000,
    );
    unauthorized.actor.session_id = Some(SessionId("another-session".into()));
    assert!(matches!(
        store.remember_project_memory(&unauthorized, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryBindingInvalid)
    ));
    let mut blank_session = project_memory_request(
        "project-memory-bounds",
        "   ",
        Some("blank-session"),
        "body",
        1_700_000_002_500,
    );
    blank_session.actor.session_id = Some(SessionId("   ".into()));
    assert!(matches!(
        store.remember_project_memory(&blank_session, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryBindingInvalid)
    ));
    for invalid_generation in [
        "x".repeat(MAX_CONTEXT_GENERATION_BYTES + 1),
        "invalid\ngeneration".into(),
    ] {
        assert!(matches!(
            store.project_memory_advertisement_candidate(
                &ProjectId("project-memory-bounds".into()),
                &SessionId("memory-bound-session".into()),
                Some(&invalid_generation),
            ),
            Err(StoreError::InvalidProjectMemory(message))
                if message.contains("context_generation")
        ));
    }
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'memory_version'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count versions"),
        0
    );
}

#[test]
fn project_memory_attribution_is_bounded_and_redacted_on_create_and_forget() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-attribution".into());
    let session = SessionId("memory-attribution-session".into());

    let mut oversized = project_memory_request(
        &project.0,
        &session.0,
        Some("oversized-attribution"),
        "body",
        1_700_000_000_000,
    );
    oversized.actor.source_tool = Some("x".repeat(MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES + 1));
    assert!(matches!(
        store.remember_project_memory(&oversized, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(message))
            if message.contains("source tool")
    ));

    let mut too_many_links = project_memory_request(
        &project.0,
        &session.0,
        Some("too-many-links"),
        "body",
        1_700_000_000_001,
    );
    too_many_links.actor.provenance_chain = (0..=MAX_PROJECT_MEMORY_PROVENANCE_LINKS)
        .map(|index| ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::AssertedBy,
            source: format!("source-{index}"),
            reference: None,
        })
        .collect();
    assert!(matches!(
        store.remember_project_memory(&too_many_links, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(message))
            if message.contains("provenance links")
    ));

    let mut rejected_create = project_memory_request(
        &project.0,
        &session.0,
        Some("rejected-create"),
        "safe body",
        1_700_000_000_002,
    );
    rejected_create.actor.source_skill = Some("reject-attribution".into());
    assert!(matches!(
        store.remember_project_memory(
            &rejected_create,
            &MatchingRedactor("reject-attribution"),
        ),
        Err(StoreError::RedactionRefused(message))
            if message == "project-memory attribution refusal"
    ));

    let retained = project_memory_request(
        &project.0,
        &session.0,
        Some("retained"),
        "retained body",
        1_700_000_000_004,
    );
    store
        .remember_project_memory(&retained, &DevelopmentNoopRedactor)
        .expect("remember retained fixture");
    let mut forget_actor = actor(&session.0);
    forget_actor.reason = "reject-forget-attribution".into();
    assert!(matches!(
        store.forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: session.clone(),
                key: "retained".into(),
                actor: forget_actor,
                created_at: Utc.timestamp_millis_opt(1_700_000_000_005).unwrap(),
            },
            &MatchingRedactor("reject-forget-attribution"),
        ),
        Err(StoreError::RedactionRefused(message))
            if message == "project-memory attribution refusal"
    ));
    assert!(
        store
            .project_memory_full(&project, &session, &actor(&session.0), "retained")
            .is_ok(),
        "a refused forget must leave the retained memory live"
    );
}

#[test]
fn project_memory_context_only_retry_uses_the_stored_delivery_envelope() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-context-replay-boundary".into());
    let session = SessionId("memory-context-boundary-session".into());
    let mut request = project_memory_request(
        &project.0,
        &session.0,
        Some("context-replay-boundary"),
        &"x".repeat(crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES),
        1_700_000_000_000,
    );
    request.actor.actor_id = "a".repeat(3_850);
    let created = store
        .remember_project_memory_with_admission(
            &request,
            &DevelopmentNoopRedactor,
            admit_project_memory_full,
        )
        .expect("original bounded memory");
    assert!(!created.duplicate);

    let mut retry = request.clone();
    retry.actor.provenance_chain.push(ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: "c".repeat(crate::domain::MAX_ACTOR_CONTEXT_BYTES),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    let incoming = ProjectMemoryFull {
        key: "context-replay-boundary".into(),
        body: retry.body.clone(),
        remembered_at: retry.created_at,
        actor_id: retry.actor.actor_id.clone(),
        actor_context: retry.actor.attribution_context().map(str::to_owned),
        session_id: retry.actor.session_id.clone(),
    };
    assert!(
        admit_project_memory_full(&incoming).is_err(),
        "the retry's larger transient attribution must cross the response boundary"
    );
    let replay = store
        .remember_project_memory_with_admission(
            &retry,
            &DevelopmentNoopRedactor,
            admit_project_memory_full,
        )
        .expect("context-only retry uses the stored original envelope");
    assert!(replay.duplicate);
    let retained = store
        .project_memory_full(&project, &session, &request.actor, &created.key)
        .expect("retained original memory");
    assert_eq!(retained.actor_context, None);
    assert_eq!(retained.remembered_at, request.created_at);
}

#[test]
fn project_memory_redactor_inspects_actor_context_provenance() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = project_memory_request(
        "project-memory-context-redaction",
        "memory-context-session",
        Some("rejected-context"),
        "safe body",
        1_700_000_000_003,
    );
    request.actor.provenance_chain.push(ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: "model=secret-bearing-context".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });

    assert!(matches!(
        store.remember_project_memory(
            &request,
            &MatchingRedactor("model=secret-bearing-context"),
        ),
        Err(StoreError::RedactionRefused(message))
            if message == "project-memory attribution refusal"
    ));

    let mut duplicated = request.clone();
    duplicated.actor.provenance_chain.push(ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: "model=second-context".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    assert!(matches!(
        store.remember_project_memory(&duplicated, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(detail))
            if detail.contains("at most one value")
    ));

    let mut oversized = request;
    oversized.actor.provenance_chain[0].source =
        "x".repeat(crate::domain::MAX_ACTOR_CONTEXT_BYTES + 1);
    assert!(matches!(
        store.remember_project_memory(&oversized, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(detail))
            if detail.contains("not normalized and bounded")
    ));
}

#[test]
fn project_memory_session_spelling_matches_the_work_actor_convention() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-padded-session".into());
    let padded_session = SessionId(" padded-session ".into());
    let padded = project_memory_request(
        &project.0,
        &padded_session.0,
        Some("padded-session"),
        "padded session body",
        1_700_000_000_005,
    );
    store
        .remember_project_memory(&padded, &DevelopmentNoopRedactor)
        .expect("project memory follows the same nonblank session convention as work");
    let padded_full = store
        .project_memory_full(
            &project,
            &padded_session,
            &actor(&padded_session.0),
            "padded-session",
        )
        .expect("read padded-session attribution");
    assert_eq!(padded_full.session_id, Some(padded_session));
}

#[test]
fn project_memory_preview_uses_the_first_nonblank_line() {
    assert_eq!(
        project_memory_first_line("\n \t \nActual project observation\nLater detail"),
        "Actual project observation"
    );
}

#[test]
fn keyed_project_memory_shape_is_rechecked_from_canonical_bytes() {
    let request = project_memory_request(
        "project-memory-shape",
        "memory-shape-session",
        Some("valid-key"),
        "valid body",
        1_700_000_000_000,
    );
    let prepared = prepare_project_memory(&request, "valid-key").expect("prepare fixture");
    let mut invalid_versions = Vec::new();
    let mut invalid_key = prepared.version.clone();
    invalid_key.project_key = Some("Unsafe Key".into());
    invalid_versions.push(invalid_key);
    let mut oversized_body = prepared.version.clone();
    oversized_body.body = "x".repeat(MAX_PROJECT_MEMORY_BODY_BYTES + 1);
    invalid_versions.push(oversized_body);
    let mut forged_shape = prepared.version.clone();
    forged_shape.tags.clear();
    invalid_versions.push(forged_shape);
    let mut oversized_actor = prepared.version.clone();
    oversized_actor.actor.reason = "x".repeat(MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES + 1);
    invalid_versions.push(oversized_actor);

    for version in invalid_versions {
        let version_object = CanonicalObject::freeze(&version).expect("freeze invalid version");
        let mut assertion = prepared.assertion.clone();
        assertion.version = version_object.hash().clone();
        assert!(matches!(
            validate_keyed_project_memory_shape(&version, &assertion),
            Err(StoreError::InvalidMemoryProjection(message))
                if message.contains("invalid canonical shape")
        ));
    }
}

#[test]
fn project_memory_rebuild_refuses_a_hash_consistent_unsafe_key() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let request = project_memory_request(
        "project-memory-rebuild-shape",
        "memory-rebuild-shape-session",
        Some("valid-key"),
        "valid body",
        1_700_000_000_000,
    );
    let prepared = prepare_project_memory(&request, "valid-key").expect("prepare fixture");
    let mut version = prepared.version;
    version.project_key = Some("Unsafe Key".into());
    let version_object = CanonicalObject::freeze(&version).expect("freeze malformed version");
    let mut assertion = prepared.assertion;
    assertion.version = version_object.hash().clone();
    let assertion_object = CanonicalObject::freeze(&assertion).expect("freeze bound assertion");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin restore fixture");
    SqliteStore::insert_object(&transaction, "memory_version", &version_object)
        .expect("insert malformed but hash-consistent version");
    SqliteStore::insert_object(&transaction, "memory_assertion_event", &assertion_object)
        .expect("insert bound assertion");
    assert!(matches!(
        SqliteStore::apply_memory_projection(
            &transaction,
            version_object.hash(),
            assertion_object.hash(),
            &version,
            &assertion,
            MemoryProjectionMode::Replay,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("invalid canonical shape")
    ));
}

#[test]
fn project_memory_reads_reject_projection_and_canonical_drift() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-integrity".into());
    let session = SessionId("memory-integrity-session".into());
    let request = project_memory_request(
        &project.0,
        &session.0,
        Some("integrity-key"),
        "trusted body",
        1_700_000_000_000,
    );
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .expect("remember fixture");
    store
        .connection
        .execute(
            "UPDATE memory_heads SET body = 'forged body' WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt projected body");
    assert!(matches!(
        store.project_memories(&project, &session, &actor(&session.0), None, None),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE memory_heads SET body = 'trusted body', status = 'tombstoned' WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt projected status");
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), "integrity-key"),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE memory_heads SET status = 'active' WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("restore projected status");
    let version_hash = store
        .connection
        .query_row(
            "SELECT version_hash FROM memory_heads WHERE project_id = ?1",
            [project.0.as_str()],
            |row| row.get::<_, String>(0),
        )
        .expect("version hash");
    let mut version: MemoryVersion = store
        .get_typed_object(
            &ObjectHash::from_stored(version_hash.clone()).expect("stored hash"),
            "memory_version",
        )
        .expect("load version")
        .expect("version exists");
    version.project_key = Some("forged-key".into());
    let forged_version = CanonicalObject::freeze(&version).expect("freeze forged canonical bytes");
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            params![forged_version.bytes(), version_hash],
        )
        .expect("corrupt canonical key without changing its hash");
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), "forged-key",),
        Err(StoreError::HashMismatch { .. })
    ));
}

#[test]
fn project_memory_reads_and_forget_verify_the_complete_head_projection() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-complete-head".into());
    let session = SessionId("memory-complete-head-session".into());
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("complete-head"),
                "trusted body",
                1_700_000_000_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember fixture");
    let (version_hash, memory_id) = store
        .connection
        .query_row(
            "SELECT version_hash, memory_id FROM memory_heads WHERE project_id = ?1",
            [project.0.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("version identity");

    store
        .connection
        .execute(
            "UPDATE memory_heads SET memory_id = '00000000-0000-7000-8000-000000000001'
             WHERE version_hash = ?1",
            [version_hash.as_str()],
        )
        .expect("corrupt projected memory id");
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), "complete-head"),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    assert!(matches!(
        store.forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: session.clone(),
                key: "complete-head".into(),
                actor: actor(&session.0),
                created_at: Utc.timestamp_millis_opt(1_700_000_001_000).unwrap(),
            },
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE memory_heads SET memory_id = ?1 WHERE version_hash = ?2",
            params![memory_id, version_hash],
        )
        .expect("restore projected memory id");

    store
        .connection
        .execute(
            "UPDATE memory_heads SET project_id = 'forged-project', scope_kind = 'task'
             WHERE version_hash = ?1",
            [version_hash.as_str()],
        )
        .expect("corrupt projected project scope");
    assert!(matches!(
        store.project_memories(&project, &session, &actor(&session.0), None, None),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE memory_heads SET project_id = ?1, scope_kind = 'project',
                    title = 'forged title', delivery = 'pinned'
             WHERE version_hash = ?2",
            params![project.0, version_hash],
        )
        .expect("corrupt projected metadata");
    assert!(matches!(
        store.project_memory_full(&project, &session, &actor(&session.0), "complete-head"),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
}

#[test]
fn project_memory_unique_key_refuses_typed_when_head_is_missing() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-missing-head".into());
    let session = SessionId("memory-missing-head-session".into());
    let request = project_memory_request(
        &project.0,
        &session.0,
        Some("reserved-key"),
        "trusted body",
        1_700_000_000_000,
    );
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .expect("remember fixture");
    store
        .connection
        .execute(
            "DELETE FROM memory_heads WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("delete durable head fixture");
    assert!(matches!(
        store.remember_project_memory(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("durable head is missing")
    ));
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects
                 WHERE object_kind = 'memory_version'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count immutable versions"),
        1
    );
}

#[test]
fn project_memory_state_drift_refuses_and_rebuilds_from_canonical_history() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-memory-state-drift".into());
    let session = SessionId("memory-state-drift-session".into());
    store
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("retained"),
                "retained body",
                1_700_000_000_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember fixture");
    store
        .connection
        .execute(
            "DELETE FROM project_memory_state WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("delete advisory state fixture");
    assert!(matches!(
        store.project_memory_advertisement_candidate(&project, &session, None),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("state is missing")
    ));
    let before_versions = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_kind = 'memory_version'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count versions before refused mutation");
    assert!(matches!(
        store.remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("new-key"),
                "new body",
                1_700_000_001_000,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'memory_version'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count versions after refused mutation"),
        before_versions
    );
    store
        .rebuild_memory_index()
        .expect("rebuild derived memory state");
    let repaired = store
        .project_memory_advertisement_candidate(&project, &session, None)
        .expect("read rebuilt advisory state");
    assert_eq!(repaired.count, 1);
    assert!(repaired.changed);
}

#[test]
fn project_memory_related_reads_share_one_snapshot_across_connections() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut reader = SqliteStore::open(&database).expect("reader");
    let mut writer = SqliteStore::open(&database).expect("writer");
    let project = ProjectId("project-memory-snapshot".into());
    let session = SessionId("memory-snapshot-session".into());
    reader
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("first"),
                "first body",
                1_700_000_000_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("remember first fixture");

    let transaction = reader
        .connection
        .unchecked_transaction()
        .expect("read transaction");
    let before_rows = project_memory_rows_on(&transaction, &project, None, None, 20)
        .expect("rows before concurrent write");
    let before_state =
        project_memory_state_on(&transaction, &project).expect("state before concurrent write");
    writer
        .remember_project_memory(
            &project_memory_request(
                &project.0,
                &session.0,
                Some("second"),
                "second body",
                1_700_000_001_000,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("concurrent writer commits");
    assert_eq!(
        project_memory_state_on(&transaction, &project).expect("snapshot state"),
        before_state
    );
    assert_eq!(before_state.0, before_rows.0.len());
    transaction.commit().expect("finish read snapshot");
    assert_eq!(
        reader
            .project_memories(&project, &session, &actor(&session.0), None, None)
            .expect("fresh snapshot")
            .memories
            .len(),
        2
    );
}

#[test]
fn concurrent_project_memory_create_has_one_typed_winner() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    SqliteStore::open(&database).expect("initialize store");
    let barrier = Arc::new(Barrier::new(3));
    let spawn = |session: &'static str, body: &'static str| {
        let database = database.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut store = SqliteStore::open(&database).expect("contending store");
            let request = project_memory_request(
                "project-memory-create-race",
                session,
                Some("shared-key"),
                body,
                1_700_000_000_000,
            );
            barrier.wait();
            store.remember_project_memory(&request, &DevelopmentNoopRedactor)
        })
    };
    let first = spawn("memory-race-first", "first contender");
    let second = spawn("memory-race-second", "second contender");
    barrier.wait();
    let results = [
        first.join().expect("first contender joins"),
        second.join().expect("second contender joins"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| {
                matches!(result, Err(StoreError::ProjectMemoryExists(key)) if key == "shared-key")
            })
            .count(),
        1
    );
}

#[test]
fn project_memory_unique_index_collision_fails_closed_as_a_typed_refusal() {
    let mut store = SqliteStore::open_in_memory().expect("project memory unique-index fixture");
    let key = "shared-key";
    let first_request = project_memory_request(
        "project-memory-unique-index",
        "memory-first",
        Some(key),
        "first body",
        1_700_000_000_000,
    );
    let second_request = project_memory_request(
        "project-memory-unique-index",
        "memory-second",
        Some(key),
        "second body",
        1_700_000_001_000,
    );
    let first = prepare_project_memory(&first_request, key).expect("prepare first memory");
    let second = prepare_project_memory(&second_request, key).expect("prepare second memory");
    let transaction = store
        .connection
        .transaction()
        .expect("project memory unique-index transaction");
    SqliteStore::insert_object(&transaction, "memory_version", &first.version_object)
        .expect("reserve the project-memory key");

    assert!(matches!(
        SqliteStore::insert_project_memory_version_object(
            &transaction,
            &second.version_object,
            &first_request.project_id,
            key,
        ),
        Err(StoreError::InvalidMemoryProjection(detail))
            if detail == "project memory key is reserved but its durable head is missing"
    ));
}
