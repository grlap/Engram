use super::*;

#[test]
fn show_mcp_preserves_each_verifications_recorded_workspace_and_plain_note_absence() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (work, records) = crate::storage::verification_workspace_notes_fixture(
        &database,
        "mcp-workspaces",
        "workspace-B",
    );
    let server = McpServer::new_with_actor_context(
        database,
        ProjectId("mcp-workspaces".into()),
        "reader".into(),
        SessionId("reader".into()),
        None,
        None,
    );
    let args = |note: Option<&str>| {
        serde_json::from_value::<ShowArgs>(match note {
            Some(note) => json!({"work_ref": work, "note": note}),
            None => json!({"work_ref": work, "notes": true}),
        })
        .unwrap()
    };
    let result = server.show(Parameters(args(None)));
    assert_eq!(result.is_error, Some(false));
    let window = result.structured_content.expect("structured window");
    let rows = window["notes"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows.iter()
            .map(|row| row["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "generic",
            "environment",
            "verification",
            "environment",
            "verification"
        ]
    );
    for (index, record) in [0, 2, 4].into_iter().zip(&records) {
        let row = &rows[index];
        assert_eq!(row["locator"], record.as_str());
    }
    for index in [0, 1, 3] {
        assert!(rows[index].get("verification").is_none());
        assert!(rows[index].get("source_workspace").is_none());
        assert!(rows[index].get("source_revision").is_none());
    }
    for (index, workspace) in [2, 4].into_iter().zip(["workspace-A", "workspace-B"]) {
        let row = &rows[index];
        assert_eq!(
            row["verification"],
            json!({
                "result": "passed", "check_kind": "test", "source_revision": "A3",
                "producer_outcome": "succeeded", "source_workspace": workspace
            })
        );
        let detail = server
            .show(Parameters(args(row["locator"].as_str())))
            .structured_content
            .expect("structured detail");
        assert_eq!(detail["note"]["verification"], row["verification"]);
    }
    assert_eq!(rows[2]["summary"], "first check");
    assert_eq!(rows[4]["body_omitted"], true);
}
