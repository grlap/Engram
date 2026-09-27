//! Import admission: what the current format has no place for is refused by
//! name, and nothing is published.

use super::*;

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
        Json::from(crate::storage::CONTROL_POLICY_STATE_SCHEMA_VERSION + 1);
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

/// The obligation rule sets a store with requirements in every record that
/// can carry one selected: the stock set of the first policy, then the
/// operator's set the active policy selects.
struct RequirementRuleSets {
    earlier: crate::ObjectId,
    active: crate::ObjectId,
}

/// A policy-bootstrapped store (unlike `populated`, so bindings open their
/// obligations) where an operator replaced the stock rule set with one that
/// pins a check, and one work item binds two criteria, opening two
/// obligations on one run.
fn requirement_store(path: &Path) -> RequirementRuleSets {
    let mut store = SqliteStore::open(path).expect("store");
    let initial = store.control_diagnostics().expect("initial policy");
    let mut pinned = crate::control::builtin_obligation_rule_set();
    pinned.rules[0].rule.rule_id = "source_mutation_requires_pinned_test".into();
    pinned.rules[0].requirement.check_fingerprint =
        Some(crate::ObjectId::from_canonical_bytes(b"pinned-check"));
    store
        .set_obligation_rule_set(
            &pinned,
            &actor("policy-admin"),
            "pin-a-check",
            Some(&initial.active_policy),
            chrono::DateTime::parse_from_rfc3339("2026-09-27T09:00:00Z")
                .expect("time")
                .with_timezone(&Utc),
            &DevelopmentNoopRedactor,
        )
        .expect("select the pinned rule set");
    let active = store
        .control_diagnostics()
        .expect("changed policy")
        .obligation_rule_set;
    let test_requirement = crate::domain::VerificationRequirement {
        check_kind: crate::domain::VerificationKind::Test,
        check_fingerprint: None,
    };
    store
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: [1, 2]
                    .map(|criterion| crate::domain::AcceptanceBinding {
                        criterion,
                        requirement: test_requirement.clone(),
                    })
                    .to_vec(),
                evaluation_mode: None,
                project_id: ProjectId("project-environment-pin".into()),
                parent_id: None,
                child_requirement: crate::domain::ChildRequirement::Required,
                title: "Carry two bound criteria".into(),
                outcome: "The obligations arrive".into(),
                acceptance: vec!["tests pass".into(), "the other tests pass".into()],
                kind: crate::domain::WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                external_ref: None,
                notes: Vec::new(),
                origin: crate::domain::WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor("author"),
                idempotency_key: "create-bound".into(),
                created_at: chrono::DateTime::parse_from_rfc3339("2026-09-27T10:00:00Z")
                    .expect("time")
                    .with_timezone(&Utc),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("bound work");
    RequirementRuleSets {
        earlier: initial.obligation_rule_set,
        active,
    }
}

/// Where one kind of exported record carries a requirement.
struct RequirementSite<'a> {
    what: &'a str,
    table: &'a str,
    select: Box<dyn Fn(&Json) -> bool + 'a>,
    column: &'a str,
    requirement: &'a str,
    /// What the refusal names for the record.
    record: String,
}

/// Imports `original` with the first `site` record's requirement changed by
/// `edit`, and returns the refusal.
fn import_with_requirement(
    file: &Path,
    original: &str,
    site: &RequirementSite<'_>,
    edit: impl FnOnce(&mut Json),
) -> String {
    fs::write(file, original).expect("restore file");
    let marker = format!("\"table\":\"{}\"", site.table);
    let row = original
        .lines()
        .filter(|line| line.contains(&marker))
        .filter_map(|line| serde_json::from_str::<Json>(line).ok())
        .find(|row| (site.select)(&row["row"]["values"]))
        .unwrap_or_else(|| panic!("{}: no exported record", site.what));
    // A JSON column is exported as its value (`json`) or as its text (`text`).
    let mut cell = row["row"]["values"][site.column].clone();
    let text = cell["text"].as_str().map(str::to_owned);
    let mut value = match &text {
        Some(text) => serde_json::from_str(text).expect("json text"),
        None => cell["json"].take(),
    };
    assert!(
        value.pointer(site.requirement).is_some(),
        "{}: no requirement at {} in {value}",
        site.what,
        site.requirement
    );
    edit(value.pointer_mut(site.requirement).expect("requirement"));
    if text.is_some() {
        cell["text"] = Json::String(value.to_string());
    } else {
        cell["json"] = value;
    }
    with_row_cell(file, site.table, &site.select, site.column, cell);
    let target = file.with_file_name("target.db");
    let error = import_json(file, &target).expect_err(site.what).to_string();
    assert!(!target.exists(), "{}: a store was published", site.what);
    error
}

