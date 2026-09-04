use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::{
    DIFFERENT_BUILD_STORE_MESSAGE, normalized_schema_definition, schema_object_matches_owner,
    stored_schema_definitions, test_database_shape_snapshot, test_support::*,
};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{EffectClass, NoteVisibility, ProjectId},
};

fn remove_schema_family(connection: &Connection, owner: SchemaOwner, fts_table: &str) {
    connection
        .pragma_update(None, "foreign_keys", false)
        .expect("disable fixture foreign keys");
    drop_schema_object(connection, fts_table).expect("drop family FTS table");
    let mut definitions = stored_schema_definitions(connection)
        .expect("enumerate family schema")
        .into_iter()
        .filter(|definition| schema_object_matches_owner(definition, owner))
        .collect::<Vec<_>>();
    definitions.sort_by_key(|definition| definition.object_type == "table");
    for definition in definitions {
        drop_schema_object(connection, &definition.name).expect("drop family schema object");
    }
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("restore fixture foreign keys");
}

#[test]
fn schema_reference_normalization_is_whitespace_stable_and_sorted() {
    assert_eq!(
        normalized_schema_definition("CREATE  TABLE sample (\n value TEXT\t)"),
        "CREATE TABLE sample ( value TEXT )"
    );
    let store = SqliteStore::open_in_memory().expect("fresh store");
    let definitions = stored_schema_definitions(&store.connection).expect("schema definitions");
    assert!(definitions.windows(2).all(|pair| {
        (&pair[0].object_type, &pair[0].name) <= (&pair[1].object_type, &pair[1].name)
    }));
}

