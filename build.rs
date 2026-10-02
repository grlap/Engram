//! Records the source revision the build was made from, for the build
//! identity. A build never fails because of Git: without Git, outside a
//! checkout of this package, or when any probe fails, the revision is
//! `unavailable`.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

const UNAVAILABLE: &str = "unavailable";

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    // Any rerun-if-changed replaces cargo's default of rerunning on every
    // package file, so the package's own build inputs are always listed.
    let mut watched: BTreeSet<PathBuf> = ["build.rs", "Cargo.toml", "Cargo.lock", "src"]
        .into_iter()
        .map(|input| manifest_dir.join(input))
        .collect();
    let revision = if let Some((revision, inputs)) = probe(&manifest_dir) {
        watched.extend(inputs);
        revision
    } else {
        // Watch what would let a later probe succeed: Git appearing on the
        // path, or the checkout's own Git entry changing.
        println!("cargo:rerun-if-env-changed=PATH");
        let git = manifest_dir.join(".git");
        if git.is_file() {
            watched.insert(git);
        } else if git.join("HEAD").is_file() {
            watched.insert(git.join("HEAD"));
        }
        UNAVAILABLE.to_owned()
    };
    println!("cargo:rustc-env=ENGRAM_SOURCE_REVISION={revision}");
    for path in watched {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The revision, `<commit>` or `<commit>+dirty`, and the files whose change
/// can change it: every tracked file, and the Git files that a commit, a
/// checkout or a staged change moves. `None` when any part cannot be read.
fn probe(manifest_dir: &Path) -> Option<(String, Vec<PathBuf>)> {
    let top = git(manifest_dir, &["rev-parse", "--show-toplevel"])?;
    if !same_directory(Path::new(&top), manifest_dir) {
        // Git found an enclosing checkout, not this package's own.
        return None;
    }
    let commit = git(manifest_dir, &["rev-parse", "--verify", "HEAD"])?;
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let status = git_raw(
        manifest_dir,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    )?;
    let revision = if status.trim().is_empty() {
        commit.to_ascii_lowercase()
    } else {
        format!("{}+dirty", commit.to_ascii_lowercase())
    };
    // Each tracked file by itself: a directory would be scanned whole,
    // ignored build output and nested checkouts included. A tracked file
    // that is missing makes cargo rerun this script until it is back, which
    // is right while the deletion makes the tree dirty.
    let tracked = git_raw(manifest_dir, &["ls-files", "-z"])?;
    let mut inputs: Vec<PathBuf> = tracked
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| manifest_dir.join(path))
        .collect();
    // Only Git files that exist: cargo reruns a script on every build while a
    // watched path is missing. A branch kept only in packed-refs is caught
    // through its directory, which changes when a loose ref appears there.
    let mut git_files = vec![
        git_path(manifest_dir, "HEAD")?,
        git_path(manifest_dir, "index")?,
        git_path(manifest_dir, "packed-refs")?,
    ];
    if let Some(branch) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        let reference = git_path(manifest_dir, &branch)?;
        if let Some(directory) = reference.parent() {
            git_files.push(directory.to_path_buf());
        }
        git_files.push(reference);
    }
    inputs.extend(git_files.into_iter().filter(|path| path.exists()));
    Some((revision, inputs))
}

fn git_path(directory: &Path, name: &str) -> Option<PathBuf> {
    git(directory, &["rev-parse", "--git-path", name]).map(PathBuf::from)
}

fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = git_raw(directory, args)?;
    let line = output.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// Runs Git read-only: no optional locks, so it never refreshes the index,
/// no file-system monitor, and no repository chosen by the environment.
fn git_raw(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
