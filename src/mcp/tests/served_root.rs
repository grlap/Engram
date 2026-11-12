use super::*;
use crate::storage::{NamedRootClaim, named_roots_fixture};

const SHARED: &str = "C:/roots/shared";
const OWN: &str = "C:/roots/own";

fn claim(
    holder: &'static str,
    ttl_seconds: i64,
    root: Option<&'static str>,
) -> NamedRootClaim<'static> {
    NamedRootClaim {
        holder,
        ttl_seconds,
        root,
    }
}

/// The reminders that speak of a named root, with the item's own ref written
/// as `REF` so that two stores' answers compare.
fn served(reminders: &[String], work_ref: &str) -> Vec<String> {
    reminders
        .iter()
        .filter(|reminder| reminder.contains("named root"))
        .map(|reminder| reminder.replace(work_ref, "REF"))
        .collect()
}

fn mcp_reminders(value: &Value) -> Vec<String> {
    value["reminders"]
        .as_array()
        .expect("reminders")
        .iter()
        .map(|reminder| reminder.as_str().expect("reminder text").to_owned())
        .collect()
}

/// One store per surface: two items of `runner` share a root, one has its
/// own, one has none. Another session's live claim and `runner`'s own lapsed
/// claim also name the item's own root; neither may hold its reminder back.
fn store(
    directory: &std::path::Path,
    name: &str,
) -> (std::path::PathBuf, crate::ProjectId, Vec<String>) {
    let database = directory.join(format!("{name}.sqlite3"));
    let project = crate::ProjectId(format!("served-roots-{name}"));
    let refs = named_roots_fixture(
        &database,
        &project.0,
        Utc::now() - chrono::Duration::seconds(60),
        &[
            claim("runner", 3_600, Some(SHARED)),
            claim("runner", 3_600, Some(SHARED)),
            claim("runner", 3_600, Some(OWN)),
            claim("runner", 3_600, None),
            claim("other", 3_600, Some(OWN)),
            claim("runner", 10, Some(OWN)),
        ],
    );
    (database, project, refs)
}

#[test]
fn done_names_the_served_named_root_alike_over_cli_and_mcp() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let (cli_database, cli_project, cli_refs) = store(directory.path(), "cli");
    let (mcp_database, mcp_project, mcp_refs) = store(directory.path(), "mcp");
    let words = AgentVerbs::new(
        cli_database,
        cli_project,
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let server = McpServer::new_with_actor_context(
        mcp_database,
        mcp_project,
        "runner".into(),
        SessionId("runner".into()),
        None,
        None,
    );
    let mut answers = Vec::new();
    for (index, (cli_ref, mcp_ref)) in cli_refs.iter().zip(&mcp_refs).take(4).enumerate() {
        let cli = words
            .done(
                crate::verbs::DoneInput {
                    work_ref: Some(cli_ref.clone()),
                    summary: Some("delivered".into()),
                    ..Default::default()
                },
                Utc::now(),
            )
            .expect("done");
        assert!(!cli.owed, "{}", cli.text());
        let text = cli.text();
        for reminder in &cli.reminders {
            assert!(text.contains(reminder.as_str()), "{text}");
        }
        let mcp = acted(
            server.done(Parameters(done_args(Some(mcp_ref.as_str())))),
            "done",
        );
        let cli_served = served(&cli.reminders, cli_ref);
        assert_eq!(
            cli_served,
            served(&mcp_reminders(&mcp), mcp_ref),
            "item {index}"
        );
        assert_eq!(
            cli.value.get("named_root"),
            mcp.get("named_root"),
            "item {index}"
        );
        answers.push((cli_served, cli.value.get("named_root").cloned()));
    }
    let reminder = |root: &str| {
        vec![format!(
            "REF's named root {root} has served it: remove that worktree once nothing else needs it (Engram removes nothing)"
        )]
    };
    // The second item still names the shared root, so the first is not told.
    assert_eq!(
        answers[0],
        (
            Vec::new(),
            Some(json!({"workspace": SHARED, "shared_with_open_claim": true}))
        )
    );
    assert_eq!(
        answers[1],
        (
            reminder(SHARED),
            Some(json!({"workspace": SHARED, "shared_with_open_claim": false}))
        )
    );
    assert_eq!(
        answers[2],
        (
            reminder(OWN),
            Some(json!({"workspace": OWN, "shared_with_open_claim": false}))
        )
    );
    assert_eq!(answers[3], (Vec::new(), None));
}

#[test]
fn a_failed_served_root_read_is_disclosed_and_the_completion_stands() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = crate::ProjectId("served-roots-failure".into());
    let refs = named_roots_fixture(
        &database,
        &project.0,
        Utc::now() - chrono::Duration::seconds(60),
        &[claim("runner", 3_600, Some(OWN))],
    );
    let words = AgentVerbs::new(
        database,
        project,
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    crate::work_service::FAIL_NEXT_SERVED_ROOT_READ.with(|fail| fail.set(true));
    let receipt = words
        .done(
            crate::verbs::DoneInput {
                work_ref: Some(refs[0].clone()),
                summary: Some("delivered".into()),
                ..Default::default()
            },
            Utc::now(),
        )
        .expect("done");
    assert!(!receipt.owed, "{}", receipt.text());
    assert!(receipt.value["seal"].is_string(), "{}", receipt.value);
    let disclosed = served(&receipt.reminders, &refs[0]);
    assert_eq!(disclosed.len(), 1, "{disclosed:?}");
    assert!(
        disclosed[0].starts_with(
            "could not read the named root REF used, so check yourself whether its worktree can be removed: "
        ),
        "{disclosed:?}"
    );
    assert!(receipt.value.get("named_root").is_none());
}
