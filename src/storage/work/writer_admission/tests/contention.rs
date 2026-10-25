//! A synthetic two-process holder/waiter protocol. All files and stores are
//! below the repository target directory; no operational store is opened.
use super::*;
use crate::diagnostics::{
    Correlation, Diagnostics, Fingerprint, Surface,
    tests::{drain, records},
    transaction::{Context, Scope},
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
const CHILD_HOME: &str = "ENGRAM_WRITER_CONTENTION_HOME";
const CHILD_CAPTURE: &str = "ENGRAM_WRITER_CONTENTION_CAPTURE";
const WATCHDOG: Duration = Duration::from_secs(120);

fn scoped(home: &Path, operation: &str) -> (Diagnostics, Scope) {
    let sink = Diagnostics::start(home, "session-12329", Surface::Mcp);
    let scope = Scope::enter(
        Context::new(
            sink.clone(),
            operation,
            Correlation::default(),
            Fingerprint::store(&home.join("shared.db")),
        )
        .capture_all_for_test(),
    );
    (sink, scope)
}
pub(super) fn within_target(home: &Path) -> PathBuf {
    let home = home.canonicalize().unwrap();
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .canonicalize()
        .unwrap();
    assert!(home.starts_with(target));
    home
}
pub(super) fn wait_file(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < WATCHDOG,
            "barrier watchdog: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn holder(home: &Path) {
    let home = within_target(home);
    let capture = std::env::var(CHILD_CAPTURE).unwrap() == "1";
    let mut store = SqliteStore::open(home.join("shared.db")).unwrap();
    let diagnostic = capture.then(|| scoped(&home, "update"));
    let transaction = store.begin_work_mutation().unwrap();
    let started = Instant::now();
    // Test protocol only: make the acquisition evidence durable before the
    // parent starts waiting. Product transaction hooks never flush to disk.
    if let Some((sink, _)) = &diagnostic {
        drain(sink);
    }
    fs::write(home.join("ready"), b"acquired").unwrap();
    wait_file(&home.join("release"));
    transaction.commit().unwrap();
    let held_ms = u64::try_from(started.elapsed().as_millis()).unwrap();
    if let Some((sink, _)) = &diagnostic {
        drain(sink);
    }
    fs::write(
        home.join("holder.json"),
        serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "observed_interval_ms": held_ms,
        }))
        .unwrap(),
    )
    .unwrap();
}

