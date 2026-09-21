use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use rusqlite::{Connection, OptionalExtension, types::Value};

use super::*;
use crate::{
    ProjectId,
    domain::{ActorContext, AssuranceLevel, CreateWorkRequest},
    memory::DevelopmentNoopRedactor,
};

/// Retired operational rows are fixture data, never a schema opened by the
/// current product. Export must still carry all of them without interpretation.
fn with_retired_tables(path: &Path) {
    populated(path);
    let connection = Connection::open(path).expect("fixture");
    connection
        .execute_batch(
            "CREATE TABLE task_claims (
            task_id TEXT PRIMARY KEY, lease_id TEXT NOT NULL UNIQUE,
            holder_session_id TEXT NOT NULL, idempotency_key TEXT NOT NULL,
            expires_at_ms INTEGER NOT NULL, revision INTEGER NOT NULL
         ) STRICT;
         CREATE TABLE task_claim_intents (
            idempotency_key TEXT PRIMARY KEY, task_id TEXT NOT NULL,
            holder_session_id TEXT NOT NULL, lease_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE publication_intents (
            idempotency_key TEXT PRIMARY KEY,
            report_hash TEXT NOT NULL REFERENCES objects(object_id),
            external_ref TEXT, state TEXT NOT NULL, last_error TEXT,
            attempt_count INTEGER NOT NULL DEFAULT 0, receipt_json TEXT
         ) STRICT;
         INSERT INTO task_claims VALUES ('task', 'lease', 'old-session', 'claim', 1, 1);
         INSERT INTO task_claim_intents VALUES ('claim', 'task', 'old-session', X'7B7D');
         INSERT INTO publication_intents
            SELECT 'publication', object_id, 'external', 'pending', NULL, 0, NULL
            FROM objects LIMIT 1;",
        )
        .expect("retired fixture rows");
    // Historical canonical audit remains an opaque record with its original id.
    let event = crate::CanonicalObject::mint(&serde_json::json!({
        "schema_version": 1, "lease": {"task_id": "task"}
    }))
    .expect("historical object");
    connection.execute(
        "INSERT INTO objects (object_id, object_kind, canonical_json) VALUES (?1, 'task_claim_event', ?2)",
        rusqlite::params![event.key().as_str(), event.bytes()],
    ).expect("historical audit");
}

#[test]
fn retired_tables_export_losslessly_and_import_reports_only_explicit_omissions() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    let target = directory.path().join("target.db");
    with_retired_tables(&source);
    let before = rows(&source);
    let exported = export_json(&source, &file).expect("lossless export");
    let input = fs::read(&file).expect("export bytes");
    let imported = import_json(&file, &target).expect("fresh schema import");
    let after = rows(&target);
    for name in ["task_claims", "task_claim_intents", "publication_intents"] {
        assert_eq!(
            exported
                .tables
                .iter()
                .find(|table| table.name == name)
                .unwrap()
                .rows,
            1
        );
        assert!(!exported.left_out.iter().any(|table| table.name == name));
        assert!(!after.contains_key(name), "new schema retained {name}");
        assert!(!imported.tables.iter().any(|table| table.name == name));
        let omitted = imported
            .left_out
            .iter()
            .find(|table| table.name == name)
            .unwrap();
        assert_eq!(omitted.rows, 1);
        assert!(omitted.reason.contains("explicitly retired"));
        let exported_row = lines(&file)
            .unwrap()
            .find_map(|line| match line.unwrap() {
                Line::Row { table, values } if table == name => Some(values),
                _ => None,
            })
            .expect("retired row remains in export");
        let declaration = exported
            .tables
            .iter()
            .find(|table| table.name == name)
            .unwrap();
        let decoded = declaration
            .columns
            .iter()
            .map(|column| decode(exported_row[column].clone()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(decoded, before[name][0], "retired export changed {name}");
    }
    for (name, values) in &after {
        if name != "work_schema_metadata" {
            assert_eq!(values, &before[name], "retained table {name} changed");
        }
    }
    assert_eq!(rows(&source), before, "source must be unchanged");
    assert_eq!(
        fs::read(&file).unwrap(),
        input,
        "import must not rewrite export"
    );
    assert!(
        SqliteStore::open_unresolved(&target)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    assert!(matches!(
        SqliteStore::open_unresolved(&source),
        Err(crate::StoreError::DifferentBuildSchema)
    ));
}

#[test]
fn retired_table_rows_are_validated_and_counted_before_publication() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    with_retired_tables(&source);
    export_json(&source, &file).unwrap();
    let original: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (case, expected) in [
        (
            "unknown column",
            "column unknown_payload of retired table task_claims has no place",
        ),
        (
            "missing value",
            "a row of table task_claims lacks column lease_id",
        ),
        (
            "extra value",
            "a row of table task_claims has undeclared column unknown_payload",
        ),
        (
            "bad value",
            "column lease_id of a row of table task_claims: a blob is written as json, text, or hex",
        ),
        (
            "count swap",
            "table task_claim_intents declares 2 rows but the file holds 1",
        ),
        (
            "duplicate table",
            "duplicate table task_claims in the header",
        ),
        (
            "duplicate column",
            "duplicate column task_id of table task_claims",
        ),
        ("second end", "the file has a second end line"),
    ] {
        let mut modified = original.clone();
        let header = &mut modified[0]["engram_export"]["tables"];
        let declarations = header.as_array_mut().unwrap();
        let index = declarations
            .iter()
            .position(|table| table["name"] == "task_claims")
            .unwrap();
        match case {
            "unknown column" => declarations[index]["columns"]
                .as_array_mut()
                .unwrap()
                .push(Json::String("unknown_payload".into())),
            "count swap" => {
                declarations[index]["rows"] = Json::from(0);
                let other = declarations
                    .iter_mut()
                    .find(|table| table["name"] == "task_claim_intents")
                    .unwrap();
                other["rows"] = Json::from(2);
            }
            "duplicate table" => declarations.push(declarations[index].clone()),
            "duplicate column" => declarations[index]["columns"]
                .as_array_mut()
                .unwrap()
                .push(Json::String("task_id".into())),
            "second end" => modified.push(modified.last().unwrap().clone()),
            _ => {
                let row = modified
                    .iter_mut()
                    .find(|line| line["row"]["table"] == "task_claims")
                    .unwrap();
                let values = row["row"]["values"].as_object_mut().unwrap();
                match case {
                    "missing value" => {
                        values.remove("lease_id");
                    }
                    "extra value" => {
                        values.insert("unknown_payload".into(), Json::Null);
                    }
                    "bad value" => {
                        values.insert(
                            "lease_id".into(),
                            serde_json::json!({"unknown_blob": "private-value"}),
                        );
                    }
                    _ => unreachable!(),
                }
            }
        }
        fs::write(
            &file,
            modified
                .iter()
                .map(Json::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let target = directory.path().join("target.db");
        let error = import_json(&file, &target).expect_err(case).to_string();
        assert!(error.contains(expected), "{case}: wrong refusal: {error}");
        assert!(
            !error.contains("private-value"),
            "{case}: leaked a cell value"
        );
        assert!(!target.exists(), "{case}: published invalid input");
    }
}

#[test]
fn omitted_format_marker_rows_are_validated_and_counted() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    let target = directory.path().join("target.db");
    with_retired_tables(&source);
    export_json(&source, &file).unwrap();
    let original: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for corrupt_value in [true, false] {
        let mut modified = original.clone();
        let expected = if corrupt_value {
            let values = modified
                .iter_mut()
                .find(|line| line["row"]["table"] == "work_schema_metadata")
                .unwrap()["row"]["values"]
                .as_object_mut()
                .unwrap();
            let column = values.keys().next().unwrap().clone();
            values.insert(
                column.clone(),
                serde_json::json!({"unknown_blob": "private-value"}),
            );
            format!(
                "column {column} of a row of table work_schema_metadata: a blob is written as json, text, or hex"
            )
        } else {
            let declarations = modified[0]["engram_export"]["tables"]
                .as_array_mut()
                .unwrap();
            let index = declarations
                .iter()
                .position(|table| table["name"] == "work_schema_metadata")
                .unwrap();
            let mut marker = declarations.remove(index);
            assert_eq!(marker["rows"], 1);
            marker["rows"] = Json::from(0);
            // Keep the global total unchanged; the marker's own count must refuse.
            declarations
                .iter_mut()
                .find(|table| table["name"] == "task_claims")
                .unwrap()["rows"] = Json::from(2);
            declarations.insert(0, marker);
            "table work_schema_metadata declares 0 rows but the file holds 1".into()
        };
        fs::write(
            &file,
            modified
                .iter()
                .map(Json::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let error = import_json(&file, &target)
            .expect_err("invalid format marker")
            .to_string();
        assert!(error.contains(&expected), "wrong refusal: {error}");
        assert!(!error.contains("private-value"), "leaked a cell value");
        assert!(!target.exists(), "published invalid marker");
    }
}

#[test]
fn unknown_table_beside_retired_tables_still_refuses_by_name() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    let target = directory.path().join("target.db");
    with_retired_tables(&source);
    Connection::open(&source)
        .unwrap()
        .execute_batch("CREATE TABLE unknown_data (body TEXT);")
        .unwrap();
    export_json(&source, &file).unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(error.contains("table unknown_data has no place"), "{error}");
    assert!(!target.exists());
}

fn actor(session: &str) -> ActorContext {
    ActorContext {
        actor_id: session.into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(crate::SessionId(session.into())),
        source_tool: Some("migration-test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "migration test".into(),
    }
}

/// A store with work, a note, a claim and a project memory in it.
fn populated(path: &Path) {
    let mut store = SqliteStore::open_unresolved(path).expect("store");
    let project = ProjectId("project-json-transfer".into());
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-17T10:00:00Z")
        .expect("time")
        .with_timezone(&Utc);
    let item = store
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                project_id: project.clone(),
                parent_id: None,
                child_requirement: crate::domain::ChildRequirement::Required,
                title: "Carry a store across formats".into(),
                outcome: "Every row arrives".into(),
                acceptance: vec!["rows are equal".into()],
                kind: crate::domain::WorkItemKind::Task,
                priority: 1,
                labels: vec!["transfer".into()],
                assigned_to: None,
                deferred_until: None,
                external_ref: None,
                notes: vec!["created with a note — naïve ünïcode and \"quotes\"".into()],
                origin: crate::domain::WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor("author"),
                idempotency_key: "create".into(),
                created_at: at,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("work item");
    assert!(item.active_run_id.is_some());
    assert!(store.verify_all().expect("doctor").is_healthy());
}

/// Every stored row of every ordinary table, keyed for comparison.
///
/// The tables come from SQLite itself rather than from the exporter, so a table
/// the exporter wrongly leaves out cannot hide from this comparison.
fn rows(path: &Path) -> BTreeMap<String, Vec<Vec<Value>>> {
    let connection = Connection::open(path).expect("open");
    let names: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_list
             WHERE schema = 'main' AND type = 'table'
               AND substr(name, 1, 7) COLLATE NOCASE != 'sqlite_'
             ORDER BY name",
        )
        .expect("prepare table list")
        .query_map([], |row| row.get(0))
        .expect("table list")
        .collect::<Result<_, _>>()
        .expect("table names");
    names
        .into_iter()
        .map(|name| {
            let columns: Vec<String> = connection
                .prepare(&format!("PRAGMA table_xinfo({})", quoted(&name)))
                .expect("prepare columns")
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(6)?, row.get::<_, String>(1)?))
                })
                .expect("columns")
                .map(|column| column.expect("column"))
                .filter(|(hidden, _)| *hidden == 0)
                .map(|(_, column)| column)
                .collect();
            let select = format!(
                "SELECT {} FROM {}",
                columns
                    .iter()
                    .map(|column| quoted(column))
                    .collect::<Vec<_>>()
                    .join(", "),
                quoted(&name)
            );
            let mut statement = connection.prepare(&select).expect("select");
            let width = columns.len();
            let mut found = statement
                .query_map([], |row| {
                    (0..width).map(|index| row.get::<_, Value>(index)).collect()
                })
                .expect("rows")
                .collect::<Result<Vec<Vec<Value>>, _>>()
                .expect("row values");
            found.sort_by_key(|row| format!("{row:?}"));
            (name, found)
        })
        .collect()
}

