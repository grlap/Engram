//! Local deltas from Git's reported paths, without a repository census.

use super::{Child, Query, before_deadline, status_entries, submodule_states, submodule_status};
use crate::source::{Change, Entry, MAX_JSON_BYTES, Mode, SourceId, Submodule};
use anyhow::{Context, Result, bail, ensure};
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
    time::{Instant, SystemTime},
};

const STATUS: &[&str] = &[
    "status",
    "--porcelain=v1",
    "--untracked-files=all",
    "--ignore-submodules=none",
    "-z",
];

pub(super) fn capture(
    query: &Query<'_>,
    root: &Path,
    commit: &str,
    tree: &str,
    status: &[u8],
    children: &[Child],
) -> Result<SourceId> {
    let deadline = query.deadline;
    let mut children = children.to_vec();
    children.sort_by(|a, b| a.path.cmp(&b.path));
    ensure!(
        children.windows(2).all(|pair| pair[0].path != pair[1].path),
        "Git returned duplicate submodule paths"
    );
    let mut observations = BTreeMap::new();
    observations.insert(String::new(), status_entries(status)?);
    let mut submodules = Vec::new();
    let mut snapshots = Vec::new();
    for child in &children {
        before_deadline(deadline, Ok(()))?;
        validate_path(&child.path)?;
        let parent = children
            .iter()
            .filter(|parent| {
                child
                    .path
                    .strip_prefix(&parent.path)
                    .is_some_and(|tail| tail.starts_with('/'))
            })
            .max_by_key(|parent| parent.path.len())
            .map_or("", |parent| parent.path.as_str());
        let relative = relative_child(parent, &child.path);
        let dirty = observations.get(parent).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.path == relative || entry.origin.as_deref() == Some(relative))
        });
        let changes = if dirty || child.off_pin {
            let child_root = root.join(&child.path);
            ensure!(
                fs::symlink_metadata(&child_root)?.file_type().is_dir(),
                "submodule '{}': unavailable directory association",
                child.path
            );
            let entries = status_entries(&query.run(&child_root, STATUS, None)?)?;
            let local = changes(query, &child_root, &child.path, &entries, &children)?;
            snapshots.push((child_root, local.paths));
            observations.insert(child.path.clone(), entries);
            local.changes
        } else {
            Vec::new()
        };
        submodules.push(Submodule {
            path: child.path.clone(),
            commit: child.commit.clone(),
            changes,
        });
    }
    let root_entries = observations.get("").context("missing root status")?;
    let root_delta = changes(query, root, "", root_entries, &children)?;
    snapshots.push((root.to_owned(), root_delta.paths));
    // A second status detects paths appearing/disappearing during dirty capture.
    // This remains endpoint evidence, not a claim of transient continuity.
    if !status.is_empty() || children.iter().any(|child| child.off_pin) {
        for (path, before) in &observations {
            let after = status_entries(&query.run(&root.join(path), STATUS, None)?)?;
            ensure!(
                &after == before,
                "source status changed during capture in '{path}'"
            );
        }
        let mut after =
            submodule_states(&submodule_status(|args| query.run(root, args, None))?, true)?;
        after.sort_by(|a, b| a.path.cmp(&b.path));
        ensure!(
            after == children,
            "submodule HEAD or association changed during capture"
        );
    }
    for (repository, paths) in &snapshots {
        unchanged(repository, paths, deadline)?;
    }
    before_deadline(
        deadline,
        Ok(SourceId {
            commit: commit.to_owned(),
            tree: tree.to_owned(),
            submodules,
            changes: root_delta.changes,
        }),
    )
}

fn relative_child<'a>(parent: &str, child: &'a str) -> &'a str {
    if parent.is_empty() {
        child
    } else {
        &child[parent.len() + 1..]
    }
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\n', '\r'])
            && Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "changed path is unavailable for line-based Git hashing: {path:?}"
    );
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    mode: Mode,
    link: Option<Vec<u8>>,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
}

fn stamp(path: &Path) -> Result<Option<Stamp>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read changed path metadata"),
    };
    let (mode, link) = if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = target
            .to_str()
            .context("symlink text is not UTF-8")?
            .as_bytes()
            .to_vec();
        ensure!(
            bytes.len() <= MAX_JSON_BYTES,
            "symlink text exceeds source bound"
        );
        (Mode::Symlink, Some(bytes))
    } else if metadata.is_file() {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                Mode::Regular
            } else {
                Mode::Executable
            }
        };
        #[cfg(not(unix))]
        let mode = Mode::Regular;
        (mode, None)
    } else {
        bail!(
            "changed path has an unsupported filesystem type: {}",
            path.display()
        );
    };
    Ok(Some(Stamp {
        len: metadata.len(),
        modified: metadata.modified()?,
        created: metadata.created().ok(),
        mode,
        link,
        #[cfg(unix)]
        inode: {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    }))
}

