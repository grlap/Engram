//! Explicit local provisioning. A failed Git operation is preserved for inspection;
//! no registration, branch or worktree is forcibly removed or reset on retry.

use super::{DIRECTORY_NAME, DataWorktree, PrivateDirectory, git_path, query};
use anyhow::{Context, Result, ensure};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

const LOCK_REASON: &str = "Persistent Engram task and evidence data; retire explicitly";

#[derive(Debug, serde::Serialize)]
pub struct Initialization {
    pub checkout: std::path::PathBuf,
    pub reference: String,
    pub created: bool,
    pub locked: bool,
}

/// Create the fixed, ignored orphan checkout, or lock the same admitted checkout.
/// The selected main-code marker binds it to the caller's project. No commit,
/// network operation, SQLite access or code-checkout edit is performed.
///
/// # Errors
/// Refuses another project, unsafe paths, an occupied destination, an existing
/// branch without its admitted checkout, or contention. Partial Git state stays.
pub fn initialize(
    marker: &Path,
    project: &crate::ProjectId,
    branch: &str,
) -> Result<Initialization> {
    let code = super::project::code_for_project(marker, project)?;
    ensure!(
        git_path(&code, "--show-toplevel")? == code,
        "select the exact main code checkout"
    );
    let common = git_path(&code, "--git-common-dir")?;
    ensure!(
        git_path(&code, "--git-dir")? == common,
        "select the main code checkout, not a linked code worktree"
    );
    ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "select an ordinary data branch name"
    );
    let reference = format!("refs/heads/{branch}");
    query(&code, &["check-ref-format", &reference], false)?;
    super::check_code_path_untracked(&code)?;
    let admin = PrivateDirectory::open(&common)?;
    let lock_name = Path::new("engram-data.lock");
    let file = match admin.open_file(lock_name, true, true) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            admin.open_file(lock_name, true, false)?
        }
        Err(error) => return Err(error),
    };
    let _lock = crate::task_sync::file_lock::HeldLock::try_acquire(file)
        .map_err(|error| anyhow::anyhow!("data-worktree initialization busy: {error}"))?;
    let checkout = code.join(DIRECTORY_NAME);
    let checkout_argument = crate::task_sync::git::plain_path(&checkout)
        .context("data-worktree path has no supported Git spelling")?;
    let checkout_argument = checkout_argument
        .to_str()
        .context("data-worktree path must be UTF-8")?;
    let created = match fs::symlink_metadata(&checkout) {
        Ok(_) => {
            // Never add an ignore rule to hide an unadmitted occupant.
            DataWorktree::observe(&code, &checkout, branch)?;
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                query(
                    &code,
                    &["rev-parse", "--verify", "--quiet", &reference],
                    true
                )?
                .is_empty(),
                "existing data branch requires its admitted worktree; no branch is reset or replaced"
            );
            ensure_ignored(&code, &admin)?;
            query(&code, &["worktree", "add", "--orphan", "-b", branch, "--lock", "--reason", LOCK_REASON,
                checkout_argument], false)
                .context("data-worktree creation refused; preserve any partial registration and checkout before retrying")?;
            true
        }
        Err(error) => return Err(error.into()),
    };
    let selected = DataWorktree::observe(&code, &checkout, branch)?;
    let data_admin = PrivateDirectory::open(&selected.git_directory)?;
    if data_admin
        .read_bounded(Path::new("locked"), 4096)?
        .is_none()
    {
        query(
            &code,
            &[
                "worktree",
                "lock",
                "--reason",
                LOCK_REASON,
                checkout_argument,
            ],
            false,
        )?;
    }
    ensure!(
        data_admin
            .read_bounded(Path::new("locked"), 4096)?
            .is_some(),
        "data worktree lock was not retained"
    );
    Ok(Initialization {
        checkout,
        reference,
        created,
        locked: true,
    })
}

fn ensure_ignored(code: &Path, admin: &PrivateDirectory) -> Result<()> {
    // The directory does not exist yet. Preserve the trailing slash so Git
    // evaluates directory-only ignore rules against the prospective directory.
    let directory = format!("{DIRECTORY_NAME}/");
    let ignored = || {
        query(
            code,
            &["check-ignore", "--no-index", "--", &directory],
            true,
        )
    };
    if !ignored()?.is_empty() {
        return Ok(());
    }
    let info = match admin.child(Path::new("info")) {
        Ok(info) => info,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            admin.create_child(Path::new("info"))?
        }
        Err(error) => return Err(error),
    };
    // Append, never replace or truncate the operator's local rules. A no-follow
    // retained handle prevents a linked exclude file from redirecting the write.
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .append(true)
        .create(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut exclude = info.dir().open_with("exclude", &options)?.into_std();
    let metadata = exclude.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "local exclude must be an ordinary file within 64 KiB"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "local exclude is a reparse point"
        );
    }
    let mut before = Vec::new();
    (&exclude).take(64 * 1024 + 1).read_to_end(&mut before)?;
    ensure!(
        before.len() <= 64 * 1024,
        "local exclude grew past its bound"
    );
    ensure!(
        before.len()
            + usize::from(!before.is_empty() && !before.ends_with(b"\n"))
            + b"/.engram-data/\n".len()
            <= 64 * 1024,
        "local exclude has no room for the data-worktree rule within 64 KiB"
    );
    if !before.is_empty() && !before.ends_with(b"\n") {
        exclude.write_all(b"\n")?;
    }
    exclude.write_all(b"/.engram-data/\n")?;
    exclude.sync_all()?;
    ensure!(
        !ignored()?.is_empty(),
        "local data-worktree exclusion did not take effect"
    );
    Ok(())
}