type RecordLinkColumn = (String, String);

/// This inventory covers columns whose schema declares a foreign key to
/// `objects(object_id)`, rather than inferring links from suffixes: compared
/// fingerprints legitimately coexist in the same tables. The objects primary
/// key is the one record id that cannot point back to itself.
fn current_record_link_columns(
    connection: &Connection,
) -> Result<BTreeSet<RecordLinkColumn>, String> {
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_list
             WHERE schema = 'main' AND type = 'table'
               AND substr(name, 1, 7) COLLATE NOCASE != 'sqlite_'
             ORDER BY name",
        )
        .expect("prepare table inventory")
        .query_map([], |row| row.get(0))
        .expect("table inventory")
        .collect::<Result<_, _>>()
        .expect("table names");
    let mut links = BTreeSet::from([("objects".into(), "object_id".into())]);
    for table in tables {
        let foreign_keys: Vec<(String, String, Option<String>)> = connection
            .prepare(&format!("PRAGMA foreign_key_list({})", quoted(&table)))
            .expect("prepare foreign keys")
            .query_map([], |row| Ok((row.get(2)?, row.get(3)?, row.get(4)?)))
            .expect("foreign keys")
            .collect::<Result<_, _>>()
            .expect("foreign key columns");
        for (destination_table, source_column, destination_column) in foreign_keys {
            if !destination_table.eq_ignore_ascii_case("objects") {
                continue;
            }
            match destination_column {
                Some(destination) if destination.eq_ignore_ascii_case("object_id") => {}
                Some(other) => {
                    return Err(format!(
                        "record-link foreign key {table}.{source_column} targets objects.{other}, not objects.object_id"
                    ));
                }
                None => {
                    return Err(format!(
                        "record-link foreign key {table}.{source_column} uses an implicit objects primary key; declare objects(object_id) explicitly"
                    ));
                }
            }
            if !links.insert((table.clone(), source_column.clone())) {
                return Err(format!(
                    "duplicate record-link foreign key {table}.{source_column}"
                ));
            }
        }
    }
    Ok(links)
}

fn mapped_record_link_columns(
    connection: &Connection,
    mappings: &[(&str, &str, &str)],
) -> Result<BTreeSet<RecordLinkColumn>, String> {
    let current = current_record_link_columns(connection)?;
    let mapped: BTreeSet<RecordLinkColumn> = mappings
        .iter()
        .map(|(table, _, destination)| ((*table).into(), (*destination).into()))
        .collect();
    if mapped.len() != mappings.len() {
        return Err("duplicate record-link mapping destination".into());
    }
    let missing = current.difference(&mapped).cloned().collect::<Vec<_>>();
    let unknown = mapped.difference(&current).cloned().collect::<Vec<_>>();
    if !missing.is_empty() || !unknown.is_empty() {
        return Err(format!(
            "record-link mapping mismatch; missing={missing:?}; unknown={unknown:?}"
        ));
    }
    Ok(current)
}

const REBUILT_RECORD_LINK_COLUMNS: &[(&str, &str)] = &[
    ("work_observations", "observation_id"),
    ("work_restored_evidence", "evidence_id"),
    ("work_restored_evidence", "record_id"),
    ("work_restored_records", "record_id"),
];

/// The pre-rename DDL consistently changed a trailing `_id` to `_hash`, with
/// this one named exception where `offer_id` was already an operational id.
const OLD_RECORD_LINK_NAME_EXCEPTIONS: &[(&str, &str, &str)] =
    &[("work_handoff_offers", "offer_object_id", "offer_hash")];

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OldRecordLinkNameMismatch {
    table: String,
    destination: String,
    expected: String,
    actual: String,
}

