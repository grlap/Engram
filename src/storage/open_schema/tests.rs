use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::{
    normalized_schema_definition, schema_object_matches_owner, stored_schema_definitions,
    test_database_shape_snapshot, test_support::*,
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
fn unrecognized_schema_and_corrupt_records_remain_distinct_on_open_and_repair() {
    use crate::storage::StoreOpenRefusalKind;

    for (fixture_sql, expected_kind) in [
        (
            "CREATE INDEX extra_objects_index ON objects(object_kind)",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "CREATE INDEX work_extra_index ON work_items(priority)",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "CREATE INDEX object_fts_extra ON objects(object_kind)",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "CREATE INDEX work_catalog_fts_extra ON work_items(priority)",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "CREATE TRIGGER object_fts_extra_trigger AFTER INSERT ON objects BEGIN SELECT 1; END",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "CREATE TABLE work_catalog_fts_extra_table(value TEXT)",
            StoreOpenRefusalKind::DifferentBuildSchema,
        ),
        (
            "UPDATE control_policy_versions SET policy_json = X'7B7D'",
            StoreOpenRefusalKind::CorruptStore,
        ),
    ] {
        assert_open_schema_refusal_without_mutation(fixture_sql, &expected_kind);
    }
}

fn assert_open_schema_refusal_without_mutation(
    fixture_sql: &str,
    expected_kind: &crate::storage::StoreOpenRefusalKind,
) {
    use crate::storage::{StoreOpenRefusalKind, store_open_refusal_kind};

    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("refusal.db");
    drop(SqliteStore::open(&database).unwrap());
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch(fixture_sql).unwrap();
    let before_shape = test_database_shape_snapshot(&connection).unwrap();
    drop(connection);
    let before_bytes = std::fs::read(&database).unwrap();

    for mode in ["open", "unresolved", "read_only", "repair"] {
        let result = match mode {
            "open" => SqliteStore::open(&database).map(|_| ()),
            "unresolved" => SqliteStore::open_unresolved(&database).map(|_| ()),
            "read_only" => SqliteStore::open_existing_read_only(&database).map(|_| ()),
            "repair" => SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
            _ => unreachable!(),
        };
        let error = result.expect_err("the fixture must remain refused");
        assert_eq!(
            &store_open_refusal_kind(&error),
            expected_kind,
            "{mode}: {error}"
        );
        if expected_kind == &StoreOpenRefusalKind::DifferentBuildSchema {
            let message = error.to_string();
            assert!(
                message.contains("use the Engram build that owns this store"),
                "{message}"
            );
            for unsafe_advice in [
                "restore",
                "re-initialize",
                "durable state is invalid",
                "invalid data",
            ] {
                assert!(!message.contains(unsafe_advice), "{mode}: {message}");
            }
        }
        assert_eq!(std::fs::read(&database).unwrap(), before_bytes, "{mode}");
        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            test_database_shape_snapshot(&connection).unwrap(),
            before_shape,
            "{mode}"
        );
    }
}

fn schema_object_namespace_collision_fixtures() -> [&'static str; 6] {
    [
        "CREATE TRIGGER object_fts_data AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER work_catalog_fts_data AFTER INSERT ON work_items BEGIN SELECT 1; END",
        "CREATE TRIGGER objects_memory_assertion_version AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER objects_work_event_work_id AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER project_memory_state AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE INDEX work_feed_entries_require_work_id ON work_items(priority)",
    ]
}

#[test]
fn schema_object_namespace_collisions_refuse_on_open_and_repair() {
    for sql in schema_object_namespace_collision_fixtures() {
        assert_open_schema_refusal_without_mutation(
            sql,
            &crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
        );
    }
}