#[test]
fn centralized_schema_versions_match_fresh_store_projections_and_policy_objects() {
    let mut store = SqliteStore::open_in_memory().expect("fresh store");
    let work_schema: i64 = store
        .connection
        .query_row(
            "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("work schema projection");
    let control_state_schema: i64 = store
        .connection
        .query_row(
            "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("control policy state projection");
    let (_, policy, authority) =
        SqliteStore::load_control_policy_head(&store.connection).expect("active policy");

    assert_eq!(work_schema, crate::schema::WORK_SCHEMA_VERSION);
    assert_eq!(
        control_state_schema,
        crate::schema::CONTROL_POLICY_STATE_SCHEMA_VERSION
    );
    assert_eq!(
        policy.schema_version,
        crate::schema::CONTROL_POLICY_SCHEMA_VERSION
    );
    assert_eq!(
        policy.control_schema_version,
        crate::schema::CONTROL_SCHEMA_VERSION
    );
    assert_eq!(
        authority.schema_version,
        crate::schema::CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION
    );
    let rule_set_column = store
        .connection
        .query_row(
            "SELECT type, \"notnull\" FROM pragma_table_info('work_run_obligations')
             WHERE name = 'rule_set_hash'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .expect("required obligation rule-set column");
    assert_eq!(rule_set_column, ("TEXT".into(), true));
    let rule_set_hash = &policy.obligation_rule_set;
    let rule_set = SqliteStore::load_obligation_rule_set_on(&store.connection, rule_set_hash)
        .expect("live obligation rule set");
    assert_eq!(
        rule_set.schema_version,
        crate::schema::OBLIGATION_RULE_SET_SCHEMA_VERSION
    );

    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["schema-agent"]);
    let receipt = store
        .capture_note(
            &note_request(
                task_id,
                "schema-agent",
                "Fact: live memory schema marker",
                "schema-memory",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("capture live memory");
    let version: MemoryVersion = store
        .get(&receipt.version)
        .expect("load live memory")
        .expect("memory object exists");
    assert_eq!(version.schema_version, crate::schema::SCHEMA_VERSION);
}

#[test]
fn different_build_marker_refuses_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("different-build.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));
    let fixture = Connection::open(&database).expect("open fixture");
    fixture
        .execute(
            "UPDATE work_schema_metadata SET schema_version = ?1 WHERE singleton = 1",
            [crate::schema::WORK_SCHEMA_VERSION + 1],
        )
        .expect("install different-build marker");
    fixture
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint fixture");
    drop(fixture);
    for sidecar in store_sidecars(&database) {
        let _ = std::fs::remove_file(sidecar);
    }

    let before = Connection::open(&database).expect("inspect fixture");
    let before_shape = test_database_shape_snapshot(&before).expect("capture database shape");
    drop(before);
    let before_bytes = std::fs::read(&database).expect("read database bytes");

    let Err(error) = SqliteStore::open(&database) else {
        panic!("different-build marker must refuse");
    };
    assert!(matches!(
        error,
        StoreError::InvalidControlProjection(message)
            if message == DIFFERENT_BUILD_STORE_MESSAGE
    ));

    let after = Connection::open(&database).expect("inspect refused fixture");
    let after_shape = test_database_shape_snapshot(&after).expect("capture refused database shape");
    drop(after);
    assert_eq!(after_shape, before_shape);
    assert_eq!(
        std::fs::read(&database).expect("read refused database bytes"),
        before_bytes
    );
}
#[test]
fn store_persists_and_enforces_one_host_path_identity_policy() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("path-policy.sqlite3");
    let policy = HostPathPolicy {
        case_fold_paths: false,
        windows_alias_rules: false,
    };
    drop(
        SqliteStore::open_with_host_path_policy(&database, policy)
            .expect("bind explicit path policy"),
    );
    drop(SqliteStore::open_with_host_path_policy(&database, policy).expect("same policy reopens"));
    let mismatch = SqliteStore::open_with_host_path_policy(
        &database,
        HostPathPolicy {
            case_fold_paths: true,
            windows_alias_rules: true,
        },
    );
    assert!(matches!(
        mismatch,
        Err(StoreError::InvalidControlSession(_))
    ));

    let recovery_database = directory.path().join("path-policy-recovery.sqlite3");
    drop(
        SqliteStore::open_with_host_path_policy(&recovery_database, policy)
            .expect("initialize recoverable policy store"),
    );
    let recovery = Connection::open(&recovery_database).expect("recovery connection");
    recovery
        .execute("DELETE FROM control_host_path_policy", [])
        .expect("simulate crash after table creation");
    drop(recovery);
    drop(
        SqliteStore::open_with_host_path_policy(&recovery_database, policy)
            .expect("empty policy table is rebound atomically"),
    );

    let unsafe_database = directory.path().join("path-policy-unsafe.sqlite3");
    drop(
        SqliteStore::open_with_host_path_policy(&unsafe_database, policy)
            .expect("initialize unsafe policy store"),
    );
    let unsafe_connection = Connection::open(&unsafe_database).expect("unsafe connection");
    unsafe_connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .expect("disable fixture foreign keys");
    unsafe_connection
        .execute("DELETE FROM control_host_path_policy", [])
        .expect("remove policy binding");
    unsafe_connection
        .execute(
            "INSERT INTO control_work_leases (
                 lease_id, task_id, holder_session_id, lease_hash, lease_json,
                 state, expires_at_ms
              ) VALUES ('existing-path', 'task', 'session', 'hash',
                       CAST('{\"subject\":{\"kind\":\"path\"}}' AS BLOB),
                       'active', 1)",
            [],
        )
        .expect("insert existing path-bearing state");
    drop(unsafe_connection);
    assert!(matches!(
        SqliteStore::open_with_host_path_policy(&unsafe_database, policy),
        Err(StoreError::InvalidControlSession(_))
    ));
}

#[test]
fn backup_copies_a_live_store_and_verifies_the_copy() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("live.sqlite3");
    let mut store = SqliteStore::open(&database).expect("live store");
    let project = ProjectId("backup-project".into());
    let session = SessionId("backup-session".into());
    store
        .start_task(
            &project,
            "dummy:BACKUP-1",
            "Backup fixture",
            &session,
            actor("backup-agent"),
            Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
        )
        .expect("fixture task");
    let copy = directory.path().join("backups").join("copy.sqlite3");
    let manifest = store.backup_to(&copy).expect("backup");
    assert_eq!(manifest.path, copy);
    assert!(manifest.checked_objects > 0);
    assert_eq!(manifest.file_sha256.len(), 64);
    assert_eq!(
        manifest.file_bytes,
        std::fs::metadata(&copy).expect("copy metadata").len()
    );
    let reverified = SqliteStore::verify_backup(&copy).expect("verify the copy again");
    assert_eq!(reverified.file_sha256, manifest.file_sha256);
    assert_eq!(reverified.checked_objects, manifest.checked_objects);
    // The copy is a complete store: it opens and answers on its own.
    let restored = SqliteStore::open(&copy).expect("open the copy");
    assert!(restored.bound_task(&project, &session).is_ok());
    // An existing target is never overwritten.
    assert!(matches!(
        store.backup_to(&copy),
        Err(StoreError::InvalidWork(_))
    ));
}

#[test]
fn unresolved_path_identity_refuses_path_leases_but_not_logical_ones() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store =
        SqliteStore::open_in_memory_with_host_path_identity(None).expect("unresolved store");
    assert_eq!(store.host_path_identity(), None);
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session = bind_control_for(&mut store, "unresolved", "bind-unresolved", &effects, now);
    complete_control_turn(
        &mut store,
        &session,
        "sync-unresolved",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let path = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    assert!(matches!(
        store.acquire_work_lease(
            &ProjectId("project-a".into()),
            &session.status.session_id,
            &session.connection_token,
            &session.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &path,
            300,
            "lease-unresolved-path",
            now + TimeDelta::seconds(2),
        ),
        Err(StoreError::HostPathIdentityUnresolved)
    ));
    let logical = crate::domain::ResourceSubject::Logical {
        namespace: "engram".into(),
        segments: vec!["report".into()],
        coverage: crate::domain::ResourceCoverage::Exact,
    };
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session.status.session_id,
                &session.connection_token,
                &session.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &logical,
                300,
                "lease-unresolved-logical",
                now + TimeDelta::seconds(3),
            )
            .expect("logical leases need no path identity"),
        WorkLeaseDecision::Granted { .. }
    ));
    // A persisted policy is still binding for a later resolved opener,
    // while an unresolved opener may read the same store.
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("identity.sqlite3");
    let folded = HostPathPolicy {
        case_fold_paths: true,
        windows_alias_rules: false,
    };
    drop(SqliteStore::open_with_host_path_policy(&database, folded).expect("bind folded"));
    let reader = SqliteStore::open_unresolved(&database).expect("unresolved opener reads");
    assert_eq!(
        reader.stored_host_path_policy().expect("stored policy"),
        Some(folded)
    );
    assert!(matches!(
        SqliteStore::open_with_host_path_policy(
            &database,
            HostPathPolicy {
                case_fold_paths: false,
                windows_alias_rules: false,
            },
        ),
        Err(StoreError::InvalidControlSession(_))
    ));
}

