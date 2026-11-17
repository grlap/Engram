//! The shared exchange service admits a request before any Git process or store
//! is created, on every platform with a confined runner.

use super::super::{
    Cancellation, ExchangeOperation, RunError, RunLimits, TargetSelection, test_support,
};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join("target/tmp")
            .join(format!("exchange-service-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        super::policy_cases::snapshot(&self.0)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn task_exchange_service_refuses_read_only_and_opt_out_before_any_git_process() {
    let fixture = Dir::new();
    let database = fixture.0.join("not-created/db.sqlite");
    let project = crate::ProjectId("exchange/service".into());
    let service = || {
        crate::LocalWorkService::new(
            database.clone(),
            project.clone(),
            "fixture".into(),
            crate::SessionId("fixture".into()),
            None,
        )
    };
    let selection = TargetSelection {
        repository: fixture.0.join("missing-repository"),
        remote: "origin".into(),
        reference: "refs/heads/main".into(),
    };
    let cancel = Cancellation::default();
    let error = service()
        .into_read_only()
        .run_task_sync_exchange(
            &selection,
            &ExchangeOperation::Observe,
            RunLimits::default(),
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    let error = service()
        .run_task_sync_exchange(
            &selection,
            &ExchangeOperation::Observe,
            RunLimits::default(),
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not enabled"));
    assert!(!fixture.0.join("not-created").exists());
}

#[tokio::test]
async fn task_exchange_service_charges_admission_before_initial_capture() {
    let fixture = Dir::new();
    let project = crate::ProjectId("exchange/budget".into());
    let database = crate::project_database_path(&fixture.0.join("home"), &project);
    let service = crate::LocalWorkService::new(
        database.clone(),
        project,
        "fixture".into(),
        crate::SessionId("fixture".into()),
        None,
    );
    service
        .task_sync_settings(
            &crate::task_sync::settings::Request {
                action: crate::task_sync::settings::Action::Enable,
                user_instruction: Some("Enable task sync for this owned test".into()),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let selection = TargetSelection {
        repository: fixture.0.clone(),
        remote: "origin".into(),
        reference: "refs/heads/main".into(),
    };
    let before = fixture.snapshot();
    let count = test_support::LAUNCHES.with(std::cell::Cell::get);
    test_support::SERVICE_STARTED.with(|value| {
        value.set(Some(
            Instant::now().checked_sub(Duration::from_secs(61)).unwrap(),
        ));
    });
    let error = service
        .run_task_sync_exchange(
            &selection,
            &ExchangeOperation::Observe,
            RunLimits {
                deadline: Duration::from_secs(60),
                ..RunLimits::default()
            },
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.downcast_ref::<RunError>(), Some(&RunError::Deadline));
    assert_eq!(test_support::LAUNCHES.with(std::cell::Cell::get), count);
    assert_eq!(fixture.snapshot(), before);
    assert!(!database.exists());
}