fn old_record_link_name_mismatches(
    mappings: &[(&str, &str, &str)],
) -> BTreeSet<OldRecordLinkNameMismatch> {
    mappings
        .iter()
        .filter_map(|(table, old, destination)| {
            let expected = OLD_RECORD_LINK_NAME_EXCEPTIONS
                .iter()
                .find(|(exception_table, exception_destination, _)| {
                    exception_table == table && exception_destination == destination
                })
                .map_or_else(
                    || {
                        format!(
                            "{}_hash",
                            destination
                                .strip_suffix("_id")
                                .expect("record-link destination ends in _id")
                        )
                    },
                    |(_, _, source)| (*source).to_owned(),
                );
            (*old != expected).then(|| OldRecordLinkNameMismatch {
                table: (*table).into(),
                destination: (*destination).into(),
                expected,
                actual: (*old).into(),
            })
        })
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RecordLinkExercise {
    header: BTreeSet<RecordLinkColumn>,
    rows: BTreeSet<RecordLinkColumn>,
    non_null_rows: BTreeSet<RecordLinkColumn>,
}

const REQUIRED_NON_NULL_RECORD_LINK_EXERCISE: &[(&str, &str)] = &[
    ("objects", "object_id"),
    ("work_feed_entries", "object_id"),
    ("work_items", "latest_event_id"),
    ("work_root_executions", "head_id"),
];

fn expected_record_link_exercise(
    document: &[Json],
    current_links: &BTreeSet<RecordLinkColumn>,
) -> RecordLinkExercise {
    let declarations = document[0]["engram_export"]["tables"]
        .as_array()
        .expect("exported tables");
    let mut expected = RecordLinkExercise::default();
    for link @ (table, _) in current_links {
        let Some(declaration) = declarations.iter().find(|entry| entry["name"] == *table) else {
            continue;
        };
        expected.header.insert(link.clone());
        if declaration["rows"].as_u64().expect("declared row count") > 0 {
            expected.rows.insert(link.clone());
        }
    }
    expected
}

fn rewrite_record_link_columns(
    document: &mut [Json],
    mappings: &[(&str, &str, &str)],
) -> RecordLinkExercise {
    let mut exercised = RecordLinkExercise::default();
    for (table, old, current) in mappings {
        let Some(declaration) = document[0]["engram_export"]["tables"]
            .as_array_mut()
            .expect("exported tables")
            .iter_mut()
            .find(|entry| entry["name"] == *table)
        else {
            continue;
        };
        let link = ((*table).into(), (*current).into());
        let column = declaration["columns"]
            .as_array_mut()
            .expect("declared columns")
            .iter_mut()
            .find(|column| **column == *current)
            .expect("mapped destination column");
        *column = Json::String((*old).into());
        assert!(exercised.header.insert(link.clone()));
        for line in document.iter_mut().skip(1) {
            if line["row"]["table"] != *table {
                continue;
            }
            let values = line["row"]["values"].as_object_mut().unwrap();
            let value = values.remove(*current).expect("mapped row value");
            if !value.is_null() {
                exercised.non_null_rows.insert(link.clone());
            }
            assert!(values.insert((*old).into(), value).is_none());
            exercised.rows.insert(link.clone());
        }
    }
    exercised
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RecordLinkExerciseDifference {
    missing_header: BTreeSet<RecordLinkColumn>,
    unexpected_header: BTreeSet<RecordLinkColumn>,
    missing_rows: BTreeSet<RecordLinkColumn>,
    unexpected_rows: BTreeSet<RecordLinkColumn>,
}

fn record_link_exercise_difference(
    expected: &RecordLinkExercise,
    exercised: &RecordLinkExercise,
) -> RecordLinkExerciseDifference {
    RecordLinkExerciseDifference {
        missing_header: expected
            .header
            .difference(&exercised.header)
            .cloned()
            .collect(),
        unexpected_header: exercised
            .header
            .difference(&expected.header)
            .cloned()
            .collect(),
        missing_rows: expected.rows.difference(&exercised.rows).cloned().collect(),
        unexpected_rows: exercised.rows.difference(&expected.rows).cloned().collect(),
    }
}

fn missing_required_non_null_record_links(
    exercised: &RecordLinkExercise,
) -> BTreeSet<RecordLinkColumn> {
    REQUIRED_NON_NULL_RECORD_LINK_EXERCISE
        .iter()
        .map(|(table, column)| ((*table).into(), (*column).into()))
        .collect::<BTreeSet<_>>()
        .difference(&exercised.non_null_rows)
        .cloned()
        .collect()
}

fn rebuilt_projection_record_links(
    document: &[Json],
    current_links: &BTreeSet<RecordLinkColumn>,
) -> Result<BTreeSet<RecordLinkColumn>, String> {
    let copied = document[0]["engram_export"]["tables"]
        .as_array()
        .expect("exported tables")
        .iter()
        .map(|entry| entry["name"].as_str().expect("table name"))
        .collect::<BTreeSet<_>>();
    let omitted = current_links
        .iter()
        .filter(|(table, _)| !copied.contains(table.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let left_out = document[0]["engram_export"]["left_out"]
        .as_array()
        .expect("left-out tables");
    for table in omitted
        .iter()
        .map(|(table, _)| table)
        .collect::<BTreeSet<_>>()
    {
        let entries = left_out
            .iter()
            .filter(|entry| entry["name"] == *table)
            .collect::<Vec<_>>();
        if entries.len() != 1 {
            return Err(format!(
                "rebuilt record-link table {table} has {} left_out entries, expected one",
                entries.len()
            ));
        }
        let reason = entries[0]["reason"].as_str().unwrap_or("<missing>");
        if reason != REBUILT_PROJECTION {
            return Err(format!(
                "rebuilt record-link table {table} has left_out reason {reason:?}, expected {REBUILT_PROJECTION:?}"
            ));
        }
    }
    Ok(omitted)
}

#[test]
fn record_link_mapping_inventory_rejects_an_omitted_applicable_column() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("source.jsonl");
    populated(&source);
    let schema = Connection::open(&source).unwrap();
    assert!(
        old_record_link_name_mismatches(RENAMED_COLUMNS).is_empty(),
        "current source names follow the historical DDL convention"
    );

    let mut typo = RENAMED_COLUMNS.to_vec();
    *typo
        .iter_mut()
        .find(|(table, _, destination)| *table == "objects" && *destination == "object_id")
        .unwrap() = ("objects", "object_hahs", "object_id");
    assert_eq!(
        old_record_link_name_mismatches(&typo),
        BTreeSet::from([OldRecordLinkNameMismatch {
            table: "objects".into(),
            destination: "object_id".into(),
            expected: "object_hash".into(),
            actual: "object_hahs".into(),
        }])
    );

    let mut crossed = RENAMED_COLUMNS.to_vec();
    *crossed
        .iter_mut()
        .find(|(table, _, destination)| {
            *table == "work_items" && *destination == "source_snapshot_id"
        })
        .unwrap() = ("work_items", "latest_event_hash", "source_snapshot_id");
    *crossed
        .iter_mut()
        .find(|(table, _, destination)| *table == "work_items" && *destination == "latest_event_id")
        .unwrap() = ("work_items", "source_snapshot_hash", "latest_event_id");
    assert_eq!(
        old_record_link_name_mismatches(&crossed),
        BTreeSet::from([
            OldRecordLinkNameMismatch {
                table: "work_items".into(),
                destination: "latest_event_id".into(),
                expected: "latest_event_hash".into(),
                actual: "source_snapshot_hash".into(),
            },
            OldRecordLinkNameMismatch {
                table: "work_items".into(),
                destination: "source_snapshot_id".into(),
                expected: "source_snapshot_hash".into(),
                actual: "latest_event_hash".into(),
            },
        ])
    );

    let incomplete = RENAMED_COLUMNS
        .iter()
        .copied()
        .filter(|(table, _, current)| !(*table == "work_items" && *current == "latest_event_id"))
        .collect::<Vec<_>>();
    let error = mapped_record_link_columns(&schema, &incomplete)
        .expect_err("an omitted applicable mapping must fail the inventory");
    assert!(
        error.contains("(\"work_items\", \"latest_event_id\")"),
        "{error}"
    );

    let mut unknown = RENAMED_COLUMNS.to_vec();
    unknown.push(("work_items", "unknown_hash", "unknown_id"));
    let error = mapped_record_link_columns(&schema, &unknown)
        .expect_err("an unknown mapping destination must fail the inventory");
    assert!(
        error.contains("(\"work_items\", \"unknown_id\")"),
        "{error}"
    );

    let mut duplicate = RENAMED_COLUMNS.to_vec();
    duplicate.push(("work_items", "other_event_hash", "latest_event_id"));
    let error = mapped_record_link_columns(&schema, &duplicate)
        .expect_err("a duplicate mapping destination must fail the inventory");
    assert_eq!(error, "duplicate record-link mapping destination");

    export_json(&source, &file).unwrap();
    let mut document = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect::<Vec<Json>>();
    let current_links = mapped_record_link_columns(&schema, RENAMED_COLUMNS).unwrap();
    let expected = expected_record_link_exercise(&document, &current_links);
    let exercised = rewrite_record_link_columns(&mut document, &incomplete);
    let omitted = BTreeSet::from([("work_items".into(), "latest_event_id".into())]);
    assert_eq!(
        record_link_exercise_difference(&expected, &exercised),
        RecordLinkExerciseDifference {
            missing_header: omitted.clone(),
            missing_rows: omitted.clone(),
            ..RecordLinkExerciseDifference::default()
        },
        "skipping one copied mapping must name only that exercise gap"
    );
    assert_eq!(
        missing_required_non_null_record_links(&exercised),
        omitted,
        "the named non-null exercise floor must fail for the skipped link"
    );
}

#[test]
fn record_link_inventory_handles_declared_foreign_key_shapes() {
    let implicit = Connection::open_in_memory().unwrap();
    implicit
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE objects (object_id TEXT PRIMARY KEY);
             CREATE TABLE implicit_link (
                 record_id TEXT REFERENCES objects
             );",
        )
        .unwrap();
    let error = current_record_link_columns(&implicit)
        .expect_err("implicit record-link targets must receive a readable refusal");
    assert_eq!(
        error,
        "record-link foreign key implicit_link.record_id uses an implicit objects primary key; declare objects(object_id) explicitly"
    );

    let wrong_target = Connection::open_in_memory().unwrap();
    wrong_target
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE objects (
                 object_id TEXT PRIMARY KEY,
                 other_id TEXT UNIQUE
             );
             CREATE TABLE wrong_link (
                 record_id TEXT REFERENCES objects(other_id)
             );",
        )
        .unwrap();
    let error = current_record_link_columns(&wrong_target)
        .expect_err("a foreign key to another objects column must refuse");
    assert_eq!(
        error,
        "record-link foreign key wrong_link.record_id targets objects.other_id, not objects.object_id"
    );

    let duplicate = Connection::open_in_memory().unwrap();
    duplicate
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE objects (object_id TEXT PRIMARY KEY);
             CREATE TABLE duplicate_link (
                 record_id TEXT,
                 FOREIGN KEY(record_id) REFERENCES objects(object_id),
                 FOREIGN KEY(record_id) REFERENCES objects(object_id)
             );",
        )
        .unwrap();
    let error = current_record_link_columns(&duplicate)
        .expect_err("duplicate record-link foreign keys must refuse");
    assert_eq!(
        error,
        "duplicate record-link foreign key duplicate_link.record_id"
    );

    let mixed_case = Connection::open_in_memory().unwrap();
    mixed_case
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE Objects (OBJECT_ID TEXT PRIMARY KEY);
             CREATE TABLE case_link (
                 record_id TEXT REFERENCES Objects(OBJECT_ID)
             );",
        )
        .unwrap();
    let links = current_record_link_columns(&mixed_case).expect("SQLite identifiers ignore case");
    assert!(links.contains(&("case_link".into(), "record_id".into())));
}

#[test]
fn rebuilt_record_link_exclusions_require_the_exported_reason() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("source.jsonl");
    populated(&source);
    export_json(&source, &file).unwrap();
    let document = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect::<Vec<Json>>();
    let schema = Connection::open(&source).unwrap();
    let current_links = mapped_record_link_columns(&schema, RENAMED_COLUMNS).unwrap();
    rebuilt_projection_record_links(&document, &current_links)
        .expect("current rebuilt record-link reasons");

    let mut missing = document.clone();
    missing[0]["engram_export"]["left_out"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| entry["name"] != "work_observations");
    let error = rebuilt_projection_record_links(&missing, &current_links)
        .expect_err("a missing rebuilt reason must fail");
    assert!(
        error.contains("work_observations has 0 left_out entries"),
        "{error}"
    );

    let mut wrong = document;
    wrong[0]["engram_export"]["left_out"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["name"] == "work_observations")
        .unwrap()["reason"] = Json::String(SEARCH_INDEX.into());
    let error = rebuilt_projection_record_links(&wrong, &current_links)
        .expect_err("a wrong rebuilt reason must fail");
    assert_eq!(
        error,
        format!(
            "rebuilt record-link table work_observations has left_out reason {SEARCH_INDEX:?}, expected {REBUILT_PROJECTION:?}"
        )
    );
}

#[test]
fn renamed_record_columns_preserve_every_value_and_canonical_byte() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("previous.jsonl");
    let target = directory.path().join("target.db");
    populated(&source);
    let before = rows(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let schema = Connection::open(&source).unwrap();
    let current_links = mapped_record_link_columns(&schema, RENAMED_COLUMNS)
        .expect("every current record link has one explicit rename mapping");
    let rebuilt_links = rebuilt_projection_record_links(&document, &current_links)
        .expect("every omitted record-link table is a named rebuilt projection");
    let expected_rebuilt_links = REBUILT_RECORD_LINK_COLUMNS
        .iter()
        .map(|(table, column)| ((*table).into(), (*column).into()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        rebuilt_links, expected_rebuilt_links,
        "rebuilt record-link exclusions are explicit"
    );
    let expected_exercise = expected_record_link_exercise(&document, &current_links);
    let exercised = rewrite_record_link_columns(&mut document, RENAMED_COLUMNS);
    assert_eq!(
        record_link_exercise_difference(&expected_exercise, &exercised),
        RecordLinkExerciseDifference::default(),
        "every copied mapping is admitted by its header and every populated one rewrites rows"
    );
    assert!(
        missing_required_non_null_record_links(&exercised).is_empty(),
        "the populated fixture must retain its named non-null record-link exercise floor"
    );
    let bytes = document
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&file, &bytes).unwrap();
    import_json(&file, &target).expect("explicit old-column conversion");
    assert_eq!(
        rows(&target),
        before,
        "ids, rows, and canonical bytes are unchanged"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), bytes);
}

