use super::*;

struct FailedRead;

impl std::io::Read for FailedRead {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

#[test]
fn startup_refusal_is_one_bounded_terminal_response_without_dispatch() {
    for input in [b"{}\n{}\n".as_slice(), b"malformed\n", b"partial"] {
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        HostControlServer::serve_startup_failure(
            &StoreError::DifferentBuildSchema,
            &mut reader,
            &mut output,
        )
        .expect("terminal refusal");
        assert_eq!(std::str::from_utf8(&output).unwrap().lines().count(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            serde_json::json!({"status": "error", "error": {
                "code": "control_startup_refused",
                "message": "Engram control startup was refused; inspect the startup diagnostic."
            }})
        );
        assert_eq!(
            usize::try_from(reader.position()).unwrap(),
            input
                .iter()
                .position(|&b| b == b'\n')
                .map_or(input.len(), |i| i + 1)
        );
    }
    // SQLITE_PROTOCOL is the code a store open's locking-protocol refusal
    // keeps once its allowance has ended; it is busy, safe to open again.
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
        rusqlite::ffi::SQLITE_PROTOCOL,
        rusqlite::ffi::SQLITE_LOCKED,
    ] {
        let mut output = Vec::new();
        let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("private diagnostic".into()),
        ));
        HostControlServer::serve_startup_failure(&error, Cursor::new(b"{}\n"), &mut output)
            .unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response["error"]["code"],
            if code == rusqlite::ffi::SQLITE_LOCKED {
                "control_startup_refused"
            } else {
                "control_startup_busy"
            }
        );
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("private diagnostic")
        );
    }
}

thread_local! {
    static CLOCK: std::cell::Cell<std::time::Instant> =
        std::cell::Cell::new(std::time::Instant::now());
}

fn now() -> std::time::Instant {
    CLOCK.with(std::cell::Cell::get)
}

fn open_startup(database: &Path) -> Result<HostControlServer, StoreError> {
    HostControlServer::open(
        database,
        ProjectId("startup-project".into()),
        "host-test".into(),
        SessionId("startup-session".into()),
        None,
    )
}

// Startup's lock waits and its connection write share one control allowance.
// With a second of it left, a store another connection is writing is refused
// as busy after about that second, not after the one-minute ordinary wait, and
// a store that opens keeps the ordinary wait for its later operations.
#[test]
fn startup_waits_only_what_is_left_of_its_control_allowance() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("startup.db");
    drop(SqliteStore::open(&database).unwrap());
    let writer = rusqlite::Connection::open(&database).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let left = std::time::Duration::from_secs(1);
    CLOCK.with(|clock| clock.set(std::time::Instant::now()));
    let _frame = crate::storage::ControlRequestScope::with_clock(now);
    let spent = crate::storage::CONTROL_REQUEST_ALLOWANCE.saturating_sub(left);
    CLOCK.with(|clock| clock.set(clock.get() + spent));
    let started = std::time::Instant::now();
    let Err(refused) = open_startup(&database) else {
        panic!("a store another connection is writing must refuse startup");
    };
    let waited = started.elapsed();
    assert!(
        waited < crate::operation_time::MINIMUM / 2,
        "waited {waited:?}: {refused}"
    );
    let mut output = Vec::new();
    HostControlServer::serve_startup_failure(&refused, Cursor::new(b"{}\n"), &mut output).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap()["error"]["code"],
        "control_startup_busy"
    );
    writer.execute_batch("ROLLBACK").unwrap();
    let Ok(server) = open_startup(&database) else {
        panic!("an idle store opens inside the same allowance");
    };
    assert_eq!(
        server.store.test_busy_timeout(),
        crate::operation_time::MINIMUM
    );
}

/// Delivers a frame's first byte at once and the rest ten seconds later.
struct SlowFrame {
    clock: std::rc::Rc<crate::operation_time::test_clock::Probe>,
    bytes: Cursor<&'static [u8]>,
}

