use super::*;

#[test]
fn sealed_source_mcp_and_cli_words_expose_the_same_diagnostic_and_cursor() {
    let fixture = crate::storage::sealed_source_transport_fixture("difference");
    let words = AgentVerbs::new(
        fixture.database.clone(),
        fixture.work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let server = McpServer::new_with_actor_context(
        fixture.database.clone(),
        fixture.work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
        None,
    );
    let shown = words.show(&fixture.work.short_ref, Utc::now()).unwrap();
    let mcp = server
        .show(Parameters(
            serde_json::from_value(json!({
                "work_ref": fixture.work.short_ref,
            }))
            .unwrap(),
        ))
        .structured_content
        .unwrap();
    assert_eq!(mcp["sealed_source"], shown.value["sealed_source"]);
    assert!(mcp["sealed_source"].get("seal").is_none());
    assert!(mcp["sealed_source"].get("evaluation").is_none());
    let token = shown.value["sealed_source"]["observations_detail"]
        .as_str()
        .unwrap()
        .split(" --after ")
        .nth(1)
        .unwrap();
    let cli_window = words
        .show_records(
            &fixture.work.short_ref,
            &crate::verbs::ShowInput {
                observations: true,
                after: Some(token.into()),
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap();
    let mcp_window = server
        .show(Parameters(
            serde_json::from_value(json!({
                "work_ref": fixture.work.short_ref, "observations": true, "after": token,
            }))
            .unwrap(),
        ))
        .structured_content
        .unwrap();
    assert_eq!(mcp_window["observations"], cli_window.value["observations"]);
    assert_eq!(
        mcp_window["sealed_source"],
        cli_window.value["sealed_source"]
    );
    assert_eq!(mcp_window["sealed_source"]["seal"], fixture.seal.as_str());
    crate::LocalWorkService::new(
        fixture.database.clone(),
        fixture.work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    )
    .work_focus(&fixture.work.short_ref, Utc::now())
    .unwrap();
    let next = server
        .next(Parameters(
            serde_json::from_value(json!({"peek": true})).unwrap(),
        ))
        .structured_content
        .unwrap();
    assert_eq!(next["focus"]["sealed_source"], shown.value["sealed_source"]);
}
