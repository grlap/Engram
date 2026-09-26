//! Stable host-local project store discovery and project-root filesystem
//! identity probing.

use std::{
    fs, io,
    path::{Path, PathBuf},
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
    #[error(
        "could not probe the filesystem identity of {0}: the project file name is not Unicode text or has no ASCII letter to look up under the opposite case"
    )]
    NoLetterToSwap(PathBuf),
    #[error(
        "could not probe the filesystem identity of {0}: the project file was missing, changed while it was probed, or is not listed in its directory under this name up to ASCII case"
    )]
    Unsettled(PathBuf),
    #[error("could not probe the filesystem identity of {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Probes whether the filesystem holding the project root, the directory of
/// `project_file`, folds case. It looks the project file up again under its
/// name with every ASCII letter's case inverted. Nothing found there proves
/// the directory tells the spellings apart. When something is found, the
/// directory's listing says what: one entry answering to the name, ignoring
/// ASCII case, means both spellings reached it and the directory folds; two
/// or more mean it holds both spellings as separate entries, which only a
/// case-sensitive directory can. Comparing the two lookups' file identities
/// instead would mislead: hard links, or links to one target, share an
/// identity across distinct entries, and some network and user-space
/// filesystems give one entry a different identity per spelling.
///
/// It writes nothing, so file watchers see no change and a read-only checkout
/// probes too. The project file is the entry tested because the caller has
/// just read it and it does not churn, unlike lock or swap files. Windows
/// alias rules follow the running target, because they are operating-system
/// semantics rather than a property of one filesystem. A probe that cannot
/// decide is an error, so a caller that cannot prove identity fails closed
/// instead of guessing; the host can then supply the policy itself.
///
/// # Errors
///
/// Returns [`HostPathProbeError`] when the project file name is not Unicode
/// text or has no ASCII letter to invert, when the project file is missing or
/// changes while it is probed, when its directory does not list it under the
/// given name up to ASCII case (the name reached it through a wider alias,
/// such as a non-ASCII case variant or a short 8.3 name), or when a lookup or
/// the listing fails.
pub fn probe_host_path_policy(project_file: &Path) -> Result<HostPathPolicy, HostPathProbeError> {
    let unprobeable = || HostPathProbeError::NoLetterToSwap(project_file.to_path_buf());
    let name = project_file.file_name().ok_or_else(unprobeable)?;
    let swapped = name
        .to_str()
        .and_then(swap_ascii_case)
        .ok_or_else(unprobeable)?;
    let mut lookups = ProjectFileLookups {
        given: project_file,
        swapped: project_file.with_file_name(swapped),
        root: project_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
        name,
    };
    let decided = decide_case_fold(&mut lookups).map_err(|source| HostPathProbeError::Io {
        path: project_file.to_path_buf(),
        source,
    })?;
    let case_fold_paths =
        decided.ok_or_else(|| HostPathProbeError::Unsettled(project_file.to_path_buf()))?;
    Ok(HostPathPolicy {
        case_fold_paths,
        windows_alias_rules: cfg!(target_os = "windows"),
    })
}

/// Which spelling of the project file name a lookup uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Spelling {
    Given,
    Swapped,
}

/// What the case decision observes: the filesystem in production, a script
/// in tests.
trait Observe {
    type Identity: PartialEq;
    /// The entry a spelling names, or `None` when nothing is there.
    fn identify(&mut self, spelling: Spelling) -> io::Result<Option<Self::Identity>>;
    /// How many entries of the directory have the given name, ignoring ASCII
    /// case.
    fn entries_named(&mut self) -> io::Result<usize>;
}

/// Decides case folding: `Some(true)` folds, `Some(false)` tells the
/// spellings apart, `None` cannot tell.
///
/// The swapped spelling finding nothing proves the directory case-sensitive.
/// When it finds something, the listing decides, as
/// [`probe_host_path_policy`] explains. Each spelling is then looked up again
/// and must name what it named before: a rename to the swapped spelling, or a
/// delete and re-create, while the probe runs could otherwise pass for either
/// answer, and is `None` instead.
fn decide_case_fold(observe: &mut impl Observe) -> io::Result<Option<bool>> {
    let Some(given) = observe.identify(Spelling::Given)? else {
        return Ok(None);
    };
    let swapped = observe.identify(Spelling::Swapped)?;
    let entries = match swapped {
        Some(_) => Some(observe.entries_named()?),
        None => None,
    };
    if observe.identify(Spelling::Given)?.as_ref() != Some(&given) {
        return Ok(None);
    }
    if swapped.is_some() && observe.identify(Spelling::Swapped)? != swapped {
        return Ok(None);
    }
    Ok(match entries {
        // Only one entry answers to the name, so both spellings reached it.
        Some(1) => Some(true),
        // The name is not listed: the entry changed under the probe.
        Some(0) => None,
        // Nothing under the swapped spelling, or both spellings listed.
        None | Some(_) => Some(false),
    })
}

