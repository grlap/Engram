use super::*;

#[test]
fn data_commit_service_refuses_before_git_or_store_for_read_only_opt_out_and_foreign_marker() {
    for case in ["read_only", "opt_out", "foreign_marker"] {
        let owned = crate::test_support::temp_home().unwrap();
        let database = crate::project_database_path(
            &owned.path().join("home"),
            &ProjectId("service/opt-in".into()),
        );
        let mut service = service(&database);
        let marker = owned.path().join(".engram-project");
        if case == "foreign_marker" {
            service
                .task_sync_settings(
                    &task_sync::settings::Request {
                        action: task_sync::settings::Action::Enable,
                        user_instruction: Some(
                            "Enable Git task sync for this test fixture.".into(),
                        ),
                    },
                    chrono::Utc::now(),
                )
                .unwrap();
            fs::write(&marker, b"another/project").unwrap();
        }
        if case == "read_only" {
            service = service.into_read_only();
        }
        let request = task_sync::data_worktree::StagedCommitRequest {
            operation: uuid::Uuid::new_v4(),
            tree: delta_export_request(&service).producer.to_string(),
            expected_head: None,
            parents: Vec::new(),
            author_name: "Fixture".into(),
            author_email: "fixture@example.invalid".into(),
            authored_at: chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
                .unwrap(),
            message: "Retain selected data".into(),
        };
        let before = entries(owned.path());
        let error = service
            .create_task_sync_data_commit(&marker, "engram-data", &request)
            .unwrap_err();
        let expected = match case {
            "read_only" => "read-only",
            "opt_out" => "task sync is not enabled",
            _ => "selected project marker differs from project",
        };
        assert!(error.to_string().contains(expected), "{case}: {error}");
        assert!(!database.exists());
        assert_eq!(entries(owned.path()), before, "{case}");
    }
}