#[test]
fn case_aliases_conflict_only_under_a_folding_policy() {
    for (case_fold_paths, expect_conflict) in [(true, true), (false, false)] {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory_with_host_path_identity(Some(HostPathPolicy {
            case_fold_paths,
            windows_alias_rules: false,
        }))
        .expect("explicit policy store");
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session_a = bind_control_for(&mut store, "case-a", "bind-case-a", &effects, now);
        let session_b = bind_control_for(&mut store, "case-b", "bind-case-b", &effects, now);
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-case-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-case-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(2),
        );
        let lower = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into(), "Main.rs".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &lower,
                    300,
                    "lease-case-a",
                    now + TimeDelta::seconds(3),
                )
                .expect("first lease"),
            WorkLeaseDecision::Granted { .. }
        ));
        complete_control_turn(
            &mut store,
            &session_b,
            "resync-case-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(3),
        );
        let upper = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["SRC".into(), "main.RS".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        let decision = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &upper,
                300,
                "lease-case-b",
                now + TimeDelta::seconds(4),
            )
            .expect("second lease decision");
        assert_eq!(
            matches!(decision, WorkLeaseDecision::Defer { .. }),
            expect_conflict,
            "case_fold_paths={case_fold_paths} decided {decision:?}"
        );
    }
}

