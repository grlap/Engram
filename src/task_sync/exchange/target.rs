//! Select a configured endpoint for literal Git operations.

use super::{Cancellation, RunError, RunLimits, command, execute};
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Transport {
    Https,
    Ssh,
    Local,
}

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
    pub(super) transport: Transport,
    pub(super) helpers: Vec<String>,
    pub(super) helper_config: Vec<u8>,
}

impl fmt::Debug for ExchangeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExchangeTarget")
            .field("transport", &self.transport)
            .field("helper_count", &self.helpers.len())
            .finish_non_exhaustive()
    }
}

impl ExchangeTarget {
    /// Compare this captured effective target without disclosing its URL/helper
    /// configuration. This fingerprint grants no publication authority.
    ///
    /// # Errors
    /// Refuses a canonical serialization failure.
    pub fn fingerprint(&self) -> Result<crate::ObjectId, crate::storage::StoreError> {
        Ok(crate::CanonicalObject::freeze(&(
            &self.selection.remote,
            &self.selection.reference,
            &self.url,
            &self.helpers,
            &self.helper_config,
        ))?
        .key()
        .clone())
    }

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
        let started = std::time::Instant::now();
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange capture");
        let limits = crate::task_sync::exchange::remaining(limits, started)?;
        Self::capture_remaining(selection, limits, cancel).await
    }

    // The public ingress admitted the full budget; internal calls spend its remainder.
    pub(crate) async fn capture_remaining(
        selection: &TargetSelection,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> Result<Self, RunError> {
        Self::capture_endpoint(selection, limits, cancel, false).await
    }

    /// The ordinary folder command also accepts admitted SSH and local paths.
    pub(crate) async fn capture_ordinary_remaining(
        selection: &TargetSelection,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> Result<Self, RunError> {
        Self::capture_endpoint(selection, limits, cancel, true).await
    }

    async fn capture_endpoint(
        selection: &TargetSelection,
        limits: RunLimits,
        cancel: &Cancellation,
        ordinary: bool,
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
        let (url, transport) = single_url(&fetch)?;
        if (!ordinary && transport != Transport::Https)
            || single_url(&push)? != (url.clone(), transport)
        {
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
        if transport != Transport::Https {
            return Ok(Self {
                selection: selection.clone(),
                url,
                transport,
                helpers: Vec::new(),
                helper_config: Vec::new(),
            });
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
            transport,
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

fn single_url(bytes: &[u8]) -> Result<(String, Transport), RunError> {
    let value = std::str::from_utf8(bytes).map_err(|_| RunError::UnsupportedTarget)?;
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(RunError::UnsupportedTarget);
    }
    let transport = if let Some(tail) = value.strip_prefix("https://") {
        validate_https(value, tail)?;
        Transport::Https
    } else if Path::new(value).is_absolute() && !value.contains("://") {
        Transport::Local
    } else if let Some(tail) = value.strip_prefix("ssh://") {
        let (host, path) = tail.split_once('/').ok_or(RunError::UnsupportedTarget)?;
        validate_ssh(host, path, true)?;
        Transport::Ssh
    } else {
        if value.contains("://") {
            return Err(RunError::UnsupportedTarget);
        }
        let (host, path) = value.split_once(':').ok_or(RunError::UnsupportedTarget)?;
        validate_ssh(host, path, false)?;
        Transport::Ssh
    };
    Ok((value.to_owned(), transport))
}

fn validate_https(value: &str, tail: &str) -> Result<(), RunError> {
    let (authority, path) = tail.split_once('/').unwrap_or((tail, ""));
    if authority.is_empty()
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
    Ok(())
}

fn validate_ssh(authority: &str, path: &str, allow_port: bool) -> Result<(), RunError> {
    let (user, host) = authority
        .split_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    let host_character = |byte: u8| byte.is_ascii_alphanumeric() || b".-_".contains(&byte);
    let host_valid = |value: &str| {
        !value.is_empty() && !value.starts_with('-') && value.bytes().all(host_character)
    };
    let (host, port) = if allow_port {
        host.rsplit_once(':')
            .map_or((host, None), |(host, port)| (host, Some(port)))
    } else {
        (host, None)
    };
    if !host_valid(host)
        || user.is_some_and(|user| !host_valid(user))
        || port.is_some_and(|port| !port.parse::<u16>().is_ok_and(|port| port != 0))
        || path.is_empty()
        || path.starts_with('-')
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte))
    {
        return Err(RunError::UnsupportedTarget);
    }
    Ok(())
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
    let values = bytes.split(|b| *b == 0).collect::<Vec<_>>();
    if values.len() > 64 {
        return Err(RunError::UnsupportedHelper);
    }
    // Git's empty helper entry discards all earlier entries, including helpers
    // inherited from another configuration scope. Validate only the helpers
    // that could actually be invoked by this effective selection.
    let start = values
        .iter()
        .rposition(|value| value.is_empty())
        .map_or(0, |i| i + 1);
    for value in &values[start..] {
        let helper = std::str::from_utf8(value).map_err(|_| RunError::UnsupportedHelper)?;
        if !admitted_helper(helper) || result.len() >= 8 {
            return Err(RunError::UnsupportedHelper);
        }
        result.push(helper.to_owned());
    }
    Ok(result)
}

/// The platform's own keychain helper, beside Git Credential Manager.
pub(super) const NATIVE_HELPER: Option<&str> = if cfg!(windows) {
    Some("wincred")
} else if cfg!(target_os = "macos") {
    Some("osxkeychain")
} else if cfg!(target_os = "linux") {
    Some("libsecret")
} else {
    None
};

/// Only non-interactive credential facilities, by exact name. The `cache`
/// helper starts a daemon in its own session, outside the runner's ownership,
/// and `store` keeps plaintext secrets; neither is admitted on any platform.
/// Git Credential Manager's own store setting is not inspected: a manager set
/// to a cache or plaintext store, or a keychain that raises a graphical unlock
/// prompt, is bounded only by the request deadline.
fn admitted_helper(helper: &str) -> bool {
    matches!(helper, "manager" | "manager-core") || Some(helper) == NATIVE_HELPER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_exchange_url_and_helper_policy_refuse_secrets_shells_and_ambiguity() {
        for url in [
            "ssh://-host/repo",
            "git@host:path;unexpected",
            "ext::git upload-pack",
            "ssh://user:password@host/repo",
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
            ("https://example.invalid/repo.git".into(), Transport::Https)
        );
        assert_eq!(
            single_url(b"https://example.invalid/repo%3Dname.git").unwrap(),
            (
                "https://example.invalid/repo%3Dname.git".into(),
                Transport::Https
            )
        );
        assert!(single_url(b"https://host/repo\xff").is_err());
        for url in [
            "git@example.invalid:project/repo.git",
            "ssh://git@example.invalid:2222/repo",
        ] {
            assert_eq!(
                single_url(url.as_bytes()).unwrap(),
                (url.into(), Transport::Ssh)
            );
        }
        let local = if cfg!(windows) {
            "C:/task data/remote.git"
        } else {
            "/task data/remote.git"
        };
        assert_eq!(
            single_url(local.as_bytes()).unwrap(),
            (local.into(), Transport::Local)
        );
        for helper in [
            b"!steal\0".as_slice(),
            b"manager --argument\0",
            b"custom\0",
            b"manager\n",
            b"manager\0!script\0",
            b"cache\0",
            b"store\0",
            b"cache --timeout=1\0",
        ] {
            assert_eq!(helper_names(helper), Err(RunError::UnsupportedHelper));
        }
        // Another platform's keychain helper is not admitted here.
        for foreign in ["wincred", "osxkeychain", "libsecret"]
            .into_iter()
            .filter(|helper| Some(*helper) != NATIVE_HELPER)
        {
            assert_eq!(
                helper_names(format!("{foreign}\0").as_bytes()),
                Err(RunError::UnsupportedHelper),
                "{foreign}"
            );
        }
        assert_eq!(helper_names(b"manager\0\0").unwrap(), Vec::<String>::new());
        assert_eq!(
            helper_names(b"osxkeychain\0\0").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            helper_names(b"!ignored-helper\0\0manager\0").unwrap(),
            ["manager"]
        );
        assert_eq!(
            helper_names(b"\0!active-helper\0"),
            Err(RunError::UnsupportedHelper)
        );
        if let Some(native) = NATIVE_HELPER {
            assert_eq!(
                helper_names(format!("manager\0\0{native}\0").as_bytes()).unwrap(),
                [native]
            );
            assert_eq!(
                helper_names(format!("manager\0manager-core\0{native}\0").as_bytes())
                    .unwrap()
                    .len(),
                3
            );
        }
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
