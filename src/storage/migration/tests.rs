use std::{collections::BTreeMap, fs, path::Path};

use rusqlite::{Connection, OptionalExtension, types::Value};

use super::*;
use crate::{
    ProjectId,
    domain::{ActorContext, AssuranceLevel, CreateWorkRequest},
    memory::DevelopmentNoopRedactor,
};

#[test]
fn omitted_format_marker_rows_are_validated_and_counted() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    let target = directory.path().join("target.db");
    populated(&source);
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
            let objects = declarations
                .iter_mut()
                .find(|table| table["name"] == "objects")
                .unwrap();
            objects["rows"] = Json::from(objects["rows"].as_u64().unwrap() + 1);
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
                let mut header: Json = serde_json::from_str(&lines[0]).unwrap();
                let objects = header["engram_export"]["tables"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|table| table["name"] == "objects")
                    .unwrap();
                let column = objects["columns"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|column| **column == "object_kind")
                    .unwrap();
                *column = Json::from("object_flavour");
                lines[0] = serde_json::to_string(&header).unwrap();
                lines
            }),
            "column object_flavour of table objects has no place",
        ),
        (
            "unknown format-marker column",
            Box::new(|lines| {
                // Declared in the header and carried by the row, so only the
                // current format can refuse it: the marker table is left out.
                lines
                    .into_iter()
                    .map(|line| {
                        let mut value: Json = serde_json::from_str(&line).unwrap();
                        if let Some(tables) = value
                            .pointer_mut("/engram_export/tables")
                            .and_then(Json::as_array_mut)
                        {
                            let marker = tables
                                .iter_mut()
                                .find(|table| table["name"] == "work_schema_metadata")
                                .unwrap();
                            marker["columns"]
                                .as_array_mut()
                                .unwrap()
                                .push(Json::from("marker_of_tomorrow"));
                        } else if value["row"]["table"] == "work_schema_metadata" {
                            value["row"]["values"]["marker_of_tomorrow"] = Json::from(1);
                        } else {
                            return line;
                        }
                        serde_json::to_string(&value).unwrap()
                    })
                    .collect()
            }),
            "column marker_of_tomorrow of table work_schema_metadata has no place",
        ),
        (
            "format marker without its required column",
            Box::new(|lines| {
                lines
                    .into_iter()
                    .map(|line| {
                        let mut value: Json = serde_json::from_str(&line).unwrap();
                        if let Some(tables) = value
                            .pointer_mut("/engram_export/tables")
                            .and_then(Json::as_array_mut)
                        {
                            tables
                                .iter_mut()
                                .find(|table| table["name"] == "work_schema_metadata")
                                .unwrap()["columns"]
                                .as_array_mut()
                                .unwrap()
                                .retain(|column| column != "schema_version");
                        } else if value["row"]["table"] == "work_schema_metadata" {
                            value["row"]["values"]
                                .as_object_mut()
                                .unwrap()
                                .remove("schema_version")
                                .unwrap();
                        } else {
                            return line;
                        }
                        serde_json::to_string(&value).unwrap()
                    })
                    .collect()
            }),
            "table work_schema_metadata lacks required destination column schema_version",
        ),
        (
            "record table claimed as left out",
            Box::new(|mut lines| {
                // A build that classified this table as derived would leave its
                // records out of the file; this build keeps them, so it refuses.
                let mut header: Json = serde_json::from_str(&lines[0]).unwrap();
                let export = &mut header["engram_export"];
                let tables = export["tables"].as_array_mut().unwrap();
                let position = tables
                    .iter()
                    .position(|table| table["name"] == "note_intents")
                    .unwrap();
                assert_eq!(tables.remove(position)["rows"], 0, "fixture has no rows");
                export["left_out"].as_array_mut().unwrap().push(serde_json::json!({
                    "name": "note_intents", "rows": 0,
                    "reason": "rebuilt projection; import starts it empty and repair derives it again"
                }));
                lines[0] = serde_json::to_string(&header).unwrap();
                lines
            }),
            "left-out table note_intents holds records in the current format",
        ),
        (
            "rebuilt projection carried as copied rows",
            Box::new(|mut lines| {
                // Repair drops and derives this table again, so rows copied into
                // it would be counted by the report but never held by the store.
                let mut header: Json = serde_json::from_str(&lines[0]).unwrap();
                let export = &mut header["engram_export"];
                export["left_out"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|entry| entry["name"] != "work_observations");
                export["tables"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "name": "work_observations", "columns": [], "rows": 0
                    }));
                lines[0] = serde_json::to_string(&header).unwrap();
                lines
            }),
            "table work_observations is a derived rebuilt projection",
        ),
        (
            "unknown table claimed as left out",
            Box::new(|mut lines| {
                let mut header: Json = serde_json::from_str(&lines[0]).unwrap();
                header["engram_export"]["left_out"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "name": "tasks_of_yesterday", "rows": 2, "reason": "retired"
                    }));
                lines[0] = serde_json::to_string(&header).unwrap();
                lines
            }),
            "left-out tasks_of_yesterday has no place in the current format",
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
    wrong_record["changes"][0]["entry"]["object_id"] = Json::String(unknown_record.clone());
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
            "DELETE FROM sqlite_sequence WHERE name = 'control_changes';
             INSERT INTO sqlite_sequence (name, seq) VALUES ('control_changes', 1000);",
        )
        .expect("a mark above every surviving row");
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");
    let mark: i64 = Connection::open(&target)
        .expect("target")
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'control_changes'",
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

