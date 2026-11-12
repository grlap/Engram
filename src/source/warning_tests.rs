//! Drive real source queries while only elapsed time and warning delivery are controlled.

use super::*;
use crate::operation_time::test_clock::Probe;
use std::{cell::Cell, path::Path, process::Command, rc::Rc};

type QueryHook = Box<dyn Fn(&str)>;

struct Queries(Option<QueryHook>);

impl Queries {
    fn advance(clock: Rc<Probe>, seconds: u64, count: usize) -> (Self, Rc<Cell<usize>>) {
        let queries = Rc::new(Cell::new(0));
        let seen = Rc::clone(&queries);
        let previous = crate::task_sync::git::BEFORE_QUERY.with(|hook| {
            hook.replace(Some(Box::new(move |_| {
                let next = seen.get() + 1;
                seen.set(next);
                if next <= count {
                    clock.advance(Duration::from_secs(seconds));
                }
            })))
        });
        (Self(previous), queries)
    }
}

impl Drop for Queries {
    fn drop(&mut self) {
        crate::task_sync::git::BEFORE_QUERY.with(|hook| hook.replace(self.0.take()));
    }
}

fn fixture() -> crate::test_support::TempHome {
    let scratch = crate::test_support::temp_home().unwrap();
    let root = scratch.path();
    git(root, &["init", "-q", "--object-format=sha1"]);
    std::fs::write(root.join("tracked"), "source fixture\n").unwrap();
    git(root, &["add", "tracked"]);
    git(root, &["commit", "-qm", "Seed source warning fixture"]);
    scratch
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let result = command
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_AUTHOR_NAME", "Source fixture")
        .env("GIT_AUTHOR_EMAIL", "source@example.invalid")
        .env("GIT_COMMITTER_NAME", "Source fixture")
        .env("GIT_COMMITTER_EMAIL", "source@example.invalid")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn one_warning(clock: &Probe, operation: &str) {
    let warnings: Vec<_> = clock
        .events()
        .into_iter()
        .filter(|event| event.0 == operation)
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "one whole-operation warning: {warnings:?}"
    );
    assert_eq!(warnings[0].0, operation);
    assert_eq!(warnings[0].2, Duration::from_secs(10));
}

#[test]
fn individually_short_real_queries_warn_once_for_the_whole_source_operation() {
    let scratch = fixture();
    let clock = Rc::new(Probe::install());
    let (_queries, seen) = Queries::advance(Rc::clone(&clock), 8, 3);
    let (status, json) = inspect_with(scratch.path(), MAX_BUDGET, |observation| {
        assert!(seen.get() >= 3);
        assert_eq!(clock.elapsed(), Duration::from_secs(24));
        observation.json().unwrap()
    });
    assert_eq!(status, Status::Clean, "{json}");
    one_warning(&clock, "source observation");
    assert_eq!(clock.events().len(), 1, "no individually slow query");
    clock.advance(Duration::from_secs(60));
    one_warning(&clock, "source observation");
    assert!(!clock.active("source observation"));
}

#[test]
fn source_warning_remains_active_during_final_json_processing() {
    let scratch = fixture();
    let clock = Probe::install();
    let (status, json) = inspect_with(scratch.path(), MAX_BUDGET, |observation| {
        assert_eq!(clock.events(), [] as [(&str, u64, Duration); 0]);
        clock.advance(Duration::from_secs(11));
        let encoded = observation.json().unwrap();
        assert!(clock.active("source observation"));
        encoded
    });
    assert_eq!(status, Status::Clean, "{json}");
    one_warning(&clock, "source observation");
    assert!(!clock.active("source observation"));
}

#[test]
fn source_query_still_refuses_the_original_spent_deadline() {
    let scratch = fixture();
    let clock = Rc::new(Probe::install());
    let (_queries, seen) = Queries::advance(Rc::clone(&clock), 61, 1);
    let observation = inspect(scratch.path(), MAX_BUDGET);
    assert!(seen.get() >= 1);
    assert_eq!(observation.status, Status::Unavailable);
    assert_eq!(observation.refusal.unwrap().code, RefusalCode::Deadline);
    one_warning(&clock, "source observation");
}

#[test]
fn final_json_still_refuses_the_original_spent_deadline() {
    let scratch = fixture();
    let clock = Probe::install();
    let (status, json) = inspect_with(scratch.path(), MAX_BUDGET, |observation| {
        clock.advance(Duration::from_secs(61));
        observation.json().unwrap()
    });
    assert_eq!(status, Status::Unavailable);
    let result: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(result["refusal"]["code"], "deadline");
    one_warning(&clock, "source observation");
}

#[test]
fn registered_committed_input_read_has_one_aggregate_warning() {
    let scratch = fixture();
    let clock = Rc::new(Probe::install());
    let (_queries, seen) = Queries::advance(Rc::clone(&clock), 8, 3);
    assert!(crate::committed_input::read(scratch.path()).unwrap().clean);
    assert!(seen.get() >= 3);
    one_warning(&clock, "registered committed input read");
}

#[test]
fn refusal_and_callback_unwind_leave_no_pending_warning() {
    let scratch = fixture();
    let clock = Probe::install();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        inspect_with(scratch.path(), MAX_BUDGET, |_| panic!("caller failed"));
    }));
    assert!(outcome.is_err());
    assert_eq!(
        inspect(Path::new("relative"), MAX_BUDGET).status,
        Status::Unavailable
    );
    clock.advance(Duration::from_secs(60));
    assert_eq!(clock.events(), [] as [(&str, u64, Duration); 0]);
    assert!(!clock.active("source observation"));
}

#[tokio::test]
async fn task_sync_command_warning_covers_short_queries_and_result_processing() {
    let scratch = fixture();
    let clock = Rc::new(Probe::install());
    let (_queries, seen) = Queries::advance(Rc::clone(&clock), 8, 3);
    let json = crate::operation_time::observe("task-sync command", async {
        let mut results = Vec::new();
        for args in [
            ["rev-parse", "--verify", "HEAD"].as_slice(),
            ["rev-parse", "--verify", "HEAD^{tree}"].as_slice(),
            ["status", "--porcelain=v1"].as_slice(),
        ] {
            results.push(
                crate::task_sync::git::query(scratch.path(), args, false, None, MAX_BUDGET)
                    .unwrap(),
            );
        }
        assert_eq!(seen.get(), 3);
        assert_eq!(clock.elapsed(), Duration::from_secs(24));
        // The same guard remains responsible while the command prints its result.
        clock.advance(Duration::from_secs(3));
        assert!(clock.active("task-sync command"));
        serde_json::to_string(&results).unwrap()
    })
    .await;
    let results: Vec<Vec<u8>> = serde_json::from_str(&json).unwrap();
    assert_eq!(results[0].len(), 41);
    assert_eq!(results[1].len(), 41);
    assert_eq!(results[2], [] as [u8; 0]);
    one_warning(&clock, "task-sync command");
    assert_eq!(clock.events().len(), 1);
    assert!(!clock.active("task-sync command"));
}
