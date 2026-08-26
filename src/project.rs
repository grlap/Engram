//! Stable host-local project store discovery.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::ProjectId;

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

#[cfg(test)]
mod tests {
    use super::*;

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
