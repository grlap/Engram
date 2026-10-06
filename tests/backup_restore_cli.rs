//! `engram backup restore` onto a clean home: it installs a checked copy
//! without replacing anything, reports the live authority the copy holds and
//! changes none of it, refuses each case it must refuse with a typed code, and
//! leaves a copy it cannot accept in place with both ways on.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{ProjectId, project_database_path, project_digest};
use flate2::{Compression, write::GzEncoder};
use serde_json::Value;
use sha2::{Digest, Sha256};

const PROJECT: &str = "restore-cli-fixture";
const OTHER: &str = "restore-cli-other-project";

#[cfg(windows)]
#[path = "backup_restore_cli/windows_paths.rs"]
mod windows_paths;

struct Homes {
    root: test_support::TempHome,
}

impl Homes {
    fn new() -> Self {
        let root = test_support::temp_home().unwrap();
        fs::write(root.path().join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        fs::write(root.path().join(".other-project"), format!("{OTHER}\n")).unwrap();
        fs::create_dir_all(root.path().join("copies")).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn engram_for(&self, project_file: &str, home: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(self.root.path())
            .env_remove("ENGRAM_HOME")
            .env_remove("ENGRAM_SESSION_ID")
            .env_remove("ENGRAM_ACTOR_ID")
            .arg("--home")
            .arg(self.path(home))
            .arg("--project-file")
            .arg(self.path(project_file))
            .args(args)
            .output()
            .expect("run engram")
    }

    fn engram(&self, home: &str, args: &[&str]) -> Output {
        self.engram_for(".engram-project", home, args)
    }

    fn succeeded(&self, home: &str, args: &[&str]) -> Output {
        let output = self.engram(home, args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        output
    }

    /// Runs a refused restore and returns its JSON refusal.
    fn refused(&self, home: &str, args: &[&str], code: &str) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        let output = self.engram(home, &args);
        assert!(!output.status.success(), "{args:?} must be refused");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(code), "{args:?}: {stderr}");
        let refusal: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?}: {error}: {}", text(&output.stdout)));
        assert_eq!(refusal["code"], code, "{refusal}");
        refusal
    }

