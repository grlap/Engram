use super::*;

#[test]
fn criterion_links_mcp_matches_the_word_and_refuses_other_modes() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("criterion-mcp".into());
    let session = SessionId("reader".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "reader".into(),
        session.clone(),
        None,
    );
    let now = Utc::now();
    let added = verbs
        .add(
            AddInput {
                title: "Sealed empty mapping".into(),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    let work = added.value["work"]["short_ref"].as_str().unwrap();
    verbs
        .claim(
            crate::ClaimInput {
                work_ref: work.into(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            now,
        )
        .unwrap();
    verbs
        .note(
            &crate::NoteInput {
                work_ref: Some(work.into()),
                text: "Unlinked completion evidence".into(),
                refs: Vec::new(),
                status: false,
            },
            now,
        )
        .unwrap();
    verbs
        .done(
            crate::DoneInput {
                work_ref: Some(work.into()),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    let server = McpServer::new_with_actor_context(
        database.clone(),
        project,
        "reader".into(),
        session,
        None,
        None,
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let args: ShowArgs =
        serde_json::from_value(json!({"work_ref":work,"criterion_links":true})).unwrap();
    let mcp = server.show(Parameters(args)).structured_content.unwrap();
    let cli = verbs
        .show_records(
            work,
            &crate::verbs::ShowInput {
                criterion_links: true,
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(mcp, cli.value);
    assert_eq!(mcp["criterion_links_window"]["total"], 0);
    for mode in [
        "notes",
        "gates",
        "history",
        "full",
        "evaluations",
        "observations",
    ] {
        let mut input = json!({"work_ref":work,"criterion_links":true});
        input[mode] = json!(true);
        let result = server.show(Parameters(serde_json::from_value(input).unwrap()));
        assert_eq!(result.is_error, Some(true), "{mode}");
    }
    for mode in ["note", "evaluation"] {
        let mut input = json!({"work_ref":work,"criterion_links":true});
        input[mode] = json!("record");
        assert_eq!(
            server
                .show(Parameters(serde_json::from_value(input).unwrap()))
                .is_error,
            Some(true)
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(
        serde_json::from_value::<ShowArgs>(json!({"work_ref":work,"criterion_links":1})).is_err()
    );
}
