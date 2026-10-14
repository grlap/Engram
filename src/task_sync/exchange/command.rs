//! Fixed argument construction; only Git interprets admitted credential facilities.

use super::{ExchangeOperation, ExchangeTarget, RunError};
use std::{
    path::Path,
    process::{Command, Stdio},
};

pub(super) fn local(repository: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "core.commitGraph=false",
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "protocol.allow=never",
        ])
        .args(args);
    // Configuration queries must observe the real helper selection. These
    // fixed local commands never request credentials or invoke a transport.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_INTERNAL_SUPER_PREFIX",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_EXEC_PATH",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_SSH_VARIANT",
        "GIT_PROXY_COMMAND",
        "GIT_SSL_NO_VERIFY",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "SSH_ASKPASS_REQUIRE",
        "GIT_TRACE",
        "GIT_TRACE_PACKET",
        "GIT_TRACE_PACK_ACCESS",
        "GIT_TRACE_SETUP",
        "GIT_TRACE_PERFORMANCE",
        "GIT_TRACE_CURL",
        "GIT_CURL_VERBOSE",
        "GIT_TRACE2",
        "GIT_TRACE2_EVENT",
        "GIT_TRACE2_PERF",
        "GCM_TRACE",
        "GCM_TRACE_SECRETS",
        "GCM_TRACE_MSAUTH",
        "GCM_DEBUG",
    ] {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_TRACE")
        {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GCM_INTERACTIVE", "false")
        .env("GCM_GUI_PROMPT", "false")
        .env("GCM_TRACE", "0")
        .env("GCM_TRACE_SECRETS", "0")
        .env("GCM_TRACE_MSAUTH", "0")
        .env("GCM_DEBUG", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(parent) = repository.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
}

pub(super) fn network(
    target: &ExchangeTarget,
    operation: &ExchangeOperation,
) -> Result<Command, RunError> {
    let mut command = local(&target.selection.repository, &[]);
    command
        .args([
            "-c",
            "protocol.https.allow=always",
            "-c",
            "credential.interactive=false",
            "-c",
            "credential.trace=false",
            "-c",
            "credential.traceSecrets=false",
            "-c",
            "credential.traceMsAuth=false",
            "-c",
            "credential.debug=false",
            "-c",
            "core.askPass=",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.sslVerify=true",
            "-c",
            "submodule.recurse=false",
            "-c",
            "fetch.recurseSubmodules=false",
            "-c",
            "transfer.bundleURI=false",
            "-c",
            "fetch.uriProtocols=",
            "-c",
            "push.negotiate=false",
        ])
        .env("GIT_ALLOW_PROTOCOL", "https");
    // Separate keys and values cannot reinterpret URL bytes as the value of a
    // different key. Admission requires Git >= 2.31 and restricts URL syntax too.
    let mut settings = vec![
        ("credential.helper".into(), String::new()),
        (format!("credential.{}.helper", target.url), String::new()),
    ];
    for (key, value) in [("sslVerify", "true"), ("followRedirects", "false")] {
        settings.push((format!("http.{}.{key}", target.url), value.to_owned()));
    }
    for key in [
        "interactive",
        "trace",
        "traceSecrets",
        "traceMsAuth",
        "debug",
    ] {
        settings.push((format!("credential.{}.{key}", target.url), "false".into()));
    }
    for helper in &target.helpers {
        settings.push((format!("credential.{}.helper", target.url), helper.clone()));
    }
    command.env("GIT_CONFIG_COUNT", settings.len().to_string());
    for (index, (key, value)) in settings.into_iter().enumerate() {
        command.env(format!("GIT_CONFIG_KEY_{index}"), key);
        command.env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    match operation {
        ExchangeOperation::Observe => {
            command.args([
                "ls-remote",
                "--refs",
                "--heads",
                "--",
                &target.url,
                &target.selection.reference,
            ]);
        }
        ExchangeOperation::Fetch {
            commit,
            retention_ref,
        } => {
            if !object_id(commit)
                || !retention_ref.starts_with("refs/engram-exchange/")
                || !reference(retention_ref)
            {
                return Err(RunError::InvalidOperation);
            }
            command.args([
                "fetch",
                "--no-tags",
                "--no-prune",
                "--no-prune-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
                "--",
                &target.url,
            ]);
            command.arg(format!("{commit}:{retention_ref}"));
        }
        ExchangeOperation::Push { commit } => {
            if !object_id(commit) {
                return Err(RunError::InvalidOperation);
            }
            command.args([
                "push",
                "--porcelain",
                "--no-verify",
                "--recurse-submodules=no",
                "--no-follow-tags",
                "--no-mirror",
                "--no-prune",
                "--signed=false",
                "--",
                &target.url,
            ]);
            command.arg(format!("{commit}:{}", target.selection.reference));
        }
    }
    Ok(command)
}

pub(super) fn object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// Git ref syntax reserves the lowercase literal `.lock`, not a filesystem extension.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(super) fn reference(value: &str) -> bool {
    value.starts_with("refs/")
        && value.len() <= 1024
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && value
            .split('/')
            .all(|s| !s.starts_with('.') && !s.ends_with(".lock") && !s.is_empty())
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b))
}
