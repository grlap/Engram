//! Real Git target capture and credential-helper policy cases, run on every
//! platform with a confined runner. Each platform runs them in an isolated
//! child whose home and global Git configuration belong to its own fixture,
//! with `credential.helper = manager` configured globally.

use super::super::{
    Cancellation, ExchangeOperation, ExchangeTarget, RunError, RunLimits, TargetSelection, command,
    run, target::NATIVE_HELPER,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The platform's fixture around one isolated repository.
pub(super) trait Host {
    fn root(&self) -> &Path;
    /// A direct fixture Git command that must succeed.
    fn git(&self, args: &[&str]);
    /// A direct command these cases run themselves, outside the product runner.
    fn output(&self, command: &mut Command) -> Output;
    /// One case of the running group is complete.
    fn case_finished(&self) {}
    /// Launches so far, as (direct, product).
    fn launches(&self) -> (usize, usize);
}

pub(super) const MALFORMED_URLS: [&str; 14] = [
    "https://user=token@host/repo",
    "https://ho=st/repo",
    "https://host:4=43/repo",
    "https://host/re=po",
    "https://host/repo?q=x",
    "https://host/repo#x=y",
    "https://host/.helper=!echo;",
    "https://host/repo!",
    "https://host/repo;",
    "https://host/re po",
    "https://host/re\tpo",
    "https://host/re\rpo",
    "https://host/re\npo",
    "https://host/répo",
];

/// Every file under the fixture, so a refused capture can be shown to write nothing.
pub(super) fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                visit(root, &path, entries);
            } else {
                entries.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

pub(super) async fn malformed_url_cases(
    host: &impl Host,
    selection: &TargetSelection,
    urls: &[&str],
) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    for url in urls {
        host.git(&["remote", "set-url", "origin", url]);
        let before = snapshot(host.root());
        let error = ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error, RunError::UnsupportedTarget, "{url:?}");
        assert!(error.to_string().contains("ASCII unreserved path"));
        assert_eq!(snapshot(host.root()), before, "{url:?}");
        host.case_finished();
    }
}

pub(super) async fn encoded_url_cases(host: &impl Host, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    host.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo%3Dname.git",
    ]);
    let encoded = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(encoded.url, "https://example.invalid/repo%3Dname.git");
    assert_config(host, &encoded, "credential.helper", "manager");
    assert_config(host, &encoded, "http.sslVerify", "true");
    assert_config(host, &encoded, "http.followRedirects", "false");
    host.case_finished();

    // Even a fabricated internal target cannot split a key into a helper value.
    // Public capture above refuses this spelling before constructing a target.
    let mut fabricated = encoded.clone();
    fabricated.url = "https://example.invalid/.helper=!sentinel;".into();
    let before_build = host.launches();
    let built = command::network(&fabricated, &ExchangeOperation::Observe).unwrap();
    assert!(
        built
            .get_args()
            .take_while(|arg| *arg != "ls-remote")
            .all(|arg| !arg.to_string_lossy().contains("sentinel"))
    );
    assert!(
        built
            .get_envs()
            .filter(|(key, _)| key.to_string_lossy().starts_with("GIT_CONFIG_VALUE_"))
            .all(|(_, value)| !value.unwrap().to_string_lossy().contains("sentinel"))
    );
    assert_eq!(host.launches(), before_build, "building launches nothing");
    assert_config(host, &fabricated, "credential.helper", "manager");
    assert_config(host, &fabricated, "http.sslVerify", "true");
    assert_config(host, &fabricated, "http.followRedirects", "false");
    host.case_finished();
}

pub(super) async fn helper_order_and_drift_cases(host: &impl Host, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    host.git(&["config", "--add", "credential.helper", "manager-core"]);
    host.git(&["config", "--add", "credential.helper", "manager"]);
    let pinned = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(pinned.helpers, ["manager", "manager-core", "manager"]);
    host.case_finished();
    // An admitted helper inserted ahead of the pinned selection is drift.
    let admitted = NATIVE_HELPER.unwrap_or("manager-core");
    host.git(&["config", "--unset-all", "credential.helper"]);
    host.git(&["config", "--add", "credential.helper", admitted]);
    host.git(&["config", "--add", "credential.helper", "manager"]);
    assert_eq!(
        run(&pinned, &ExchangeOperation::Observe, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::TargetChanged
    );
    host.case_finished();
}

pub(super) async fn helper_refusal_cases(host: &impl Host, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    host.git(&[
        "config",
        "--add",
        "credential.helper",
        "!echo invoked > helper-invoked",
    ]);
    host.git(&["config", "--add", "credential.helper", "manager"]);
    assert_eq!(
        ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::UnsupportedHelper
    );
    assert!(!host.root().join("helper-invoked").exists());
    host.case_finished();
    host.git(&["config", "--unset-all", "credential.helper"]);
    let scoped = "credential.https://user:fake-token@example.invalid/.helper";
    host.git(&["config", scoped, "manager"]);
    let error = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("credential.<redacted URL scope>.helper"),
        "{error}"
    );
    assert!(error.contains("file:.git/config"), "{error}");
    assert!(error.contains("unscoped credential.helper"), "{error}");
    assert!(!error.contains("fake-token"), "{error}");
    host.case_finished();
    host.git(&["config", "--unset", scoped]);
    host.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo.git",
    ]);
}

/// Real Git reads `key` for the target's URL as the product command configures it.
fn assert_config(host: &impl Host, target: &ExchangeTarget, key: &str, expected: &str) {
    let built = command::network(target, &ExchangeOperation::Observe).unwrap();
    let mut query = Command::new(built.get_program());
    query.current_dir(built.get_current_dir().unwrap());
    query.args(built.get_args().take_while(|arg| *arg != "ls-remote"));
    for (name, value) in built.get_envs() {
        if let Some(value) = value {
            query.env(name, value);
        } else {
            query.env_remove(name);
        }
    }
    query.args(["config", "--get-urlmatch", key, &target.url]);
    let output = host.output(&mut query);
    assert!(output.status.success(), "{key}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        expected,
        "{key}"
    );
}
