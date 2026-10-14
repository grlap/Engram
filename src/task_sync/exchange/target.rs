//! Capture and recheck effective configuration, without inferring Git authority.

use super::{Cancellation, RunError, RunLimits, command, execute};
use std::{fmt, path::PathBuf, time::Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetSelection {
    pub repository: PathBuf,
    pub remote: String,
    pub reference: String,
}

/// Fields stay private so a caller cannot fabricate an admitted URL/helper.
#[derive(Clone, Eq, PartialEq)]
pub struct ExchangeTarget {
    pub(super) selection: TargetSelection,
    pub(super) url: String,
    pub(super) helpers: Vec<String>,
    pub(super) helper_config: Vec<u8>,
}

impl fmt::Debug for ExchangeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExchangeTarget")
            .field("transport", &"https")
            .field("helper_count", &self.helpers.len())
            .finish_non_exhaustive()
    }
}

impl ExchangeTarget {
    /// Capture one configured HTTPS destination used for both fetch and push.
    /// This is configuration observation, not network, import or publication.
    ///
    /// # Errors
    /// Refuses unsafe paths, ambiguous targets, unsupported helpers and failures.
    pub async fn capture(
        selection: &TargetSelection,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> Result<Self, RunError> {
        limits.validate()?;
        let started = Instant::now();
        validate_selection(selection)?;
        require_git_version(
            &query(
                selection,
                &["--version"],
                super::remaining(limits, started)?,
                cancel,
            )
            .await?,
        )?;
        let fetch = query(
            selection,
            &["remote", "get-url", "--all", "--", &selection.remote],
            super::remaining(limits, started)?,
            cancel,
        )
        .await?;
        let push = query(
            selection,
            &[
                "remote",
                "get-url",
                "--push",
                "--all",
                "--",
                &selection.remote,
            ],
            super::remaining(limits, started)?,
            cancel,
        )
        .await?;
        let url = single_url(&fetch)?;
        if single_url(&push)? != url {
            return Err(RunError::UnsupportedTarget);
        }
        // Reusing a get-url result as an explicit URL can apply insteadOf a
        // second time (and pushInsteadOf differs). Refuse that configuration
        // rather than connect to a destination different from the pinned one.
        let rewrites = command::local(
            &selection.repository,
            &["config", "--name-only", "--get-regexp", "^url\\."],
        );
        if !super::execute_config(rewrites, super::remaining(limits, started)?, cancel)
            .await?
            .is_empty()
        {
            return Err(RunError::UnsupportedRewrite);
        }
        // These settings introduce another fetch source or server-side push
        // options beyond the typed operation. They are not admitted implicitly.
        for key in ["fetch.bundleURI", "push.pushOption"] {
            let config = command::local(
                &selection.repository,
                &["config", "--null", "--get-all", key],
            );
            if !super::execute_config(config, super::remaining(limits, started)?, cancel)
                .await?
                .is_empty()
            {
                return Err(RunError::UnsupportedConfiguration);
            }
        }
        let mut config = command::local(
            &selection.repository,
            &[
                "config",
                "--null",
                "--show-origin",
                "--get-regexp",
                "^credential\\.(.+\\.)?helper$",
            ],
        );
        // Missing helper is valid for anonymous HTTPS; Git may never prompt.
        config.stdout(std::process::Stdio::piped());
        let output =
            super::execute_config(config, super::remaining(limits, started)?, cancel).await?;
        let helpers = configured_helpers(&output)?;
        Ok(Self {
            selection: selection.clone(),
            url,
            helpers,
            helper_config: output,
        })
    }
}

async fn query(
    selection: &TargetSelection,
    args: &[&str],
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<Vec<u8>, RunError> {
    Ok(
        execute(command::local(&selection.repository, args), limits, cancel)
            .await?
            .stdout,
    )
}

fn validate_selection(selection: &TargetSelection) -> Result<(), RunError> {
    if !selection.repository.is_absolute()
        || !selection.repository.is_dir()
        || selection
            .repository
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || !selection.reference.starts_with("refs/heads/")
        || !command::reference(&selection.reference)
        || selection.remote.is_empty()
        || selection.remote.len() > 128
        || selection.remote.starts_with('-')
        || !selection
            .remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(RunError::InvalidSelection);
    }
    for path in selection.repository.ancestors() {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| RunError::InvalidSelection)?;
        if metadata.file_type().is_symlink() {
            return Err(RunError::InvalidSelection);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(RunError::InvalidSelection);
            }
        }
    }
    Ok(())
}

fn single_url(bytes: &[u8]) -> Result<String, RunError> {
    let value = std::str::from_utf8(bytes).map_err(|_| RunError::UnsupportedTarget)?;
    let value = value.strip_suffix('\n').unwrap_or(value);
    let tail = value
        .strip_prefix("https://")
        .ok_or(RunError::UnsupportedTarget)?;
    let (authority, path) = tail.split_once('/').unwrap_or((tail, ""));
    if value.len() > 4096
        || authority.is_empty()
        || value.contains(['@', '?', '#', '\\'])
        || !value.bytes().all(|b| b.is_ascii_graphic())
        || !authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~/%".contains(&b))
    {
        return Err(RunError::UnsupportedTarget);
    }
    Ok(value.to_owned())
}