/// The project file's two spellings and the directory that holds it.
struct ProjectFileLookups<'a> {
    given: &'a Path,
    swapped: PathBuf,
    root: &'a Path,
    name: &'a std::ffi::OsStr,
}

impl Observe for ProjectFileLookups<'_> {
    type Identity = EntryIdentity;

    fn identify(&mut self, spelling: Spelling) -> io::Result<Option<EntryIdentity>> {
        let path = match spelling {
            Spelling::Given => self.given,
            Spelling::Swapped => &self.swapped,
        };
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(entry_identity(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn entries_named(&mut self) -> io::Result<usize> {
        let mut entries = 0;
        for entry in fs::read_dir(self.root)? {
            if entry?.file_name().eq_ignore_ascii_case(self.name) {
                entries += 1;
            }
        }
        Ok(entries)
    }
}

/// Device and inode, without following a final symbolic link. The probe
/// compares an identity only with the same spelling's earlier one.
#[cfg(unix)]
type EntryIdentity = (u64, u64);

#[cfg(unix)]
fn entry_identity(metadata: &fs::Metadata) -> EntryIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// Creation and write times, size and attributes, without following a final
/// symbolic link. Stable Rust exposes no file index on Windows, so a delete
/// and re-create that keeps all four between two lookups is not seen; the
/// project file does not churn while a command starts.
#[cfg(windows)]
type EntryIdentity = (u64, u64, u64, u32);

#[cfg(windows)]
fn entry_identity(metadata: &fs::Metadata) -> EntryIdentity {
    use std::os::windows::fs::MetadataExt;
    (
        metadata.creation_time(),
        metadata.last_write_time(),
        metadata.file_size(),
        metadata.file_attributes(),
    )
}

/// `name` with every ASCII letter's case inverted, or `None` when it has no
/// ASCII letter and so no other spelling to look up.
fn swap_ascii_case(name: &str) -> Option<String> {
    name.bytes()
        .any(|byte| byte.is_ascii_alphabetic())
        .then(|| {
            name.chars()
                .map(|character| {
                    if character.is_ascii_uppercase() {
                        character.to_ascii_lowercase()
                    } else {
                        character.to_ascii_uppercase()
                    }
                })
                .collect()
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

    /// A plain temporary directory follows the platform's usual semantics; the
    /// probe reads the filesystem rather than assuming them.
    fn assert_platform_policy(policy: HostPathPolicy) {
        assert_eq!(
            policy.case_fold_paths,
            cfg!(any(target_os = "windows", target_os = "macos"))
        );
        assert_eq!(policy.windows_alias_rules, cfg!(target_os = "windows"));
    }

    fn listing(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .expect("list root")
            .map(|entry| entry.expect("entry").file_name().into_string().unwrap())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn probe_looks_up_the_project_file_and_writes_nothing() {
        let directory = crate::test_support::temp_home().expect("temp directory");
        let project_file = directory.path().join(".engram-project");
        fs::write(&project_file, "project").unwrap();
        let policy = probe_host_path_policy(&project_file).expect("probe the root");
        assert_platform_policy(policy);
        assert_eq!(listing(directory.path()), [".engram-project"]);
    }

    /// Probes a case-sensitive directory holding the project file and, as a
    /// separate entry under the swapped spelling, what `second` makes.
    #[cfg(target_os = "linux")]
    fn case_fold_with_second_spelling(second: impl FnOnce(&Path, &Path) -> io::Result<()>) -> bool {
        let directory = crate::test_support::temp_home().expect("temp directory");
        let project_file = directory.path().join(".engram-project");
        fs::write(&project_file, "project").unwrap();
        second(&project_file, &directory.path().join(".ENGRAM-PROJECT")).unwrap();
        probe_host_path_policy(&project_file)
            .expect("probe the root")
            .case_fold_paths
    }

    /// Only a case-sensitive directory can hold both spellings as two
    /// entries, whether the second is its own file, a hard link to the same
    /// inode, or a symbolic link to the project file.
    #[cfg(target_os = "linux")]
    #[test]
    fn both_spellings_as_separate_entries_are_case_sensitive() {
        assert!(!case_fold_with_second_spelling(|_, second| fs::write(
            second, "other"
        )));
        assert!(!case_fold_with_second_spelling(|first, second| {
            fs::hard_link(first, second)
        }));
        assert!(!case_fold_with_second_spelling(|first, second| {
            std::os::unix::fs::symlink(first, second)
        }));
    }

    #[test]
    fn a_project_file_name_without_an_ascii_letter_is_unresolved_and_writes_nothing() {
        let directory = crate::test_support::temp_home().expect("temp directory");
        let project_file = directory.path().join("2026");
        fs::write(&project_file, "project").unwrap();
        assert!(matches!(
            probe_host_path_policy(&project_file),
            Err(HostPathProbeError::NoLetterToSwap(_))
        ));
        assert_eq!(listing(directory.path()), ["2026"]);
    }

    #[test]
    fn a_missing_project_file_is_unresolved() {
        let directory = crate::test_support::temp_home().expect("temp directory");
        assert!(matches!(
            probe_host_path_policy(&directory.path().join(".engram-project")),
            Err(HostPathProbeError::Unsettled(_))
        ));
        assert!(listing(directory.path()).is_empty());
    }

    /// A scripted filesystem: lookups answer in turn from `identities`, and
    /// the listing counts `entries_named` entries.
    struct Script {
        identities: std::collections::VecDeque<Option<u32>>,
        entries_named: usize,
        looked_up: Vec<Spelling>,
        listed: usize,
    }

    impl Observe for Script {
        type Identity = u32;

        fn identify(&mut self, spelling: Spelling) -> io::Result<Option<u32>> {
            self.looked_up.push(spelling);
            Ok(self
                .identities
                .pop_front()
                .expect("no more lookups expected"))
        }

        fn entries_named(&mut self) -> io::Result<usize> {
            self.listed += 1;
            Ok(self.entries_named)
        }
    }

    fn decide(identities: &[Option<u32>], entries_named: usize) -> (Option<bool>, Script) {
        let mut script = Script {
            identities: identities.iter().copied().collect(),
            entries_named,
            looked_up: Vec::new(),
            listed: 0,
        };
        let decided = decide_case_fold(&mut script).expect("no lookup fails");
        (decided, script)
    }

    use Spelling::{Given, Swapped};

    #[test]
    fn one_listed_entry_answering_to_both_spellings_folds() {
        let (decided, script) = decide(&[Some(1), Some(1), Some(1), Some(1)], 1);
        assert_eq!(decided, Some(true));
        assert_eq!(script.looked_up, [Given, Swapped, Given, Swapped]);
        assert_eq!(script.listed, 1);
    }

    #[test]
    fn a_different_identity_per_spelling_still_folds_when_one_entry_is_listed() {
        // Some network and user-space filesystems give each spelling of one
        // entry its own inode; the listing, not the identities, decides.
        assert_eq!(
            decide(&[Some(1), Some(7), Some(1), Some(7)], 1).0,
            Some(true)
        );
    }

    #[test]
    fn two_listed_entries_are_case_sensitive_even_with_one_identity() {
        // A hard link, or a link to one target, under the other spelling.
        assert_eq!(
            decide(&[Some(1), Some(1), Some(1), Some(1)], 2).0,
            Some(false)
        );
    }

    #[test]
    fn nothing_under_the_swapped_name_is_case_sensitive_without_a_listing() {
        let (decided, script) = decide(&[Some(1), None, Some(1)], 0);
        assert_eq!(decided, Some(false));
        assert_eq!(script.looked_up, [Given, Swapped, Given]);
        assert_eq!(script.listed, 0);
    }

    #[test]
    fn an_entry_missing_from_the_listing_decides_nothing() {
        assert_eq!(decide(&[Some(1), Some(1), Some(1), Some(1)], 0).0, None);
    }

    #[test]
    fn a_missing_project_file_decides_nothing_and_looks_no_further() {
        let (decided, script) = decide(&[None], 1);
        assert_eq!(decided, None);
        assert_eq!(script.looked_up, [Given]);
    }

    #[test]
    fn a_rename_to_the_swapped_spelling_mid_probe_decides_nothing() {
        // Case-sensitive directory: the entry is renamed to the swapped
        // spelling after the first lookup, so the swapped lookup reaches it
        // and the listing holds one entry.
        assert_eq!(decide(&[Some(1), Some(1), None], 1).0, None);
    }

    #[test]
    fn a_delete_and_re_create_mid_probe_decides_nothing() {
        // Case-folding directory: the entry is removed before the swapped
        // lookup and re-created before the last one, so the swapped lookup
        // misses and would otherwise read as case-sensitive.
        assert_eq!(decide(&[Some(1), None, Some(2)], 1).0, None);
    }

    #[test]
    fn a_swapped_entry_replaced_mid_probe_decides_nothing() {
        assert_eq!(decide(&[Some(1), Some(5), Some(1), Some(6)], 1).0, None);
    }

    #[test]
    fn a_failed_lookup_is_an_error_not_a_guess() {
        struct Failing;
        impl Observe for Failing {
            type Identity = u32;
            fn identify(&mut self, _: Spelling) -> io::Result<Option<u32>> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn entries_named(&mut self) -> io::Result<usize> {
                unreachable!("the first lookup fails")
            }
        }
        let decided = decide_case_fold(&mut Failing);
        assert_eq!(decided.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn only_ascii_letters_swap_case() {
        assert_eq!(swap_ascii_case("Foo.Bar-9").as_deref(), Some("fOO.bAR-9"));
        assert_eq!(swap_ascii_case("Łódź").as_deref(), Some("ŁóDź"));
        assert_eq!(swap_ascii_case("2026_"), None);
        assert_eq!(swap_ascii_case("żółć"), None);
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
