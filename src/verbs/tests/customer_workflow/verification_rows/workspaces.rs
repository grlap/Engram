use super::*;

#[test]
fn verification_workspaces_survive_equal_revisions_and_a_later_root_change() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    // A source basis permits 512 bytes; this fixture also records populated
    // environment components, whose workspace field permits 256 bytes.
    let second_workspace = format!("{}X", "界".repeat(85));
    assert_eq!(second_workspace.len(), 256);
    let (work_ref, records) = crate::storage::verification_workspace_notes_fixture(
        &database,
        "verification-workspaces",
        &second_workspace,
    );
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId("verification-workspaces".into()),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let before = store_snapshot(&database);
    let window = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                notes: true,
                ..Default::default()
            },
            at(10),
        )
        .expect("window");
    // CLI --json serializes this shared receipt value.
    let wire: Value = serde_json::from_slice(&serde_json::to_vec(&window.value).unwrap()).unwrap();
    let rows = wire["notes"].as_array().unwrap();
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
        for field in [
            "kind",
            "family",
            "by",
            "created_at",
            "body_bytes",
            "feed_position",
        ] {
            assert!(row.get(field).is_some(), "{field}: {row}");
        }
    }
    for index in [0, 1, 3] {
        assert!(rows[index].get("verification").is_none());
        assert!(rows[index].get("source_workspace").is_none());
        assert!(rows[index].get("source_revision").is_none());
    }
    for (index, workspace) in [2, 4].into_iter().zip(["workspace-A", &second_workspace]) {
        let row = &rows[index];
        let mut facts = row["verification"].clone();
        assert_eq!(facts["source_workspace"], workspace);
        facts.as_object_mut().unwrap().remove("source_workspace");
        assert_eq!(
            facts,
            json!({
                "result": "passed", "check_kind": "test",
                "source_revision": "A3", "producer_outcome": "succeeded"
            })
        );
        let detail = verbs
            .show_records(
                &work_ref,
                &ShowInput {
                    note: Some(row["locator"].as_str().unwrap().into()),
                    ..Default::default()
                },
                at(10),
            )
            .expect("detail");
        assert_eq!(detail.value["note"]["verification"], row["verification"]);
    }
    assert_eq!(rows[2]["summary"], "first check");
    assert_eq!(rows[4]["body_omitted"], true);
    assert!(rows[4].get("summary").is_none());
    assert!(emitted_receipt_bytes(&window) < MAX_AGENT_WORK_RESPONSE_BYTES);
    let text = window.text();
    assert_eq!(
        text.matches("verification: passed test on source revision A3; producer outcome succeeded")
            .count(),
        2
    );
    for workspace in [
        "workspace-A",
        &second_workspace,
        "workspace-current",
        "source_workspace",
    ] {
        assert!(!text.contains(workspace), "text changed: {text}");
    }
    assert_eq!(store_snapshot(&database), before);
}
