//! Configuration parity and read-only admission without any SQLite store.

use super::*;
use crate::task_sync::settings::{Action, Request};

#[test]
fn task_sync_settings_mcp_refuses_instructions_on_status_and_disable() {
    let fixture = crate::test_support::temp_home().unwrap();
    let project = ProjectId("settings/mcp-instruction".into());
    let database = crate::project_database_path(&fixture.path().join("missing-home"), &project);
    let server = McpServer::new_with_actor_context(
        database.clone(),
        project,
        "actor".into(),
        SessionId("session".into()),
        None,
        None,
    );
    for action in [Action::Status, Action::Disable] {
        let refused = server.task_sync_settings(Parameters(Request {
            action,
            user_instruction: Some("rejected instruction sentinel".into()),
        }));
        assert_eq!(refused.is_error, Some(true));
        let value = refused.structured_content.unwrap();
        assert_eq!(value["format"], "engram-task-sync-error-v1");
        assert_eq!(value["error"]["code"], "invalid_user_instruction");
        assert_eq!(value["operation"], json!(action));
        assert!(!value.to_string().contains("rejected instruction sentinel"));
        assert!(!database.parent().unwrap().exists());
    }
    for arguments in [
        json!({"action":"status"}),
        json!({"action":"status","user_instruction":null}),
    ] {
        let request = serde_json::from_value(arguments).unwrap();
        let status = server.task_sync_settings(Parameters(request));
        assert_ne!(status.is_error, Some(true));
        assert_eq!(status.structured_content.unwrap()["state"], "disabled");
    }
    assert!(!database.parent().unwrap().exists());
}

#[test]
fn task_sync_settings_mcp_uses_bound_project_actor_and_session_without_sqlite() {
    let fixture = crate::test_support::temp_home().unwrap();
    let project = ProjectId("settings/mcp".into());
    let database = crate::project_database_path(fixture.path(), &project);
    let server = McpServer::new_with_actor_context(
        database.clone(),
        project.clone(),
        "human".into(),
        SessionId("UI action".into()),
        None,
        None,
    );
    let call = |action, instruction: Option<&str>| {
        server.task_sync_settings(Parameters(Request {
            action,
            user_instruction: instruction.map(str::to_owned),
        }))
    };
    let absent = call(Action::Status, None);
    assert_ne!(absent.is_error, Some(true));
    assert_eq!(
        absent.structured_content.unwrap()["reason"],
        "not configured (off by default)"
    );
    assert!(!database.parent().unwrap().exists());
    let blank = call(Action::Enable, Some(" "));
    assert_eq!(blank.is_error, Some(true));
    assert_eq!(
        blank.structured_content.unwrap()["error"]["code"],
        "invalid_user_instruction"
    );
    assert!(!database.parent().unwrap().exists());
    let enabled = call(Action::Enable, Some("Enable this project on this machine."));
    assert_ne!(enabled.is_error, Some(true));
    let observed = enabled.structured_content.unwrap();
    assert_eq!(observed["state"], "enabled");
    assert_eq!(observed["project_id"], project.0);
    assert_eq!(
        call(Action::Status, None).structured_content.unwrap(),
        observed
    );
    let setting = database.parent().unwrap().join("task-sync-setting.json");
    let stored: Value = serde_json::from_slice(&std::fs::read(setting).unwrap()).unwrap();
    assert_eq!(stored["actor_id"], "human");
    assert_eq!(stored["session_id"], "UI action");
    assert!(!database.exists());
    let disabled = call(Action::Disable, None).structured_content.unwrap();
    assert_eq!(disabled["state"], "disabled");
    assert!(disabled["user_instruction"].is_null());
}

#[test]
fn task_sync_settings_read_only_admission_and_service_refuse_before_creating_paths() {
    let fixture = crate::test_support::temp_home().unwrap();
    let project = ProjectId("settings/reader".into());
    let database = crate::project_database_path(fixture.path(), &project);
    let server = McpServer::new_read_only_with_actor_context(
        database.clone(),
        project,
        "reader".into(),
        SessionId("reader".into()),
        None,
        None,
    );
    let arguments = json!({"action":"status"}).as_object().unwrap().clone();
    assert!(super::super::read_only::admit("task_sync_settings", Some(&arguments)).is_ok());
    for action in [Action::Enable, Action::Disable] {
        let arguments = json!({"action":action,"user_instruction":"Enable this project."})
            .as_object()
            .unwrap()
            .clone();
        assert!(super::super::read_only::admit("task_sync_settings", Some(&arguments)).is_err());
        let refused = server.task_sync_settings(Parameters(Request {
            action,
            user_instruction: Some("Enable this project.".into()),
        }));
        assert_eq!(refused.is_error, Some(true));
        assert_eq!(
            refused.structured_content.unwrap()["error"]["code"],
            "mcp_read_only_refused"
        );
        assert!(!database.parent().unwrap().exists());
    }
    assert_eq!(
        server
            .task_sync_settings(Parameters(Request {
                action: Action::Status,
                user_instruction: None
            }))
            .structured_content
            .unwrap()["state"],
        "disabled"
    );
}

#[test]
fn task_sync_settings_requires_standard_project_home_and_status_ignores_attribution() {
    let fixture = crate::test_support::temp_home().unwrap();
    let database = fixture.path().join("arbitrary.db");
    let server = McpServer::new_with_actor_context(
        database.clone(),
        ProjectId("settings/nonstandard".into()),
        String::new(),
        SessionId(String::new()),
        None,
        None,
    );
    let status = server.task_sync_settings(Parameters(Request {
        action: Action::Status,
        user_instruction: None,
    }));
    assert_ne!(status.is_error, Some(true));
    assert_eq!(status.structured_content.unwrap()["state"], "unknown");
    let refused = server.task_sync_settings(Parameters(Request {
        action: Action::Disable,
        user_instruction: None,
    }));
    assert_eq!(
        refused.structured_content.unwrap()["error"]["code"],
        "invalid_attribution"
    );
    assert!(!database.exists());
}