    fn set_target_for(&self, project_file: &str, home: &str) {
        let copies = self.path("copies");
        let output = self.engram_for(
            project_file,
            home,
            &[
                "backup",
                "target",
                "set",
                "--kind",
                "store",
                "--adapter",
                "directory",
                "--dir",
                copies.to_str().unwrap(),
                "--disclosure-authorized-by",
                "greg",
                "--off-host-asserted-by",
                "greg",
            ],
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
    }

    fn set_target(&self, home: &str) {
        self.set_target_for(".engram-project", home);
    }

    fn database(&self, home: &str) -> PathBuf {
        project_database_path(&self.path(home), &ProjectId(PROJECT.into()))
    }

    fn project_dir(&self, project: &str) -> PathBuf {
        self.path("copies")
            .join(project_digest(&ProjectId(project.into())))
    }

    /// An origin store with one pushed copy; returns the copy's manifest.
    fn origin_copy(&self) -> Value {
        self.succeeded("origin", &["init"]);
        self.set_target("origin");
        self.succeeded("origin", &["backup", "push"]);
        self.newest_manifest("origin")
    }

    fn newest_manifest(&self, home: &str) -> Value {
        let listing: Value =
            serde_json::from_slice(&self.succeeded(home, &["backup", "list", "--json"]).stdout)
                .unwrap();
        listing["copies"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone()
    }

    fn manifest_path(&self, project: &str, copy: &str) -> PathBuf {
        self.project_dir(project)
            .join(format!("{copy}.manifest.json"))
    }

    fn edit_manifest(&self, copy: &str, edit: impl FnOnce(&mut Value)) {
        let path = self.manifest_path(PROJECT, copy);
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut manifest);
        fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    /// Adds an item and claims it as `session` for `ttl` seconds; returns its
    /// reference and the claim's expiry in milliseconds.
    fn held_item(&self, title: &str, session: &str, ttl: u64) -> String {
        let added: Value = serde_json::from_slice(
            &self
                .succeeded(
                    "origin",
                    &[
                        "work",
                        "--actor-id",
                        "origin-agent",
                        "--session-id",
                        session,
                        "add",
                        title,
                        "--json",
                    ],
                )
                .stdout,
        )
        .unwrap();
        let reference = added["work"]["short_ref"].as_str().unwrap().to_owned();
        self.succeeded(
            "origin",
            &[
                "work",
                "--actor-id",
                "origin-agent",
                "--session-id",
                session,
                "claim",
                &reference,
                "--ttl",
                &ttl.to_string(),
                "--json",
            ],
        );
        reference
    }

    fn claim_in(&self, home: &str, reference: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "work",
            "--actor-id",
            "new-agent",
            "--session-id",
            "new-session",
            "claim",
            reference,
            "--json",
        ];
        args.extend_from_slice(extra);
        self.engram(home, &args)
    }

    /// Every line of a store's export, parsed, the header without its export
    /// time, which is the only part that differs between two exports of the
    /// same rows.
    fn exported_rows(&self, database: &Path, name: &str) -> Vec<Value> {
        let out = self.path(name);
        let output = Command::new(env!("CARGO_BIN_EXE_engram"))
            .args(["migration", "export", "--database"])
            .arg(database)
            .arg("--out")
            .arg(&out)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        let text = fs::read_to_string(&out).unwrap();
        let mut lines = text.lines();
        let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let export = header["engram_export"].as_object_mut().unwrap();
        assert!(export.remove("exported_at").is_some(), "{header}");
        std::iter::once(header)
            .chain(lines.map(|line| serde_json::from_str(line).unwrap()))
            .collect()
    }

    /// Asserts that no write-ahead log beside the home's store holds frames.
    fn assert_quiescent(&self, home: &str) {
        let wal = PathBuf::from(format!("{}-wal", self.database(home).display()));
        let frames = match fs::metadata(&wal) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("{} cannot be examined: {error}", wal.display()),
        };
        assert_eq!(frames, 0, "{} holds frames", wal.display());
    }

