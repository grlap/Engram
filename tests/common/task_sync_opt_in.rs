//! Explicit opt-in in owned integration homes; no SQLite store is opened.

use engram::task_sync::settings::{Action, Request, State};
use engram::{LocalWorkService, ProjectId, SessionId, project_database_path};
use std::path::Path;

pub fn enable(home: &Path, project: &str) {
    let project = ProjectId(project.into());
    let service = LocalWorkService::new(
        project_database_path(home, &project),
        project,
        "fixture-operator".into(),
        SessionId("fixture-settings-session".into()),
        None,
    );
    let status = service
        .task_sync_settings(
            &Request {
                action: Action::Enable,
                user_instruction: Some("Enable task sync in this owned test fixture".into()),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    assert_eq!(status.state, State::Enabled);
}

#[allow(dead_code)]
pub fn assert_no_database(home: &Path) {
    if !home.exists() {
        return;
    }
    for entry in std::fs::read_dir(home).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_database(&path);
        } else {
            assert!(
                path.extension().is_none_or(|extension| extension != "db"),
                "unexpected SQLite database: {}",
                path.display()
            );
        }
    }
}