fn link_blob(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", bytes.len()).as_bytes());
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

struct LocalDelta {
    changes: Vec<Change>,
    paths: BTreeMap<String, Option<Stamp>>,
}

fn unchanged(
    root: &Path,
    paths: &BTreeMap<String, Option<Stamp>>,
    deadline: Instant,
) -> Result<()> {
    for (path, before) in paths {
        before_deadline(deadline, Ok(()))?;
        ensure!(
            &stamp(&root.join(path))? == before,
            "changed path drifted during capture: {path}"
        );
    }
    Ok(())
}

fn changes(
    query: &Query<'_>,
    root: &Path,
    repository: &str,
    entries: &[Entry],
    children: &[Child],
) -> Result<LocalDelta> {
    let deadline = query.deadline;
    let mut paths = BTreeMap::new();
    let mut records = Vec::new();
    for entry in entries {
        before_deadline(deadline, Ok(()))?;
        validate_path(&entry.path)?;
        if let Some(origin) = &entry.origin {
            validate_path(origin)?;
        }
        let full = if repository.is_empty() {
            entry.path.clone()
        } else {
            format!("{repository}/{}", entry.path)
        };
        if children.iter().any(|child| child.path == full) {
            continue;
        }
        if !paths.contains_key(&entry.path) {
            paths.insert(entry.path.clone(), stamp(&root.join(&entry.path))?);
        }
        if paths[&entry.path].is_none() {
            ensure!(
                entry.status.contains('D'),
                "changed path disappeared during capture: {}",
                entry.path
            );
        }
        records.push(entry);
    }
    let mut input = Vec::new();
    let mut names = Vec::new();
    let mut blobs = BTreeMap::new();
    for (path, before) in &paths {
        before_deadline(deadline, Ok(()))?;
        if let Some(before) = before {
            if let Some(link) = &before.link {
                blobs.insert(path.clone(), link_blob(link));
            } else {
                // Leading '.' prevents Git's leading-quote pathname decoder.
                input.extend_from_slice(b"./");
                input.extend_from_slice(path.as_bytes());
                input.push(b'\n');
                if input.len() > MAX_JSON_BYTES {
                    return Err(crate::task_sync::git::QueryFailure::OutputLimit.into());
                }
                names.push(path);
            }
        }
    }
    if !names.is_empty() {
        let bytes = query.run(root, &["hash-object", "--stdin-paths"], Some(&input))?;
        let text = std::str::from_utf8(&bytes).context("Git returned non-UTF-8 blob ids")?;
        let ids: Vec<_> = text.lines().collect();
        ensure!(
            ids.len() == names.len(),
            "Git returned an incomplete blob-id batch"
        );
        for (name, id) in names.into_iter().zip(ids) {
            ensure!(
                id.len() == 40
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "Git returned a malformed blob id"
            );
            blobs.insert(name.clone(), id.to_owned());
        }
    }
    unchanged(root, &paths, deadline)?;
    let mut result: Vec<_> = records
        .into_iter()
        .map(|entry| Change {
            path: entry.path.clone(),
            status: entry.status.clone(),
            origin: entry.origin.clone(),
            blob: blobs.get(&entry.path).cloned(),
            mode: paths[&entry.path].as_ref().map(|stamp| stamp.mode),
        })
        .collect();
    result.sort_by(|a, b| (&a.path, &a.status, &a.origin).cmp(&(&b.path, &b.status, &b.origin)));
    before_deadline(
        deadline,
        Ok(LocalDelta {
            changes: result,
            paths,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_path_metadata_and_expired_capture_refuse_without_partial_state() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        let path = root.join("changed");
        fs::write(&path, "before").unwrap();
        let before = BTreeMap::from([("changed".to_owned(), stamp(&path).unwrap())]);
        let deadline = Instant::now() + crate::source::MAX_BUDGET;
        unchanged(root, &before, deadline).unwrap();
        fs::write(&path, "different content and size").unwrap();
        assert!(
            unchanged(root, &before, deadline)
                .unwrap_err()
                .to_string()
                .contains("drifted")
        );
        let expired = Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            super::super::refusal_code(&unchanged(root, &before, expired).unwrap_err()),
            crate::source::RefusalCode::Deadline
        );
        fs::remove_file(&path).unwrap();
        assert!(stamp(&path).unwrap().is_none());
        assert!(unchanged(root, &before, deadline).is_err());
    }

    #[test]
    fn link_text_oid_matches_git_without_reading_the_target() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        let git = super::super::resolve_git(root).unwrap();
        for text in [
            b"missing target".as_slice(),
            b"../ignored/file",
            b"unicode \xe2\x98\x83",
        ] {
            let bytes = Query {
                git: &git,
                global: None,
                deadline: Instant::now() + crate::source::MAX_BUDGET,
            }
            .run(root, &["hash-object", "--stdin"], Some(text))
            .unwrap();
            assert_eq!(link_blob(text), std::str::from_utf8(&bytes).unwrap().trim());
        }
    }
}
