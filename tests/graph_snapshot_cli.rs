#[path = "../src/test_support.rs"]
mod test_support;

use std::{fs, path::PathBuf, process::Command};

use chrono::{Duration, TimeZone, Utc};
use engram::{
    ActorContext, DevelopmentNoopRedactor, ProjectId, SqliteStore,
    WorkGraphSnapshotDestinationKind, domain::AssuranceLevel,
};
use serde_json::Value;

fn engram(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command.arg("--home").arg(home);
    command
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn doctor_json(home: &std::path::Path) -> Value {
    let output = engram(home)
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor JSON");
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("parse doctor JSON")
}

fn save_mixed_precision_audits(
    store: &mut SqliteStore,
    project: &ProjectId,
) -> Vec<chrono::DateTime<Utc>> {
    let actor = ActorContext {
        actor_id: "audit-order-fixture".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(engram::SessionId("audit-order-session".into())),
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "exercise disclosure attempt ordering".into(),
    };
    let whole = Utc
        .with_ymd_and_hms(2026, 9, 4, 12, 0, 0)
        .single()
        .expect("timestamp");
    // One older attempt falls outside doctor's latest-32 page. The retained
    // whole second sorts AFTER the fractional timestamps as RFC 3339 text.
    let times: Vec<_> = std::iter::once(whole - Duration::seconds(1))
        .chain((0..32).map(|millis| whole + Duration::milliseconds(millis)))
        .collect();
    assert!(times[1] < times[2]);
    assert!(
        serde_json::to_string(&times[1]).expect("whole-second JSON")
            > serde_json::to_string(&times[2]).expect("fractional-second JSON")
    );
    for time in &times {
        store
            .save_work_graph_snapshot(
                project,
                &actor,
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                *time,
                &DevelopmentNoopRedactor,
            )
            .expect("save audited snapshot");
    }
    times
}

#[test]
fn doctor_save_audit_page_orders_mixed_timestamp_precision_by_attempt() {
    let directory = test_support::temp_home().expect("temporary Engram home");
    let home = directory.path();
    let initialized = engram(home).arg("init").output().expect("run init");
    assert!(
        initialized.status.success(),
        "{}",
        output_text(&initialized.stderr)
    );
    let initial = doctor_json(home);
    let project = ProjectId(initial["project_id"].as_str().expect("project id").into());
    let mut store =
        SqliteStore::open_unresolved(initial["database"].as_str().expect("database path"))
            .expect("open isolated initialized store");
    let times = save_mixed_precision_audits(&mut store, &project);
    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("all audits");
    assert_eq!(
        audits
            .iter()
            .map(|audit| audit.attempted_at)
            .collect::<Vec<_>>(),
        times
    );
    assert!(
        audits
            .windows(2)
            .all(|pair| pair[0].attempt_id < pair[1].attempt_id)
    );
    for limit in [1, 2, 32, 33] {
        let (total, page) = store
            .recent_work_graph_snapshot_save_audits(&project, limit)
            .expect("recent audit page");
        assert_eq!(total, 33);
        assert_eq!(page, audits[33 - limit..]);
    }
    drop(store);

    let diagnosis = doctor_json(home);
    assert_eq!(diagnosis["healthy"], true);
    assert_eq!(diagnosis["checked"]["graph_snapshot_audits"], 33);
    let disclosure = &diagnosis["graph_snapshot_disclosure_attempts"];
    assert_eq!(disclosure["total"], 33);
    let items = disclosure["items"].as_array().expect("audit page");
    assert_eq!(items.len(), 32);
    for (item, expected) in items.iter().zip(&audits[1..]) {
        assert_eq!(item["attempt_id"], expected.attempt_id);
        assert_eq!(
            item["attempted_at"],
            serde_json::to_value(expected.attempted_at).expect("audit timestamp JSON")
        );
    }
    let text = engram(home)
        .arg("doctor")
        .output()
        .expect("run doctor text");
    assert!(text.status.success(), "{}", output_text(&text.stderr));
    let rendered = output_text(&text.stdout);
    assert!(rendered.contains("33 total; showing the latest 32"));
    let lines: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with("Graph snapshot disclosure attempted at "))
        .collect();
    assert_eq!(lines.len(), 32);
    for (line, expected) in lines.iter().zip(&times[1..]) {
        assert!(
            line.starts_with(&format!(
                "Graph snapshot disclosure attempted at {expected}:"
            )),
            "{line}"
        );
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one CLI scenario keeps save, retry, audit, and refusal state causally connected"
)]
fn graph_save_cli_uses_digest_paths_and_never_replaces() {
    let directory = crate::test_support::temp_home().expect("temporary Engram home");
    let home = directory.path();

    let initialized = engram(home).arg("init").output().expect("run init");
    assert!(
        initialized.status.success(),
        "{}",
        output_text(&initialized.stderr)
    );
    let added = engram(home)
        .args([
            "work",
            "--actor-id",
            "graph-fixture",
            "--session-id",
            "graph-fixture-session",
            "add",
            "Snapshot planning state",
        ])
        .output()
        .expect("run work add");
    assert!(added.status.success(), "{}", output_text(&added.stderr));

    let save = || {
        engram(home)
            .args([
                "graph",
                "--actor-id",
                "operator",
                "--session-id",
                "operator-session",
                "save",
            ])
            .output()
            .expect("run graph save")
    };
    let first = save();
    assert!(first.status.success(), "{}", output_text(&first.stderr));
    let snapshot_path = PathBuf::from(output_text(&first.stdout));
    let original = fs::read(&snapshot_path).expect("read snapshot");
    let snapshot: Value = serde_json::from_slice(&original).expect("parse snapshot");
    let body_hash = snapshot["manifest"]["body_sha256"]
        .as_str()
        .expect("body hash");
    let cut = &snapshot["body"]["as_of"];
    let expected_name = format!(
        "graph-{}-{}-{}.json",
        cut["work_feed"].as_i64().expect("work cut"),
        cut["project_memory"].as_i64().expect("memory cut"),
        body_hash.get(..12).expect("body hash prefix")
    );
    assert_eq!(
        snapshot_path.file_name().and_then(|name| name.to_str()),
        Some(expected_name.as_str())
    );
    assert!(
        !snapshot["body"]
            .as_object()
            .expect("body")
            .contains_key("exporting_build")
    );
    assert!(snapshot["manifest"]["exporting_build"].is_string());

    let repeated = save();
    assert!(
        repeated.status.success(),
        "{}",
        output_text(&repeated.stderr)
    );
    assert_eq!(
        output_text(&repeated.stdout),
        format!("already saved: {}", snapshot_path.display())
    );
    assert_eq!(
        fs::read(&snapshot_path).expect("snapshot preserved"),
        original
    );

    let explicit_path = home.join("exports").join("graph.json");
    let save_explicit = || {
        engram(home)
            .args([
                "graph",
                "--actor-id",
                "operator",
                "--session-id",
                "operator-session",
                "save",
                "--out",
            ])
            .arg(&explicit_path)
            .output()
            .expect("run explicit graph save")
    };
    let explicit = save_explicit();
    assert!(
        explicit.status.success(),
        "{}",
        output_text(&explicit.stderr)
    );
    let explicit_original = fs::read(&explicit_path).expect("read explicit snapshot");
    let explicit_repeated = save_explicit();
    assert!(
        explicit_repeated.status.success(),
        "{}",
        output_text(&explicit_repeated.stderr)
    );
    assert_eq!(
        output_text(&explicit_repeated.stdout),
        format!("already saved: {}", explicit_path.display())
    );

    let doctor = engram(home)
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor");
    assert!(doctor.status.success(), "{}", output_text(&doctor.stderr));
    let diagnosis: Value = serde_json::from_slice(&doctor.stdout).expect("parse doctor");
    assert_eq!(diagnosis["checked"]["graph_snapshot_audits"], 4);
    assert_eq!(
        diagnosis["graph_snapshot_disclosure_attempts"]["items"]
            .as_array()
            .expect("snapshot audits")
            .len(),
        4
    );
    assert_eq!(diagnosis["graph_snapshot_disclosure_attempts"]["total"], 4);

    let destination_directory =
        crate::test_support::temp_home().expect("temporary destination home");
    let destination_home = destination_directory.path();
    let initialized = engram(destination_home)
        .arg("init")
        .output()
        .expect("initialize destination");
    assert!(
        initialized.status.success(),
        "{}",
        output_text(&initialized.stderr)
    );
    let load = |dry_run: bool| {
        let mut command = engram(destination_home);
        command.args([
            "graph",
            "--actor-id",
            "restore-operator",
            "--session-id",
            "restore-session",
            "load",
        ]);
        command.arg(&snapshot_path);
        if dry_run {
            command.arg("--dry-run");
        }
        command.output().expect("run graph load")
    };
    let preview = load(true);
    assert!(preview.status.success(), "{}", output_text(&preview.stderr));
    let preview: Value = serde_json::from_slice(&preview.stdout).expect("parse load preview");
    assert_eq!(preview["loaded"], false);
    assert_eq!(preview["preview"]["summary"]["section_counts"]["items"], 1);
    assert_eq!(preview["preview"]["refs"].as_array().map(Vec::len), Some(1));

    let loaded = load(false);
    assert!(loaded.status.success(), "{}", output_text(&loaded.stderr));
    assert!(output_text(&loaded.stdout).starts_with("loaded 1 work items, 1 records"));
    let work_ref = snapshot["body"]["items"][0]["ref"]
        .as_str()
        .expect("snapshot work ref");
    let shown = engram(destination_home)
        .args([
            "work",
            "--actor-id",
            "restore-reader",
            "--session-id",
            "restore-reader-session",
            "show",
            work_ref,
            "--json",
        ])
        .output()
        .expect("show restored work");
    assert!(shown.status.success(), "{}", output_text(&shown.stderr));
    let shown: Value = serde_json::from_slice(&shown.stdout).expect("parse restored show");
    assert_eq!(shown["status"]["work"]["restored"], true);
    assert!(
        shown["restored_history"]["total"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    let repeated_load = load(false);
    assert!(!repeated_load.status.success());
    assert!(output_text(&repeated_load.stderr).contains("graph_destination_not_empty"));
    let destination_doctor = engram(destination_home)
        .args(["doctor", "--json"])
        .output()
        .expect("run destination doctor");
    assert!(
        destination_doctor.status.success(),
        "{}",
        output_text(&destination_doctor.stderr)
    );
    let destination_diagnosis: Value =
        serde_json::from_slice(&destination_doctor.stdout).expect("parse destination doctor");
    assert_eq!(destination_diagnosis["graph_snapshot_loads"]["total"], 1);
    assert_eq!(
        destination_diagnosis["graph_snapshot_loads"]["items"][0]["body_sha256"],
        snapshot["manifest"]["body_sha256"]
    );

    let sidecar = format!(
        "{}-wal",
        diagnosis["database"].as_str().expect("database path")
    );
    let sidecar_save = engram(home)
        .args([
            "graph",
            "--actor-id",
            "operator",
            "--session-id",
            "operator-session",
            "save",
            "--out",
            &sidecar,
        ])
        .output()
        .expect("run sidecar save");
    assert!(!sidecar_save.status.success());
    assert!(output_text(&sidecar_save.stderr).contains("outside Engram's project stores"));

    let changed = engram(home)
        .args([
            "work",
            "--actor-id",
            "graph-fixture",
            "--session-id",
            "graph-fixture-session",
            "add",
            "Change the graph cut",
        ])
        .output()
        .expect("change graph");
    assert!(changed.status.success(), "{}", output_text(&changed.stderr));
    let replacement = save_explicit();
    assert!(!replacement.status.success());
    assert!(output_text(&replacement.stderr).contains("already exists with different bytes"));
    assert_eq!(
        fs::read(&explicit_path).expect("explicit snapshot preserved"),
        explicit_original
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&snapshot_path)
                .expect("snapshot metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
