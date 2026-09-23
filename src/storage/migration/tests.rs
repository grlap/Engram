use std::{collections::BTreeMap, fs, path::Path};

use rusqlite::{Connection, OptionalExtension, types::Value};

use super::*;
use crate::{
    ProjectId,
    domain::{ActorContext, AssuranceLevel, CreateWorkRequest},
    memory::DevelopmentNoopRedactor,
};

mod historical;
mod pending_delivery;
mod publication;
mod refusals;
mod round_trip;

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
