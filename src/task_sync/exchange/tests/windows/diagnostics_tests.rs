use super::*;
use crate::task_sync::exchange::process;

const OUT_MARKER: &str = "owned-deadline-secret-stdout";
const ERR_MARKER: &str = "owned-deadline-secret-stderr";

fn diagnostic_command(fixture: &Fixture) -> Command {
    fixture.isolated_config_command_for(
        "task_sync::exchange::tests::windows::diagnostics_tests::diagnostic_fixture",
    )
}

fn captured_readiness(stdout: &[u8], stderr: &[u8]) -> bool {
    let trace = diagnostics::parse(stdout);
    !trace.incomplete
        && trace.entries.len() == 3
        && trace.entries[0].kind == "direct"
        && trace.entries[0].end.is_some()
        && trace.entries[1].kind == "product"
        && trace.entries[1].end.is_some()
        && trace.entries[2].kind == "product"
        && trace.entries[2].label == "fixture"
        && trace.entries[2].end.is_none()
        && stdout
            .windows(OUT_MARKER.len())
            .any(|value| value == OUT_MARKER.as_bytes())
        && stderr
            .windows(ERR_MARKER.len())
            .any(|value| value == ERR_MARKER.as_bytes())
}

#[tokio::test]
async fn task_exchange_deadline_diagnostics_keep_partial_output_and_name_active_launch() {
    let fixture = Fixture::new();
    let limits = RunLimits::default();
    let output = process::execute_diagnostic(
        diagnostic_command(&fixture),
        limits,
        &Cancellation::default(),
        Some(&captured_readiness),
    )
    .await;
    assert_eq!(output.result, Err(RunError::Deadline));
    // Only bytes delivered into the capture buffers before expiry are retained.
    // No pipe drain is awaited after the deadline; bytes still in OS/Tokio
    // buffering are outside this guarantee. Readiness requires both markers
    // already captured, so startup failure cannot pass as this forced deadline.
    assert!(captured_readiness(&output.stdout, &output.stderr));
    assert!(output.stdout.len() <= limits.stdout_bytes);
    assert!(output.stderr.len() <= limits.stderr_bytes);
    let report = diagnostics::report(&output);
    assert!(report.contains("completed launch=1 kind=direct label=version elapsed_us="));
    assert!(report.contains("completed launch=2 kind=product label=version elapsed_us="));
    assert!(report.contains("active launch=3 kind=product label=fixture began_at_group_us="));
    assert!(report.contains(OUT_MARKER));
    assert!(report.contains(ERR_MARKER));
    fixture.assert_released();
}

#[tokio::test]
async fn task_exchange_deadline_diagnostics_production_error_never_discloses_capture() {
    let fixture = Fixture::new();
    // Exercise the same sanitized result mapping used by production execute,
    // with only its per-call, test-compiled deadline predicate injected.
    let error = process::execute_sanitized(
        diagnostic_command(&fixture),
        RunLimits::default(),
        &Cancellation::default(),
        false,
        Some(&captured_readiness),
    )
    .await
    .unwrap_err();
    assert_eq!(error, RunError::Deadline);
    for display in [error.to_string(), format!("{error:?}")] {
        assert!(!display.contains(OUT_MARKER));
        assert!(!display.contains(ERR_MARKER));
        assert!(!display.contains("exchange-launch"));
    }
    fixture.assert_released();
}

#[tokio::test]
async fn task_exchange_deadline_diagnostics_capture_stays_bounded_on_both_pipes() {
    for mode in ["flood", "stderr-flood"] {
        let fixture = Fixture::new();
        let mut limits = RunLimits::default();
        if mode == "flood" {
            // Leave room for libtest's test-name banner before the real flood.
            limits.stdout_bytes = 256;
        } else {
            limits.stderr_bytes = 64;
        }
        let mut command = fixture.command(mode);
        // Single-thread libtest prints a longer stdout preamble. Keep the
        // non-flooded pipe's ordinary bound so it cannot mask the tested pipe.
        command.arg("--test-threads=1");
        let output =
            process::execute_diagnostic(command, limits, &Cancellation::default(), None).await;
        assert_eq!(output.result, Err(RunError::OutputLimit));
        assert!(output.stdout.len() <= limits.stdout_bytes);
        assert!(output.stderr.len() <= limits.stderr_bytes);
        if mode == "flood" {
            assert_eq!(output.stdout.len(), limits.stdout_bytes);
        } else {
            assert_eq!(output.stderr.len(), limits.stderr_bytes);
        }
        fixture.assert_released();
    }
}

#[tokio::test]
async fn task_exchange_deadline_diagnostics_spawn_failure_is_completed_not_active() {
    let fixture = Fixture::new();
    let mut command = diagnostic_command(&fixture);
    command.env("ENGRAM_EXCHANGE_DIAGNOSTIC_MODE", "spawn-failure");
    let output = process::execute_diagnostic(
        command,
        RunLimits::default(),
        &Cancellation::default(),
        None,
    )
    .await;
    assert_eq!(output.result, Ok(()));
    let trace = diagnostics::parse(&output.stdout);
    assert!(!trace.incomplete);
    assert_eq!(trace.entries.len(), 1);
    assert_eq!(trace.entries[0].end.as_ref().unwrap().1, "spawn");
}

