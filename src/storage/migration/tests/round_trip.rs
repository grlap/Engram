//! Export format and round trip: every row arrives, values keep their
//! storage class and bytes, and the report counts what the store holds.

use super::*;

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
