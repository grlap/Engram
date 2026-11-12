//! Bind read-only observation to the caller's exact project marker.

use super::{DIRECTORY_NAME, DataWorktree, PrivateDirectory, Result};
use anyhow::{Context, ensure};
use std::path::{Path, PathBuf};

pub(super) fn code_for_project(marker: &Path, project: &crate::ProjectId) -> Result<PathBuf> {
    let marker = std::path::absolute(marker)?;
    let code = marker.parent().context("project marker has no parent")?;
    let directory = PrivateDirectory::open(code)?;
    let bytes = directory
        .read_bounded(
            Path::new(marker.file_name().context("project marker has no name")?),
            4096,
        )?
        .context("project marker missing")?;
    ensure!(
        std::str::from_utf8(&bytes)?.trim() == project.0,
        "selected project marker differs from project"
    );
    Ok(std::fs::canonicalize(code)?)
}

impl DataWorktree {
    /// Select only `.engram` directly inside this admitted data checkout.
    /// The directory may be absent for a first export; parent aliases and
    /// linked directories never select another publication location.
    ///
    /// # Errors
    /// Refuses another destination, parent traversal or linked directories.
    pub fn select_folder(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
                && path.file_name() == Some(std::ffi::OsStr::new(".engram")),
            "task exchange requires .engram in the dedicated data worktree"
        );
        let path = std::path::absolute(path)?;
        let parent = path.parent().context("exchange directory has no parent")?;
        PrivateDirectory::open(parent)
            .context("task exchange requires .engram in the dedicated data worktree")?;
        ensure!(
            std::fs::canonicalize(parent)? == self.checkout,
            "task exchange requires .engram in the dedicated data worktree"
        );
        let selected = self.checkout.join(".engram");
        match std::fs::symlink_metadata(&selected) {
            Ok(_) => {
                PrivateDirectory::open(&selected)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.revalidate()?;
        Ok(selected)
    }

    /// Observe the fixed data checkout for an exact main-code project marker.
    /// Creates no checkout, lock, branch, index or retained artifact.
    ///
    /// # Errors
    /// Refuses another project, unsafe marker paths or unadmitted Git state.
    pub fn observe_project(
        marker: &Path,
        project: &crate::ProjectId,
        branch: &str,
    ) -> Result<Self> {
        let code = code_for_project(marker, project)?;
        Self::observe(&code, &code.join(DIRECTORY_NAME), branch)
    }
}