#[tokio::test]
#[ignore = "owned diagnostic child; exercised by forced-deadline tests"]
async fn diagnostic_fixture() {
    let fixture = Fixture(PathBuf::from(
        std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap(),
    ));
    let _trace = diagnostics::begin();
    let cancel = Cancellation::default();
    if std::env::var("ENGRAM_EXCHANGE_DIAGNOSTIC_MODE").as_deref() == Ok("spawn-failure") {
        assert_eq!(
            super::super::super::execute(
                Command::new(fixture.0.join("missing.exe")),
                RunLimits::default(),
                &cancel,
            )
            .await
            .unwrap_err(),
            RunError::Spawn
        );
        return;
    }
    let mut direct = Command::new("git");
    direct.arg("--version");
    assert!(policy::workload::output(&mut direct).status.success());
    let product = crate::task_sync::exchange::command::local(&fixture.0, &["--version"]);
    super::super::super::execute(product, RunLimits::default(), &cancel)
        .await
        .unwrap();
    let blocking =
        super::super::super::execute(fixture.command("block"), RunLimits::default(), &cancel);
    tokio::pin!(blocking);
    readiness::while_running(&fixture.0, blocking.as_mut())
        .await
        .unwrap();
    {
        // These must come from this outer child. The blocking product process
        // owns different pipes; its output is not forwarded to our supervisor.
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{OUT_MARKER}").unwrap();
        stdout.flush().unwrap();
        drop(stdout);
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "{ERR_MARKER}").unwrap();
        stderr.flush().unwrap();
    }
    let result = blocking.await;
    panic!("owned diagnostic child should be stopped by outer deadline: {result:?}");
}

#[test]
fn task_exchange_deadline_diagnostics_parser_preserves_prefix_and_rejects_bad_records() {
    let complete =
        "exchange-launch begin 1 direct version 1\nexchange-launch end 1 direct version 5 ok\n";
    let prefix = diagnostics::parse(complete.as_bytes());
    assert!(!prefix.incomplete);
    assert_eq!(prefix.entries[0].end.as_ref().unwrap().0, 5);
    for tail in [
        "exchange-launch begin 2 product fixture 9", // incomplete final record
        "exchange-launch begin 2 product secret-url 9\n",
        "exchange-launch begin 1 product fixture 9\n", // duplicate ordinal
        "exchange-launch end 1 direct version 6 ok\n", // duplicate END
        "exchange-launch begin 3 product fixture 9\n", // skipped ordinal
        "exchange-launch begin 2 product fixture nope\n",
        "exchange-launch end 2 product fixture 9 ok\n",
    ] {
        let trace = diagnostics::parse(format!("{complete}{tail}").as_bytes());
        assert!(trace.incomplete, "{tail}");
        assert_eq!(trace.entries.len(), 1);
        assert!(trace.entries[0].end.is_some());
    }
    let missing = diagnostics::parse(b"ordinary child output\n");
    assert!(missing.entries.is_empty());
    let active = diagnostics::parse(
        format!("{complete}exchange-launch begin 2 product fixture 9\n").as_bytes(),
    );
    assert!(!active.incomplete);
    assert_eq!(active.entries[1].end, None);
}

#[test]
fn task_exchange_deadline_diagnostics_labels_never_copy_arguments_or_environment() {
    let secret = "https://operator:secret-token@example.invalid";
    let mut command = Command::new(secret);
    command
        .args(["config", "--get", secret])
        .env("SECRET", secret);
    assert_eq!(diagnostics::label(&command), "config");
}

#[test]
fn task_exchange_deadline_diagnostics_report_states_when_activity_is_unknown() {
    let complete =
        "exchange-launch begin 1 direct version 1\nexchange-launch end 1 direct version 5 ok\n";
    let active = format!("{complete}exchange-launch begin 2 product fixture 9\n");
    for (stdout, expected) in [
        (
            String::new(),
            "active launch unknown: no unmatched BEGIN was captured",
        ),
        (
            complete.into(),
            "active launch unknown: no unmatched BEGIN was captured",
        ),
        (
            format!("{complete}exchange-launch begin 2"),
            "active launch unknown: trace incomplete",
        ),
        (
            format!("{active}exchange-launch end 2"),
            "active launch unknown: trace incomplete",
        ),
        (
            active,
            "active launch=2 kind=product label=fixture began_at_group_us=9",
        ),
    ] {
        let output = process::DiagnosticOutput {
            result: Err(RunError::Deadline),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        };
        let report = diagnostics::report(&output);
        let summary = report.split_once("captured stdout:\n").unwrap().0;
        assert!(summary.contains(expected), "{summary}");
        if summary.contains("trace_incomplete=true") {
            assert!(!summary.contains("active launch="), "{summary}");
        }
    }
}