#[test]
fn old_and_current_record_columns_cannot_alias_one_destination() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("ambiguous.jsonl");
    let target = directory.path().join("target.db");
    populated(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let table = document[0]["engram_export"]["tables"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["name"] == "objects")
        .unwrap();
    table["columns"]
        .as_array_mut()
        .unwrap()
        .push(Json::String("object_hash".into()));
    fs::write(
        &file,
        document
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(
        error.contains("multiple source columns map to column object_id of table objects"),
        "{error}"
    );
    assert!(!target.exists());
}

#[test]
fn missing_required_import_columns_are_named_before_inserting_rows() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("missing.jsonl");
    let target = directory.path().join("target.db");
    populated(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let table = document[0]["engram_export"]["tables"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["name"] == "objects")
        .unwrap();
    table["columns"]
        .as_array_mut()
        .unwrap()
        .retain(|column| column != "canonical_json");
    fs::write(
        &file,
        document
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(
        error.contains("table objects lacks required destination column canonical_json"),
        "{error}"
    );
    assert!(!target.exists());
}

fn populated_control(path: &Path) {
    use crate::domain::*;
    use crate::storage::test_support::{bind_control_for, complete_control_turn, turn_evaluation};
    populated(path);
    let mut store =
        SqliteStore::open_with_host_path_policy(path, crate::HostPathPolicy::host_default())
            .unwrap();
    let now = DateTime::parse_from_rfc3339("2026-09-20T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let effects = [
        EffectClass::Observe,
        EffectClass::Communicate,
        EffectClass::MutateLocal,
    ];
    let binding = bind_control_for(
        &mut store,
        "control-session",
        "migration-bind",
        &effects,
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "migration-sync",
        vec![EffectClass::Observe],
        vec![],
        now,
    );
    let lease = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            LeaseKind::Execution,
            LeaseMode::Exclusive,
            &ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["src".into()],
                coverage: ResourceCoverage::Tree,
            },
            300,
            "migration-lease",
            now + chrono::Duration::seconds(1),
        )
        .unwrap();
    assert!(matches!(lease, WorkLeaseDecision::Granted { .. }));
    for (index, key) in ["migration-first", "migration-replacement"]
        .iter()
        .enumerate()
    {
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: (*key).into(),
                    intent_fingerprint: crate::ObjectId::from_canonical_bytes(key.as_bytes()),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: vec![],
                },
                now + chrono::Duration::seconds(i64::try_from(index).unwrap() + 2),
            )
            .unwrap();
        assert!(matches!(decision, ControlTurnDecision::Grant { .. }));
    }
    store
        .record_turn_observation(&turn_evaluation(binding.status.task_id))
        .unwrap();
    store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("operator"),
            "migration-policy",
            None,
            now + chrono::Duration::seconds(4),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn uncompared_control_checksums_are_counted_and_payloads_survive_import() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("checksums.jsonl");
    let target = directory.path().join("target.db");
    populated_control(&source);
    let before = rows(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut expected = Vec::new();
    for (table, column) in RETIRED_COLUMNS
        .iter()
        .filter(|(table, _)| table.starts_with("control_"))
    {
        let declaration = document[0]["engram_export"]["tables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["name"] == *table)
            .unwrap();
        assert!(
            declaration["rows"].as_u64().unwrap() > 0,
            "fixture must exercise {table}"
        );
        let columns = declaration["columns"].as_array_mut().unwrap();
        assert!(
            !columns.iter().any(|name| name == *column),
            "current schema must not retain {table}.{column}"
        );
        columns.push(Json::String((*column).into()));
        let mut count = 0;
        for line in document.iter_mut().skip(1) {
            if line["row"]["table"] == *table {
                let value = if count == 0 {
                    Json::String("uncompared historical value".into())
                } else {
                    Json::Null
                };
                line["row"]["values"]
                    .as_object_mut()
                    .unwrap()
                    .insert((*column).into(), value);
                count += 1;
            }
        }
        assert!(count > 0);
        expected.push(RetiredField {
            table: (*table).into(),
            column: (*column).into(),
            values: 1,
        });
    }
    fs::write(
        &file,
        document
            .iter()
            .map(Json::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let mut report = import_json(&file, &target).unwrap();
    expected.sort_by(|a, b| (&a.table, &a.column).cmp(&(&b.table, &b.column)));
    report
        .retired_fields
        .sort_by(|a, b| (&a.table, &a.column).cmp(&(&b.table, &b.column)));
    assert_eq!(report.retired_fields, expected);
    assert_eq!(
        rows(&target),
        before,
        "retained JSON, replay intents, ids and rows must survive"
    );
}

#[test]
fn control_format_marker_mismatch_is_named_before_publication() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("marker.jsonl");
    let target = directory.path().join("target.db");
    populated(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let row = document
        .iter_mut()
        .find(|line| line["row"]["table"] == "control_policy_state")
        .unwrap();
    row["row"]["values"]["schema_version"] =
        Json::from(super::super::CONTROL_POLICY_STATE_SCHEMA_VERSION + 1);
    fs::write(
        &file,
        document
            .iter()
            .map(Json::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(
        error.contains("format marker control_policy_state.schema_version"),
        "{error}"
    );
    assert!(!target.exists());
}

/// `populated`, plus the derived state repair builds again: a project memory
/// (the memory-state projection), one session's advertisement of it (delivery
/// bookkeeping) and an observation on the work (the observation projection).
/// Returns the source row count of each of those tables.
fn with_derived_projections(path: &Path) -> BTreeMap<String, u64> {
    populated(path);
    let project = ProjectId("project-json-transfer".into());
    let at = |second: i64| {
        chrono::DateTime::parse_from_rfc3339("2026-09-17T10:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            + chrono::Duration::seconds(second)
    };
    {
        let mut store = SqliteStore::open_unresolved(path).expect("store");
        store
            .remember_project_memory(
                &crate::domain::RememberProjectMemoryRequest {
                    project_id: project.clone(),
                    session_id: crate::SessionId("author".into()),
                    key: Some("transfer-note".into()),
                    revise: false,
                    expected_revision: None,
                    body: "a project note that travels".into(),
                    actor: actor("author"),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("a project memory");
    }
    let short_ref: String = Connection::open(path)
        .expect("open")
        .query_row("SELECT short_ref FROM work_items LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("the work item");
    let observer = crate::work_service::LocalWorkService::new(
        path.to_path_buf(),
        project,
        "observer".into(),
        crate::SessionId("observer-session".into()),
        None,
    );
    observer
        .work_note_on(
            Some(&short_ref),
            "seen while the transfer fixture was built",
            &[],
            at(2),
        )
        .expect("an observation");
    observer
        .work_next(20, crate::work_service::WorkNextQuery::default(), at(3))
        .expect("an advertisement of the memory");
    let connection = Connection::open(path).expect("open");
    [
        "project_memory_state",
        "project_memory_advertisements",
        "work_observations",
    ]
    .into_iter()
    .map(|table| {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count");
        (table.to_owned(), u64::try_from(count).expect("count"))
    })
    .collect()
}

#[test]
fn a_store_round_trips_row_for_row_and_is_healthy() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    with_derived_projections(&source);
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export");
    assert!(exported.rows > 0);
    assert!(
        exported.left_out.iter().all(|left| {
            left.reason == SEARCH_INDEX
                || left.reason == REBUILT_PROJECTION
                || left.reason == DELIVERY_BOOKKEEPING
        }),
        "{:?}",
        exported.left_out
    );

    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");

    let before = rows(&source);
    let after = rows(&target);
    // Delivery bookkeeping is derived state: left out by name, and started
    // empty by the import rather than carried.
    let mut expected = before.clone();
    expected.insert("project_memory_advertisements".into(), Vec::new());
    assert_eq!(after, expected);
    assert!(before["objects"].len() > 3, "the fixture wrote records");

    let store = SqliteStore::open_unresolved(&target).expect("imported store opens");
    assert!(store.verify_all().expect("doctor").is_healthy());
    // The source was only read.
    assert_eq!(rows(&source), before);
}

#[test]
fn every_line_of_the_file_is_plain_json_with_nested_records() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let text = fs::read_to_string(&file).expect("file");
    let parsed = text
        .lines()
        .map(|line| serde_json::from_str::<Json>(line).expect("each line is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(parsed[0]["engram_export"]["format"], FORMAT);
    assert!(parsed.last().expect("end")["end"]["rows"].is_u64());
    let event = parsed
        .iter()
        .find(|line| line["row"]["values"]["object_kind"] == "work_event")
        .expect("a work event row");
    // A record is readable where it sits: nested JSON, not an escaped string.
    assert_eq!(
        event["row"]["values"]["canonical_json"]["json"]["project_id"],
        "project-json-transfer"
    );
}

#[test]
fn values_keep_their_storage_class_and_bytes() {
    for (stored, written) in [
        (Value::Null, serde_json::json!(null)),
        (Value::Integer(i64::MIN), serde_json::json!(i64::MIN)),
        (Value::Real(1.5), serde_json::json!(1.5)),
        (
            Value::Text("{\"a\":1}".into()),
            serde_json::json!("{\"a\":1}"),
        ),
        (
            Value::Blob(b"{\"a\":1,\"b\":[true,null]}".to_vec()),
            serde_json::json!({"json": {"a": 1, "b": [true, null]}}),
        ),
        // Valid JSON that is not in canonical form keeps its exact text.
        (
            Value::Blob(b"{\"b\": 1, \"a\": 2}".to_vec()),
            serde_json::json!({"text": "{\"b\": 1, \"a\": 2}"}),
        ),
        (
            Value::Blob(vec![0xff, 0x00, 0x7f]),
            serde_json::json!({"hex": "ff007f"}),
        ),
    ] {
        let encoded = encode((&stored).into()).expect("encode");
        assert_eq!(encoded, written);
        assert_eq!(decode(encoded).expect("decode"), stored);
    }
    assert!(decode(serde_json::json!(true)).is_err());
    assert!(decode(serde_json::json!({"hex": "f"})).is_err());
    assert!(decode(serde_json::json!(18_446_744_073_709_551_615_u64)).is_err());
}

type Edit = Box<dyn Fn(Vec<String>) -> Vec<String>>;

fn rewritten(file: &Path, edit: impl Fn(Vec<String>) -> Vec<String>) {
    let lines = fs::read_to_string(file)
        .expect("file")
        .lines()
        .map(str::to_owned)
        .collect();
    fs::write(file, edit(lines).join("\n") + "\n").expect("rewrite");
}

#[test]
fn import_refuses_without_publishing_anything() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let target = directory.path().join("target.db");

    let cases: Vec<(&str, Edit, &str)> = vec![
        (
            "truncated",
            Box::new(|mut lines| {
                lines.pop();
                lines
            }),
            "truncated",
        ),
        (
            "missing row",
            Box::new(|mut lines| {
                lines.remove(1);
                lines
            }),
            "truncated or its row counts",
        ),
        (
            "unknown table",
            Box::new(|mut lines| {
                lines[0] =
                    lines[0].replace("\"name\":\"objects\"", "\"name\":\"objects_of_tomorrow\"");
                lines
            }),
            "has no place in the current format",
        ),
        (
            "unknown column",
            Box::new(|mut lines| {
                lines[0] = lines[0].replace("\"object_kind\"", "\"object_flavour\"");
                lines
            }),
            "column object_flavour of table objects has no place",
        ),
        (
            "not an export",
            Box::new(|mut lines| {
                lines[0] = "{\"hello\":1}".into();
                lines
            }),
            "",
        ),
    ];
    for (label, edit, reason) in cases {
        fs::write(&file, &original).expect("restore file");
        rewritten(&file, edit);
        let error = import_json(&file, &target).expect_err(label).to_string();
        assert!(error.contains(reason), "{label}: {error}");
        assert!(!target.exists(), "{label} published a store");
        let leftovers = fs::read_dir(directory.path())
            .expect("directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".engram-migration-")
            })
            .count();
        assert_eq!(leftovers, 0, "{label} left a staging file");
    }

    fs::write(&file, &original).expect("restore file");
    fs::write(&target, b"already here").expect("occupied destination");
    assert!(import_json(&file, &target).is_err());
    assert_eq!(fs::read(&target).expect("kept"), b"already here");
    assert!(
        export_json(&source, &file).is_err(),
        "export never replaces a file"
    );
}

#[test]
fn a_broken_reference_between_rows_is_refused() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    // Drop one record that a feed entry names, and lower the counts to match.
    let text = fs::read_to_string(&file).expect("file");
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let victim = lines
        .iter()
        .position(|line| line.contains("\"object_kind\":\"work_event\""))
        .expect("an event row");
    lines.remove(victim);
    let mut header: Json = serde_json::from_str(&lines[0]).expect("header");
    for table in header["engram_export"]["tables"]
        .as_array_mut()
        .expect("tables")
    {
        if table["name"] == "objects" {
            table["rows"] = Json::from(table["rows"].as_u64().expect("rows") - 1);
        }
    }
    lines[0] = header.to_string();
    let end = lines.len() - 1;
    let mut last: Json = serde_json::from_str(&lines[end]).expect("end");
    last["end"]["rows"] = Json::from(last["end"]["rows"].as_u64().expect("rows") - 1);
    lines[end] = last.to_string();
    fs::write(&file, lines.join("\n") + "\n").expect("rewrite");

    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("dangling reference");
    assert!(matches!(error, MigrationError::Sqlite(_)), "{error}");
    assert!(!target.exists());
}

#[test]
fn a_store_of_the_previous_design_is_refused_by_name() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    {
        // The shape the previous design left behind: its id-pair table beside a
        // copy of every pre-migration record. Nothing in this build knows them,
        // so they are unknown tables like any other, and the build kept beside
        // the old-format backups is the one that reads them.
        let connection = Connection::open(&source).expect("source");
        connection
            .execute_batch(
                "CREATE TABLE migration_object_map (
                     source_hash TEXT PRIMARY KEY, target_hash TEXT NOT NULL, binding_hash TEXT NOT NULL);
                 INSERT INTO migration_object_map VALUES ('old', 'current', 'binding');
                 CREATE TABLE migration_original_objects (object_id TEXT PRIMARY KEY, body BLOB);
                 INSERT INTO migration_original_objects VALUES ('old', x'00');",
            )
            .expect("previous-design fixture");
    }
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export writes the source as it is");
    assert!(
        exported
            .tables
            .iter()
            .any(|table| table.name == "migration_object_map" && table.rows == 1),
        "export carries the table like any other"
    );
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("a table of the previous design");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.starts_with("table migration_")
                && reason.contains("has no place in the current format")),
        "{error}"
    );
    assert!(!target.exists(), "nothing was published");
}

#[test]
fn a_source_table_the_current_format_has_no_place_for_is_refused_without_output() {
    for table in ["object_fts", "work_catalog_fts"] {
        for plain_parent in [false, true] {
            let directory = crate::test_support::temp_home().expect("directory");
            let source = directory.path().join("source.db");
            drop(SqliteStore::open_unresolved(&source).expect("store"));
            let connection = Connection::open(&source).expect("source");
            connection
                .execute_batch(&format!("DROP TABLE {table};"))
                .expect("drop the search index");
            if plain_parent {
                connection
                    .execute_batch(&format!("CREATE TABLE {table}(value TEXT);"))
                    .expect("plain table under the index name");
            }
            connection
                .execute_batch(&format!(
                    "CREATE TABLE {table}_data(value TEXT);
                     INSERT INTO {table}_data VALUES ('preserved');"
                ))
                .expect("plain table under a shadow name");
            drop(connection);
            let before = fs::read(&source).expect("source bytes");
            let file = directory.path().join("export.jsonl");
            export_json(&source, &file).expect("export reads whatever tables exist");
            let output = directory.path().join("imported.db");
            // Both names belong to derived tables in the current format, so the
            // refusal names the collision rather than a missing table.
            let error = import_json(&file, &output).expect_err("a derived destination");
            assert!(
                matches!(&error, MigrationError::Refused(reason)
                    if reason.contains(table) && reason.contains("derived")),
                "{table}: {error}"
            );
            assert!(!output.exists());
            assert_eq!(fs::read(&source).expect("source bytes"), before);
        }
    }
}

#[test]
fn the_store_is_built_in_the_reserved_file_itself() {
    use std::io::Read;

    let directory = crate::test_support::temp_home().expect("directory");
    let staged = Staged::beside(&directory.path().join("out.db")).expect("reserve");
    // A handle on the file that was reserved. If the initializer deleted that
    // file and let SQLite create a replacement — the sequence that widens the
    // permissions — this handle would still refer to the reserved file and
    // would never see a database header through it.
    let mut reserved = fs::File::open(&staged.path).expect("hold the reserved file open");

    let store = SqliteStore::open_unresolved(&staged.path).expect("open the reserved file");
    // The mode is inspected here, before any sensitive row is written, not only
    // on the published result.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let reserved_identity = reserved.metadata().expect("metadata").ino();
        let opened = fs::metadata(&staged.path).expect("metadata");
        assert_eq!(
            opened.ino(),
            reserved_identity,
            "the same file, not a new one"
        );
        assert_eq!(
            opened.permissions().mode() & 0o777,
            0o600,
            "private before the first sensitive write"
        );
    }
    assert!(store.verify_all().expect("doctor").is_healthy());
    drop(store);

    let mut header = [0_u8; 16];
    reserved
        .read_exact(&mut header)
        .expect("the reserved file now holds the store");
    assert_eq!(
        &header, b"SQLite format 3\0",
        "the store was built in the reserved file"
    );

    // What this shows and does not show: the initializer import uses opens the
    // reserved file in place, so the mode it was created with still governs
    // every later write. The journals import itself writes are not observed
    // here; their privacy rests on SQLite matching the database file's mode,
    // which the unix test below exercises on a store of the same mode.
}

#[cfg(unix)]
#[test]
fn a_journal_beside_a_private_store_is_private_too() {
    use std::os::unix::fs::PermissionsExt;

    let directory = crate::test_support::temp_home().expect("directory");
    let staged = Staged::beside(&directory.path().join("out.db")).expect("reserve");
    let store = SqliteStore::open_unresolved(&staged.path).expect("open the reserved file");
    // Hold a write open, so a journal exists while it is inspected.
    store
        .connection
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE probe (value TEXT);")
        .expect("begin a write");
    let mut found = 0;
    for suffix in ["-wal", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", staged.path.display()));
        if sidecar.exists() {
            found += 1;
            assert_eq!(
                fs::metadata(&sidecar)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{suffix} carries the same rows as the store"
            );
        }
    }
    assert!(found > 0, "the fixture needs an active journal");
    store
        .connection
        .execute_batch("ROLLBACK;")
        .expect("release the write");
}

// Windows has no POSIX mode, so this pins the permission itself only where the
// permission exists.
#[cfg(unix)]
#[test]
fn an_imported_store_and_its_journals_stay_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        mode(&target),
        0o600,
        "the imported store holds private data"
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", target.display()));
        if sidecar.exists() {
            assert_eq!(mode(&sidecar), 0o600, "{suffix} carries the same rows");
        }
    }
}