    /// The files beside where the store goes, by name.
    fn store_directory_names(&self, home: &str) -> Vec<String> {
        let directory = self.database(home).parent().unwrap().to_path_buf();
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut names: Vec<_> = entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn a_clean_home_restores_a_checked_copy_reports_its_live_authority_and_changes_no_row() {
    let homes = Homes::new();
    homes.succeeded("origin", &["init"]);
    homes.held_item("Held for long", "origin-session-long", 7200);
    let shorter = homes.held_item("Held for less long", "origin-session-shorter", 3600);
    homes.set_target("origin");
    // The rows of the origin before the push and before it changes; the
    // store-equality test also asserts the origin is quiescent here.
    let pushed_rows = homes.exported_rows(&homes.database("origin"), "pushed.jsonl");
    homes.succeeded("origin", &["backup", "push"]);
    let manifest = homes.newest_manifest("origin");
    let copy = manifest["copy"].as_str().unwrap();
    homes.held_item("Added after the push", "origin-session-later", 7200);
    let changed_rows = homes.exported_rows(&homes.database("origin"), "changed.jsonl");

    homes.set_target("clean");
    let output = homes.succeeded(
        "clean",
        &[
            "backup",
            "restore",
            copy,
            "--origin-retired-by",
            "greg",
            "--json",
        ],
    );
    let restored: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(restored["schema_version"], 1);
    assert_eq!(restored["copy"], copy);
    assert_eq!(restored["sha256"], manifest["capture"]["sha256"]);
    assert_eq!(restored["bytes"], manifest["capture"]["bytes"]);
    assert_eq!(restored["origin_retired_by"], "greg");
    assert_eq!(restored["completed_interrupted"], false);
    let authority = &restored["authority"];
    assert_eq!(authority["unexpired_claims"], 2, "{authority}");
    assert_eq!(authority["unexpired_grants"], 0, "{authority}");
    assert_eq!(authority["grants_expire_by"], Value::Null, "{authority}");
    assert_eq!(authority["begun_turns"], 0, "{authority}");
    let as_of = chrono::DateTime::parse_from_rfc3339(authority["as_of"].as_str().unwrap()).unwrap();
    let last =
        chrono::DateTime::parse_from_rfc3339(authority["claims_expire_by"].as_str().unwrap())
            .unwrap();
    // The last claim to expire is the long one, about two hours out.
    assert!(last - as_of > chrono::Duration::minutes(110), "{authority}");

    // The installed store is the copy's bytes, alone in its directory, and
    // its rows are the copy's.
    let database = homes.database("clean");
    assert_eq!(homes.store_directory_names("clean"), ["engram.db"]);
    assert_eq!(
        sha256(&fs::read(&database).unwrap()),
        manifest["capture"]["sha256"].as_str().unwrap()
    );
    let restored_rows = homes.exported_rows(&database, "restored.jsonl");
    assert_eq!(restored_rows, pushed_rows);
    assert_ne!(restored_rows, changed_rows);

    // The status line names the restore.
    let status = text(&homes.succeeded("clean", &["backup", "status"]).stdout);
    assert!(
        status.contains(&format!(
            "restore: restored {copy} (sha256 {}",
            restored["sha256"].as_str().unwrap()
        )) && status.contains("origin retired by greg"),
        "{status}"
    );
    let status: Value = serde_json::from_slice(
        &homes
            .succeeded("clean", &["backup", "status", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(status["restore"]["state"], "restored");
    assert_eq!(status["restore"]["record"]["copy"], copy);

    // A new session cannot claim an item still held in the copy; that it can
    // once the claim expires is shown with a supplied clock in the restore's
    // unit tests.
    let held = homes.claim_in("clean", &shorter, &[]);
    assert!(!held.status.success());
    // A refused work word prints its JSON error on standard error.
    let refusal: Value = serde_json::from_slice(&held.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", text(&held.stderr)));
    assert_eq!(refusal["error"]["code"], "work_claim_held", "{refusal}");
    let expires_at_ms = refusal["error"]["details"]["expires_at_ms"]
        .as_i64()
        .unwrap();
    assert!(expires_at_ms > chrono::Utc::now().timestamp_millis());
}

#[test]
fn the_plain_report_names_the_copy_its_origin_and_the_live_authority() {
    let homes = Homes::new();
    let manifest = homes.origin_copy();
    let copy = manifest["copy"].as_str().unwrap();
    homes.set_target("clean");
    let output = text(
        &homes
            .succeeded(
                "clean",
                &["backup", "restore", copy, "--origin-retired-by", "greg"],
            )
            .stdout,
    );
    for expected in [
        format!("restored {copy} into "),
        format!("sha256 {}", manifest["capture"]["sha256"].as_str().unwrap()),
        "origin retired by greg (asserted; recorded under this home)".to_owned(),
        "0 unexpired active claim(s), the last expiring none".to_owned(),
        "0 unexpired issued grant(s), the last expiring none".to_owned(),
        "0 begun turn(s)".to_owned(),
        "no row was changed".to_owned(),
        "run `engram doctor` and `engram readiness`".to_owned(),
    ] {
        assert!(output.contains(&expected), "{expected:?} in {output}");
    }
}

#[test]
fn restore_refuses_without_a_statement_over_a_store_while_a_push_runs_and_for_another_project() {
    let homes = Homes::new();
    let manifest = homes.origin_copy();
    let copy = manifest["copy"].as_str().unwrap().to_owned();
    homes.set_target("clean");

    // Without a statement, or with a blank one.
    homes.refused(
        "clean",
        &["backup", "restore", &copy],
        "backup_restore_origin_unstated",
    );
    homes.refused(
        "clean",
        &["backup", "restore", &copy, "--origin-retired-by", "  "],
        "backup_restore_origin_unstated",
    );
    assert_eq!(homes.store_directory_names("clean"), Vec::<String>::new());

    // While a push holds the lock.
    let lock_path = homes
        .path("clean")
        .join("backup-records")
        .join(project_digest(&ProjectId(PROJECT.into())))
        .join("store.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    homes.refused(
        "clean",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_push_running",
    );
    lock.unlock().unwrap();
    drop(lock);
    assert_eq!(homes.store_directory_names("clean"), Vec::<String>::new());

    // Over a store, and over what a read leaves without a store: an empty
    // write-ahead log and its index.
    homes.set_target("occupied");
    homes.succeeded("occupied", &["init"]);
    let before = fs::read(homes.database("occupied")).unwrap();
    homes.refused(
        "occupied",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_restore_store_exists",
    );
    assert_eq!(fs::read(homes.database("occupied")).unwrap(), before);
    homes.set_target("sidecar");
    let database = homes.database("sidecar");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    for suffix in ["-wal", "-shm"] {
        fs::write(format!("{}{suffix}", database.display()), b"").unwrap();
    }
    homes.refused(
        "sidecar",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_restore_store_exists",
    );
    assert_eq!(
        homes.store_directory_names("sidecar"),
        ["engram.db-shm", "engram.db-wal"]
    );

    // A manifest of another project's copy, found in this project's place.
    homes.succeeded_other_push();
    let other = homes.other_copy();
    for suffix in [".db.gz", ".manifest.json"] {
        fs::copy(
            homes.project_dir(OTHER).join(format!("{other}{suffix}")),
            homes.project_dir(PROJECT).join(format!("{other}{suffix}")),
        )
        .unwrap();
    }
    let listing: Value = serde_json::from_slice(
        &homes
            .succeeded("clean", &["backup", "list", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(listing["foreign"], serde_json::json!([other]));
    let refusal = homes.refused(
        "clean",
        &["backup", "restore", &other, "--origin-retired-by", "greg"],
        "backup_project_mismatch",
    );
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("names another project"),
        "{refusal}"
    );
    assert_eq!(homes.store_directory_names("clean"), Vec::<String>::new());

    // A manifest that names this project over a store of another project.
    let ours = project_digest(&ProjectId(PROJECT.into()));
    homes.edit_manifest(&other, |manifest| {
        manifest["capture"]["project_digest"] = ours.clone().into();
    });
    let refusal = homes.refused(
        "clean",
        &["backup", "restore", &other, "--origin-retired-by", "greg"],
        "backup_project_mismatch",
    );
    assert!(
        refusal["message"].as_str().unwrap().contains(OTHER),
        "{refusal}"
    );
    assert_eq!(homes.store_directory_names("clean"), Vec::<String>::new());
}

impl Homes {
    /// Another project's store, with a row that names it, and one pushed
    /// copy of it.
    fn succeeded_other_push(&self) {
        let add = [
            "work",
            "--actor-id",
            "other-agent",
            "--session-id",
            "other-session",
            "add",
            "Another project's item",
        ];
        for args in [&["init"][..], &add[..], &["backup", "push"][..]] {
            if args == ["backup", "push"] {
                self.set_target_for(".other-project", "other");
            }
            let output = self.engram_for(".other-project", "other", args);
            assert!(output.status.success(), "{}", text(&output.stderr));
        }
    }

    fn other_copy(&self) -> String {
        let output = self.engram_for(".other-project", "other", &["backup", "list", "--json"]);
        let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
        listing["copies"][0]["copy"].as_str().unwrap().to_owned()
    }
}

#[test]
fn a_copy_of_another_format_is_refused_and_left_in_place_with_both_ways_on() {
    let homes = Homes::new();
    let manifest = homes.origin_copy();
    let copy = manifest["copy"].as_str().unwrap().to_owned();
    homes.set_target("clean");
    let other_format = engram::ObjectId::from_canonical_bytes(b"another store format");
    homes.edit_manifest(&copy, |manifest| {
        manifest["capture"]["format_identity"] = other_format.as_str().into();
        manifest["capture"]["source_revision"] = "0123456789abcdef".into();
    });
    let refusal = homes.refused(
        "clean",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_restore_format_unaccepted",
    );
    let message = refusal["message"].as_str().unwrap();
    let staging = left_staging(&homes, message);
    assert!(staging.is_file(), "{message}");
    assert_eq!(
        sha256(&fs::read(&staging).unwrap()),
        manifest["capture"]["sha256"].as_str().unwrap()
    );
    for expected in [
        other_format.as_str(),
        "install the build at source revision 0123456789abcdef",
        "`engram migration export` on that file and `engram migration import`",
    ] {
        assert!(message.contains(expected), "{expected:?} in {message}");
    }
    assert!(!homes.database("clean").exists());

    // An `unavailable` revision names no build to install.
    homes.edit_manifest(&copy, |manifest| {
        manifest["capture"]["source_revision"] = "unavailable".into();
    });
    let refusal = homes.refused(
        "clean",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_restore_format_unaccepted",
    );
    let message = refusal["message"].as_str().unwrap();
    assert!(!message.contains("install the build"), "{message}");
    assert!(message.contains("`engram migration export`"), "{message}");
}

#[test]
fn a_copy_that_fails_the_full_check_is_refused_and_left_in_place() {
    let homes = Homes::new();
    let manifest = homes.origin_copy();
    let copy = manifest["copy"].as_str().unwrap().to_owned();
    homes.set_target("clean");
    // Replace the stored copy with bytes that decode and match a rewritten
    // manifest but are no store.
    let bytes = b"these bytes are not a SQLite store".repeat(64);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&bytes).unwrap();
    let stored = encoder.finish().unwrap();
    fs::write(
        homes.project_dir(PROJECT).join(format!("{copy}.db.gz")),
        &stored,
    )
    .unwrap();
    homes.edit_manifest(&copy, |manifest| {
        manifest["stored_bytes"] = stored.len().into();
        manifest["capture"]["bytes"] = bytes.len().into();
        manifest["capture"]["sha256"] = sha256(&bytes).into();
    });
    let refusal = homes.refused(
        "clean",
        &["backup", "restore", &copy, "--origin-retired-by", "greg"],
        "backup_restore_check_failed",
    );
    let message = refusal["message"].as_str().unwrap();
    assert!(
        message.contains("the full check of the copy failed"),
        "{message}"
    );
    assert!(
        message.contains("restore a healthy, accepted copy"),
        "{message}"
    );
    assert!(
        !message.contains("migration") && !message.contains("install the build"),
        "{message}"
    );
    let staging = left_staging(&homes, message);
    assert_eq!(fs::read(&staging).unwrap(), bytes);
    assert!(!homes.database("clean").exists());
}

/// The staging file a refusal names as left in place, which must lie beside
/// the store.
fn left_staging(homes: &Homes, message: &str) -> PathBuf {
    let marker = "The fetched copy is left at ";
    let start = message.find(marker).unwrap_or_else(|| panic!("{message}")) + marker.len();
    let end = message[start..].find(". Ways on").unwrap() + start;
    let staging = PathBuf::from(&message[start..end]);
    assert_eq!(staging.parent(), homes.database("clean").parent());
    staging
}

#[test]
fn doctor_and_readiness_take_a_leftover_staging_file_for_no_store_and_create_none() {
    let homes = Homes::new();
    let directory = homes.database("leftover").parent().unwrap().to_path_buf();
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(".backup-restore-0000.staging"), b"partial").unwrap();
    let words: [&[&str]; 3] = [
        &["doctor", "--json"],
        &["doctor", "--check-landings", "--json"],
        &["readiness", "--json"],
    ];
    for word in words {
        let output = homes.engram("leftover", word);
        assert!(!output.status.success(), "{word:?} must refuse");
        let value: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{word:?}: {error}: {}", text(&output.stdout)));
        assert_eq!(value["code"], "store_not_initialized", "{word:?}: {value}");
    }
    assert_eq!(
        homes.store_directory_names("leftover"),
        [".backup-restore-0000.staging"]
    );
}

#[test]
fn status_and_doctor_show_a_pending_restore_without_contacting_the_target() {
    use engram::backup::{
        CopyKind,
        restore::{RestoreRecord, RestoreState, write_restore_record},
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    };
    let homes = Homes::new();
    homes.set_target("pending");
    // The target's directory is gone: a word that contacted it would fail.
    fs::remove_dir_all(homes.path("copies")).unwrap();
    let project = ProjectId(PROJECT.into());
    let home = homes.path("pending");
    let lock = PushLock::try_acquire(&RecordPaths::new(&home, &project, CopyKind::Store)).unwrap();
    let now = chrono::Utc::now();
    write_restore_record(
        &home,
        &project,
        &lock,
        &RestoreRecord {
            format_version: RECORD_FORMAT_VERSION,
            project: PROJECT.into(),
            copy: "20261002T000000Z-pending".into(),
            sha256: "ef".repeat(32),
            origin_host: Some("old-host".into()),
            origin_retired: Statement {
                by: "greg".into(),
                at: now,
            },
            staging: "staging".into(),
            state: RestoreState::Pending,
            pending_at: now,
            completed_at: None,
        },
    )
    .unwrap();
    drop(lock);

    let status = text(&homes.succeeded("pending", &["backup", "status"]).stdout);
    assert!(
        status.contains("restore: pending since ") && status.contains("20261002T000000Z-pending"),
        "{status}"
    );
    let doctor = homes.engram("pending", &["doctor", "--json"]);
    assert!(!doctor.status.success(), "there is no store yet");
    let value: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(value["code"], "store_not_initialized");
    assert_eq!(value["backup"]["restore"]["state"], "pending");
    assert_eq!(
        value["backup"]["restore"]["record"]["copy"],
        "20261002T000000Z-pending"
    );
    let doctor = text(&homes.engram("pending", &["doctor"]).stdout);
    assert!(doctor.contains("restore: pending since "), "{doctor}");
    assert!(!homes.database("pending").exists());
}

#[cfg(unix)]
#[test]
fn doctor_takes_a_store_link_that_leads_nowhere_for_no_store_and_creates_none() {
    let homes = Homes::new();
    let database = homes.database("dangling");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let nowhere = homes.path("nowhere.db");
    std::os::unix::fs::symlink(&nowhere, &database).unwrap();
    for word in [
        &["doctor", "--json"][..],
        &["doctor", "--check-landings", "--json"][..],
    ] {
        let output = homes.engram("dangling", word);
        assert!(!output.status.success(), "{word:?} must refuse");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["code"], "store_not_initialized", "{word:?}: {value}");
    }
    assert!(!nowhere.exists());
}

#[test]
fn a_restored_store_equals_the_pushed_copy_byte_for_byte_and_row_for_row_and_doctor_finds_it_healthy()
 {
    let homes = Homes::new();
    homes.succeeded("origin", &["init"]);
    homes.held_item("Held at the push", "origin-session", 7200);
    homes.set_target("origin");
    // The source is quiescent: every process that opened it has ended and
    // closed it, and no write-ahead log holds frames, so the export and the
    // copy describe the same state.
    homes.assert_quiescent("origin");
    let pushed_rows = homes.exported_rows(&homes.database("origin"), "equality-pushed.jsonl");
    homes.assert_quiescent("origin");
    homes.succeeded("origin", &["backup", "push"]);
    let manifest = homes.newest_manifest("origin");
    let copy = manifest["copy"].as_str().unwrap();
    // The source changes after the push.
    homes.held_item("Added after the push", "origin-session-later", 7200);
    let changed_rows = homes.exported_rows(&homes.database("origin"), "equality-changed.jsonl");
    assert_ne!(pushed_rows, changed_rows, "the source must have changed");

    homes.set_target("clean");
    homes.succeeded(
        "clean",
        &["backup", "restore", copy, "--origin-retired-by", "greg"],
    );
    let database = homes.database("clean");
    // First, before doctor or any claim opens it: the restored file is the
    // copy's bytes.
    assert_eq!(
        sha256(&fs::read(&database).unwrap()),
        manifest["capture"]["sha256"].as_str().unwrap()
    );
    let doctor = homes.succeeded("clean", &["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["healthy"], true, "{report}");
    // Its rows, everything but the export header's time, are the rows
    // exported before the push, not the changed source's.
    let restored_rows = homes.exported_rows(&database, "equality-restored.jsonl");
    assert_eq!(restored_rows, pushed_rows);
    assert_ne!(restored_rows, changed_rows);
}

#[test]
fn a_pending_restore_is_abandoned_by_copy_without_contacting_the_target() {
    use engram::backup::{
        CopyKind,
        restore::{RestoreRecord, RestoreState, write_restore_record},
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    };
    let homes = Homes::new();
    homes.set_target("pending");
    // The target's directory is gone: a word that contacted it would fail.
    fs::remove_dir_all(homes.path("copies")).unwrap();
    let project = ProjectId(PROJECT.into());
    let home = homes.path("pending");
    let copy = "20261002T000000Z-pending";
    let write = |staging: &Path| {
        let lock =
            PushLock::try_acquire(&RecordPaths::new(&home, &project, CopyKind::Store)).unwrap();
        let now = chrono::Utc::now();
        write_restore_record(
            &home,
            &project,
            &lock,
            &RestoreRecord {
                format_version: RECORD_FORMAT_VERSION,
                project: PROJECT.into(),
                copy: copy.into(),
                sha256: "ef".repeat(32),
                origin_host: Some("old-host".into()),
                origin_retired: Statement {
                    by: "Greg O'Neil".into(),
                    at: now,
                },
                staging: staging.display().to_string(),
                state: RestoreState::Pending,
                pending_at: now,
                completed_at: None,
            },
        )
        .unwrap();
    };

    // The status line names both ways on, the stored name quoted.
    write(Path::new("staging"));
    let status = text(&homes.succeeded("pending", &["backup", "status"]).stdout);
    for way in [
        format!("engram backup restore {copy} --origin-retired-by='Greg O'\"'\"'Neil'"),
        format!("engram backup restore {copy} --abandon-pending --abandoned-by=NAME"),
    ] {
        assert!(status.contains(&way), "{status}");
    }

    // The flags go together, and never with a statement about the origin.
    for args in [
        vec!["backup", "restore", copy, "--abandon-pending"],
        vec!["backup", "restore", copy, "--abandoned-by", "greg"],
        vec![
            "backup",
            "restore",
            copy,
            "--abandon-pending",
            "--abandoned-by",
            "greg",
            "--origin-retired-by",
            "greg",
        ],
    ] {
        let output = homes.engram("pending", &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&output.stderr)
        );
    }

    // A staging path that is not the restore's own is refused, removing
    // nothing and leaving the restore pending.
    let refusal = homes.refused(
        "pending",
        &[
            "backup",
            "restore",
            copy,
            "--abandon-pending",
            "--abandoned-by",
            "greg",
        ],
        "backup_restore_staging_unowned",
    );
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("stays pending"),
        "{refusal}"
    );
    let status = text(&homes.succeeded("pending", &["backup", "status"]).stdout);
    assert!(status.contains("restore: pending since "), "{status}");

    // Its own staging file is removed and its record archived.
    let directory = homes.database("pending").parent().unwrap().to_path_buf();
    fs::create_dir_all(&directory).unwrap();
    let staging = directory.join(format!(".backup-restore-{}.staging", uuid::Uuid::now_v7()));
    fs::write(&staging, b"staged copy").unwrap();
    write(&staging);
    let output = homes.succeeded(
        "pending",
        &[
            "backup",
            "restore",
            copy,
            "--abandon-pending",
            "--abandoned-by",
            "greg",
            "--json",
        ],
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["abandoned"], copy);
    assert_eq!(receipt["abandoned_by"], "greg");
    assert_eq!(receipt["staging_removed"], true);
    assert_eq!(receipt["staging"], staging.display().to_string());
    assert!(!staging.exists());
    let archive = PathBuf::from(receipt["archive"].as_str().unwrap());
    let envelope: Value = serde_json::from_slice(&fs::read(&archive).unwrap()).unwrap();
    assert_eq!(envelope["abandoned"]["by"], "greg");
    assert_eq!(envelope["pending"]["copy"], copy);
    let status = text(&homes.succeeded("pending", &["backup", "status"]).stdout);
    assert!(!status.contains("restore:"), "{status}");
    homes.refused(
        "pending",
        &[
            "backup",
            "restore",
            copy,
            "--abandon-pending",
            "--abandoned-by",
            "greg",
        ],
        "backup_restore_not_pending",
    );
    assert!(!homes.database("pending").exists());
}
