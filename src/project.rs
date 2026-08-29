//! Stable host-local project store discovery and project-root filesystem
//! identity probing.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{HostPathPolicy, ProjectId};

/// Resolves a stable project identity to an opaque directory below the
/// caller's platform-specific Engram data directory.
#[must_use]
pub fn project_database_path(engram_home: &Path, project_id: &ProjectId) -> PathBuf {
    let digest = Sha256::digest(project_id.0.as_bytes());
    engram_home
        .join("projects")
        .join(format!("{digest:x}"))
        .join("engram.db")
}

/// Why the project root's filesystem identity could not be probed.
#[derive(Debug, Error)]
pub enum HostPathProbeError {
    #[error("project root {0} is not a directory")]
    NotADirectory(PathBuf),
    #[error("could not probe the filesystem identity of {root}: {source}")]
    Io {
        root: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Probes whether the filesystem holding `root` folds case, by creating one
/// uniquely named file and looking it up under the opposite case. Windows
/// alias rules follow the running target, because they are operating-system
/// semantics rather than a property of one filesystem. The probe file is
/// removed before returning; an unwritable or missing root is an error, so a
/// caller that cannot prove identity fails closed instead of guessing.
///
/// # Errors
///
/// Returns [`HostPathProbeError`] when `root` is not a directory or the probe
/// file cannot be created, inspected, or removed.
pub fn probe_host_path_policy(root: &Path) -> Result<HostPathPolicy, HostPathProbeError> {
    if !root.is_dir() {
        return Err(HostPathProbeError::NotADirectory(root.to_path_buf()));
    }
    let io_error = |source: io::Error| HostPathProbeError::Io {
        root: root.to_path_buf(),
        source,
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let marker = format!("engram-path-probe-{}-{nanos}", std::process::id());
    let lower = root.join(format!(".{marker}.Probe"));
    let upper = root.join(format!(".{marker}.PROBE"));
    fs::write(&lower, marker.as_bytes()).map_err(io_error)?;
    let folded = match fs::read(&upper) {
        Ok(bytes) => bytes == marker.as_bytes(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = fs::remove_file(&lower);
            return Err(io_error(error));
        }
    };
    fs::remove_file(&lower).map_err(io_error)?;
    Ok(HostPathPolicy {
        case_fold_paths: folded,
        windows_alias_rules: cfg!(target_os = "windows"),
    })
}

/// Parses a host-supplied path policy name: `case_fold` or `case_sensitive`.
/// Windows alias rules again follow the running target.
#[must_use]
pub fn parse_host_path_policy(value: &str) -> Option<HostPathPolicy> {
    let case_fold_paths = match value.trim().to_ascii_lowercase().as_str() {
        "case_fold" | "case-fold" | "case_insensitive" | "case-insensitive" => true,
        "case_sensitive" | "case-sensitive" => false,
        _ => return None,
    };
    Some(HostPathPolicy {
        case_fold_paths,
        windows_alias_rules: cfg!(target_os = "windows"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_the_real_filesystem_and_cleans_up() {
        let directory = tempfile::tempdir().expect("temp directory");
        let policy = probe_host_path_policy(directory.path()).expect("probe a writable root");
        // A plain temporary directory follows the platform's usual semantics;
        // the probe reads the filesystem rather than assuming them.
        assert_eq!(
            policy.case_fold_paths,
            cfg!(any(target_os = "windows", target_os = "macos"))
        );
        assert_eq!(policy.windows_alias_rules, cfg!(target_os = "windows"));
        assert_eq!(
            fs::read_dir(directory.path()).expect("list root").count(),
            0,
            "the probe file must not survive"
        );
    }

    #[test]
    fn probe_refuses_a_missing_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let missing = directory.path().join("absent");
        assert!(matches!(
            probe_host_path_policy(&missing),
            Err(HostPathProbeError::NotADirectory(_))
        ));
    }

    #[test]
    fn host_supplied_policy_names_parse() {
        assert_eq!(
            parse_host_path_policy("case_fold").map(|policy| policy.case_fold_paths),
            Some(true)
        );
        assert_eq!(
            parse_host_path_policy("Case-Sensitive").map(|policy| policy.case_fold_paths),
            Some(false)
        );
        assert!(parse_host_path_policy("maybe").is_none());
    }

    #[test]
    fn project_path_does_not_depend_on_the_worktree() {
        let home = Path::new("/host-local-engram");
        let project = ProjectId("project-stable-id".into());

        let first = project_database_path(home, &project);
        let second = project_database_path(home, &project);

        assert_eq!(first, second);
        assert_eq!(first.file_name().unwrap(), "engram.db");
        assert!(!first.to_string_lossy().contains("project-stable-id"));
    }
}