#[test]
fn an_ordinary_table_beside_a_search_index_is_exported_then_refused_by_name() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    {
        // object_fts is a real search index here; object_fts_notes only reads
        // like one of the shadow tables SQLite keeps for it.
        let connection = Connection::open(&source).expect("source");
        connection
            .execute_batch(
                "CREATE TABLE object_fts_notes (note TEXT NOT NULL);
                 INSERT INTO object_fts_notes VALUES ('kept'), ('also kept');",
            )
            .expect("ordinary table beside the index");
        let classified: String = connection
            .query_row(
                "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = 'object_fts'",
                [],
                |row| row.get(0),
            )
            .expect("classification");
        assert_eq!(classified, "virtual", "the fixture keeps the real index");
    }
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export");
    assert!(
        exported
            .tables
            .iter()
            .any(|table| table.name == "object_fts_notes" && table.rows == 2),
        "{:?}",
        exported.tables
    );
    assert!(
        !exported
            .left_out
            .iter()
            .any(|left| left.name == "object_fts_notes")
    );

    // The current format has no such table, so import refuses it by name
    // instead of dropping rows it reported as exported.
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("no place for the table");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("object_fts_notes") && reason.contains("no place")),
        "{error}"
    );
    assert!(!target.exists());
}

#[test]
fn a_virtual_table_this_build_cannot_rebuild_is_refused_by_name() {
    // The second name starts with a supported index, so a prefix test would
    // take it for one of that index's shadow tables and drop its rows.
    for stray in ["stray_fts", "object_fts_extra"] {
        let directory = crate::test_support::temp_home().expect("directory");
        let source = directory.path().join("source.db");
        populated(&source);
        {
            let connection = Connection::open(&source).expect("source");
            connection
                .execute_batch(&format!(
                    "CREATE VIRTUAL TABLE {stray} USING fts5(body);
                     INSERT INTO {stray}(body) VALUES ('rows nobody may drop');"
                ))
                .expect("unknown search index");
            let classified: String = connection
                .query_row(
                    "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
                    [stray],
                    |row| row.get(0),
                )
                .expect("classification");
            assert_eq!(classified, "virtual", "{stray}");
            let parent: String = connection
                .query_row(
                    "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = 'object_fts'",
                    [],
                    |row| row.get(0),
                )
                .expect("classification");
            assert_eq!(parent, "virtual", "the real index is still present");
        }
        let file = directory.path().join("export.jsonl");
        let error = export_json(&source, &file).expect_err("unknown virtual table");
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains(stray) && reason.contains("copies or rebuilds")),
            "{stray}: {error}"
        );
        assert!(!file.exists());
    }

    // Positive control: the supported indexes and their real shadow tables are
    // still reported as rebuilt, and the store still exports.
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export");
    for index in SUPPORTED_SEARCH_INDEXES {
        assert!(
            exported
                .left_out
                .iter()
                .any(|left| left.name == *index && left.reason.contains("search index")),
            "{index} is rebuilt, not copied"
        );
        assert!(
            exported
                .left_out
                .iter()
                .any(|left| left.name == format!("{index}_data")),
            "the shadow tables of {index} are rebuilt too"
        );
    }
}