#[test]
fn verify_backup_touches_nothing_and_backups_never_replace() {
    let directory = tempfile::tempdir().expect("temp directory");
    let missing = directory.path().join("absent.sqlite3");
    assert!(matches!(
        SqliteStore::verify_backup(&missing),
        Err(StoreError::InvalidWork(_))
    ));
    assert!(!missing.exists(), "verification must not create a store");

    let database = directory.path().join("live.sqlite3");
    let store = SqliteStore::open(&database).expect("live store");
    let target = directory.path().join("copies").join("one.sqlite3");
    let first = store.backup_to(&target).expect("first backup");
    let second = store.backup_to(&target);
    assert!(matches!(second, Err(StoreError::InvalidWork(_))));
    assert_eq!(
        SqliteStore::verify_backup(&target)
            .expect("the published copy is untouched")
            .file_sha256,
        first.file_sha256
    );
    let leftovers = std::fs::read_dir(target.parent().expect("copies directory"))
        .expect("list copies")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0, "staged files never survive a refusal");
}

#[test]
fn unresolved_opener_cannot_begin_a_path_bearing_grant() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("unresolved-begin.sqlite3");
    let mut store = SqliteStore::open(&database).expect("resolved opener");
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session = bind_control_for(&mut store, "ub-a", "bind-ub-a", &effects, now);
    complete_control_turn(
        &mut store,
        &session,
        "sync-ub-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let WorkLeaseDecision::Granted { .. } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session.status.session_id,
            &session.connection_token,
            &session.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "ub-lease",
            now + TimeDelta::seconds(2),
        )
        .unwrap()
    else {
        panic!("the lease must grant on the resolved opener");
    };
    let ControlTurnDecision::Grant { grant } = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &session.status.session_id,
            &session.connection_token,
            &session.routing_token,
            &TurnIntent {
                idempotency_key: "ub-turn".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"ub-turn"),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![crate::domain::ResourceSubject::Path {
                    project_id: ProjectId("project-a".into()),
                    segments: vec!["src".into(), "lib.rs".into()],
                    coverage: crate::domain::ResourceCoverage::Exact,
                }],
            },
            now + TimeDelta::seconds(3),
        )
        .unwrap()
    else {
        panic!("the mutation turn must grant on the resolved opener");
    };
    drop(store);
    let delivery_tokens = grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();
    let mut unresolved = SqliteStore::open_unresolved(&database).expect("unresolved opener");
    assert!(matches!(
        unresolved.begin_control_turn(
            &ProjectId("project-a".into()),
            &session.status.session_id,
            &session.connection_token,
            &session.routing_token,
            &grant.grant_id,
            &delivery_tokens,
            "ub-begin",
            now + TimeDelta::seconds(4),
        ),
        Err(StoreError::HostPathIdentityUnresolved)
    ));
}

#[test]
fn current_store_reopens_through_a_read_only_connection() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));
    let connection =
        Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only SQLite connection");
    SqliteStore::from_connection(connection, Some(HostPathPolicy::host_default()), None)
        .expect("current schema opens without a write transaction");
}

#[test]
fn explicit_projection_repair_rebuilds_missing_core_index_and_fts() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));
    let fixture = Connection::open(&database).expect("open projection fixture");
    fixture
        .execute_batch(
            "DROP INDEX memory_heads_scope;
             CREATE INDEX memory_heads_scope ON memory_heads(memory_id);
             DROP INDEX objects_project_memory_key;
             CREATE INDEX objects_project_memory_key ON objects(object_hash);
             DROP TABLE project_memory_advertisements;
             CREATE TABLE project_memory_advertisements (
                 project_id TEXT PRIMARY KEY
             ) STRICT;
             INSERT INTO project_memory_advertisements (project_id)
             VALUES ('discarded-advisory-ack');
             DROP TABLE project_memory_state;
             CREATE TABLE project_memory_state (
                 project_id TEXT PRIMARY KEY
             ) STRICT;
             DROP TABLE object_fts;
             CREATE TABLE object_fts (
                 object_hash TEXT,
                 title TEXT,
                 body TEXT
             ) STRICT;",
        )
        .expect("replace rebuildable core projections with wrong definitions");
    drop(fixture);

    let Err(error) = SqliteStore::open(&database) else {
        panic!("ordinary open must refuse");
    };
    assert!(
        matches!(error, StoreError::InvalidControlProjection(message) if message.contains("--repair-projections")),
        "ordinary open should direct the operator to explicit repair"
    );
    let refused = Connection::open(&database).expect("inspect refused schema");
    assert_eq!(
        refused
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name IN (
                     'memory_heads_scope', 'objects_project_memory_key',
                     'project_memory_advertisements', 'project_memory_state',
                     'object_fts'
                 )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count missing projections"),
        5,
        "ordinary open must preserve the wrong definitions"
    );
    drop(refused);

    let report = SqliteStore::repair_rebuildable_projections(&database)
        .expect("explicitly repair core projections");
    assert!(report.is_healthy(), "{report:?}");
    let reopened = SqliteStore::open(&database).expect("open repaired schema");
    for object in [
        "memory_heads_scope",
        "objects_project_memory_key",
        "project_memory_advertisements",
        "project_memory_state",
        "object_fts",
    ] {
        assert!(
            reopened
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                    [object],
                    |row| row.get::<_, bool>(0),
                )
                .expect("inspect repaired object"),
            "missing repaired object {object}"
        );
    }
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM project_memory_advertisements",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count discarded advertisement acknowledgements"),
        0,
        "explicit repair intentionally permits one advisory reannouncement"
    );
}

