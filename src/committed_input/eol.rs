//! Only machine EOL defaults cross the isolated Git configuration boundary.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

// Unit-test readers get their own fixture inputs; CLI conformance tests exercise
// production discovery in child processes with explicit machine homes and temp.
pub(super) struct Environment {
    isolated: bool,
    scratch: PathBuf,
    #[cfg(test)]
    _scratch_owner: crate::test_support::TempHome,
}

impl Environment {
    pub(super) fn for_read() -> Self {
        #[cfg(test)]
        let owner = crate::test_support::temp_home().expect("isolated source reader fixture");
        #[cfg(test)]
        let scratch = owner.path().to_path_buf();
        #[cfg(not(test))]
        let scratch = std::env::temp_dir();
        Self {
            isolated: cfg!(test),
            scratch,
            #[cfg(test)]
            _scratch_owner: owner,
        }
    }

    fn configure(&self, command: &mut std::process::Command) {
        if self.isolated {
            // Retain git_command's null global and disabled system scopes.
            return;
        }
        // Normal file locations, never inherited command/repository overrides.
        command
            .env_remove("GIT_CONFIG_NOSYSTEM")
            .env_remove("GIT_CONFIG_GLOBAL");
        for key in [
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Policy {
    autocrlf: Option<String>,
    eol: Option<String>,
}

impl Policy {
    pub(super) fn read(
        git: &Path,
        root: &Path,
        deadline: Instant,
        environment: &Environment,
    ) -> Result<Self> {
        let mut command = super::git_command(git, root);
        environment.configure(&mut command);
        command.args([
            "config",
            "--null",
            "--show-scope",
            "--includes",
            "--get-regexp",
            "^core\\.(autocrlf|eol)$",
        ]);
        let bytes = super::run_query(command, "config", true, deadline)
            .context("cannot read machine line-ending policy")?;
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let mut policy = Self::default();
        if bytes.is_empty() {
            return Ok(policy);
        }
        ensure!(
            bytes.last() == Some(&0),
            "truncated line-ending configuration"
        );
        let mut records = bytes[..bytes.len() - 1].split(|byte| *byte == 0);
        while let Some(scope) = records.next() {
            let record = records.next().context("missing scoped line-ending value")?;
            ensure!(
                matches!(
                    scope,
                    b"system" | b"global" | b"local" | b"worktree" | b"command"
                ),
                "unknown line-ending configuration scope"
            );
            if !matches!(scope, b"system" | b"global") {
                continue;
            }
            let record =
                std::str::from_utf8(record).context("non-UTF-8 line-ending configuration")?;
            let (key, value) = record
                .split_once('\n')
                .map_or((record, None), |(key, value)| (key, Some(value)));
            match key {
                "core.autocrlf" => {
                    let value = value.unwrap_or("true").to_ascii_lowercase();
                    let normalized = match value.as_str() {
                        "input" => "input",
                        "true" | "yes" | "on" => "true",
                        "false" | "no" | "off" | "" => "false",
                        _ => match value.parse::<i64>() {
                            Ok(0) => "false",
                            Ok(_) => "true",
                            Err(_) => bail!("unsupported machine core.autocrlf value"),
                        },
                    };
                    policy.autocrlf = Some(normalized.to_owned());
                }
                "core.eol" => {
                    let value = value
                        .context("machine core.eol has no value")?
                        .to_ascii_lowercase();
                    ensure!(
                        matches!(value.as_str(), "lf" | "crlf" | "native"),
                        "unsupported machine core.eol value"
                    );
                    policy.eol = Some(value);
                }
                _ => bail!("unexpected machine line-ending key"),
            }
        }
        Ok(policy)
    }

    pub(super) fn materialize(
        &self,
        root: &Path,
        deadline: Instant,
        environment: &Environment,
    ) -> Result<Option<Fallback>> {
        if self.autocrlf.is_none() && self.eol.is_none() {
            return Ok(None);
        }
        super::before_deadline(deadline, Ok(()))?;
        let scratch = environment
            .scratch
            .canonicalize()
            .context("source EOL scratch is unavailable")?;
        ensure!(
            !scratch.starts_with(root),
            "source EOL scratch is inside the inspected tree"
        );
        let homes = [
            std::env::var_os("ENGRAM_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".engram")),
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".engram")),
        ];
        for home in homes.into_iter().flatten() {
            let home = home.canonicalize().unwrap_or(home);
            ensure!(
                !scratch.starts_with(home),
                "source EOL scratch is inside the Engram home"
            );
        }
        self.materialize_in(&scratch, deadline).map(Some)
    }

    fn materialize_in(&self, scratch: &Path, deadline: Instant) -> Result<Fallback> {
        super::before_deadline(deadline, Ok(()))?;
        // tempfile creates a private directory (0700 on Unix); the file stays inside it.
        let directory = tempfile::Builder::new()
            .prefix("engram-source-eol-")
            .tempdir_in(scratch)?;
        let owned_path = directory.path().canonicalize()?;
        let _ = directory.keep();
        let fallback = Fallback {
            file: owned_path.join("config"),
            directory: owned_path,
        };
        let mut text = String::from("[core]\n");
        if let Some(value) = &self.autocrlf {
            writeln!(text, "autocrlf = {value}")?;
        }
        if let Some(value) = &self.eol {
            writeln!(text, "eol = {value}")?;
        }
        fs::write(&fallback.file, text).context("cannot materialize source EOL fallback")?;
        super::before_deadline(deadline, Ok(fallback))
    }
}

pub(super) struct Fallback {
    directory: PathBuf,
    pub(super) file: PathBuf,
}

impl Drop for Fallback {
    fn drop(&mut self) {
        // Private EOL-only scratch cleanup is best-effort; it never changes the verdict or output.
        let owned = fs::symlink_metadata(&self.directory)
            .is_ok_and(|entry| entry.is_dir() && !entry.is_symlink())
            && self
                .directory
                .canonicalize()
                .is_ok_and(|path| path == self.directory)
            && self.file.parent() == Some(self.directory.as_path());
        if owned {
            if fs::symlink_metadata(&self.file)
                .is_ok_and(|entry| entry.is_file() && !entry.is_symlink())
            {
                let _ = fs::remove_file(&self.file);
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn private_global_preserves_hardening_scope_and_local_precedence() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path().join("repo");
        fs::create_dir(&root).unwrap();
        let git = super::super::resolve_git(&root).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        super::super::query(
            &git,
            &root,
            &["init", "--quiet", "--template=", "--initial-branch=main"],
            false,
            deadline,
            None,
        )
        .unwrap();
        let policy = Policy {
            autocrlf: Some("true".to_owned()),
            eol: Some("crlf".to_owned()),
        };
        let fallback = policy.materialize_in(scratch.path(), deadline).unwrap();
        let run = |args: &[&str]| {
            super::super::query(&git, &root, args, false, deadline, Some(&fallback.file)).unwrap()
        };
        run(&["config", "core.safecrlf", "warn"]);
        run(&["config", "core.longpaths", "false"]);
        assert_eq!(run(&["config", "--get", "core.safecrlf"]), b"false\n");
        #[cfg(windows)]
        assert_eq!(run(&["config", "--get", "core.longpaths"]), b"true\n");
        // A real Git query sees these only at GLOBAL scope, never command scope.
        assert_eq!(
            run(&["config", "--null", "--show-scope", "--get", "core.eol"]),
            b"global\0crlf\0"
        );
        assert_eq!(
            run(&["config", "--null", "--show-scope", "--get", "core.autocrlf"]),
            b"global\0true\0"
        );
        run(&["config", "core.autocrlf", "false"]);
        assert_eq!(run(&["config", "--get", "core.autocrlf"]), b"false\n");

        // Every Rust test calling the owner gets these hermetic ambient inputs,
        // without changing process environment or touching real machine/temp files.
        let environment = Environment::for_read();
        assert!(environment.isolated);
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .canonicalize()
            .unwrap();
        assert!(
            environment
                .scratch
                .canonicalize()
                .unwrap()
                .starts_with(target)
        );
        assert_eq!(
            Policy::read(&git, &root, deadline, &environment).unwrap(),
            Policy::default()
        );
        assert!(
            Policy::default()
                .materialize(&root, deadline, &environment)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_dir(&environment.scratch).unwrap().count(), 0);
    }

    #[test]
    fn machine_scopes_are_normalized_without_promoting_local_policy() {
        let policy = Policy::parse(
            b"system\0core.autocrlf\nyes\0system\0core.eol\nlf\0global\0core.autocrlf\ninput\0global\0core.eol\ncrlf\0local\0core.autocrlf\nfalse\0worktree\0core.eol\nlf\0",
        ).unwrap();
        assert_eq!(policy.autocrlf.as_deref(), Some("input"));
        assert_eq!(policy.eol.as_deref(), Some("crlf"));
        for (value, expected) in [
            ("true", "true"),
            ("false", "false"),
            ("input", "input"),
            ("", "false"),
            ("OFF", "false"),
            ("-2", "true"),
        ] {
            let policy =
                Policy::parse(format!("global\0core.autocrlf\n{value}\0").as_bytes()).unwrap();
            assert_eq!(policy.autocrlf.as_deref(), Some(expected));
        }
        assert_eq!(
            Policy::parse(b"global\0core.autocrlf\0")
                .unwrap()
                .autocrlf
                .as_deref(),
            Some("true")
        );
        for (value, normalized) in [
            ("lf", "lf"),
            ("crlf", "crlf"),
            ("native", "native"),
            ("LF", "lf"),
            ("CRLF", "crlf"),
            ("Native", "native"),
        ] {
            assert_eq!(
                Policy::parse(format!("global\0core.eol\n{value}\0").as_bytes())
                    .unwrap()
                    .eol
                    .as_deref(),
                Some(normalized)
            );
        }
        assert_eq!(
            Policy::parse(b"local\0core.autocrlf\ntrue\0").unwrap(),
            Policy::default()
        );
        for invalid in [
            b"global\0core.eol\ninvalid\0".as_slice(),
            b"global\0core.autocrlf\ninvalid\0",
            b"global\0core.eol\0",
            b"global\0",
            b"global\0core.eol\nlf",
            b"unknown\0core.eol\nlf\0",
        ] {
            assert!(Policy::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn fallback_is_eol_only_and_removed_on_all_result_paths() {
        use crate::task_sync::git::QueryFailure;
        let scratch = crate::test_support::temp_home().unwrap();
        let policy = Policy {
            autocrlf: Some("input".to_owned()),
            eol: Some("lf".to_owned()),
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        for failure in [
            None,
            Some(QueryFailure::Deadline),
            Some(QueryFailure::OutputLimit),
        ] {
            let fallback = policy.materialize_in(scratch.path(), deadline).unwrap();
            let directory = fallback.directory.clone();
            assert_eq!(directory.canonicalize().unwrap(), directory);
            assert!(!crate::storage::store_path_text(&fallback.file).starts_with(r"\\?\"));
            assert_eq!(
                fs::read_to_string(&fallback.file).unwrap(),
                "[core]\nautocrlf = input\neol = lf\n"
            );
            let result: Result<()> = {
                let _owner = fallback;
                match failure {
                    None => Ok(()),
                    Some(error) => Err(error.into()),
                }
            };
            assert!(
                !directory.exists(),
                "scratch must disappear even when the query fails: {result:?}"
            );
        }
        let result: Result<()> = (|| {
            let _owner = policy.materialize_in(scratch.path(), deadline)?;
            bail!("later input refusal")
        })();
        assert!(result.is_err());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        assert!(
            policy
                .materialize_in(scratch.path(), Instant::now())
                .is_err()
        );
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        // Absence is preserved and creates no scratch, even with an unusable root.
        assert!(
            Policy::default()
                .materialize(Path::new("absent"), deadline, &Environment::for_read())
                .unwrap()
                .is_none()
        );
    }
}
