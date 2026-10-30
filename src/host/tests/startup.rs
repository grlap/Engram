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
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
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