#[test]
fn migration_schema_namespace_collisions_refuse_restore_without_output() {
    use crate::storage::migration::{MigrationError, export_store, restore_source_layout};

    for sql in schema_object_namespace_collision_fixtures() {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("source.db");
        drop(SqliteStore::open_unresolved(&source).unwrap());
        let connection = Connection::open(&source).unwrap();
        connection.execute_batch(sql).unwrap();
        let before_shape = test_database_shape_snapshot(&connection).unwrap();
        drop(connection);
        let before = std::fs::read(&source).unwrap();
        let archive = directory.path().join("archive.db");
        export_store(&source, &archive).unwrap();
        let archive_before = std::fs::read(&archive).unwrap();
        let output = directory.path().join("restored.db");
        let error = restore_source_layout(&archive, &output).unwrap_err();
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.starts_with("source schema is not an explicitly supported migration profile")),
            "{sql}: {error}",
        );
        assert!(!output.exists());
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert_eq!(std::fs::read(&archive).unwrap(), archive_before);
        let connection = Connection::open(&source).unwrap();
        assert_eq!(
            test_database_shape_snapshot(&connection).unwrap(),
            before_shape
        );
    }
}

#[test]
fn missing_work_schema_metadata_refuses_on_open_and_repair() {
    assert_open_schema_refusal_without_mutation(
        "DROP TABLE work_schema_metadata",
        &crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
    );
}

#[test]
fn orphan_fts_schema_shadows_refuse_on_open_and_repair() {
    for table in ["object_fts", "work_catalog_fts"] {
        for suffix in ["_data", "_idx", "_content", "_docsize", "_config"] {
            for parent in [
                String::new(),
                format!("CREATE TABLE {table}(value TEXT);"),
                format!("CREATE VIEW {table} AS SELECT 'preserved' AS value;"),
            ] {
                let sql = format!(
                    "DROP TABLE {table}; {parent}
                     CREATE TABLE {table}{suffix}(value TEXT);
                     INSERT INTO {table}{suffix} VALUES ('preserved');"
                );
                assert_open_schema_refusal_without_mutation(
                    &sql,
                    &crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
                );
            }
        }
        assert_open_schema_refusal_without_mutation(
            &format!("DROP TABLE {table}; CREATE VIRTUAL TABLE {table} USING fts5(value);"),
            &crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
        );
    }
}

#[test]
fn non_table_fts_schema_shadows_refuse_on_open_and_repair() {
    for table in ["object_fts", "work_catalog_fts"] {
        for object in [
            format!("CREATE INDEX {table}_data ON objects(object_kind);"),
            format!("CREATE TRIGGER {table}_data AFTER INSERT ON objects BEGIN SELECT 1; END;"),
            format!("CREATE VIEW {table}_data AS SELECT 'preserved' AS value;"),
        ] {
            assert_open_schema_refusal_without_mutation(
                &format!("DROP TABLE {table}; {object}"),
                &crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
            );
        }
    }
}

