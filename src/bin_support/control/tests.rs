use super::*;
use std::{fs, io::Cursor, time::Instant};

struct FailedStartupWrite;
impl io::Write for FailedStartupWrite {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn terminal_record(home: &Path) -> serde_json::Value {
    (0..4)
        .flat_map(|index| {
            fs::read_to_string(home.join(format!("diagnostics/engram-{index}.jsonl")))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .find(|row| row["event"]["code"] == "control_abnormal_exit")
        .unwrap()
}

#[test]
fn startup_refusal_failed_response_is_durable_and_preserves_transport_error() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = engram::diagnostics::Diagnostics::start(
        home.path(),
        "session-12325",
        engram::diagnostics::Surface::Control,
    );
    let error = engram::StoreError::InvalidHostContext("PRIVATE source detail".into());
    let failure = startup_response(
        &sink,
        Instant::now(),
        &error,
        Cursor::new(b"{}\n"),
        FailedStartupWrite,
    )
    .unwrap_err();
    assert_eq!(failure.kind(), io::ErrorKind::BrokenPipe);
    let record = terminal_record(home.path());
    assert_eq!(record["event"]["reason"], "transport_failure");
    assert_eq!(record["event"]["phase"], "startup_response");
    assert!(!record.to_string().contains("PRIVATE"));
}

#[test]
fn startup_unwind_is_durable_and_maps_to_the_terminal_cli_exit_code() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = engram::diagnostics::Diagnostics::start(
        home.path(),
        "session-12325",
        engram::diagnostics::Surface::Control,
    );
    let error =
        catch_startup(&sink, Instant::now(), || panic!("PRIVATE panic payload")).unwrap_err();
    assert!(error.is::<engram::host::ControlPanic>());
    assert_eq!(failure_exit_code(&error), std::process::ExitCode::from(70));
    assert_eq!(
        failure_exit_code(&anyhow::anyhow!("ordinary refusal")),
        std::process::ExitCode::FAILURE
    );
    let record = terminal_record(home.path());
    assert_eq!(record["event"]["reason"], "panic");
    assert_eq!(record["event"]["phase"], "startup");
    assert!(!record.to_string().contains("PRIVATE"));
}

#[test]
fn startup_unwind_keeps_exit_70_when_diagnostic_append_lock_expires() {
    let _ = engram::build_identity::current();
    let home = crate::test_support::temp_home().unwrap();
    let directory = home.path().join("diagnostics");
    fs::create_dir(&directory).unwrap();
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join("engram.lock"))
        .unwrap();
    lock.lock().unwrap();
    let sink = engram::diagnostics::Diagnostics::start(
        home.path(),
        "session-12325",
        engram::diagnostics::Surface::Control,
    );
    let error =
        catch_startup(&sink, Instant::now(), || panic!("injected startup unwind")).unwrap_err();
    assert!(!directory.join("engram-0.jsonl").exists());
    lock.unlock().unwrap();
    assert!(error.is::<engram::host::ControlPanic>());
    assert_eq!(failure_exit_code(&error), std::process::ExitCode::from(70));
}
