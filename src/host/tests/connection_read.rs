use std::io::Cursor;

use rusqlite::Connection;

use super::*;

fn open_server(database: &Path, session: &str) -> HostControlServer {
    HostControlServer::open(
        database,
        ProjectId("connection-read-project".into()),
        "host-test".into(),
        SessionId(session.into()),
        None,
    )
    .expect("control server")
}

fn read(server: &mut HostControlServer) -> Result<Value, StoreError> {
    server.handle(HostControlRequest::ControlConnectionRead {})
}

fn refusal(server: &mut HostControlServer) -> &'static str {
    store_error_code(&read(server).expect_err("the read must refuse"))
}

/// Everything another connection can see change in the store file.
fn observed(observer: &Connection) -> (i64, Vec<(String, String, i64)>) {
    let version = observer
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    let mut statement = observer
        .prepare(
            "SELECT session_id, connection_token, opened_at_ms FROM control_connections \
             ORDER BY session_id",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (version, rows)
}

/// No platform can yet prove that the file at the store's path is the one
/// SQLite holds, so the read refuses on a current connection, original or
/// resumed, and never returns a path. It checks the connection first, needs
/// no bind, token or grant, and writes nothing.
#[test]
fn the_read_refuses_after_the_connection_check_and_writes_nothing() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("control.db");
    let mut original = open_server(&database, "read-session");

    let observer = Connection::open(&database).unwrap();
    let before = observed(&observer);
    assert_eq!(
        refusal(&mut original),
        "control_connection_identity_unavailable"
    );
    assert_eq!(observed(&observer), before);

    // A resumed successor refuses alike, and its superseded predecessor is
    // refused by the connection check, which comes first.
    let mut successor = open_server(&database, "read-session");
    let resumed = observed(&observer);
    assert_eq!(
        refusal(&mut successor),
        "control_connection_identity_unavailable"
    );
    assert_eq!(refusal(&mut original), "control_connection_superseded");

    // On the wire the operation takes no fields and refuses any; the
    // refusal carries its code and no path.
    let mut input = Vec::new();
    for frame in [
        serde_json::json!({"operation": "control_connection_read"}),
        serde_json::json!({"operation": "control_connection_read", "routing_token": "any"}),
    ] {
        serde_json::to_writer(&mut input, &frame).unwrap();
        input.push(b'\n');
    }
    let mut output = Vec::new();
    successor.serve(Cursor::new(input), &mut output).unwrap();
    let replies = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies[0]["status"], "error");
    assert_eq!(
        replies[0]["error"]["code"],
        "control_connection_identity_unavailable"
    );
    assert!(replies[0].get("result").is_none(), "{}", replies[0]);
    assert!(
        !replies[0].to_string().contains("control.db"),
        "{}",
        replies[0]
    );
    assert_eq!(replies[1]["error"]["code"], "invalid_request");
    assert_eq!(observed(&observer), resumed);
}

/// An in-memory store has no file at all; it refuses like every store, and
/// guards that a future design that can prove a file still refuses here.
#[test]
fn a_temporary_or_in_memory_store_is_refused() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let session_id = SessionId("memory-session".into());
    let connection_token = store
        .resume_control_connection(&session_id, Utc::now())
        .unwrap();
    let mut server = HostControlServer {
        store,
        diagnostic_store: crate::diagnostics::Fingerprint::store(Path::new(":memory:")),
        project_id: ProjectId("memory-project".into()),
        actor_id: "host-test".into(),
        session_id,
        connection_token,
        source_skill: None,
        actor_context: None,
        actor_context_normalized: false,
        diagnostics: None,
        diagnostic_frame: None,
        diagnostic_sequence: 0,
    };
    assert_eq!(
        refusal(&mut server),
        "control_connection_identity_unavailable"
    );
}

#[cfg(windows)]
#[test]
fn the_windows_verbatim_prefix_is_removed_as_readiness_removes_it() {
    use crate::storage::store_path_text;
    assert_eq!(
        store_path_text(Path::new(r"\\?\C:\store\x.db")),
        r"C:\store\x.db"
    );
    assert_eq!(
        store_path_text(Path::new(r"\\?\UNC\server\share\x.db")),
        r"\\server\share\x.db"
    );
    assert_eq!(
        store_path_text(Path::new(r"C:\plain\x.db")),
        r"C:\plain\x.db"
    );
}
