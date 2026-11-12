//! Admission binds the persistent data worktree to the selected code project.

use super::{Assertions, Basis, policy};
use crate::{ProjectId, task_sync::data_worktree::DataWorktree};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
const GIT_BOUND: Duration = Duration::from_secs(30);

pub(super) fn inspect(
    marker: &Path,
    project: &ProjectId,
    root: &Path,
    branch: &str,
    assertions: Assertions,
) -> Result<Basis> {
    let marker_path = std::path::absolute(marker)?;
    ordinary_ancestors(&marker_path)?;
    let marker_path = fs::canonicalize(marker_path)?;
    let marker_bytes = read_optional(&marker_path)?.context("project marker missing")?;
    ensure!(
        marker_bytes.len() <= 4096 && std::str::from_utf8(&marker_bytes)?.trim() == project.0,
        "selected project marker changed or differs from project"
    );
    let selected = DataWorktree::observe(
        marker_path.parent().context("marker has no directory")?,
        root,
        branch,
    )?;
    let root = selected.checkout;
    let admin = selected.git_directory;
    ordinary_ancestors(&root.join(".engram"))?;
    match fs::symlink_metadata(root.join(".engram")) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "reserved shared path is occupied: .engram is not a directory; relocate explicitly before preparation"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    for relative in [
        ".engram/project.json",
        ".engram/tasks",
        ".engram/.gitignore",
        ".engram/.gitattributes",
    ] {
        ordinary_ancestors(&root.join(relative))?;
        ensure!(
            fs::symlink_metadata(root.join(relative))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "reserved shared path is occupied: {relative}; relocate explicitly before preparation"
        );
    }
    ensure!(
        git(&root, &["ls-files", "-z"])?.is_empty(),
        "data setup index must be empty; preserve existing metadata before preparation"
    );
    if let Some(head) = &selected.head {
        ensure!(
            git(&root, &["ls-tree", "-r", "--name-only", "-z", head])?.is_empty(),
            "reserved shared path is occupied in HEAD; preserve existing metadata before preparation"
        );
    }
    let marker_name = marker_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("selected marker must have a UTF-8 filename")?;
    super::attributes::check(&root, &selected.common_directory, marker_name)?;
    let edits = [".gitignore", ".gitattributes"]
        .into_iter()
        .map(|name| policy::propose(name, None))
        .collect::<Result<Vec<_>>>()?;
    Ok(Basis {
        repository: root,
        git_directory: admin,
        project_file: marker_path,
        project_id: project.clone(),
        marker: marker_bytes,
        branch: branch.into(),
        head: selected.head,
        assertions,
        edits,
    })
}

pub(super) fn index_basis(basis: &Basis) -> Result<Vec<u8>> {
    git(&basis.repository, &["ls-files", "--stage", "-z"])
}

/// Recover only the same data checkout, branch, index and external code marker.
pub(super) fn recheck(basis: &Basis, index: &[u8]) -> Result<()> {
    let selected = DataWorktree::observe(
        basis
            .project_file
            .parent()
            .context("marker has no directory")?,
        &basis.repository,
        &basis.branch,
    )?;
    ensure!(
        selected.git_directory == basis.git_directory && selected.head == basis.head,
        "selected data worktree administration or prepared HEAD changed"
    );
    ensure!(
        read_optional(&basis.project_file)?.as_ref() == Some(&basis.marker),
        "prepared project marker changed"
    );
    ensure!(index_basis(basis)? == index, "prepared data index changed");
    for entry in git(&basis.repository, &["ls-files", "-v", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        ensure!(
            entry.len() > 2 && entry[1] == b' ',
            "malformed Git index result"
        );
        ensure!(
            entry[0] != b'S' && !entry[0].is_ascii_lowercase(),
            "index inspection is suppressed for a data path; resolve its flags first"
        );
    }
    Ok(())
}

pub(super) fn git_path(root: &Path, flag: &str) -> Result<PathBuf> {
    let bytes = git(root, &["rev-parse", "--path-format=absolute", flag])?;
    let text = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context("unterminated Git path")?;
    ensure!(!text.contains(['\r', '\n', '\0']), "ambiguous Git path");
    // git() passes this exact directory to current_dir; relative output is based here.
    let path = root.join(text);
    ordinary_ancestors(&path)?;
    Ok(fs::canonicalize(path)?)
}

pub(super) fn administration(root: &Path) -> Result<PathBuf> {
    ordinary_ancestors(root)?;
    git_path(root, "--git-dir")
}

pub(super) fn ordinary_ancestors(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    for part in absolute.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "linked path refused: {}",
                    part.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse path refused: {}",
                        part.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    ordinary_ancestors(path)?;
    let parent = super::files::Directory::open(path.parent().context("setup file has no parent")?)?;
    let bytes = parent.read(Path::new(
        path.file_name().context("setup file has no name")?,
    ))?;
    ensure!(
        bytes.as_ref().is_none_or(|bytes| bytes.len() <= 64 * 1024),
        "setup file exceeds 64 KiB bound"
    );
    Ok(bytes)
}

pub(super) fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_run(root, args, false, None)
}

pub(super) fn git_allow_missing(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_run(root, args, true, None)
}

pub(super) fn git_input_allow_missing(root: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    git_run(root, args, true, Some(input))
}

fn git_run(root: &Path, args: &[&str], missing: bool, input: Option<&[u8]>) -> Result<Vec<u8>> {
    crate::task_sync::git::query(root, args, missing, input, GIT_BOUND)
}