/// A populated store with one bound control session, the moment it was bound.
fn bound_control(
    path: &Path,
) -> (
    crate::storage::test_support::TestControlBinding,
    chrono::DateTime<Utc>,
) {
    use crate::domain::EffectClass;
    populated(path);
    let mut store =
        SqliteStore::open_with_host_path_policy(path, crate::HostPathPolicy::host_default())
            .unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-20T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let binding = crate::storage::test_support::bind_control_for(
        &mut store,
        "historical-session",
        "historical-bind",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    (binding, now)
}

#[test]
fn historical_control_operation_receipts_import_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let (binding, _) = bound_control(&source);
    let connection = Connection::open(&source).unwrap();
    let bind_intent: String = connection
        .query_row(
            "SELECT bind_intent_hash FROM control_sessions WHERE session_id = ?1",
            [&binding.status.session_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let definition = crate::ObjectId::mint();
    let obligation = uuid::Uuid::new_v4();
    // Receipts of operations this build no longer offers, retained as history
    // rather than as authority.
    for (operation, mut intent, result) in [
        (
            "lease_acquire",
            serde_json::json!({
                "fingerprint_schema_version": 1, "bind_intent_hash": bind_intent,
                "kind": "execution", "mode": "exclusive",
                "subject": {"kind": "logical", "namespace": "historical",
                    "segments": ["resource"], "coverage": "exact"}, "ttl_seconds": 60
            }),
            serde_json::json!({"decision": "refuse", "directive": {
                "directive_id": "historical-policy-refusal", "code": "capability_not_permitted",
                "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
            }}),
        ),
        (
            "lease_release",
            serde_json::json!({
                "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
                "lease_id": "historical-lease"
            }),
            serde_json::json!({
                "lease_id": "historical-lease", "task_id": binding.status.task_id,
                "holder": binding.status.session_id, "fence": 1, "cursor": 1,
                "released_at": "2026-09-20T10:00:00Z"
            }),
        ),
        (
            "obligation_waive",
            serde_json::json!({
                "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
                "bind_intent_hash": bind_intent,
                "obligation_id": obligation, "expected_definition": definition,
                "waived_by": "historical-operator", "reason": "historical waiver request"
            }),
            serde_json::json!({
                "decision": "refused", "code": "waiver_not_admitted", "obligation_id": obligation,
                "current_definition": definition,
                "remedy": "bind the host control session to the live claim for this obligation run"
            }),
        ),
    ] {
        let key = format!("historical-{operation}");
        intent["session_id"] = serde_json::json!(binding.status.session_id);
        intent["idempotency_key"] = serde_json::json!(key);
        let frozen = crate::CanonicalObject::freeze(&intent).unwrap();
        connection
            .execute(
                "INSERT INTO control_operation_results (session_id, operation, idempotency_key,
                 intent_hash, intent_json, result_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    binding.status.session_id.0,
                    operation,
                    key,
                    frozen.key().as_str(),
                    frozen.bytes(),
                    crate::canonical::canonical_bytes(&result).unwrap(),
                    1_790_000_000_000_i64
                ],
            )
            .unwrap();
    }
    drop(connection);
    let before = rows(&source)["control_operation_results"].clone();
    assert_eq!(before.len(), 3, "the fixture holds each historical receipt");
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["control_operation_results"], before);
    let imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());

    // A historical receipt is admitted by its shape, not by its name alone.
    let connection = Connection::open(&source).unwrap();
    let release: i64 = connection
        .query_row(
            "SELECT sequence FROM control_operation_results WHERE operation = 'lease_release'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE control_operation_results SET result_json = ?1
             WHERE operation = 'lease_release'",
            [crate::canonical::canonical_bytes(
                &serde_json::json!({"released_at": "2026-09-20T10:00:00Z"}),
            )
            .unwrap()],
        )
        .unwrap();
    drop(connection);
    let broken_file = directory.path().join("broken.jsonl");
    let broken_target = directory.path().join("broken.db");
    export_json(&source, &broken_file).unwrap();
    let error = import_json(&broken_file, &broken_target)
        .expect_err("a lease release receipt without its lease must refuse")
        .to_string();
    assert!(
        error.contains(&format!("invalid labels: control_operation:{release}")),
        "{error}"
    );
    assert!(!broken_target.exists(), "published an unhealthy store");
}

