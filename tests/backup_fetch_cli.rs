//! `engram backup list` and `engram backup fetch` from a home that has only
//! configured the target: they read the copies another home pushed, write a
//! checked copy, and never create a store.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{ProjectId, project_database_path};
use serde_json::Value;
use sha2::{Digest, Sha256};

const PROJECT: &str = "fetch-cli-fixture";

struct Homes {
    root: test_support::TempHome,
}

impl Homes {
    fn new() -> Self {
        let root = test_support::temp_home().unwrap();
        fs::write(root.path().join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn engram(&self, home: &str, args: &[&str]) -> Output {
        self.engram_in(self.root.path(), home, args)
    }

    /// Runs engram with `directory` as the current directory.
    fn engram_in(&self, directory: &Path, home: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(directory)
            .env_remove("ENGRAM_HOME")
            .arg("--home")
            .arg(self.path(home))
            .arg("--project-file")
            .arg(self.path(".engram-project"))
            .args(args)
            .output()
            .expect("run engram")
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

    fn refused(&self, home: &str, args: &[&str], code: &str) -> String {
        let output = self.engram(home, args);
        assert!(!output.status.success(), "{args:?} must be refused");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(code), "{args:?}: {stderr}");
        stderr
    }

    fn set_target(&self, home: &str) {
        let copies = self.path("copies");
        fs::create_dir_all(&copies).unwrap();
        self.succeeded(
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
    }

    fn database(&self, home: &str) -> PathBuf {
        project_database_path(&self.path(home), &ProjectId(PROJECT.into()))
    }

    fn list(&self, home: &str) -> Value {
        let output = self.succeeded(home, &["backup", "list", "--json"]);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[test]
fn a_home_that_only_configured_the_target_lists_and_fetches_a_checked_copy() {
    let homes = Homes::new();
    // The origin home: a store, a target and one pushed copy.
    homes.succeeded("origin", &["init"]);
    homes.set_target("origin");
    homes.succeeded("origin", &["backup", "push"]);
    let origin_listing = homes.list("origin");
    let origin_copies = origin_listing["copies"].as_array().unwrap();
    assert_eq!(origin_copies.len(), 1, "{origin_listing}");

    // A clean home configures the same target and nothing else.
    homes.set_target("clean");
    let listing = homes.list("clean");
    assert_eq!(listing["kind"], "store");
    assert_eq!(listing["copies"], origin_listing["copies"]);
    assert_eq!(listing["unreadable"], serde_json::json!([]));
    let manifest = &listing["copies"][0];
    let copy = manifest["copy"].as_str().unwrap();
    let plain = text(&homes.succeeded("clean", &["backup", "list"]).stdout);
    assert!(
        plain.contains(copy) && plain.contains(manifest["capture"]["sha256"].as_str().unwrap()),
        "{plain}"
    );

    let out = homes.path("fetched.db");
    let fetched = homes.succeeded(
        "clean",
        &[
            "backup",
            "fetch",
            copy,
            "--out",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    let fetched: Value = serde_json::from_slice(&fetched.stdout).unwrap();
    assert_eq!(fetched["copy"], copy);
    assert_eq!(
        fs::metadata(&out).unwrap().len(),
        manifest["capture"]["bytes"].as_u64().unwrap()
    );
    assert_eq!(
        sha256(&out),
        manifest["capture"]["sha256"].as_str().unwrap()
    );
    assert_eq!(fetched["sha256"], manifest["capture"]["sha256"]);
    // The move took its staging file: no warning, and nothing hidden beside
    // the fetched copy.
    assert_eq!(fetched["warnings"], serde_json::json!([]));
    let hidden: Vec<_> = fs::read_dir(out.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".fetching"))
        .collect();
    assert!(hidden.is_empty(), "{hidden:?}");
    // Neither word created a store in the clean home.
    assert!(!homes.database("clean").exists());
    assert!(!homes.database("clean").parent().unwrap().exists());

    // A fetch never replaces a file, and names only copies the target holds.
    let before = fs::read(&out).unwrap();
    homes.refused(
        "clean",
        &["backup", "fetch", copy, "--out", out.to_str().unwrap()],
        "backup_copy_exists",
    );
    assert_eq!(fs::read(&out).unwrap(), before);
    let elsewhere = homes.path("elsewhere.db");
    homes.refused(
        "clean",
        &[
            "backup",
            "fetch",
            "20260101T000000Z-no-such-copy",
            "--out",
            elsewhere.to_str().unwrap(),
        ],
        "backup_copy_unknown",
    );
    assert!(!elsewhere.exists());
}

#[test]
fn list_and_fetch_without_a_configured_target_are_refused() {
    let homes = Homes::new();
    homes.refused("bare", &["backup", "list"], "backup_not_configured");
    let out = homes.path("fetched.db");
    homes.refused(
        "bare",
        &["backup", "fetch", "any", "--out", out.to_str().unwrap()],
        "backup_not_configured",
    );
    assert!(!out.exists());
    assert!(!homes.database("bare").exists());
}

#[test]
fn a_bare_relative_out_is_written_in_the_current_directory() {
    let homes = Homes::new();
    homes.succeeded("origin", &["init"]);
    homes.set_target("origin");
    homes.succeeded("origin", &["backup", "push"]);
    let listing = homes.list("origin");
    let manifest = &listing["copies"][0];
    let copy = manifest["copy"].as_str().unwrap();
    let here = homes.path("here");
    fs::create_dir_all(&here).unwrap();
    let output = homes.engram_in(
        &here,
        "origin",
        &["backup", "fetch", copy, "--out", "fetched.db"],
    );
    assert!(
        output.status.success(),
        "{}{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert_eq!(
        sha256(&here.join("fetched.db")),
        manifest["capture"]["sha256"].as_str().unwrap()
    );
}

#[test]
fn a_fetch_out_of_time_leaves_neither_the_file_nor_its_staging_and_says_why_in_json() {
    let homes = Homes::new();
    homes.succeeded("origin", &["init"]);
    homes.set_target("origin");
    homes.succeeded("origin", &["backup", "push"]);
    let listing = homes.list("origin");
    let copy = listing["copies"][0]["copy"].as_str().unwrap().to_owned();
    let out_dir = homes.path("out");
    fs::create_dir_all(&out_dir).unwrap();
    let out = out_dir.join("fetched.db");
    // Two seconds leave the decode no time of its own: it stops at once and
    // removes what it wrote.
    let output = homes.engram(
        "origin",
        &[
            "backup",
            "fetch",
            &copy,
            "--out",
            out.to_str().unwrap(),
            "--deadline-secs",
            "2",
            "--json",
        ],
    );
    assert!(!output.status.success());
    let refusal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refusal["schema_version"], 1);
    assert_eq!(refusal["code"], "backup_transport_deadline", "{refusal}");
    assert!(!out.exists());
    assert_eq!(
        fs::read_dir(&out_dir).unwrap().count(),
        0,
        "no staging file is left"
    );
}