#[test]
fn an_undeclared_trigger_refuses_and_an_undeclared_index_is_named() {
    let directory = crate::test_support::temp_home().expect("directory");
    let indexed = directory.path().join("indexed.db");
    populated(&indexed);
    {
        let connection = Connection::open(&indexed).expect("source");
        connection
            .execute_batch("CREATE INDEX rogue_work_items_priority ON work_items(priority);")
            .expect("undeclared index");
    }
    let file = directory.path().join("indexed.jsonl");
    let exported = export_json(&indexed, &file).expect("an index is derived data");
    assert!(
        exported
            .left_out
            .iter()
            .any(|left| left.name == "rogue_work_items_priority"
                && left.reason.contains("does not declare")),
        "{:?}",
        exported.left_out
    );
    let target = directory.path().join("indexed-target.db");
    import_json(&file, &target).expect("import");
    let connection = Connection::open(&target).expect("target");
    let rogue: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'rogue_work_items_priority'",
            [],
            |row| row.get(0),
        )
        .expect("schema");
    assert_eq!(rogue, 0, "the new store has the current indexes");

    let triggered = directory.path().join("triggered.db");
    populated(&triggered);
    {
        let connection = Connection::open(&triggered).expect("source");
        connection
            .execute_batch(
                "CREATE TRIGGER rogue_work_items_guard BEFORE DELETE ON work_items
                 BEGIN SELECT RAISE(ABORT, 'no'); END;",
            )
            .expect("undeclared trigger");
    }
    let refused = directory.path().join("triggered.jsonl");
    let error = export_json(&triggered, &refused).expect_err("a trigger is behaviour");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("rogue_work_items_guard") && reason.contains("not declared")),
        "{error}"
    );
    assert!(!refused.exists());
}

/// A store whose session has a delivery page staged and not yet acknowledged.
fn staged_pending_delivery(database: &Path) -> (ProjectId, crate::SessionId) {
    let project = ProjectId("project-pending-transfer".into());
    let session = crate::SessionId("pending-session".into());
    let service = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "author".into(),
        session.clone(),
        None,
    );
    let at = |second: i64| {
        chrono::DateTime::parse_from_rfc3339("2026-09-17T10:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            + chrono::Duration::seconds(second)
    };
    let peer = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "peer".into(),
        crate::SessionId("peer-session".into()),
        None,
    );
    peer.work_propose(
        crate::work_service::WorkProposeInput::Root {
            acceptance_bindings: Vec::new(),
            evaluation_mode: None,
            external_ref: None,
            notes: Vec::new(),
            title: "Work another session proposed".into(),
            outcome: "the page carries a peer change".into(),
            acceptance: vec!["the peer change stays a peer change".into()],
            work_kind: None,
            priority: None,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            idempotency_key: "peer-root".into(),
        },
        at(1),
    )
    .expect("peer root");
    service
        .work_propose(
            crate::work_service::WorkProposeInput::Root {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                title: "Carry a staged delivery".into(),
                outcome: "the page survives".into(),
                acceptance: vec!["the page replays".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "pending-root".into(),
            },
            at(1),
        )
        .expect("root");
    service
        .work_next(20, crate::work_service::WorkNextQuery::default(), at(2))
        .expect("stage a delivery page");
    assert!(
        pending_cursor(database).is_some(),
        "the fixture needs a page staged but unacknowledged"
    );
    (project, session)
}

/// The confirmed cursor, the cursor a page is staged through, and its capability.
fn pending_state(path: &Path) -> (i64, i64, Option<String>) {
    Connection::open(path)
        .expect("open")
        .query_row(
            "SELECT project_cursor, tentative_project_cursor, tentative_delivery_token
             FROM work_session_state WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("a staged page")
}

/// The cursor a page is staged through, when one is pending.
fn pending_cursor(path: &Path) -> Option<i64> {
    Connection::open(path)
        .expect("open")
        .query_row(
            "SELECT tentative_project_cursor FROM work_session_state
             WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("staged cursor")
}

/// The session holding a staged page, and the page.
fn pending_payload(path: &Path) -> (String, Json) {
    let connection = Connection::open(path).expect("open");
    let (id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT session_id, tentative_delivery_payload
             FROM work_session_state WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("a staged page");
    (
        id,
        serde_json::from_slice(&payload).expect("staged page json"),
    )
}

#[test]
fn a_staged_delivery_page_survives_the_transfer_and_replays_at_its_confirmed_cursor() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let (project, session) = staged_pending_delivery(&source);
    let (_, before) = pending_payload(&source);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let imported = import_json(&file, &target).expect("import");
    assert_eq!(imported.checked_pending_deliveries, 1);
    let (_, after) = pending_payload(&target);
    assert_eq!(after, before, "the frozen payload is carried verbatim");

    assert_replays_without_acknowledging(&target, &project, &session, &before);
}

/// The pending state a source holds before a transfer, captured for comparison.
struct SourcePending {
    confirmed: i64,
    through: i64,
    token: Option<String>,
    source_bytes: Vec<u8>,
}

fn source_pending(source: &Path) -> SourcePending {
    let (confirmed, through, token) = pending_state(source);
    SourcePending {
        confirmed,
        through,
        token,
        source_bytes: fs::read(source).expect("source bytes"),
    }
}

/// One transfer under test: the store it read, the file it wrote and that
/// file's bytes as written, the store it published, and whose page it carried.
struct Transfer<'a> {
    source: &'a Path,
    export: &'a Path,
    export_bytes_before: &'a [u8],
    target: &'a Path,
    project: &'a ProjectId,
    session: &'a crate::SessionId,
}

/// Proves the transfer carried the source's pending delivery state: the target
/// holds the same confirmed cursor, staged cursor and delivery capability the
/// source held before export, the page replays there at the confirmed cursor
/// without being acknowledged away, and neither the source nor the export file
/// changed in the process.
fn assert_transfer_keeps_pending_state(
    transfer: &Transfer<'_>,
    before: &SourcePending,
    expected_page: &Json,
) {
    let Transfer {
        source,
        export,
        export_bytes_before,
        target,
        project,
        session,
    } = *transfer;
    let (confirmed, through, token) = pending_state(target);
    assert_eq!(
        (confirmed, through, &token),
        (before.confirmed, before.through, &before.token),
        "import carried the source's cursors and delivery capability"
    );
    assert_replays_without_acknowledging(target, project, session, expected_page);
    let (still_confirmed, still_through, still_token) = pending_state(target);
    assert_eq!(
        (still_confirmed, still_through, &still_token),
        (before.confirmed, before.through, &before.token),
        "replay changed nothing"
    );
    assert_eq!(
        fs::read(source).expect("source bytes"),
        before.source_bytes,
        "the source was only read"
    );
    assert_eq!(
        fs::read(export).expect("export bytes"),
        export_bytes_before,
        "the export file was only read"
    );
}

/// Replays the retained page the way a core retry does: at the cursor already
/// confirmed, with no acknowledgement capability, so the pending page is
/// re-read rather than cleared. Proves the page, its capability and the cursors
/// are exactly as they were afterwards.
fn assert_replays_without_acknowledging(
    database: &Path,
    project: &ProjectId,
    session: &crate::SessionId,
    expected: &Json,
) {
    let (confirmed, through, token) = pending_state(database);
    let service = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "author".into(),
        session.clone(),
        None,
    );
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-17T11:00:00Z")
        .expect("time")
        .with_timezone(&Utc);
    let replayed = service
        .work_next_with_delivery_token(
            20,
            Some(confirmed),
            None,
            crate::work_service::WorkNextQuery::default(),
            at,
        )
        .expect("the retained page replays");
    let delivered = replayed.changes.as_ref().expect("the retained page");
    let staged = expected["changes"].as_array().expect("changes");
    assert_eq!(delivered.len(), staged.len(), "the exact retained entries");
    for (delivered, staged) in delivered.iter().zip(staged) {
        let delivered = serde_json::to_value(delivered).expect("change");
        assert_eq!(delivered["entry"], staged["entry"], "the exact feed entry");
        assert_eq!(
            delivered["from_current_session"].as_bool().unwrap_or(false),
            staged["from_current_session"].as_bool().unwrap_or(false),
            "the exact attribution"
        );
    }
    // Nothing was acknowledged: the same page, capability and cursors remain.
    let (still_confirmed, still_through, still_token) = pending_state(database);
    assert_eq!((still_confirmed, still_through), (confirmed, through));
    assert_eq!(still_token, token, "the delivery capability is unchanged");
    let (_, unchanged) = pending_payload(database);
    assert_eq!(&unchanged, expected, "the frozen page is unchanged");
}

#[test]
fn a_staged_page_that_omits_the_attribution_the_source_proves_is_refused() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (id, mut payload) = pending_payload(&source);
    // A page that leaves out the bit saying a change is the session's own reads
    // as claiming it is not, which the record contradicts. Nothing is supplied
    // on its behalf: the page is refused before publication, exactly as the
    // next retry would refuse it.
    let mut stripped = 0;
    for change in payload["changes"].as_array_mut().expect("changes") {
        if change
            .as_object_mut()
            .expect("change")
            .remove("from_current_session")
            .is_some_and(|bit| bit == Json::Bool(true))
        {
            stripped += 1;
        }
    }
    assert!(stripped > 0, "the fixture needs an own-session change");
    write_pending_payload(&source, &id, &payload);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("omitted attribution");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("pending-session") && reason.contains("attribution differs")),
        "{error}"
    );
    assert!(!target.exists(), "nothing was published");
}

#[test]
fn a_staged_page_whose_attribution_the_source_denies_refuses_before_publication() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (id, mut payload) = pending_payload(&source);
    // A claim in the other direction: the page says a change another session
    // made is this session's own. Nothing may supply that, because the record
    // names the other session. An absent bit is indistinguishable from false,
    // so only this direction can be contradicted.
    let mut claimed = 0;
    for change in payload["changes"].as_array_mut().expect("changes") {
        let change = change.as_object_mut().expect("change");
        if change.get("from_current_session") != Some(&Json::Bool(true)) {
            change.insert("from_current_session".into(), Json::Bool(true));
            claimed += 1;
        }
    }
    assert!(claimed > 0, "the fixture needs a peer change");
    write_pending_payload(&source, &id, &payload);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("contradicted attribution");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("pending-session") && reason.contains("attribution differs")),
        "{error}"
    );
    assert!(!target.exists());
}

fn write_pending_payload(path: &Path, id: &str, payload: &Json) {
    let bytes = serde_json_canonicalizer::to_vec(payload).expect("canonical page");
    let connection = Connection::open(path).expect("open");
    let changed = connection
        .execute(
            "UPDATE work_session_state SET tentative_delivery_payload = ?2
             WHERE session_id = ?1",
            rusqlite::params![id, bytes],
        )
        .expect("rewrite the staged page");
    assert_eq!(changed, 1);
}