fn environments() -> [Json; 2] {
    [
        serde_json::json!(crate::ObjectId::from_canonical_bytes(b"environment")),
        Json::Null,
    ]
}

/// A stored requirement cannot name an environment. An exported obligation
/// whose requirement names one, with a value or as null, is refused where
/// the import reads it back, naming the record and the member, and nothing
/// is published. Its sibling on the same run, which fails with it, is not
/// blamed for the member.
#[test]
fn an_obligation_that_names_an_environment_is_refused_by_name() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    requirement_store(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let obligations = original
        .lines()
        .filter_map(|line| serde_json::from_str::<Json>(line).ok())
        .filter(|row| {
            row["row"]["table"] == "objects"
                && row["row"]["values"]["object_kind"] == "work_obligation"
        })
        .map(|row| row["row"]["values"]["canonical_json"]["json"]["obligation_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(obligations.len(), 2, "two obligations on one run");
    let site = RequirementSite {
        what: "an obligation",
        table: "objects",
        select: Box::new(|values: &Json| values["object_kind"] == "work_obligation"),
        column: "canonical_json",
        requirement: "/requirement",
        record: format!(
            "work_obligation:{}:",
            obligations[0].as_str().expect("obligation id")
        ),
    };
    for environment in environments() {
        let error = import_with_requirement(&file, &original, &site, |requirement| {
            requirement["required_environment"] = environment;
        });
        assert!(error.contains("invalid work record"), "{error}");
        let blamed = blamed_records(&error);
        assert_eq!(
            blamed.len(),
            1,
            "only the obligation that names the member is blamed: {error}"
        );
        assert!(blamed[0].contains(&site.record), "{error}");
    }
}

/// The parts of a refusal that name the removed member: each is one record's
/// label with its reason.
fn blamed_records(error: &str) -> Vec<&str> {
    error
        .split(", ")
        .filter(|label| label.contains("unknown field `required_environment`"))
        .collect()
}

/// Every exported record other than an obligation that carries a
/// requirement: the work event that recorded a work item's bindings, the
/// work item itself, and the rule sets the active and the earlier policy
/// selected.
fn other_requirement_sites(rule_sets: RequirementRuleSets) -> Vec<RequirementSite<'static>> {
    let rule_set = |what, hash: crate::ObjectId| RequirementSite {
        what,
        table: "objects",
        record: format!("obligation_rule_set:{hash}:"),
        select: Box::new(move |values: &Json| values["object_id"] == hash.as_str()),
        column: "canonical_json",
        requirement: "/rules/0/requirement",
    };
    vec![
        RequirementSite {
            what: "a work event",
            table: "objects",
            select: Box::new(|values: &Json| {
                values["object_kind"] == "work_event"
                    && values["canonical_json"]["json"]["work"]["acceptance_bindings"]
                        .as_array()
                        .is_some_and(|bindings| !bindings.is_empty())
            }),
            column: "canonical_json",
            requirement: "/work/acceptance_bindings/0/requirement",
            record: "work_event:".into(),
        },
        RequirementSite {
            what: "a work item",
            table: "work_items",
            select: Box::new(|_: &Json| true),
            column: "item_json",
            requirement: "/acceptance_bindings/0/requirement",
            record: "work_catalog:".into(),
        },
        rule_set("the active rule set", rule_sets.active),
        rule_set("an earlier rule set", rule_sets.earlier),
    ]
}

/// Every other exported record that carries a requirement is refused the
/// same way when it names an environment. Each refusal names the record and
/// the member.
#[test]
fn every_record_that_carries_a_requirement_is_refused_by_name() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let rule_sets = requirement_store(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    for site in &other_requirement_sites(rule_sets) {
        for environment in environments() {
            let error = import_with_requirement(&file, &original, site, |requirement| {
                requirement["required_environment"] = environment;
            });
            assert!(
                blamed_records(&error)
                    .iter()
                    .any(|label| label.contains(&site.record)),
                "{}: {error}",
                site.what
            );
        }
    }
}

/// A refusal reaches the operator's terminal, so the reason it gives for a
/// record that does not decode names the record and the shape it met, never
/// a value from the record: here a check kind no build knows.
#[test]
fn a_refused_requirement_never_repeats_its_values() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let rule_sets = requirement_store(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let mut sites = other_requirement_sites(rule_sets);
    sites.push(RequirementSite {
        what: "an obligation",
        table: "objects",
        select: Box::new(|values: &Json| values["object_kind"] == "work_obligation"),
        column: "canonical_json",
        requirement: "/requirement",
        record: "work_obligation:".into(),
    });
    for site in &sites {
        let error = import_with_requirement(&file, &original, site, |requirement| {
            requirement["check_kind"] = Json::String("private-check-kind".into());
        });
        assert!(
            error.contains(&site.record) && !error.contains("private-check-kind"),
            "{}: {error}",
            site.what
        );
    }
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
