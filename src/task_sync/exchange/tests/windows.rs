use super::*;
use std::{
    fs, io::Write, os::windows::fs::OpenOptionsExt, path::Path, process::Stdio, time::Instant,
};

mod admission;
mod configuration;
mod diagnostics_tests;
mod policy;
pub(in crate::task_sync::exchange) mod readiness;
use super::super::fixture_diagnostics as diagnostics;
use readiness::descendant_holds_lock;

struct Fixture(PathBuf);
impl Fixture {
    fn phase(&self, phase: &str) {
        fs::write(self.0.join("phase"), phase).unwrap();
    }
    fn new() -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join("target/tmp")
            .join(format!("exchange-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "task_sync::exchange::tests::windows::process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("ENGRAM_EXCHANGE_FIXTURE", mode)
            .env("ENGRAM_EXCHANGE_FIXTURE_ROOT", &self.0);
        command
    }
    fn assert_released(&self) {
        for name in ["child", "grandchild"] {
            let path = self.0.join(format!("{name}.lock"));
            assert!(path.exists(), "fixture descendant never started: {name}");
            let start = Instant::now();
            loop {
                if fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(&path)
                    .is_ok()
                {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(3),
                    "descendant survived job close: {name}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    fn git(&self, args: &[&str]) {
        let mut command = Command::new("git");
        command
            .current_dir(&self.0)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", self.0.join("global.config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        configuration::record_direct_launch();
        let output = policy::workload::output(&mut command);
        assert!(
            output.status.success(),
            "fixture Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn isolated_config_command_for(&self, test: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env("ENGRAM_EXCHANGE_FIXTURE_ROOT", &self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("xdg"))
            .env("GIT_CONFIG_GLOBAL", self.0.join("global.config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1");
        for name in [
            "GIT_CONFIG",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            command.env_remove(name);
        }
        command
    }
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                if metadata.is_dir() {
                    visit(root, &path, entries);
                } else {
                    entries.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }
        let mut entries = Vec::new();
        visit(&self.0, &self.0, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}

#[test]
#[ignore = "subprocess entry; exercised by owned exchange tests"]
fn process_fixture() {
    readiness::process_fixture();
}

#[tokio::test]
async fn task_exchange_windows_cancellation_kills_descendants_and_inherited_pipes() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    let limits = RunLimits {
        deadline: Duration::from_secs(15),
        ..RunLimits::default()
    };
    let command = fixture.command("block");
    let run = super::super::execute(command, limits, &cancel);
    tokio::pin!(run);
    // The existing 15s execution watchdog bounds startup and cancellation.
    readiness::while_running(&fixture.0, run.as_mut())
        .await
        .unwrap();
    cancel.cancel();
    let result = run.await;
    readiness::finished(&fixture.0, &result);
    assert_eq!(result.unwrap_err(), RunError::Cancelled);
    fixture.assert_released();
}

#[tokio::test]
async fn task_exchange_windows_root_exit_and_output_failure_reap_helpers() {
    for mode in ["root-exits", "flood", "stderr-flood", "secret-error"] {
        let fixture = Fixture::new();
        let limits = RunLimits {
            deadline: Duration::from_secs(12),
            stdout_bytes: 4096,
            stderr_bytes: 4096,
        };
        let cancel = Cancellation::default();
        let result = super::super::execute(fixture.command(mode), limits, &cancel).await;
        readiness::finished(&fixture.0, &result);
        match mode {
            "root-exits" => {
                result.unwrap();
            }
            "flood" | "stderr-flood" => assert_eq!(result.unwrap_err(), RunError::OutputLimit),
            _ => {
                let error = result.unwrap_err();
                assert_eq!(error, RunError::Git);
                assert!(!error.to_string().contains("secret-token"));
            }
        }
        fixture.assert_released();
    }
}

#[tokio::test]
async fn task_exchange_windows_deadline_and_dropped_future_close_the_job() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    let limits = RunLimits {
        deadline: Duration::from_secs(12),
        ..RunLimits::default()
    };
    let result = super::super::execute(fixture.command("block"), limits, &cancel).await;
    readiness::finished(&fixture.0, &result);
    assert_eq!(result.unwrap_err(), RunError::Deadline);
    fixture.assert_released();
    let fixture = Fixture::new();
    let command = fixture.command("block");
    let limits = RunLimits {
        deadline: Duration::from_secs(15),
        ..RunLimits::default()
    };
    {
        let run = super::super::execute(command, limits, &cancel);
        tokio::pin!(run);
        readiness::while_running(&fixture.0, run.as_mut())
            .await
            .unwrap();
    }
    fixture.assert_released();
    let missing = Command::new(fixture.0.join("absent.exe"));
    assert_eq!(
        super::super::execute(missing, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::Spawn
    );
}

#[tokio::test]
async fn task_exchange_windows_cancelled_or_invalid_requests_create_no_process() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    cancel.cancel();
    let launches = super::super::test_support::LAUNCHES.with(std::cell::Cell::get);
    assert_eq!(
        super::super::execute(fixture.command("block"), RunLimits::default(), &cancel)
            .await
            .unwrap_err(),
        RunError::Cancelled
    );
    let limits = RunLimits {
        deadline: Duration::ZERO,
        ..RunLimits::default()
    };
    assert_eq!(
        super::super::execute(fixture.command("block"), limits, &Cancellation::default())
            .await
            .unwrap_err(),
        RunError::InvalidLimits
    );
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    assert_eq!(
        super::super::test_support::LAUNCHES.with(std::cell::Cell::get),
        launches
    );
}

#[tokio::test]
async fn task_exchange_windows_config_capture_and_drift_never_run_unsupported_helpers() {
    configuration::run_groups().await;
}

#[tokio::test]
async fn task_exchange_service_refuses_read_only_and_opt_out_before_any_git_process() {
    let fixture = Fixture::new();
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