#[test]
fn a_copied_table_may_not_take_the_place_of_a_derived_one() {
    // An ordinary source table with a search index's name and exact columns
    // would pass a column check, be inserted, and then be thrown away when the
    // index is rebuilt, while the report counted its rows as copied. The same
    // holds for one wearing a shadow table's name and columns.
    for (name, columns, row) in [
        (
            "object_fts",
            "object_id TEXT, title TEXT, body TEXT",
            "('independent', 'a row of its own', 'not derived from anything')",
        ),
        (
            "object_fts_data",
            "id INTEGER PRIMARY KEY, block BLOB",
            "(1, x'00')",
        ),
    ] {
        let directory = crate::test_support::temp_home().expect("directory");
        let source = directory.path().join("source.db");
        populated(&source);
        {
            let connection = Connection::open(&source).expect("source");
            connection
                .execute_batch(&format!(
                    "DROP TABLE object_fts;
                     CREATE TABLE {name} ({columns});
                     INSERT INTO {name} VALUES {row};"
                ))
                .expect("an ordinary table under a derived name");
            let classified: String = connection
                .query_row(
                    "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .expect("classification");
            assert_eq!(classified, "table", "{name} is ordinary in the source");
        }
        let before = fs::read(&source).expect("source bytes");
        let file = directory.path().join("export.jsonl");
        let exported = export_json(&source, &file).expect("an ordinary table exports");
        assert!(
            exported
                .tables
                .iter()
                .any(|table| table.name == name && table.rows == 1),
            "{name}: {:?}",
            exported.tables
        );

        let target = directory.path().join("target.db");
        let error = import_json(&file, &target).expect_err("a derived destination");
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains(name) && reason.contains("derived")),
            "{name}: {error}"
        );
        assert!(!target.exists(), "{name} published a store");
        let leftovers = fs::read_dir(directory.path())
            .expect("directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".engram-migration-")
            })
            .count();
        assert_eq!(leftovers, 0, "{name} left a staging file");
        assert_eq!(fs::read(&source).expect("source bytes"), before, "{name}");
    }
}

#[test]
fn a_hex_blob_is_read_from_an_explicit_alphabet_only() {
    // A numeric sign is not a hex digit, even where a number parser would take
    // one; nor is anything outside the alphabet, an odd length, or non-ASCII.
    for malformed in ["+f", "-f", "zz", "0g", "f", "abc", "é0", "0x", " 0"] {
        assert!(
            decode(serde_json::json!({ "hex": malformed })).is_err(),
            "{malformed:?} must be refused"
        );
    }
    // Both cases of the alphabet are accepted, deliberately, and the bytes are
    // exactly the bytes written.
    for (written, bytes) in [
        ("ff007f", vec![0xff, 0x00, 0x7f]),
        ("FF007F", vec![0xff, 0x00, 0x7f]),
        ("aBcD", vec![0xab, 0xcd]),
        ("", Vec::new()),
    ] {
        assert_eq!(
            decode(serde_json::json!({ "hex": written })).expect("valid hex"),
            Value::Blob(bytes),
            "{written}"
        );
    }
    // Export writes the alphabet's lowercase form, and it reads back.
    let round = encode((&Value::Blob(vec![0xde, 0xad, 0xbe, 0xef])).into()).expect("encode");
    assert_eq!(round, serde_json::json!({ "hex": "deadbeef" }));
    assert_eq!(
        decode(round).expect("decode"),
        Value::Blob(vec![0xde, 0xad, 0xbe, 0xef])
    );
}

#[test]
fn a_failed_publication_names_the_operation_and_keeps_the_cause() {
    use std::io::{Error, ErrorKind};

    let destination = Path::new("C:/somewhere/new-store.db");
    let mapped = |kind: ErrorKind| publish_failure(destination, &Error::new(kind, "the cause"));
    assert!(matches!(
        mapped(ErrorKind::AlreadyExists),
        MigrationError::Refused(reason) if reason == "destination already exists"
    ));
    for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
        assert!(
            matches!(mapped(kind), MigrationError::Refused(reason)
                if reason.contains("hard link") && reason.contains("new-store.db") && reason.contains("the cause")),
            "{kind:?}"
        );
    }
    // Any other failure keeps its kind and its cause, and says what was
    // being done, instead of arriving as a bare I/O error.
    for kind in [
        ErrorKind::Other,
        ErrorKind::NotFound,
        ErrorKind::Interrupted,
    ] {
        let MigrationError::Io(error) = mapped(kind) else {
            panic!("{kind:?} is an I/O failure");
        };
        assert_eq!(error.kind(), kind);
        let text = error.to_string();
        assert!(
            text.contains("publishing") && text.contains("new-store.db"),
            "{text}"
        );
        assert!(text.contains("the cause"), "{text}");
    }
}

#[test]
fn a_retired_column_is_named_and_its_rows_go_in_without_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let (project, session) = staged_pending_delivery(&source);
    let (_, before) = pending_payload(&source);
    {
        // A store written before the column was retired still carries it,
        // with a value on the staged row.
        let connection = Connection::open(&source).expect("source");
        connection
            .execute_batch(
                "ALTER TABLE work_session_state ADD COLUMN tentative_delivery_payload_hash TEXT;
                 UPDATE work_session_state SET tentative_delivery_payload_hash = 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'
                 WHERE tentative_project_cursor IS NOT NULL;",
            )
            .expect("the retired column, as an older store holds it");
    }
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export writes the source as it is");
    let state = exported
        .tables
        .iter()
        .find(|table| table.name == "work_session_state")
        .expect("the session table");
    assert!(
        state
            .columns
            .iter()
            .any(|column| column == "tentative_delivery_payload_hash"),
        "export carries the retired column"
    );

    let before_transfer = source_pending(&source);
    let export_bytes = fs::read(&file).expect("export bytes");
    let target = directory.path().join("target.db");
    let imported = import_json(&file, &target).expect("import names the retired column");
    assert_eq!(
        imported.retired_fields,
        vec![RetiredField {
            table: "work_session_state".into(),
            column: "tentative_delivery_payload_hash".into(),
            values: 1,
        }],
        "the field is reported apart from any rows left out"
    );
    assert!(
        !imported
            .left_out
            .iter()
            .any(|left| left.name.contains("payload_hash")),
        "a retired column is not a row count"
    );
    let connection = Connection::open(&target).expect("target");
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(work_session_state)")
        .expect("columns")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("columns")
        .collect::<Result<_, _>>()
        .expect("column names");
    assert!(
        !columns
            .iter()
            .any(|column| column == "tentative_delivery_payload_hash")
    );
    // The page went in without it, under the source's own cursors and delivery
    // capability, and still replays there at its confirmed cursor.
    assert_transfer_keeps_pending_state(
        &Transfer {
            source: &source,
            export: &file,
            export_bytes_before: &export_bytes,
            target: &target,
            project: &project,
            session: &session,
        },
        &before_transfer,
        &before,
    );
}

#[test]
fn an_unknown_column_beside_a_retired_one_still_refuses_by_name() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    {
        let connection = Connection::open(&source).expect("source");
        connection
            .execute_batch(
                "ALTER TABLE work_session_state ADD COLUMN tentative_delivery_payload_hash TEXT;
                 ALTER TABLE work_session_state ADD COLUMN stray_note TEXT;",
            )
            .expect("a retired column and an unknown one");
    }
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("an unknown column");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("stray_note") && reason.contains("no place")),
        "{error}"
    );
    assert!(!target.exists());
}

#[test]
fn a_current_store_carries_no_retired_fields() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let (project, session) = staged_pending_delivery(&source);
    let (_, page) = pending_payload(&source);
    let before = source_pending(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let export_bytes = fs::read(&file).expect("export bytes");
    let target = directory.path().join("target.db");
    let imported = import_json(&file, &target).expect("import");
    assert!(imported.retired_fields.is_empty());
    assert_transfer_keeps_pending_state(
        &Transfer {
            source: &source,
            export: &file,
            export_bytes_before: &export_bytes,
            target: &target,
            project: &project,
            session: &session,
        },
        &before,
        &page,
    );
}

/// Rewrites the first stored record's blob in an export file as `{"hex": …}`.
fn with_first_record_blob_as_hex(file: &Path, hex: &str) {
    with_row_cell(
        file,
        "objects",
        |_| true,
        "canonical_json",
        serde_json::json!({ "hex": hex }),
    );
}

/// Sets one cell of the first row of `table` in the file that `select` admits.
fn with_row_cell(
    file: &Path,
    table: &str,
    select: impl Fn(&Json) -> bool,
    column: &str,
    value: Json,
) {
    let text = fs::read_to_string(file).expect("file");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let marker = format!("\"table\":\"{table}\"");
    let at = lines
        .iter()
        .position(|line| {
            line.contains(&marker)
                && serde_json::from_str::<Json>(line).is_ok_and(|row| select(&row["row"]["values"]))
        })
        .expect("a row of the table");
    let mut row: Json = serde_json::from_str(&lines[at]).expect("row");
    row["row"]["values"][column] = value;
    lines[at] = row.to_string();
    fs::write(file, lines.join("\n") + "\n").expect("rewrite");
}

/// Staging files an import left behind in `directory`.
fn staging_leftovers(directory: &Path) -> usize {
    fs::read_dir(directory)
        .expect("directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".engram-migration-")
        })
        .count()
}

#[test]
fn a_malformed_hex_blob_refuses_the_import_and_a_valid_one_is_the_same_bytes() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let source_bytes = fs::read(&source).expect("source bytes");

    // Valid control: the first record's canonical bytes written as hex decode
    // to the identical bytes, so the store round-trips row for row.
    let (first_id, first_bytes): (String, Vec<u8>) = Connection::open(&source)
        .expect("source")
        .query_row(
            "SELECT object_id, canonical_json FROM objects ORDER BY rowid LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("first record");
    let hex = first_bytes.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    let valid = directory.path().join("valid.jsonl");
    fs::copy(&file, &valid).expect("copy");
    with_first_record_blob_as_hex(&valid, &hex);
    let target = directory.path().join("valid-target.db");
    import_json(&valid, &target).expect("valid hex imports");
    let stored: Vec<u8> = Connection::open(&target)
        .expect("target")
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [&first_id],
            |row| row.get(0),
        )
        .expect("the record");
    assert_eq!(stored, first_bytes, "the same bytes arrived through hex");
    assert_eq!(
        rows(&target),
        rows(&source)
            .into_iter()
            .map(|(name, rows)| {
                if name == "project_memory_advertisements" {
                    (name, Vec::new())
                } else {
                    (name, rows)
                }
            })
            .collect()
    );

    // Malformed: the same slot with a sign the alphabet excludes. Import must
    // reach the decoder and refuse there, publishing nothing and leaving
    // nothing behind.
    let malformed = directory.path().join("malformed.jsonl");
    fs::copy(&file, &malformed).expect("copy");
    with_first_record_blob_as_hex(&malformed, "+f");
    let input_bytes = fs::read(&malformed).expect("input bytes");
    let refused_target = directory.path().join("malformed-target.db");
    let error = import_json(&malformed, &refused_target).expect_err("malformed hex");
    assert!(
        matches!(&error, MigrationError::Refused(reason) if reason.contains("hex blob")),
        "{error}"
    );
    assert!(!refused_target.exists(), "nothing was published");
    let leftovers = fs::read_dir(directory.path())
        .expect("directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".engram-migration-")
        })
        .count();
    assert_eq!(leftovers, 0, "no staging file was left");
    assert_eq!(
        fs::read(&malformed).expect("input bytes"),
        input_bytes,
        "the input was only read"
    );
    assert_eq!(
        fs::read(&source).expect("source bytes"),
        source_bytes,
        "the source was only read"
    );
}

