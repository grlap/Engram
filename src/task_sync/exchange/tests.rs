use super::*;
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;

fn target() -> ExchangeTarget {
    ExchangeTarget {
        selection: TargetSelection {
            repository: PathBuf::from("C:/fixture"),
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
        },
        url: "https://example.invalid/project.git".into(),
        helpers: vec!["manager".into()],
        helper_config: Vec::new(),
    }
}

#[test]
fn task_exchange_typed_commands_are_exact_non_force_and_do_not_expand_callers() {
    let commit = "a".repeat(40);
    let target = target();
    let command = command::network(
        &target,
        &ExchangeOperation::Push {
            commit: commit.clone(),
        },
    )
    .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &args[args.len() - 11..],
        [
            "push",
            "--porcelain",
            "--no-verify",
            "--recurse-submodules=no",
            "--no-follow-tags",
            "--no-mirror",
            "--no-prune",
            "--signed=false",
            "--",
            "https://example.invalid/project.git",
            &format!("{commit}:refs/heads/main")
        ]
    );
    assert!(
        args.iter()
            .all(|a| !a.contains("--force") && !a.starts_with('+'))
    );
    let fetch = command::network(
        &target,
        &ExchangeOperation::Fetch {
            commit: commit.clone(),
            retention_ref: "refs/engram-exchange/fixture".into(),
        },
    )
    .unwrap();
    let fetch_args: Vec<_> = fetch
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &fetch_args[fetch_args.len() - 10..],
        [
            "fetch",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--",
            "https://example.invalid/project.git",
            &format!("{commit}:refs/engram-exchange/fixture"),
        ]
    );
    for invalid in [
        "-option".into(),
        "a;whoami".into(),
        "A".repeat(40),
        "a".repeat(39),
    ] {
        assert_eq!(
            command::network(&target, &ExchangeOperation::Push { commit: invalid }).unwrap_err(),
            RunError::InvalidOperation
        );
    }
    assert_eq!(
        command::network(
            &target,
            &ExchangeOperation::Fetch {
                commit,
                retention_ref: "refs/heads/main".into(),
            }
        )
        .unwrap_err(),
        RunError::InvalidOperation
    );
    assert!(!format!("{target:?}").contains("example.invalid"));
    assert!(
        !format!(
            "{:?}",
            RunOutput {
                stdout: b"secret-token".to_vec(),
                stderr_bytes: 1
            }
        )
        .contains("secret-token")
    );
    let env: Vec<_> = command.get_envs().collect();
    assert!(
        env.iter()
            .any(|(k, v)| *k == "GCM_INTERACTIVE" && *v == Some(std::ffi::OsStr::new("false")))
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn task_exchange_unsupported_platform_never_launches_the_command() {
    let mut command = std::process::Command::new("must-not-be-launched");
    command.arg("untrusted-input");
    assert_eq!(
        execute(command, RunLimits::default(), &Cancellation::default())
            .await
            .unwrap_err(),
        RunError::UnsupportedPlatform
    );
}

#[test]
fn task_exchange_limits_and_ref_validation_refuse_before_launch() {
    assert_eq!(
        RunLimits {
            deadline: Duration::ZERO,
            ..RunLimits::default()
        }
        .validate(),
        Err(RunError::InvalidLimits)
    );
    for reference in [
        "-main",
        "refs/heads/../x",
        "refs/heads/a.lock",
        "refs/heads/a\nb",
        "refs//heads/x",
        "refs/heads/x@{y}",
    ] {
        assert!(!command::reference(reference), "{reference}");
    }
    assert!(command::reference("refs/heads/feature/a-b"));
    let cancel = Cancellation::default();
    assert!(!cancel.is_cancelled());
    cancel.cancel();
    assert!(cancel.clone().is_cancelled());
}

#[cfg(windows)]
mod windows;