impl std::io::Read for SlowFrame {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes.position() == 1 {
            self.clock
                .advance(crate::operation_time::WARN_AFTER + std::time::Duration::from_secs(1));
        }
        std::io::Read::read(&mut self.bytes, buffer)
    }
}

// A frame still arriving after ten seconds is named once by the frame's
// warning, from its first byte, and is still answered.
#[test]
fn a_frame_still_arriving_after_ten_seconds_warns_once_and_is_answered() {
    let directory = crate::test_support::temp_home().unwrap();
    let Ok(mut server) = open_startup(&directory.path().join("slow-frame.db")) else {
        panic!("an idle store opens");
    };
    let clock = std::rc::Rc::new(crate::operation_time::test_clock::Probe::install());
    let frame = SlowFrame {
        clock: std::rc::Rc::clone(&clock),
        bytes: Cursor::new(b"{\"operation\":\"work_session_read\"}\n".as_slice()),
    };
    let mut output = Vec::new();
    server
        .serve(std::io::BufReader::with_capacity(1, frame), &mut output)
        .unwrap();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["status"], "ok", "{response}");
    assert!(response["result"].is_object(), "{response}");
    let warned: Vec<_> = clock
        .events()
        .into_iter()
        .map(|(operation, _, elapsed)| (operation, elapsed))
        .collect();
    assert_eq!(
        warned,
        [("control request", crate::operation_time::WARN_AFTER)]
    );
}

// After a refused startup, the one frame it answers is a control request too:
// still arriving after ten seconds, it is named once and still answered.
#[test]
fn a_refused_startup_names_a_slow_frame_once_and_answers_it() {
    let clock = std::rc::Rc::new(crate::operation_time::test_clock::Probe::install());
    let frame = SlowFrame {
        clock: std::rc::Rc::clone(&clock),
        bytes: Cursor::new(b"{}\n".as_slice()),
    };
    let mut output = Vec::new();
    HostControlServer::serve_startup_failure(
        &StoreError::DifferentBuildSchema,
        std::io::BufReader::with_capacity(1, frame),
        &mut output,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap()["error"]["code"],
        "control_startup_refused"
    );
    let warned: Vec<_> = clock
        .events()
        .into_iter()
        .map(|(operation, _, elapsed)| (operation, elapsed))
        .collect();
    assert_eq!(
        warned,
        [("control request", crate::operation_time::WARN_AFTER)]
    );
}

#[test]
fn an_expired_startup_allowance_answers_busy() {
    let mut output = Vec::new();
    HostControlServer::serve_startup_failure(
        &StoreError::ControlRequestExpired,
        Cursor::new(b"{}\n"),
        &mut output,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap()["error"]["code"],
        "control_startup_busy"
    );
}

#[test]
fn startup_refusal_stops_on_empty_eof_oversize_and_transport_error() {
    let mut output = Vec::new();
    HostControlServer::serve_startup_failure(
        &StoreError::DifferentBuildSchema,
        Cursor::new(b""),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, [] as [u8; 0]);
    let error = HostControlServer::serve_startup_failure(
        &StoreError::DifferentBuildSchema,
        std::io::BufReader::new(FailedRead),
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(output, [] as [u8; 0]);
    let input = vec![b'x'; MAX_HOST_CONTROL_FRAME_BYTES + 20];
    let mut reader = Cursor::new(input);
    let error = HostControlServer::serve_startup_failure(
        &StoreError::DifferentBuildSchema,
        &mut reader,
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        usize::try_from(reader.position()).unwrap(),
        MAX_HOST_CONTROL_FRAME_BYTES + 1
    );
    assert_eq!(output, [] as [u8; 0]);
    let error = HostControlServer::serve_startup_failure(
        &StoreError::DifferentBuildSchema,
        Cursor::new(b"{}\n"),
        [].as_mut_slice(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
}
