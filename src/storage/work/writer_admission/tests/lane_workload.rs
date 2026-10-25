//! Explicit measured workload, excluded from ordinary gates. Seven owned test
//! processes run real words against either one store or seven separate stores.
//! This never discovers a home or opens an operational database.
use super::contention::{OwnedChild, wait_file, within_target};
use crate::{
    diagnostics::{
        Correlation, Diagnostics, Fingerprint, Surface,
        tests::{drain, records},
        transaction::{Context, Scope},
    },
    domain::{ProjectId, SessionId},
    verbs::{AddInput, AgentVerbs, ClaimInput, NextInput, NoteInput, UpdateInput, VerbError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const CHILD: &str = "ENGRAM_TRANSACTION_LANE_FIXTURE";
const LANES: usize = 7;
const ROUNDS: usize = 2;

fn verbs(database: PathBuf, lane: usize) -> AgentVerbs {
    AgentVerbs::new(
        database,
        ProjectId("synthetic-contention".into()),
        format!("synthetic-agent-{lane}"),
        SessionId(format!("synthetic-session-{lane}")),
        None,
    )
}

fn child(home: &Path) {
    let home = within_target(home);
    let config: Value =
        serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    let database = PathBuf::from(config["database"].as_str().unwrap());
    assert!(
        database
            .parent()
            .unwrap()
            .starts_with(home.parent().unwrap())
    );
    let lane = usize::try_from(config["lane"].as_u64().unwrap()).unwrap();
    let work_ref = config["work_ref"].as_str().unwrap();
    let capture = config["capture"].as_bool().unwrap();
    let verbs = verbs(database.clone(), lane);
    // Preflight is outside measurement. No retry hides an opening failure.
    verbs
        .show(work_ref, Utc::now())
        .expect("synthetic lane preflight");
    let sink = capture.then(|| Diagnostics::start(&home, "synthetic-lane", Surface::Mcp));
    fs::write(home.join("ready"), b"ready").unwrap();
    wait_file(&home.parent().unwrap().join("go"));
    let began = Utc::now();
    let started = Instant::now();
    let mut calls = Vec::new();
    for round in 0..ROUNDS {
        for operation in ["claim", "note", "update", "next"] {
            let sequence = u64::try_from(calls.len()).unwrap() + 1;
            let _scope = sink.as_ref().map(|sink| {
                Scope::enter(
                    Context::new(
                        sink.clone(),
                        operation,
                        Correlation {
                            frame_seq: Some(sequence),
                            ..Correlation::default()
                        },
                        Fingerprint::store(&database),
                    )
                    .capture_all_for_test(),
                )
            });
            let call_start = Instant::now();
            let result = match operation {
                "claim" => verbs.claim(
                    ClaimInput {
                        work_ref: work_ref.into(),
                        ttl_seconds: Some(3600),
                        recover: None,
                    },
                    Utc::now(),
                ),
                "note" => verbs.note(
                    &NoteInput {
                        work_ref: Some(work_ref.into()),
                        text: format!("synthetic finding {round}"),
                        refs: vec![],
                        status: false,
                    },
                    Utc::now(),
                ),
                "update" => verbs.update(
                    serde_json::from_value::<UpdateInput>(json!({
                        "work_ref": work_ref,
                        "action": { "action": "revise", "priority": round + 1 },
                    }))
                    .unwrap(),
                    Utc::now(),
                ),
                "next" => verbs.next(
                    &NextInput {
                        peek: true,
                        ..NextInput::default()
                    },
                    Utc::now(),
                ),
                _ => unreachable!(),
            };
            let elapsed = u64::try_from(call_start.elapsed().as_millis()).unwrap();
            let refusal = match result {
                Ok(_) => None,
                Err(VerbError {
                    error:
                        crate::StoreError::WorkWriterAdmissionRefused {
                            reason,
                            elapsed_ms,
                            budget_ms,
                            sqlite_primary_code,
                            sqlite_extended_code,
                            ..
                        },
                    ..
                }) => Some(json!({
                    "reason": reason, "admission_ms": elapsed_ms, "budget_ms": budget_ms,
                    "sqlite_primary_code": sqlite_primary_code,
                    "sqlite_extended_code": sqlite_extended_code,
                })),
                Err(error) => {
                    panic!("unexpected {operation} failure in synthetic lane {lane}: {error:?}")
                }
            };
            calls.push(json!({"sequence":sequence, "operation":operation,
                "elapsed_ms":elapsed, "refusal":refusal}));
        }
    }
    if let Some(sink) = &sink {
        drain(sink);
    }
    fs::write(
        home.join("measurement.json"),
        serde_json::to_vec(&json!({
            "lane":lane, "pid":std::process::id(), "began_at":began,
            "ended_at":Utc::now(), "elapsed_ms":started.elapsed().as_millis(), "calls":calls,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn seed(case: &Path, shared: bool, capture: bool) -> Vec<PathBuf> {
    (0..LANES)
        .map(|lane| {
            let home = case.join(format!("lane-{lane}"));
            fs::create_dir(&home).unwrap();
            let database = if shared {
                case.join("shared.db")
            } else {
                home.join("store.db")
            };
            let verbs = verbs(database.clone(), lane);
            let work_ref = verbs
                .add(
                    AddInput {
                        title: format!("Synthetic lane {lane}"),
                        ..AddInput::default()
                    },
                    Utc::now(),
                )
                .unwrap()
                .value["work"]["short_ref"]
                .as_str()
                .unwrap()
                .to_owned();
            verbs
                .claim(
                    ClaimInput {
                        work_ref: work_ref.clone(),
                        ttl_seconds: Some(3600),
                        recover: None,
                    },
                    Utc::now(),
                )
                .unwrap();
            fs::write(
                home.join("config.json"),
                serde_json::to_vec(&json!({
                    "database":database, "lane":lane, "work_ref":work_ref, "capture":capture,
                }))
                .unwrap(),
            )
            .unwrap();
            home
        })
        .collect()
}

fn quantiles(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    assert_ne!(values, Vec::<u64>::new());
    json!({"count":values.len(), "p50_ms":values[values.len()/2],
        "p95_ms":values[(values.len()*95).div_ceil(100)-1], "max_ms":values.last()})
}

fn summarize(measurements: &[Value], rows: &[Value]) -> Value {
    let transactions: Vec<_> = rows
        .iter()
        .filter(|r| r["event"]["kind"] == "store_transaction")
        .collect();
    let settled: Vec<_> = transactions
        .iter()
        .filter(|r| r["event"]["stage"] == "settled")
        .collect();
    let failures: Vec<_> = transactions
        .iter()
        .filter(|r| r["event"]["stage"] == "acquisition_failed")
        .map(|r| {
            let tx = &r["event"]["store_transaction"];
            let timestamp =
                |value: &Value| DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap();
            let start = timestamp(&tx["attempt_at"]);
            let end = timestamp(&r["event"]["observed_at"]);
            // All timestamps come from this one host. This is an overlap search,
            // not proof of a continuous OS lock or a unique blocking owner.
            let candidates: Vec<_> = settled
                .iter()
                .filter(|other| {
                    let held = &other["event"]["store_transaction"];
                    other["producer"]["pid"] != r["producer"]["pid"]
                        && held["store_path_fingerprint"] == tx["store_path_fingerprint"]
                        && !held["acquired_at"].is_null()
                        && timestamp(&held["acquired_at"]) <= end
                        && timestamp(&other["event"]["observed_at"]) >= start
                })
                .map(|other| {
                    json!({"pid":other["producer"]["pid"],
            "instance_id":other["producer"]["instance_id"],
            "operation":other["event"]["operation"],
            "transaction":other["event"]["store_transaction"]})
                })
                .collect();
            json!({"waiter_pid":r["producer"]["pid"], "transaction":tx,
            "candidate_intervals":candidates,
            "holder_attribution":"unavailable; intervals alone do not identify the OS lock owner"})
        })
        .collect();
    let operations: Vec<_> = ["claim", "note", "update", "next"]
        .iter()
        .map(|operation| {
            let calls: Vec<_> = measurements
                .iter()
                .flat_map(|m| m["calls"].as_array().unwrap())
                .filter(|call| call["operation"] == *operation)
                .collect();
            json!({"operation":operation,
            "latency":quantiles(calls.iter().map(|c| c["elapsed_ms"].as_u64().unwrap()).collect()),
            "refusals":calls.iter().filter(|c| !c["refusal"].is_null()).count()})
        })
        .collect();
    let losses: Vec<_> = rows
        .iter()
        .filter(|r| r["event"]["kind"] == "summary")
        .map(|r| json!({"producer":r["producer"], "loss":r["loss"]}))
        .collect();
    let mut intervals = settled.clone();
    intervals.sort_by_key(|row| {
        std::cmp::Reverse(
            row["event"]["store_transaction"]["acquired_interval_ms"]
                .as_u64()
                .unwrap(),
        )
    });
    let longest: Vec<_> = intervals
        .iter()
        .take(8)
        .map(|row| {
            json!({
                "producer":row["producer"], "operation":row["event"]["operation"],
                "settled_at":row["event"]["observed_at"],
                "transaction":row["event"]["store_transaction"]
            })
        })
        .collect();
    let missing_acquisitions = settled
        .iter()
        .filter(|row| {
            !transactions.iter().any(|other| {
                other["event"]["stage"] == "acquired"
                    && other["producer"]["instance_id"] == row["producer"]["instance_id"]
                    && other["event"]["store_transaction"]["transaction_seq"]
                        == row["event"]["store_transaction"]["transaction_seq"]
            })
        })
        .count();
    json!({"operations":operations, "transaction_records":transactions.len(),
        "observed_acquired_intervals":settled.len(), "admission_failures":failures,
        "settlements_missing_acquisition_record":missing_acquisitions,
        "longest_observed_intervals":longest,
        "loss_summaries":losses, "coverage":"synthetic AgentVerbs request scopes; startup and uninstrumented writers excluded"})
}

#[test]
#[ignore = "explicit seven-process measurement; coordinate overlap with an existing full gate"]
fn seven_lane_real_words_preserve_admission_budgets() {
    if let Some(home) = std::env::var_os(CHILD) {
        child(Path::new(&home));
        return;
    }
    let home = crate::test_support::temp_home().unwrap();
    let root = within_target(home.path());
    let began = Utc::now();
    let mut cases = Vec::new();
    for shared in [false, true] {
        for capture in [false, true] {
            let case = root.join(format!("shared-{shared}-capture-{capture}"));
            fs::create_dir(&case).unwrap();
            let homes = seed(&case, shared, capture);
            let module = module_path!().split_once("::").unwrap().1;
            let mut children: Vec<_> = homes
                .iter()
                .map(|lane_home| {
                    let process = Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            &format!("{module}::seven_lane_real_words_preserve_admission_budgets"),
                            "--ignored",
                            "--nocapture",
                        ])
                        .env(CHILD, lane_home)
                        .current_dir(&root)
                        .stdout(Stdio::from(
                            fs::File::create(lane_home.join("child.stdout")).unwrap(),
                        ))
                        .stderr(Stdio::from(
                            fs::File::create(lane_home.join("child.stderr")).unwrap(),
                        ))
                        .spawn()
                        .unwrap();
                    let mut child = OwnedChild {
                        process,
                        home: lane_home.clone(),
                    };
                    // Startup is not the measured workload. Admit each opener
                    // before starting the next; all seven then wait for go.
                    child.ready();
                    child
                })
                .collect();
            let case_began = Utc::now();
            fs::write(case.join("go"), b"go").unwrap();
            for child in &mut children {
                child.wait();
            }
            let measurements: Vec<Value> = homes
                .iter()
                .map(|h| {
                    serde_json::from_slice(&fs::read(h.join("measurement.json")).unwrap()).unwrap()
                })
                .collect();
            let rows: Vec<_> = homes.iter().flat_map(|h| records(h)).collect();
            if capture {
                assert!(
                    rows.iter()
                        .any(|row| row["event"]["kind"] == "store_transaction")
                );
            } else {
                assert_eq!(rows, Vec::<Value>::new());
            }
            for row in &rows {
                let budget = &row["event"]["store_transaction"]["budget"];
                if let Some(configured) = budget["configured_ms"].as_u64() {
                    assert!(matches!(configured, 5000 | 10000), "{row}");
                    assert!(
                        budget["allowance_ms"].as_u64().unwrap()
                            <= budget["shared_window_ms"].as_u64().unwrap_or(configured)
                    );
                }
                if let Some(window) = budget["shared_window_ms"].as_u64() {
                    assert_eq!(window, 10000);
                }
            }
            assert_eq!(measurements.len(), LANES);
            assert!(
                measurements
                    .iter()
                    .all(|m| m["calls"].as_array().unwrap().len() == ROUNDS * 4)
            );
            cases.push(
                json!({"shared_store":shared, "capture":if capture {"full"} else {"no_observer"},
                "began_at":case_began, "ended_at":Utc::now(),
                "measurements":measurements, "summary":summarize(&measurements, &rows)}),
            );
        }
    }
    println!(
        "seven_lane_writer_measurement={}",
        json!({"began_at":began, "ended_at":Utc::now(),
        "gate_label_unverified":std::env::var("ENGRAM_CONTENTION_GATE_RUN").ok(),
        "lanes":LANES, "rounds":ROUNDS, "configured_ordinary_ms":5000,
        "configured_note_window_ms":10000, "cases":cases})
    );
}
