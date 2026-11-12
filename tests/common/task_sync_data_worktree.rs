//! Explicitly stage and retain private snapshots around the real CLI data folder.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=",
            "-c",
            "core.fsmonitor=",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join(".gitconfig-absent"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

pub fn initialize(marker: &Path) -> PathBuf {
    let code = marker.parent().unwrap();
    git(code, &["init", "--initial-branch=code"]);
    // The rest of this owned fixture holds stores and retained snapshots.
    fs::write(
        code.join(".gitignore"),
        "/*\n!/.engram-project\n!/.gitignore\n",
    )
    .unwrap();
    git(code, &["add", "--", ".engram-project", ".gitignore"]);
    git(
        code,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "Create code fixture",
        ],
    );
    let data = code.join(".engram-data");
    let argument = engram::task_sync::git::plain_path(&data).unwrap();
    git(
        code,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            argument.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(&data, &["symbolic-ref", "HEAD", "refs/heads/engram-data"]);
    git(&data, &["read-tree", "--empty"]);
    data.join(".engram")
}

pub fn folder(marker: &Path) -> PathBuf {
    marker.parent().unwrap().join(".engram-data/.engram")
}

/// Preserve previous public bytes as a private fixture snapshot, without deletion.
pub fn clear(marker: &Path) {
    let live = folder(marker);
    if live.exists() {
        let retired = marker
            .parent()
            .unwrap()
            .join(format!("retained-{}", uuid::Uuid::new_v4()));
        fs::rename(live, retired).unwrap();
    }
}

pub fn stage_snapshot(marker: &Path, snapshot: &Path) {
    clear(marker);
    copy_folder(snapshot, &folder(marker));
}

pub fn retain_snapshot(marker: &Path, snapshot: &Path) {
    copy_folder(&folder(marker), snapshot);
}

fn copy_folder(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_type().unwrap().is_symlink());
        if entry.file_name() == "tasks" {
            fs::create_dir(target.join("tasks")).unwrap();
            for task in fs::read_dir(entry.path()).unwrap() {
                let task = task.unwrap();
                assert!(task.file_type().unwrap().is_file());
                fs::copy(task.path(), target.join("tasks").join(task.file_name())).unwrap();
            }
        } else {
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