#[test]
fn a_pending_delivery_the_file_left_incomplete_is_refused_by_session() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let staged = |values: &Json| values["session_id"] == "pending-session";

    // The schema permits a row with only part of a pending delivery; the next
    // retry of that session refuses to read one. Either missing part must refuse
    // the import instead, so the published store never holds it.
    for column in ["tentative_delivery_payload", "tentative_delivery_token"] {
        fs::write(&file, &original).expect("restore file");
        with_row_cell(&file, "work_session_state", staged, column, Json::Null);
        let target = directory.path().join(format!("without-{column}.db"));
        let error = import_json(&file, &target).expect_err(column);
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains("pending-session") && reason.contains("present together")),
            "{column}: {error}"
        );
        assert!(!target.exists(), "{column}: a store was published");
        assert_eq!(
            staging_leftovers(directory.path()),
            0,
            "{column}: a staging file was left"
        );
    }

    // Control: the same row as exported goes in, and is counted as checked.
    fs::write(&file, &original).expect("restore file");
    let target = directory.path().join("complete.db");
    let imported = import_json(&file, &target).expect("the complete row imports");
    assert_eq!(imported.checked_pending_deliveries, 1);
}

#[test]
fn a_refused_cell_is_named_by_its_place_and_never_printed() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    // The export carries private and restricted bodies, and a refusal reaches
    // the operator's terminal: it names where and what shape, not what.
    let sentinel = "a private note nobody else may read";
    // A number is a value too: one above i64::MAX, whose digits must not
    // appear in the refusal any more than the sentinel does.
    let private_number = "18446744073709551615";
    let shapes = [
        ("an array", serde_json::json!([sentinel]), sentinel),
        (
            "a blob wrapper with an extra key",
            serde_json::json!({ "json": { "body": sentinel }, "encoding": "json" }),
            sentinel,
        ),
        (
            "a text blob that is not a string",
            serde_json::json!({ "text": [sentinel] }),
            sentinel,
        ),
        (
            "a wrapper of an unknown kind",
            serde_json::json!({ "body": sentinel }),
            sentinel,
        ),
        ("a boolean", Json::Bool(true), sentinel),
        (
            "a number outside a stored integer",
            serde_json::json!(18_446_744_073_709_551_615_u64),
            private_number,
        ),
    ];
    for (label, value, private) in shapes {
        fs::write(&file, &original).expect("restore file");
        with_row_cell(&file, "objects", |_| true, "canonical_json", value);
        let target = directory.path().join("target.db");
        let error = import_json(&file, &target).expect_err(label);
        assert!(
            matches!(&error, MigrationError::Refused(_)),
            "{label}: {error}"
        );
        let text = error.to_string();
        assert!(
            text.contains("column canonical_json of a row of table objects"),
            "{label}: {text}"
        );
        assert!(!text.contains(private), "{label} printed the cell: {text}");
        assert!(
            !text.contains("body"),
            "{label} printed a key of the cell: {text}"
        );
        assert!(!target.exists(), "{label}: a store was published");
        assert_eq!(
            staging_leftovers(directory.path()),
            0,
            "{label}: a staging file was left"
        );
    }
}

#[test]
fn a_sidecar_beside_the_destination_refuses_the_import() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    // A log or journal left at the destination's name belongs to another
    // database, and SQLite would apply it to the imported one. Each is refused
    // by name before anything is staged.
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = target.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        fs::write(&sidecar, b"left behind by the old database").expect("sidecar");
        let error = import_json(&file, &target).expect_err(suffix);
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains(suffix) && reason.contains("beside the destination")),
            "{suffix}: {error}"
        );
        assert!(!target.exists(), "{suffix}: a store was published");
        assert_eq!(
            staging_leftovers(directory.path()),
            0,
            "{suffix}: a staging file was left"
        );
        fs::remove_file(&sidecar).expect("remove the sidecar");
    }
    import_json(&file, &target).expect("with nothing beside it, the import publishes");
}

#[test]
fn a_malformed_staged_page_is_refused_without_printing_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (_, page) = pending_payload(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let staged = |values: &Json| values["session_id"] == "pending-session";
    // The page carries work titles and actor context. A page this build cannot
    // read is refused by session with the shape of the problem, never with
    // what the page holds.
    let sentinel = "a private title nobody else may read";
    let mut wrong_field = page.clone();
    wrong_field["changes"][0]["entry"]["position"] = Json::String(sentinel.into());
    // A page that decodes but names a record the feed does not hold at that
    // position fails in the verifier, whose reason is restated, not repeated:
    // the id it named must not come back either.
    let unknown_record = "f".repeat(32);
    let mut wrong_record = page.clone();
    wrong_record["changes"][0]["entry"]["object_hash"] = Json::String(unknown_record.clone());
    let shapes = [
        (
            "a page that is one string",
            serde_json::json!({ "json": sentinel }),
            "not a delivery page",
            sentinel,
        ),
        (
            "a page with a field of the wrong type",
            serde_json::json!({ "json": wrong_field }),
            "not a delivery page",
            sentinel,
        ),
        (
            "bytes that are not JSON",
            serde_json::json!({ "text": format!("{{ not json {sentinel}") }),
            "not a delivery page",
            sentinel,
        ),
        (
            "a page naming a record the feed does not hold there",
            serde_json::json!({ "json": wrong_record }),
            "dense source interval",
            unknown_record.as_str(),
        ),
    ];
    for (label, value, expected, private) in shapes {
        fs::write(&file, &original).expect("restore file");
        with_row_cell(
            &file,
            "work_session_state",
            staged,
            "tentative_delivery_payload",
            value,
        );
        let target = directory.path().join("target.db");
        let error = import_json(&file, &target).expect_err(label);
        let text = error.to_string();
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains("pending-session") && reason.contains(expected)),
            "{label}: {text}"
        );
        assert!(!text.contains(private), "{label} printed the page: {text}");
        assert!(!target.exists(), "{label}: a store was published");
    }
}

#[test]
fn the_report_counts_only_rows_the_published_store_holds() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let derived = with_derived_projections(&source);
    for (table, rows) in &derived {
        assert!(*rows > 0, "the fixture needs rows in {table}");
    }
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let imported = import_json(&file, &target).expect("import");
    // A derived table is named with the rows it held, never counted, on both
    // sides of the transfer.
    for (table, rows) in &derived {
        for (report, named) in [
            ("export", &exported.left_out),
            ("import", &imported.left_out),
        ] {
            assert!(
                named
                    .iter()
                    .any(|left| &left.name == table && left.rows == *rows),
                "{report} does not name {table} with its {rows} rows: {named:?}"
            );
        }
        assert!(
            imported.tables.iter().all(|counted| &counted.name != table),
            "{table} was counted as imported"
        );
    }
    let published = rows(&target);
    for table in &imported.tables {
        let held = u64::try_from(published.get(&table.name).map_or(0, Vec::len)).expect("count");
        assert_eq!(
            held, table.rows,
            "{} is reported with rows the published store does not hold",
            table.name
        );
    }
    assert_eq!(
        imported.rows,
        imported.tables.iter().map(|table| table.rows).sum::<u64>()
    );
    // Repair derived the projections again from the records that arrived; the
    // delivery bookkeeping starts empty.
    let source_rows = rows(&source);
    assert_eq!(
        published["project_memory_state"],
        source_rows["project_memory_state"]
    );
    assert_eq!(
        published["work_observations"],
        source_rows["work_observations"]
    );
    assert!(published["project_memory_advertisements"].is_empty());
}

#[test]
fn an_autoincrement_mark_above_the_surviving_rows_is_carried_across() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    // A dense cursor table whose highest rows were deleted keeps its mark, so
    // that no later row takes a sequence a session already confirmed.
    Connection::open(&source)
        .expect("source")
        .execute_batch(
            "DELETE FROM sqlite_sequence WHERE name = 'task_changes';
             INSERT INTO sqlite_sequence (name, seq) VALUES ('task_changes', 1000);",
        )
        .expect("a mark above every surviving row");
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");
    let mark: i64 = Connection::open(&target)
        .expect("target")
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'task_changes'",
            [],
            |row| row.get(0),
        )
        .expect("the mark travelled");
    assert_eq!(mark, 1000);
}

#[test]
fn an_export_reads_the_source_wal_and_reports_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    {
        // A consumer that stopped without a checkpoint leaves committed rows in
        // the write-ahead log. Export reads them, and says the log is there.
        let connection = Connection::open(&source).expect("source");
        connection
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .expect("no checkpoint on close");
        connection
            .execute(
                "INSERT INTO agent_context_revisions (project_id, agent_id, revision)
                 VALUES ('wal-project', 'wal-agent', 7)",
                [],
            )
            .expect("a row committed to the log");
    }
    let wal = {
        let mut name = source.as_os_str().to_os_string();
        name.push("-wal");
        std::path::PathBuf::from(name)
    };
    assert!(
        fs::metadata(&wal).is_ok_and(|log| log.len() > 0),
        "the fixture needs a log with frames in it"
    );
    let file = directory.path().join("export.jsonl");
    let exported = export_json(&source, &file).expect("export");
    assert!(exported.wal_bytes > 0, "the report names the log");
    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");
    let revision: i64 = Connection::open(&target)
        .expect("target")
        .query_row(
            "SELECT revision FROM agent_context_revisions WHERE project_id = 'wal-project'",
            [],
            |row| row.get(0),
        )
        .expect("the row that was only in the log");
    assert_eq!(revision, 7);
}
