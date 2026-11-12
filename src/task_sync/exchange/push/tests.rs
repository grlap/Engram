use super::*;
use crate::{CanonicalObject, task_sync::exchange::command};
use std::{fs, path::Path, process::Output};

fn selected_commit() -> String {
    CanonicalObject::freeze(&"selected data commit")
        .unwrap()
        .key()
        .to_string()
}

fn refusal(commit: &str, reason: &str) -> Vec<u8> {
    format!("To https://example.invalid/data.git\n!\t{commit}:refs/heads/engram-data\t[rejected] ({reason})\nDone\n").into_bytes()
}

#[test]
fn only_one_exact_selected_ref_non_fast_forward_is_a_known_refusal() {
    let commit = selected_commit();
    for reason in ["non-fast-forward", "fetch first"] {
        let bytes = refusal(&commit, reason);
        assert_eq!(
            classify_failure(
                RunError::Git,
                Some(1),
                &bytes,
                &commit,
                "refs/heads/engram-data"
            ),
            RunError::NonFastForward
        );
        for other in [
            Vec::new(),
            vec![0xff, b'\n'],
            bytes[..bytes.len() - 1].to_vec(),
            refusal(&commit, "stale info"),
            String::from_utf8(bytes.clone())
                .unwrap()
                .replace(
                    &commit,
                    CanonicalObject::freeze(&"another commit")
                        .unwrap()
                        .key()
                        .as_str(),
                )
                .into_bytes(),
            String::from_utf8(bytes.clone())
                .unwrap()
                .replace("[rejected]", "[remote rejected]")
                .into_bytes(),
            String::from_utf8(bytes.clone())
                .unwrap()
                .replace("refs/heads/engram-data", "refs/heads/other")
                .into_bytes(),
            format!(
                "{}!\t{commit}:refs/heads/other\t[rejected] (fetch first)\n",
                String::from_utf8_lossy(&bytes)
            )
            .into_bytes(),
        ] {
            assert_eq!(
                classify_failure(
                    RunError::Git,
                    Some(1),
                    &other,
                    &commit,
                    "refs/heads/engram-data"
                ),
                RunError::Git
            );
        }
    }
}

#[test]
fn incomplete_transport_errors_keep_their_uncertainty_even_with_a_refusal_excerpt() {
    let commit = selected_commit();
    let bytes = refusal(&commit, "fetch first");
    for error in [
        RunError::Deadline,
        RunError::Cancelled,
        RunError::OutputLimit,
        RunError::Spawn,
        RunError::Process,
    ] {
        assert_eq!(
            classify_failure(
                error.clone(),
                Some(1),
                &bytes,
                &commit,
                "refs/heads/engram-data"
            ),
            error
        );
    }
    let diagnostic = RunError::NonFastForward.to_string();
    for exit_code in [None, Some(0), Some(128)] {
        assert_eq!(
            classify_failure(
                RunError::Git,
                exit_code,
                &bytes,
                &commit,
                "refs/heads/engram-data"
            ),
            RunError::Git
        );
    }
    assert!(!diagnostic.contains(&commit));
    assert!(!diagnostic.contains("https://"));
}

fn git(root: &Path, config: &Path, args: &[&str]) -> Output {
    let mut command = command::local(root, &[]);
    command
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_SYSTEM", config)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("LC_ALL", "C")
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "user.name=Exchange fixture",
            "-c",
            "user.email=exchange@example.invalid",
        ])
        .args(args);
    command.output().unwrap()
}

fn ok(root: &Path, config: &Path, args: &[&str]) -> String {
    let output = git(root, config, args);
    assert!(
        output.status.success(),
        "Git fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path, config: &Path, text: &str) -> String {
    fs::create_dir_all(root.join(".engram")).unwrap();
    fs::write(root.join(".engram/project.json"), text).unwrap();
    ok(root, config, &["add", ".engram/project.json"]);
    ok(root, config, &["commit", "-m", "Record data fixture"]);
    ok(root, config, &["rev-parse", "HEAD"])
}

#[test]
fn actual_single_ref_refusal_between_two_local_clones_is_distinct_from_resolved_fast_forward() {
    let home = crate::test_support::temp_home().unwrap();
    let root = home.path();
    let config = root.join("empty-config");
    fs::write(&config, b"").unwrap();
    let bare = root.join("data.git");
    let left = root.join("left");
    let right = root.join("right");
    fs::create_dir(&left).unwrap();
    ok(root, &config, &["init", "--bare", bare.to_str().unwrap()]);
    ok(&left, &config, &["init", "--initial-branch=engram-data"]);
    ok(
        &left,
        &config,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    let seed = commit(&left, &config, "seed\n");
    let reference = "refs/heads/engram-data";
    ok(
        &left,
        &config,
        &[
            "push",
            "--porcelain",
            "--",
            "origin",
            &format!("{seed}:{reference}"),
        ],
    );
    ok(
        root,
        &config,
        &[
            "clone",
            "--no-local",
            "--branch",
            "engram-data",
            "--",
            bare.to_str().unwrap(),
            right.to_str().unwrap(),
        ],
    );
    let local = commit(&left, &config, "left\n");
    let remote = commit(&right, &config, "right\n");
    ok(
        &right,
        &config,
        &[
            "push",
            "--porcelain",
            "--",
            "origin",
            &format!("{remote}:{reference}"),
        ],
    );
    let rejected = git(
        &left,
        &config,
        &[
            "push",
            "--porcelain",
            "--",
            "origin",
            &format!("{local}:{reference}"),
        ],
    );
    assert!(!rejected.status.success());
    assert_eq!(
        classify_failure(
            RunError::Git,
            rejected.status.code(),
            &rejected.stdout,
            &local,
            reference
        ),
        RunError::NonFastForward
    );
    ok(
        &left,
        &config,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            "origin",
            "refs/heads/engram-data:refs/remotes/origin/engram-data",
        ],
    );
    fs::write(left.join(".engram/project.json"), "left and right\n").unwrap();
    ok(&left, &config, &["add", ".engram/project.json"]);
    let tree = ok(&left, &config, &["write-tree"]);
    let resolved = ok(
        &left,
        &config,
        &[
            "commit-tree",
            &tree,
            "-p",
            &local,
            "-p",
            &remote,
            "-m",
            "Resolve concurrent data",
        ],
    );
    ok(
        &left,
        &config,
        &["update-ref", reference, &resolved, &local],
    );
    let accepted = git(
        &left,
        &config,
        &[
            "push",
            "--porcelain",
            "--",
            "origin",
            &format!("{resolved}:{reference}"),
        ],
    );
    assert!(accepted.status.success());
    assert!(!non_fast_forward(&accepted.stdout, &resolved, reference));
    assert_eq!(ok(&bare, &config, &["rev-parse", reference]), resolved);
    assert_eq!(
        ok(&left, &config, &["show", "-s", "--format=%P", &resolved]),
        format!("{local} {remote}")
    );
}
