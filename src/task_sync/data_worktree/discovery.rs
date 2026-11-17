//! Current paths and optional HEAD from one bounded repository query.

use super::{Budget, Context, Path, PathBuf, Result, ensure, parse_git_path, query_within};

pub(super) struct Repository {
    pub(super) top_level: PathBuf,
    pub(super) git_directory: PathBuf,
    pub(super) common_directory: PathBuf,
    pub(super) head: Option<String>,
}

pub(super) fn read(root: &Path, budget: Budget) -> Result<Repository> {
    let bytes = query_within(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
            // Unlike --verify, --revs-only emits no revision for an unborn
            // HEAD while still returning the administration paths above.
            "--revs-only",
            "--end-of-options",
            "HEAD",
        ],
        false,
        budget,
    )?;
    let records = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context(
            "unterminated Git repository observation; check the selected repository and retry",
        )?
        .split('\n')
        .collect::<Vec<_>>();
    ensure!(
        matches!(records.len(), 3 | 4),
        "Git repository observation returned {} records; expected three paths and optional HEAD; check the selected repository and retry",
        records.len()
    );
    let head = records
        .get(3)
        .map(|value| {
            crate::task_sync::ledger::oid(value)?;
            Ok::<_, anyhow::Error>((*value).to_owned())
        })
        .transpose()?;
    Ok(Repository {
        top_level: parse_git_path(root, records[0])?,
        git_directory: parse_git_path(root, records[1])?,
        common_directory: parse_git_path(root, records[2])?,
        head,
    })
}
