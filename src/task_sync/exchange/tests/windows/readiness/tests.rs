use super::*;

#[tokio::test]
async fn task_exchange_windows_readiness_reports_early_execution_errors() {
    for (mode, expected) in [("missing", "Err(Spawn)"), ("fail-before-ready", "Err(Git)")] {
        let fixture = Fixture::new();
        let command = if mode == "missing" {
            Command::new(fixture.0.join("absent.exe"))
        } else {
            fixture.command(mode)
        };
        let cancel = Cancellation::default();
        let limits = RunLimits {
            deadline: Duration::from_secs(15),
            ..RunLimits::default()
        };
        let run = crate::task_sync::exchange::execute(command, limits, &cancel);
        tokio::pin!(run);
        let error = while_running(&fixture.0, run.as_mut()).await.unwrap_err();
        assert!(
            error.contains("execution ended before readiness"),
            "{error}"
        );
        assert!(error.contains(expected), "{error}");
        assert!(!fixture.0.join("grandchild.lock").exists());
    }
}

#[tokio::test]
async fn task_exchange_windows_readiness_distinguishes_released_handle_and_io_error() {
    for directory in [false, true] {
        let fixture = Fixture::new();
        let lock = fixture.0.join("grandchild.lock");
        if directory {
            fs::create_dir(lock).unwrap();
        } else {
            fs::write(lock, b"released").unwrap();
        }
        // No process is started. This finite control yields once for the probe,
        // then ends: a readiness regression must fail rather than hang the test.
        let mut yielded = false;
        let run = std::future::poll_fn(move |context| {
            if yielded {
                std::task::Poll::Ready(Err::<RunOutput, _>(RunError::Deadline))
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        });
        tokio::pin!(run);
        let error = while_running(&fixture.0, run.as_mut()).await.unwrap_err();
        let expected = if directory {
            "lock probe failed"
        } else {
            "released before readiness"
        };
        assert!(error.contains(expected), "{error}");
    }
}