#[test]
fn fts_schema_repair_preserves_owned_rebuild_paths() {
    use crate::storage::{StoreOpenRefusalKind, store_open_refusal_kind};

    for table in ["object_fts", "work_catalog_fts"] {
        for sql in [
            format!("DROP TABLE {table};"),
            format!("DROP TABLE {table}; CREATE TABLE {table}(value TEXT);"),
            format!("ALTER TABLE {table}_content ADD COLUMN unexpected TEXT;"),
        ] {
            let directory = crate::test_support::temp_home().unwrap();
            let database = directory.path().join("repair.db");
            drop(SqliteStore::open(&database).unwrap());
            let connection = Connection::open(&database).unwrap();
            connection.execute_batch(&sql).unwrap();
            let before_shape = test_database_shape_snapshot(&connection).unwrap();
            drop(connection);
            let before_bytes = std::fs::read(&database).unwrap();
            let error = SqliteStore::open(&database)
                .err()
                .expect("explicit repair required");
            assert_eq!(
                store_open_refusal_kind(&error),
                StoreOpenRefusalKind::ProjectionRepairRequired,
                "{sql}: {error}",
            );
            assert_eq!(std::fs::read(&database).unwrap(), before_bytes);
            let connection = Connection::open(&database).unwrap();
            assert_eq!(
                test_database_shape_snapshot(&connection).unwrap(),
                before_shape
            );
            drop(connection);
            let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
            assert!(report.is_healthy(), "{sql}: {report:?}");
            let store = SqliteStore::open(&database).unwrap();
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn migration_orphan_fts_schema_shadows_refuse_without_output() {
    use crate::storage::migration::{MigrationError, export_store, restore_source_layout};

    for table in ["object_fts", "work_catalog_fts"] {
        for plain_parent in [false, true] {
            let directory = crate::test_support::temp_home().unwrap();
            let source = directory.path().join("source.db");
            drop(SqliteStore::open_unresolved(&source).unwrap());
            let connection = Connection::open(&source).unwrap();
            connection
                .execute_batch(&format!("DROP TABLE {table};"))
                .unwrap();
            if plain_parent {
                connection
                    .execute_batch(&format!("CREATE TABLE {table}(value TEXT);"))
                    .unwrap();
            }
            connection
                .execute_batch(&format!(
                    "CREATE TABLE {table}_data(value TEXT);
                 INSERT INTO {table}_data VALUES ('preserved');"
                ))
                .unwrap();
            let before_shape = test_database_shape_snapshot(&connection).unwrap();
            drop(connection);
            let before = std::fs::read(&source).unwrap();
            let archive = directory.path().join("archive.db");
            export_store(&source, &archive).unwrap();
            let archive_before = std::fs::read(&archive).unwrap();
            let output = directory.path().join("restored.db");
            let error = restore_source_layout(&archive, &output).unwrap_err();
            assert!(
                matches!(&error, MigrationError::Refused(_)),
                "{table}: {error}"
            );
            assert!(!output.exists());
            assert_eq!(std::fs::read(&source).unwrap(), before);
            assert_eq!(std::fs::read(&archive).unwrap(), archive_before);
            let connection = Connection::open(&source).unwrap();
            assert_eq!(
                test_database_shape_snapshot(&connection).unwrap(),
                before_shape
            );
        }
    }
}

#[test]
fn declared_core_rebuildable_schema_pairs_match_runtime_reference() {
    let reference = crate::storage::current_schema_reference().unwrap();
    for &(object_type, name) in crate::storage::CORE_REBUILDABLE_SCHEMA_OBJECTS {
        assert!(
            reference.iter().any(|definition| {
                definition.object_type == object_type && definition.name == name
            }),
            "declared rebuildable {object_type} {name} must exist in the compiled schema",
        );
    }
}

#[test]
fn rebuildable_schema_classification_requires_the_declared_object_type() {
    use crate::storage::{
        SchemaDurability, current_schema_reference, schema_object_matches_durability,
    };

    for definition in current_schema_reference().unwrap() {
        if !schema_object_matches_durability(definition, SchemaDurability::Rebuildable) {
            continue;
        }
        for other_type in ["table", "index", "trigger", "view"] {
            if other_type == definition.object_type {
                continue;
            }
            let mut collision = definition.clone();
            collision.object_type = other_type.into();
            assert!(
                schema_object_matches_durability(&collision, SchemaDurability::Durable),
                "{} {} must not inherit {} repair eligibility",
                other_type,
                definition.name,
                definition.object_type,
            );
        }
    }
}

#[test]
fn prefixed_unknown_schema_objects_refuse_before_projection_repair() {
    for sql in [
        "CREATE INDEX object_fts_datax ON objects(object_kind)",
        "CREATE INDEX object_ftsx_data ON objects(object_kind)",
        "CREATE INDEX object_fts__data ON objects(object_kind)",
        "CREATE INDEX work_catalog_fts_datax ON work_items(priority)",
        "CREATE INDEX work_catalog_ftsx_data ON work_items(priority)",
        "CREATE INDEX work_catalog_fts__data ON work_items(priority)",
    ] {
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("prefixed.db");
        drop(SqliteStore::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        let error = SqliteStore::repair_rebuildable_projections(&path).unwrap_err();
        assert_eq!(
            crate::storage::store_open_refusal_kind(&error),
            crate::storage::StoreOpenRefusalKind::DifferentBuildSchema,
            "{sql}: {error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn fts_schema_object_matching_accepts_only_declared_shadow_names() {
    for table in ["object_fts", "work_catalog_fts"] {
        for suffix in ["", "_data", "_idx", "_content", "_docsize", "_config"] {
            let name = format!("{table}{suffix}");
            assert!(crate::storage::is_fts_schema_object(&name, table), "{name}");
        }
        for suffix in ["_datax", "x_data", "__data", "_", "_extra"] {
            let name = format!("{table}{suffix}");
            assert!(
                !crate::storage::is_fts_schema_object(&name, table),
                "{name}"
            );
        }
        assert!(!crate::storage::is_fts_schema_object("unrelated", table));
    }
}

#[test]
fn sqlite_name_lookalikes_remain_user_schema_on_open_and_repair() {
    for name in ["sqliteX_extra", "SQLITEX_extra", "sqlite"] {
        for foreign_only in [true, false] {
            let directory = crate::test_support::temp_home().unwrap();
            let path = directory.path().join("lookalike.db");
            if !foreign_only {
                drop(SqliteStore::open(&path).unwrap());
            }
            let connection = Connection::open(&path).unwrap();
            let sql = if foreign_only {
                format!("CREATE TABLE {name}(value TEXT); INSERT INTO {name} VALUES ('preserved')")
            } else {
                format!("CREATE INDEX {name} ON objects(object_kind)")
            };
            connection.execute_batch(&sql).unwrap();
            let before_shape = test_database_shape_snapshot(&connection).unwrap();
            drop(connection);
            let before = std::fs::read(&path).unwrap();
            for mode in ["repair", "read_only", "unresolved", "open"] {
                let result = match mode {
                    "repair" => SqliteStore::repair_rebuildable_projections(&path).map(|_| ()),
                    "read_only" => SqliteStore::open_existing_read_only(&path).map(|_| ()),
                    "unresolved" => SqliteStore::open_unresolved(&path).map(|_| ()),
                    "open" => SqliteStore::open(&path).map(|_| ()),
                    _ => unreachable!(),
                };
                let error = result.expect_err("a user schema name must not be hidden");
                assert!(
                    matches!(error, StoreError::DifferentBuildSchema),
                    "{name}, {mode}, foreign_only={foreign_only}: {error}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), before);
                let connection = Connection::open(&path).unwrap();
                assert_eq!(
                    test_database_shape_snapshot(&connection).unwrap(),
                    before_shape
                );
            }
        }
    }
}

#[test]
fn empty_schema_repair_refuses_without_initialization() {
    for empty_sqlite in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("empty.db");
        std::fs::write(&path, []).unwrap();
        if empty_sqlite {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("CREATE TABLE transient(value TEXT); DROP TABLE transient;")
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let error = SqliteStore::repair_rebuildable_projections(&path).unwrap_err();
        assert!(matches!(error, StoreError::StoreNotInitialized), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn different_build_marker_refuses_without_mutation() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
    assert!(matches!(error, StoreError::DifferentBuildSchema));

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
    let directory = crate::test_support::temp_home().expect("temp directory");
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
fn host_path_policy_open_refusal_never_advises_reinitializing_in_place() {
    use crate::storage::{StoreOpenRefusalKind, store_open_refusal_kind};

    let directory = crate::test_support::temp_home().expect("temp directory");

    for recorded_case_fold in [false, true] {
        let recorded = HostPathPolicy {
            case_fold_paths: recorded_case_fold,
            windows_alias_rules: false,
        };
        let requested = HostPathPolicy {
            case_fold_paths: !recorded_case_fold,
            windows_alias_rules: false,
        };
        let stored_flag = if recorded_case_fold {
            "case_fold"
        } else {
            "case_sensitive"
        };
        let case_database = directory
            .path()
            .join(format!("case-mismatch-{stored_flag}.sqlite3"));
        drop(
            SqliteStore::open_with_host_path_policy(&case_database, recorded)
                .expect("bind recorded case policy"),
        );
        let case_before = std::fs::read(&case_database).expect("case bytes before");
        let case_error = SqliteStore::open_with_host_path_policy(&case_database, requested)
            .err()
            .expect("case-only mismatch");
        let case_message = case_error.to_string();
        assert_eq!(
            store_open_refusal_kind(&case_error),
            StoreOpenRefusalKind::PathPolicy {
                recorded: describe_host_path_policy(recorded),
                requested: describe_host_path_policy(requested),
            }
        );
        assert!(
            case_message.contains("); if the project moved "),
            "{stored_flag}: {case_message}"
        );
        assert!(
            case_message.contains(&format!("use --host-path-policy {stored_flag}")),
            "{stored_flag}: {case_message}"
        );
        assert!(
            !case_message.contains("re-initialize") && !case_message.contains("reinitialize"),
            "{stored_flag}: {case_message}"
        );
        assert!(
            !case_message.contains("new location"),
            "{stored_flag}: {case_message}"
        );
        assert_eq!(
            std::fs::read(&case_database).expect("case bytes after"),
            case_before
        );
    }

    let recorded = HostPathPolicy {
        case_fold_paths: false,
        windows_alias_rules: false,
    };
    let alias_database = directory.path().join("alias-mismatch.sqlite3");
    drop(
        SqliteStore::open_with_host_path_policy(&alias_database, recorded)
            .expect("bind recorded alias policy"),
    );
    let alias_before = std::fs::read(&alias_database).expect("alias bytes before");
    let alias_requested = HostPathPolicy {
        case_fold_paths: false,
        windows_alias_rules: true,
    };
    let alias_error = SqliteStore::open_with_host_path_policy(&alias_database, alias_requested)
        .err()
        .expect("alias mismatch");
    let alias_message = alias_error.to_string();
    assert_eq!(
        store_open_refusal_kind(&alias_error),
        StoreOpenRefusalKind::PathPolicy {
            recorded: describe_host_path_policy(recorded),
            requested: describe_host_path_policy(alias_requested),
        }
    );
    assert!(
        alias_message.contains("); if the project moved "),
        "{alias_message}"
    );
    assert!(
        alias_message.contains("host compatible with the recorded alias rules"),
        "{alias_message}"
    );
    assert!(
        alias_message.contains("fresh store at a new location"),
        "{alias_message}"
    );
    assert!(
        !alias_message.contains("--host-path-policy"),
        "{alias_message}"
    );
    assert!(
        !alias_message.contains("re-initialize") && !alias_message.contains("reinitialize"),
        "{alias_message}"
    );
    assert_eq!(
        std::fs::read(&alias_database).expect("alias bytes after"),
        alias_before
    );
}

#[test]
fn backup_copies_a_live_store_and_verifies_the_copy() {
    let directory = crate::test_support::temp_home().expect("temp directory");
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
    let directory = crate::test_support::temp_home().expect("temp directory");
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
    let directory = crate::test_support::temp_home().expect("temp directory");
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
    let directory = crate::test_support::temp_home().expect("temp directory");
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
            matches!(&error, StoreError::DifferentBuildSchema),
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
        let directory = crate::test_support::temp_home().expect("temporary store directory");
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
                matches!(&error, StoreError::DifferentBuildSchema),
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
            matches!(&error, StoreError::DifferentBuildSchema),
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
    let directory = crate::test_support::temp_home().expect("temporary store directory");
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
