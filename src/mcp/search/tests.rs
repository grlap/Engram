use super::*;
use crate::mcp::parameters::Parameters;
use crate::storage::search_budget::TestPause;
use crate::{ProjectId, SessionId};
use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("test watchdog")
}

fn fixture() -> (crate::test_support::TempHome, McpServer, String) {
    let home = crate::test_support::temp_home().unwrap();
    let server = McpServer::new_with_actor_context(
        home.path().join("search.sqlite3"),
        ProjectId("search-worker".into()),
        "search-actor".into(),
        SessionId("search-session".into()),
        None,
        None,
    );
    let added = server.add(Parameters(
        serde_json::from_value(json!({"title":"Search Straße"})).unwrap(),
    ));
    assert_ne!(added.is_error, Some(true), "{added:?}");
    let work = added.structured_content.unwrap()["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let note = server.note(Parameters(
        serde_json::from_value(json!({"work_ref":work,"text":"literal [needle] Straße"})).unwrap(),
    ));
    assert_ne!(note.is_error, Some(true), "{note:?}");
    (home, server, work)
}

/// Independent rescue only: assertions use message/scan ordering, never sleeps.
struct Rescue {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    fired: Arc<std::sync::atomic::AtomicBool>,
}
impl Rescue {
    fn new(pause: Arc<TestPause>) -> Self {
        let (stop, recv) = std::sync::mpsc::channel();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&fired);
        let thread = std::thread::spawn(move || {
            if recv.recv_timeout(Duration::from_secs(12)).is_err() {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                pause.release.notify_one();
            }
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
            fired,
        }
    }
    fn assert_not_used(&self) {
        assert!(!self.fired.load(std::sync::atomic::Ordering::SeqCst));
    }
}
impl Drop for Rescue {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

type Reader = tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>;
type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;
async fn send(writer: &mut Writer, value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}
async fn reply(reader: &mut Reader, id: u64) -> Value {
    let value: Value =
        serde_json::from_str(&bounded(reader.next_line()).await.unwrap().unwrap()).unwrap();
    assert_eq!(value["id"], id, "{value}");
    value
}
async fn call(writer: &mut Writer, id: u64, name: &str, arguments: Value) {
    send(writer, json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await;
}
async fn connect(server: McpServer) -> (Reader, Writer, tokio::task::JoinHandle<()>) {
    let (client, io) = tokio::io::duplex(1 << 16);
    let serving = tokio::spawn(async move {
        let shutdown = server.clone();
        let running = server.serve(tokio::io::split(io)).await.unwrap();
        running.waiting().await.unwrap();
        shutdown.shutdown_searches().await;
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader).lines();
    send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"search-test","version":"0"}}})).await;
    reply(&mut reader, 1).await;
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    (reader, writer, serving)
}

#[tokio::test(flavor = "current_thread")]
async fn search_does_not_block_other_verbs_on_the_same_mcp_connection() {
    let (_home, server, work) = fixture();
    let (pause, entered) = TestPause::new();
    let rescue = Rescue::new(Arc::clone(&pause));
    *server.searches.pause.lock().unwrap() = Some(Arc::clone(&pause));
    let (mut reader, mut writer, serving) = connect(server).await;
    call(&mut writer, 2, "search", json!({"query":"[needle]"})).await;
    bounded(entered).await.unwrap();
    for (id, name, args) in [
        (3, "show", json!({"work_ref":work})),
        (4, "ls", json!({})),
        (5, "next", json!({"peek":true})),
        (
            6,
            "note",
            json!({"work_ref":work,"text":"write while search is paused"}),
        ),
    ] {
        call(&mut writer, id, name, args).await;
        let result = reply(&mut reader, id).await;
        assert!(result.get("error").is_none(), "{result}");
        assert_ne!(result["result"]["isError"], true, "{result}");
    }
    call(&mut writer, 7, "ls", json!({"search":"needle"})).await;
    let busy = reply(&mut reader, 7).await;
    assert_eq!(
        busy["result"]["structuredContent"]["error"]["code"],
        "search_busy"
    );
    rescue.assert_not_used();
    pause.release.notify_one();
    let found = reply(&mut reader, 2).await;
    assert_ne!(found["result"]["isError"], true, "{found}");
    assert!(found.to_string().contains(&work));
    writer.shutdown().await.unwrap();
    bounded(serving).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_search_cancellation_and_eof_join_the_worker() {
    for cancel in [true, false] {
        let (_home, server, _) = fixture();
        let (pause, entered) = TestPause::new();
        let rescue = Rescue::new(Arc::clone(&pause));
        *server.searches.pause.lock().unwrap() = Some(pause);
        let owner = server.clone();
        let (mut reader, mut writer, serving) = connect(server).await;
        call(&mut writer, 2, "search", json!({"query":"needle"})).await;
        bounded(entered).await.unwrap();
        if cancel {
            send(&mut writer, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}})).await;
            call(&mut writer, 3, "show", json!({"work_ref":"w-000000000000"})).await;
            reply(&mut reader, 3).await;
            // Acquiring the slot waits for the original handler's join.
            assert!(bounded(owner.searches.job.lock()).await.is_none());
        }
        writer.shutdown().await.unwrap();
        bounded(serving).await.unwrap();
        assert!(owner.searches.job.lock().await.is_none());
        rescue.assert_not_used();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_search_request_keeps_its_handle_for_shutdown() {
    let (_home, server, _) = fixture();
    let (pause, entered) = TestPause::new();
    let rescue = Rescue::new(Arc::clone(&pause));
    *server.searches.pause.lock().unwrap() = Some(pause);
    let caller = server.clone();
    let task = tokio::spawn(async move {
        caller
            .blocking_search(CancellationToken::new(), |verbs| {
                verbs.search("needle", None, chrono::Utc::now())
            })
            .await
    });
    bounded(entered).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(server.searches.job.lock().await.is_some());
    bounded(server.shutdown_searches()).await;
    assert!(server.searches.job.lock().await.is_none());
    rescue.assert_not_used();
}

#[tokio::test]
async fn panicked_search_is_joined_and_does_not_poison_admission() {
    let (_home, server, _) = fixture();
    let failure = server
        .blocking_search(CancellationToken::new(), |_| {
            panic!("injected worker failure")
        })
        .await;
    assert_eq!(
        failure.structured_content.unwrap()["error"]["code"],
        "search_worker_failed"
    );
    let next = server
        .blocking_search(CancellationToken::new(), |verbs| {
            verbs.search("STRASSE", None, chrono::Utc::now())
        })
        .await;
    assert_ne!(next.is_error, Some(true), "{next:?}");
    server.shutdown_searches().await;
}

#[tokio::test]
async fn search_worker_phases_are_merged_into_the_awaiting_call() {
    let (_home, server, _) = fixture();
    let (result, phases) =
        crate::phase_trace::scoped(server.blocking_search(CancellationToken::new(), |verbs| {
            verbs.search("STRASSE", None, chrono::Utc::now())
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(phases.read_snapshot.count > 0, "{phases:?}");
    assert!(phases.sqlite_statement.count > 0, "{phases:?}");
    assert!(phases.receipt_serialize.count > 0, "{phases:?}");
    assert!(!crate::phase_trace::active());
    server.shutdown_searches().await;
}
