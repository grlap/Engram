//! Real Git target capture and credential-helper policy on Unix.
//!
//! Each group runs in a child of this test binary whose home and global Git
//! configuration belong to its own fixture, so the host's helpers (such as a
//! keychain configured for the runner's user) never reach the capture. The
//! product runner itself supervises each child. The groups shared with Windows
//! come from the common policy cases; the rest cover Unix keychains and
//! transport refusals.

use super::super::super::{
    Cancellation, ExchangeOperation, ExchangeTarget, RunError, RunLimits, TargetSelection, process,
    run, target::NATIVE_HELPER, test_support,
};
use super::super::policy_cases::{self, Host, MALFORMED_URLS};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

const FIXTURE: &str = "task_sync::exchange::tests::unix::policy::policy_fixture";
const GROUPS: [&str; 8] = [
    "url-1-7",
    "url-8-14",
    "encoded-fabricated",
    "helper-order-drift",
    "helper-refusals",
    "capture-drift",
    "order-native",
    "transport-refusals",
];
const COMPLETE: &str = "unix-policy-group complete";

#[tokio::test]
async fn task_exchange_unix_target_capture_and_helper_policy_use_real_git() {
    for group in GROUPS {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/tmp")
            .join(format!("exchange-unix-policy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("xdg")).unwrap();
        fs::write(
            root.join("global.config"),
            b"[credential]\n\thelper = manager\n",
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", FIXTURE, "--ignored", "--nocapture"])
            .env("ENGRAM_EXCHANGE_POLICY_GROUP", group)
            .env("ENGRAM_EXCHANGE_FIXTURE_ROOT", &root);
        isolate(&mut command, &root);
        let limits = RunLimits {
            deadline: Duration::from_secs(120),
            ..RunLimits::default()
        };
        let output =
            process::execute_diagnostic(command, limits, &Cancellation::default(), None).await;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.result.is_ok() && stdout.contains(COMPLETE),
            "{group}: {:?}\n{stdout}\n{}",
            output.result,
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// A home and global Git configuration of the fixture's own, with every
/// inherited repository and configuration override removed.
pub(super) fn isolate(command: &mut Command, root: &Path) {
    command
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("GIT_CONFIG_GLOBAL", root.join("global.config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1");
    for name in [
        "GIT_CONFIG",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(name);
    }
}

#[tokio::test]
#[ignore = "isolated policy subprocess entry; exercised by the Unix policy test"]
async fn policy_fixture() {
    let group = std::env::var("ENGRAM_EXCHANGE_POLICY_GROUP").unwrap();
    let host = Fixture(PathBuf::from(
        std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap(),
    ));
    host.git(&["init", "-q"]);
    host.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repo.git",
    ]);
    let selection = TargetSelection {
        repository: host.0.clone(),
        remote: "origin".into(),
        reference: "refs/heads/main".into(),
    };
    match group.as_str() {
        "url-1-7" => {
            policy_cases::malformed_url_cases(&host, &selection, &MALFORMED_URLS[..7]).await;
        }
        "url-8-14" => {
            policy_cases::malformed_url_cases(&host, &selection, &MALFORMED_URLS[7..]).await;
        }
        "encoded-fabricated" => policy_cases::encoded_url_cases(&host, &selection).await,
        "helper-order-drift" => {
            policy_cases::helper_order_and_drift_cases(&host, &selection).await;
        }
        "helper-refusals" => policy_cases::helper_refusal_cases(&host, &selection).await,
        "capture-drift" => capture_drift(&host, &selection).await,
        "order-native" => order_native(&host, &selection).await,
        "transport-refusals" => transport_refusals(&host, &selection).await,
        _ => panic!("unknown Unix policy group {group}"),
    }
    println!("\n{COMPLETE} group={group}");
}

/// The isolated child's repository; its environment is already isolated.
struct Fixture(PathBuf);

impl Host for Fixture {
    fn root(&self) -> &Path {
        &self.0
    }

    fn git(&self, args: &[&str]) {
        let mut command = Command::new("git");
        command
            .current_dir(&self.0)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0");
        let output = self.output(&mut command);
        assert!(
            output.status.success(),
            "fixture Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn output(&self, command: &mut Command) -> Output {
        command
            .output()
            .expect("required prerequisite: Git executable")
    }

    fn launches(&self) -> (usize, usize) {
        (0, test_support::LAUNCHES.with(std::cell::Cell::get))
    }
}

async fn capture(selection: &TargetSelection) -> Result<ExchangeTarget, RunError> {
    ExchangeTarget::capture(selection, RunLimits::default(), &Cancellation::default()).await
}

async fn observe(target: &ExchangeTarget) -> RunError {
    run(
        target,
        &ExchangeOperation::Observe,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap_err()
}

/// Capture pins the configured helpers; a later shell helper refuses before it
/// can run, and a changed helper selection is drift.
async fn capture_drift(host: &Fixture, selection: &TargetSelection) {
    let target = capture(selection).await.unwrap();
    assert_eq!(target.url, "https://example.invalid/repo.git");
    assert_eq!(target.helpers, ["manager"]);
    host.git(&[
        "config",
        "credential.helper",
        "!echo invoked > helper-invoked",
    ]);
    assert_eq!(observe(&target).await, RunError::UnsupportedHelper);
    assert!(!host.0.join("helper-invoked").exists());
    host.git(&["config", "--unset-all", "credential.helper"]);
    host.git(&["config", "credential.helper", "manager-core"]);
    assert_eq!(observe(&target).await, RunError::TargetChanged);
}

/// The platform's own keychain is admitted through real configuration, and
/// every other keychain, the cache daemon and the plaintext store refuse.
async fn order_native(host: &Fixture, selection: &TargetSelection) {
    if let Some(native) = NATIVE_HELPER {
        // An empty entry resets the global selection before the native helper.
        host.git(&["config", "--add", "credential.helper", ""]);
        host.git(&["config", "--add", "credential.helper", native]);
        assert_eq!(capture(selection).await.unwrap().helpers, [native]);
        host.git(&["config", "--unset-all", "credential.helper"]);
    }
    for refused in ["wincred", "osxkeychain", "libsecret", "cache", "store"]
        .into_iter()
        .filter(|helper| Some(*helper) != NATIVE_HELPER)
    {
        host.git(&["config", "credential.helper", refused]);
        assert_eq!(
            capture(selection).await.unwrap_err(),
            RunError::UnsupportedHelper,
            "{refused}"
        );
        host.git(&["config", "--unset-all", "credential.helper"]);
    }
}

/// SSH, bundle fetch, default push options and URL rewrites refuse before any
/// transport.
async fn transport_refusals(host: &Fixture, selection: &TargetSelection) {
    host.git(&[
        "remote",
        "set-url",
        "origin",
        "git@example.invalid:repo.git",
    ]);
    assert_eq!(
        capture(selection).await.unwrap_err(),
        RunError::UnsupportedTarget
    );
    host.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo.git",
    ]);
    for key in ["fetch.bundleURI", "push.pushOption"] {
        host.git(&["config", key, "not-admitted"]);
        assert_eq!(
            capture(selection).await.unwrap_err(),
            RunError::UnsupportedConfiguration,
            "{key}"
        );
        host.git(&["config", "--unset", key]);
    }
    host.git(&[
        "config",
        "url.https://other.invalid/.insteadOf",
        "https://example.invalid/",
    ]);
    assert_eq!(
        capture(selection).await.unwrap_err(),
        RunError::UnsupportedRewrite
    );
}