#[test]
fn historical_lease_refusal_imports_and_replays_without_lease_authority() {
    use crate::domain::{EffectClass, TurnIntent, TurnPurpose};
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let (binding, now) = bound_control(&source);
    let intent = TurnIntent {
        idempotency_key: "unleased-mutation".into(),
        intent_fingerprint: crate::ObjectId::mint(),
        purpose: TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::MutateLocal],
        resource_intents: vec![],
    };
    // Written by an earlier evaluator; the current one never produces this code.
    let decision = serde_json::json!({"decision": "refuse", "directive": {
        "directive_id": "unleased-mutation:lease_required", "code": "lease_required",
        "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
    }});
    let saved_intent = crate::CanonicalObject::freeze(&serde_json::json!({
        "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
        "session_id": binding.status.session_id,
        "task_id": binding.status.task_id,
        "intent": intent
    }))
    .unwrap();
    let saved_decision = crate::CanonicalObject::freeze(&decision).unwrap();
    Connection::open(&source)
        .unwrap()
        .execute(
            "INSERT INTO control_turn_results (session_id, task_id, idempotency_key,
             intent_hash, intent_json, decision_hash, decision_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                binding.status.session_id.0,
                binding.status.task_id.0.to_string(),
                intent.idempotency_key,
                saved_intent.key().as_str(),
                saved_intent.bytes(),
                saved_decision.key().as_str(),
                saved_decision.bytes(),
                now.timestamp_millis()
            ],
        )
        .unwrap();
    let before = rows(&source)["control_turn_results"].clone();
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["control_turn_results"], before);
    let mut imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
    let replay = imported
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &intent,
            now + chrono::Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(serde_json::to_value(replay).unwrap(), decision);
    assert_eq!(rows(&target)["control_turn_results"], before);
}