#[test]
fn missing_core_durable_table_is_named_and_never_recreated() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));
    let fixture = Connection::open(&database).expect("open durable corruption fixture");
    fixture
        .execute_batch("DROP TABLE control_sessions")
        .expect("drop durable control table");
    let before = test_database_shape_snapshot(&fixture).expect("snapshot damaged store");
    drop(fixture);

    for operation in [
        SqliteStore::open(&database).map(|_| ()),
        SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
    ] {
        let error = operation.expect_err("durable corruption must be refused");
        assert!(
            matches!(&error, StoreError::InvalidControlProjection(_)),
            "unexpected durable-schema diagnostic: {error}"
        );
    }
    let after = Connection::open(&database).expect("inspect refused durable corruption");
    assert_eq!(
        test_database_shape_snapshot(&after).expect("snapshot refused store"),
        before,
        "open and explicit projection repair must leave durable corruption unchanged"
    );
}

#[test]
fn complete_schema_family_loss_is_refused_without_mutation() {
    for (owner, fts_table) in [
        (SchemaOwner::Core, "object_fts"),
        (SchemaOwner::Work, "work_catalog_fts"),
    ] {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        drop(SqliteStore::open(&database).expect("initialize current store"));
        let fixture = Connection::open(&database).expect("open family-loss fixture");
        remove_schema_family(&fixture, owner, fts_table);
        let before = test_database_shape_snapshot(&fixture).expect("snapshot damaged store");
        drop(fixture);

        for operation in [
            SqliteStore::open(&database).map(|_| ()),
            SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
        ] {
            let error = operation.expect_err("complete schema-family loss must be refused");
            assert!(
                matches!(&error, StoreError::InvalidControlProjection(_)),
                "unexpected family-loss diagnostic: {error}"
            );
        }
        let after = Connection::open(&database).expect("inspect refused family loss");
        assert_eq!(
            test_database_shape_snapshot(&after).expect("snapshot refused family loss"),
            before,
            "ordinary open and repair must not recreate a lost schema family"
        );
    }
}

#[test]
fn same_name_wrong_core_table_definition_is_refused_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));
    let fixture = Connection::open(&database).expect("open durable definition fixture");
    fixture
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             ALTER TABLE publication_intents RENAME TO publication_intents_old;
             CREATE TABLE publication_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 report_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 external_ref TEXT,
                 state TEXT NOT NULL,
                 last_error TEXT,
                 attempt_count INTEGER DEFAULT 0,
                 receipt_json TEXT
             ) STRICT;
             DROP TABLE publication_intents_old;
             PRAGMA foreign_keys = ON;",
        )
        .expect("replace durable table with a weaker same-name definition");
    let before = test_database_shape_snapshot(&fixture).expect("snapshot malformed schema");
    drop(fixture);

    for operation in [
        SqliteStore::open(&database).map(|_| ()),
        SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
    ] {
        let error = operation.expect_err("wrong durable definition must be refused");
        assert!(
            matches!(&error, StoreError::InvalidControlProjection(_)),
            "unexpected exact-schema diagnostic: {error}"
        );
    }
    let after = Connection::open(&database).expect("inspect refused schema");
    assert_eq!(
        test_database_shape_snapshot(&after).expect("snapshot refused schema"),
        before,
        "open and explicit projection repair must not rewrite durable definitions"
    );
}

