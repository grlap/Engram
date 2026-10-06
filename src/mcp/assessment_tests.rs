//! The MCP `show` tool gives a verification record's obligation assessment
//! as the agent words give it.

use super::*;
use super::{arguments::ShowArgs, parameters::Parameters};
use chrono::Utc;

/// `note` gives the summary, the same block the CLI words give; its history
/// command pages the first eight candidates, and the continuation the rest.
#[test]
fn show_pages_a_verification_records_assessment_over_mcp() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (work_ref, records) =
        crate::storage::assessed_verification_fixture(&database, "mcp-assessment", "runner", 9, 1);
    let record = &records[0];
    let (project, session) = (
        ProjectId("mcp-assessment".into()),
        SessionId("runner".into()),
    );
    let verbs = crate::verbs::AgentVerbs::new(
        database.clone(),
        project.clone(),
        "runner".into(),
        session.clone(),
        None,
    );
    let server =
        McpServer::new_with_actor_context(database, project, "runner".into(), session, None, None);
    let args = |after: Option<String>| ShowArgs {
        work_ref: work_ref.clone(),
        notes: None,
        gates: None,
        history: None,
        after,
        note: Some(record.as_str().to_owned()),
        full: None,
        evaluations: None,
        evaluation: None,
        observations: None,
        criterion_links: None,
    };
    let detail = server
        .show(Parameters(args(None)))
        .structured_content
        .expect("structured detail");
    let token = |command: &serde_json::Value| {
        command
            .as_str()
            .and_then(|command| command.split_once(" --after "))
            .map(|(_, token)| token.to_owned())
            .expect("continuation")
    };
    // The default view is the summary: exact counts, every row not already
    // closed, and the history one continuation away.
    let block = &detail["note"]["assessment"];
    assert_eq!(block["view"], "summary", "{block}");
    assert_eq!(
        (
            block["total"].as_u64(),
            block["must_show_total"].as_u64(),
            block["must_show"].as_array().map(Vec::len)
        ),
        (Some(10), Some(10), Some(10))
    );
    let input = crate::verbs::ShowInput {
        note: Some(record.as_str().to_owned()),
        ..Default::default()
    };
    let cli = verbs
        .show_records(&work_ref, &input, Utc::now())
        .expect("CLI detail");
    assert_eq!(cli.value["note"]["assessment"], *block, "CLI and MCP agree");
    let first = server
        .show(Parameters(args(Some(token(&block["history"])))))
        .structured_content
        .expect("structured history");
    let block = &first["assessment"];
    assert_eq!(block["view"], "history", "{block}");
    assert_eq!(
        (block["total"].as_u64(), block["shown"].as_u64()),
        (Some(10), Some(8))
    );
    let rest = server
        .show(Parameters(args(Some(token(&block["continuation"])))))
        .structured_content
        .expect("structured continuation");
    assert_eq!(rest["assessment"]["shown"], 2, "{rest}");
    assert_eq!(rest["assessment"]["earlier"], 8);
}