pub(super) struct OwnedChild {
    pub(super) process: Child,
    pub(super) home: PathBuf,
}
impl OwnedChild {
    fn evidence(&self) -> String {
        fn tail(path: &Path) -> String {
            let Ok(mut file) = fs::File::open(path) else {
                return "unavailable".into();
            };
            let length = file
                .metadata()
                .map_or(0, |metadata| metadata.len())
                .min(16384);
            if file
                .seek(SeekFrom::End(-i64::try_from(length).unwrap()))
                .is_err()
            {
                return "unreadable".into();
            }
            let mut bytes = Vec::new();
            if file.take(16384).read_to_end(&mut bytes).is_err() {
                return "unreadable".into();
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
        format!(
            "stdout:\n{}\nstderr:\n{}",
            tail(&self.home.join("child.stdout")),
            tail(&self.home.join("child.stderr"))
        )
    }

    pub(super) fn ready(&mut self) {
        let started = Instant::now();
        while !self.home.join("ready").exists() {
            if let Some(status) = self.process.try_wait().unwrap() {
                panic!(
                    "owned holder exited before acquisition: {status}\n{}",
                    self.evidence()
                );
            }
            assert!(
                started.elapsed() < WATCHDOG,
                "owned holder acquisition watchdog\n{}",
                self.evidence()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn wait(&mut self) {
        let started = Instant::now();
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                assert!(status.success(), "owned holder failed\n{}", self.evidence());
                return;
            }
            assert!(
                started.elapsed() < WATCHDOG,
                "owned holder did not settle\n{}",
                self.evidence()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = fs::write(self.home.join("release"), b"release");
        if self.process.try_wait().ok().flatten().is_none() {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
        if std::thread::panicking() {
            eprintln!("owned holder failure evidence\n{}", self.evidence());
        }
    }
}

#[test]
fn known_holder_is_identified_without_changing_the_note_budget() {
    if let Some(home) = std::env::var_os(CHILD_HOME) {
        holder(Path::new(&home));
        return;
    }
    let mut measurements = Vec::new();
    for capture in [false, true] {
        let home = crate::test_support::temp_home().unwrap();
        let root = within_target(home.path());
        let mut store = SqliteStore::open(root.join("shared.db")).unwrap();
        // Keep the synthetic seed inside the admitted store schema. A second
        // real opener validates it before starting the child protocol.
        drop(SqliteStore::open(root.join("shared.db")).expect("admitted synthetic seed"));
        let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::known_holder_is_identified_without_changing_the_note_budget"),
                "--nocapture",
            ])
            .env(CHILD_HOME, &root)
            .env(CHILD_CAPTURE, if capture { "1" } else { "0" })
            .current_dir(&root)
            .stdout(Stdio::from(
                fs::File::create(root.join("child.stdout")).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(root.join("child.stderr")).unwrap(),
            ))
            .spawn()
            .unwrap();
        let mut child = OwnedChild {
            process: child,
            home: root.clone(),
        };
        child.ready();
        let diagnostic = capture.then(|| scoped(&root, "note"));
        let started = Instant::now();
        let error = {
            let mut note = store.note_writer_admission();
            note.begin_work_mutation().unwrap_err()
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap();
        let StoreError::WorkWriterAdmissionRefused {
            reason: WorkWriterAdmissionReason::BusyBudgetExhausted,
            budget_ms,
            ..
        } = error
        else {
            panic!("expected native admission refusal: {error:?}");
        };
        assert!(budget_ms > 0 && budget_ms <= 10000);
        assert_eq!(timeout(&store), 5000);
        assert!(store.connection.is_autocommit());
        // Evidence protocol only: drain the waiter while the child's writer
        // is idle at the release barrier. Production ring writes remain
        // nonblocking and may lose records under concurrent lock contention.
        if let Some((sink, _)) = &diagnostic {
            drain(sink);
        }
        fs::write(root.join("release"), b"release").unwrap();
        child.wait();
        let holder: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("holder.json")).unwrap()).unwrap();
        let held_ms = holder["observed_interval_ms"].as_u64().unwrap();
        assert!(held_ms >= elapsed_ms, "{holder} vs waiter {elapsed_ms}");
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
            before,
            "neither the holding transaction nor refused attempt changes durable rows"
        );
        if let Some((sink, _)) = &diagnostic {
            drain(sink);
        }
        let rows = records(&root);
        let transactions = rows
            .iter()
            .filter(|r| r["event"]["kind"] == "store_transaction")
            .collect::<Vec<_>>();
        if capture {
            let acquired = transactions
                .iter()
                .find(|r| r["event"]["stage"] == "acquired")
                .unwrap();
            let settled = transactions
                .iter()
                .find(|r| r["event"]["stage"] == "settled")
                .unwrap();
            let refused = transactions
                .iter()
                .find(|r| r["event"]["stage"] == "acquisition_failed")
                .unwrap();
            assert_eq!(transactions.len(), 3, "{transactions:?}");
            assert_eq!(acquired["producer"]["pid"], holder["pid"]);
            assert_eq!(settled["producer"]["pid"], holder["pid"]);
            assert_ne!(refused["producer"]["pid"], holder["pid"]);
            assert_eq!(
                acquired["producer"]["instance_id"],
                settled["producer"]["instance_id"]
            );
            assert_eq!(
                acquired["event"]["store_transaction"]["transaction_seq"],
                settled["event"]["store_transaction"]["transaction_seq"]
            );
            assert_eq!(settled["event"]["operation"], "update");
            assert_eq!(refused["event"]["operation"], "note");
            assert_eq!(
                settled["event"]["store_transaction"]["store_path_fingerprint"],
                refused["event"]["store_transaction"]["store_path_fingerprint"]
            );
            assert_eq!(
                settled["event"]["store_transaction"]["connection_autocommit"],
                true
            );
            assert!(refused["event"]["store_transaction"]["acquired_interval_ms"].is_null());
            assert_eq!(
                refused["event"]["store_transaction"]["budget"]["shared_window_ms"],
                10000
            );
            assert!(
                settled["event"]["store_transaction"]["acquired_interval_ms"]
                    .as_u64()
                    .unwrap()
                    >= refused["event"]["store_transaction"]["admission_ms"]
                        .as_u64()
                        .unwrap()
            );
        } else {
            assert_eq!(transactions, Vec::<&serde_json::Value>::new());
        }
        measurements.push(serde_json::json!({
            "capture": if capture { "full" } else { "no_observer" },
            "configured_note_window_ms": 10000, "effective_allowance_ms": budget_ms,
            "waiter_elapsed_ms": elapsed_ms, "holder_observed_interval_ms": held_ms,
            "retained_transaction_records": transactions.len(), "holder_pid": holder["pid"],
            "diagnostic_holder": transactions.iter()
                .find(|r| r["event"]["stage"] == "settled")
                .map(|r| serde_json::json!({
                    "instance_id": r["producer"]["instance_id"],
                    "transaction_seq": r["event"]["store_transaction"]["transaction_seq"],
                    "acquired_interval_ms": r["event"]["store_transaction"]["acquired_interval_ms"],
                })),
        }));
    }
    println!(
        "synthetic_writer_contention={}",
        serde_json::to_string(&measurements).unwrap()
    );
}