#[test]
fn explicit_projection_repair_rebuilds_existing_object_fts_content() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).expect("initialize current store");
    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["fts-agent"]);
    let receipt = store
        .capture_note(
            &note_request(
                task_id,
                "fts-agent",
                "Fact: repairable memory full text content",
                "fts-content",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("capture indexed memory");
    store
        .connection
        .execute("DELETE FROM object_fts", [])
        .expect("drift existing object FTS content");
    let corrupt = store.verify_all().expect("diagnose drifted object FTS");
    assert!(
        corrupt
            .invalid_objects
            .iter()
            .any(|record| record.starts_with("object_fts:")),
        "object FTS drift should be visible: {corrupt:?}"
    );
    drop(store);

    let repaired = SqliteStore::repair_rebuildable_projections(&database)
        .expect("repair existing object FTS content");
    assert!(repaired.is_healthy(), "{repaired:?}");
    let reopened = SqliteStore::open(&database).expect("reopen repaired store");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM object_fts WHERE object_hash = ?1",
                [receipt.version.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("repaired object FTS row"),
        1
    );
    reopened
        .connection
        .execute(
            "UPDATE memory_heads SET title = 'tampered durable title'
             WHERE version_hash = ?1",
            [receipt.version.as_str()],
        )
        .expect("corrupt durable memory-head projection");
    let corrupt_head = reopened.verify_all().expect("diagnose durable head drift");
    assert!(
        corrupt_head
            .invalid_objects
            .iter()
            .any(|record| record.starts_with("memory_head:")),
        "durable memory-head drift should be visible: {corrupt_head:?}"
    );
    let before = test_database_shape_snapshot(&reopened.connection)
        .expect("snapshot corrupt durable memory head");
    drop(reopened);
    let error = SqliteStore::repair_rebuildable_projections(&database)
        .expect_err("repair must refuse an unverified durable memory head");
    assert!(
        matches!(&error, StoreError::InvalidMemoryProjection(message) if message.contains("durable memory heads are invalid")),
        "unexpected durable-head refusal: {error}"
    );
    let after = Connection::open(&database).expect("inspect refused durable memory head");
    assert_eq!(
        test_database_shape_snapshot(&after).expect("snapshot after durable-head refusal"),
        before,
        "projection repair must not mutate a corrupt durable memory head"
    );
}

#[test]
fn warm_open_skips_the_writer_lock_but_a_needed_binding_escalates() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    drop(SqliteStore::open(&database).expect("initialize current store"));

    let mut blocker = Connection::open(&database).expect("open blocking connection");
    let blocking_transaction = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("hold the SQLite writer slot");
    let current = Connection::open(&database).expect("open current-store connection");
    drop(
        SqliteStore::from_connection_with_busy_timeout(
            current,
            Some(HostPathPolicy::host_default()),
            None,
            Duration::from_millis(25),
        )
        .expect("a current warm open remains read-only while another writer is active"),
    );
    blocking_transaction
        .rollback()
        .expect("release the SQLite writer slot");

    let repair = Connection::open(&database).expect("open repair fixture connection");
    repair
        .execute("DELETE FROM control_host_path_policy", [])
        .expect("remove the recoverable empty-store path binding");
    drop(repair);

    let mut blocker = Connection::open(&database).expect("reopen blocking connection");
    let blocking_transaction = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("hold the writer slot across the repair attempt");
    let candidate = Connection::open(&database).expect("open repair candidate connection");
    let result = SqliteStore::from_connection_with_busy_timeout(
        candidate,
        Some(HostPathPolicy::host_default()),
        None,
        Duration::from_millis(25),
    );
    let Err(StoreError::Sqlite(error)) = result else {
        panic!("a required path-policy binding must contend for the writer slot");
    };
    assert!(matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ));
    blocking_transaction
        .rollback()
        .expect("release the writer slot after the negative probe");
    drop(blocker);

    drop(SqliteStore::open(&database).expect("retry and persist the required binding"));
}
