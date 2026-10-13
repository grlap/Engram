use super::*;
use crate::{ProjectId, SessionId, project_database_path};
use std::{collections::BTreeMap, fs, path::Path};

fn snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<std::path::PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                result.insert(path.strip_prefix(root).unwrap().to_path_buf(), None);
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    Some(fs::read(path).unwrap()),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn task_sync_settings_service_refuses_misrouted_instructions_without_changing_paths() {
    for stored in ["absent", "enabled", "invalid"] {
        let fixture = crate::test_support::temp_home().unwrap();
        let project = ProjectId("settings/instruction-service".into());
        let database = project_database_path(&fixture.path().join("missing-home"), &project);
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "actor".into(),
            SessionId("session".into()),
            None,
        );
        if stored != "absent" {
            let enabled = service
                .task_sync_settings(
                    &Request {
                        action: Action::Enable,
                        user_instruction: Some("Original enable request.".into()),
                    },
                    Utc::now(),
                )
                .unwrap();
            assert_eq!(enabled.state, settings::State::Enabled);
            if stored == "invalid" {
                fs::write(
                    database.parent().unwrap().join("task-sync-setting.json"),
                    b"invalid retained bytes",
                )
                .unwrap();
            }
        }
        let before = snapshot(fixture.path());
        for action in [Action::Status, Action::Disable] {
            for instruction in [
                String::new(),
                " \n\t".into(),
                "x".repeat(4097),
                "rejected quote sentinel".into(),
            ] {
                let error = service
                    .task_sync_settings(
                        &Request {
                            action,
                            user_instruction: Some(instruction.clone()),
                        },
                        Utc::now(),
                    )
                    .unwrap_err();
                assert_eq!(error.error.code, "invalid_user_instruction");
                assert_eq!(
                    error.error.message,
                    "user_instruction is allowed only with action enable"
                );
                assert_eq!(error.operation, action);
                assert!(
                    !serde_json::to_string(&error)
                        .unwrap()
                        .contains("rejected quote sentinel")
                );
                assert_eq!(snapshot(fixture.path()), before);
                assert!(!database.exists());
            }
        }
        let status = service
            .task_sync_settings(
                &Request {
                    action: Action::Status,
                    user_instruction: None,
                },
                Utc::now(),
            )
            .unwrap();
        assert_eq!(
            status.state,
            match stored {
                "enabled" => settings::State::Enabled,
                "invalid" => settings::State::Unknown,
                _ => settings::State::Disabled,
            }
        );
        assert_eq!(snapshot(fixture.path()), before);
    }
}

#[test]
fn task_sync_settings_service_preserves_read_only_and_attribution_precedence() {
    let fixture = crate::test_support::temp_home().unwrap();
    let project = ProjectId("settings/instruction-precedence".into());
    let database = project_database_path(&fixture.path().join("missing-home"), &project);
    for read_only in [false, true] {
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            String::new(),
            SessionId(String::new()),
            None,
        );
        let service = if read_only {
            service.into_read_only()
        } else {
            service
        };
        for action in [Action::Status, Action::Disable] {
            let error = service
                .task_sync_settings(
                    &Request {
                        action,
                        user_instruction: Some("rejected quote".into()),
                    },
                    Utc::now(),
                )
                .unwrap_err();
            assert_eq!(
                error.error.code,
                if read_only && action == Action::Disable {
                    "mcp_read_only_refused"
                } else {
                    "invalid_user_instruction"
                }
            );
        }
        assert_eq!(
            service
                .task_sync_settings(
                    &Request {
                        action: Action::Status,
                        user_instruction: None,
                    },
                    Utc::now()
                )
                .unwrap()
                .state,
            settings::State::Disabled
        );
        assert!(!database.parent().unwrap().exists());
    }
}