fn require_git_version(bytes: &[u8]) -> Result<(), RunError> {
    let version = std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().strip_prefix("git version "));
    let mut parts = version.unwrap_or_default().split('.');
    let major = parts.next().and_then(|s| s.parse::<u32>().ok());
    let minor = parts.next().and_then(|s| s.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) if (major, minor) >= (2, 31) => Ok(()),
        (Some(major), Some(minor)) => Err(RunError::UnsupportedGitVersion {
            found: format!("{major}.{minor}"),
        }),
        _ => Err(RunError::UnsupportedGitVersion {
            found: "an unrecognized version".into(),
        }),
    }
}

fn configured_helpers(bytes: &[u8]) -> Result<Vec<String>, RunError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let entries = bytes
        .strip_suffix(&[0])
        .ok_or(RunError::UnsupportedHelper)?;
    let mut fields = entries.split(|b| *b == 0);
    let mut values = Vec::new();
    let mut count = 0;
    while let Some(origin) = fields.next() {
        let entry = fields.next().ok_or(RunError::UnsupportedHelper)?;
        count += 1;
        if count > 64 {
            return Err(RunError::UnsupportedHelper);
        }
        let split = entry
            .iter()
            .position(|b| *b == b'\n')
            .ok_or(RunError::UnsupportedHelper)?;
        let (key, value) = entry.split_at(split);
        if key != b"credential.helper" {
            // Never include the subsection: it may contain credentials.
            let origin = if origin.starts_with(b"file:") {
                String::from_utf8_lossy(origin)
                    .chars()
                    .take(256)
                    .flat_map(char::escape_default)
                    .collect()
            } else {
                "non-file configuration".into()
            };
            return Err(RunError::UnsupportedScopedHelper { origin });
        }
        values.extend_from_slice(&value[1..]);
        values.push(0);
    }
    helper_names(&values)
}

fn helper_names(bytes: &[u8]) -> Result<Vec<String>, RunError> {
    let mut result = Vec::new();
    if bytes.is_empty() {
        return Ok(result);
    }
    let bytes = bytes
        .strip_suffix(&[0])
        .ok_or(RunError::UnsupportedHelper)?;
    for (index, value) in bytes.split(|b| *b == 0).enumerate() {
        if index >= 64 {
            return Err(RunError::UnsupportedHelper);
        }
        if value.is_empty() {
            result.clear();
            continue;
        }
        let helper = std::str::from_utf8(value).map_err(|_| RunError::UnsupportedHelper)?;
        if !matches!(helper, "manager" | "manager-core" | "wincred") || result.len() >= 8 {
            return Err(RunError::UnsupportedHelper);
        }
        result.push(helper.to_owned());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_exchange_url_and_helper_policy_refuse_secrets_shells_and_ambiguity() {
        for url in [
            "git@github.com:project/repo.git\n",
            "ssh://host/repo",
            "file:///repo",
            "https://user:token@host/repo",
            "https://host/repo?token=secret",
            "https://host/repo\nhttps://other/repo\n",
            "https:///missing",
            "https://host/repo\r\n",
            "https://user=token@host/repo",
            "https://ho=st/repo",
            "https://host:4=43/repo",
            "https://host/re=po",
            "https://host/repo?q=x",
            "https://host/repo#x=y",
            "https://host/.helper=!sentinel;",
            "https://host/repo!",
            "https://host/repo;",
            "https://host/re po",
            "https://host/re\tpo",
            "https://host/re\0po",
            "https://host/répo",
        ] {
            assert_eq!(single_url(url.as_bytes()), Err(RunError::UnsupportedTarget));
        }
        assert_eq!(
            single_url(b"https://example.invalid/repo.git\n").unwrap(),
            "https://example.invalid/repo.git"
        );
        assert_eq!(
            single_url(b"https://example.invalid/repo%3Dname.git").unwrap(),
            "https://example.invalid/repo%3Dname.git"
        );
        assert!(single_url(b"https://host/repo\xff").is_err());
        for helper in [
            b"!steal\0".as_slice(),
            b"manager --argument\0",
            b"custom\0",
            b"manager\n",
            b"manager\0!script\0",
        ] {
            assert_eq!(helper_names(helper), Err(RunError::UnsupportedHelper));
        }
        assert_eq!(helper_names(b"manager\0\0").unwrap(), Vec::<String>::new());
        assert_eq!(helper_names(b"manager\0\0wincred\0").unwrap(), ["wincred"]);
        assert_eq!(
            helper_names(b"manager\0manager-core\0wincred\0")
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn task_exchange_git_floor_accepts_vendor_suffixes_and_refuses_unknown_versions() {
        for version in [
            b"git version 2.31.0\n".as_slice(),
            b"git version 2.45.1.windows.1\n",
            b"git version 3.0.0\n",
        ] {
            require_git_version(version).unwrap();
        }
        for version in [
            b"git version 2.30.9\n".as_slice(),
            b"git version 1.9.5\n",
            b"unexpected fake-token\n",
        ] {
            let error = require_git_version(version).unwrap_err().to_string();
            assert!(error.contains("Git >= 2.31"));
            assert!(!error.contains("fake-token"));
        }
    }
}
