//! The one-command workflow over the ordinary folder owners and Git.

use super::LocalWorkService;
use crate::{
    storage::TaskSyncTaskStatus,
    task_sync::{
        data_worktree::{Budget, DataWorktree},
        directory,
        git::{self, ordinary},
        progress::ExportProfile,
    },
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde::Serialize;
use std::{path::Path, process::Command};

/// Terminal observation of an ordinary folder sync.
#[derive(Debug, Serialize)]
pub struct FolderSyncReceipt {
    pub local_head: String,
    pub remote_head: Option<String>,
    pub pushed: bool,
}

impl LocalWorkService {
    /// Export and reconcile an enrolled task folder, then publish by fast-forward.
    ///
    /// # Errors
    /// Refuses a dirty data worktree, incomplete import, Git conflict or failed
    /// publication. Local commits stay available when publication fails.
    pub async fn sync_task_folder(
        &self,
        marker: &Path,
        remote: &str,
        branch: &str,
        no_push: bool,
    ) -> Result<FolderSyncReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(Utc::now())?;
        let mut data = DataWorktree::observe_project(marker, &self.project_id, branch)?;
        ensure!(
            clean_data(&data).await?,
            "finish the existing data merge or preserve uncommitted data files before syncing"
        );
        ordinary::checked(
            git::command(&data.checkout, &["remote", "get-url", "--", remote]),
            "read the configured data remote",
        )
        .await?;
        let basis = directory::read_folder_basis(&data.checkout.join(".engram/project.json"))
            .context("initialize and import the committed data folder before syncing")?;
        ensure!(
            !basis.memory_capable,
            "use the explicit folder export and import commands for a memory-enabled data branch"
        );
        let (_, head) = self
            .task_sync_folder_head(&basis.profile, Utc::now())?
            .filter(|(_, head)| head.source.is_some())
            .context("import the committed data folder to enroll this host before syncing")?;
        let profile = head.profile;

        // Finish a prior commit whose import did not complete. The same import
        // owner also refuses unresolved planning before a new publication.
        self.import_sync_folder(&data)?;
        let mut needs_import = self.export_sync_folder(&mut data, &profile).await?;
        for _ in 0..2 {
            let incoming = fetch(&data, remote).await?;
            if let Some(incoming) = incoming.as_deref()
                && merge(&mut data, incoming).await?
            {
                self.import_sync_folder(&data)?;
                needs_import = self.export_sync_folder(&mut data, &profile).await?;
            }
            if needs_import {
                self.import_sync_folder(&data)?;
                needs_import = false;
            }
            let local = data.head_commit().context("missing data commit")?;
            let same = incoming.as_deref() == Some(local);
            if no_push || same {
                return Ok(FolderSyncReceipt {
                    local_head: local.into(),
                    remote_head: incoming,
                    pushed: false,
                });
            }
            data.check_head()?;
            if push(&data, remote, local).await? {
                return Ok(FolderSyncReceipt {
                    local_head: local.into(),
                    remote_head: Some(local.into()),
                    pushed: true,
                });
            }
        }
        bail!(
            "the data branch rejected two pushes; the local commit is preserved. Check the remote branch and run task-sync sync again"
        )
    }

    fn import_sync_folder(&self, data: &DataWorktree) -> Result<()> {
        let receipt =
            self.import_task_sync_data_folder(data, None, false, false, None, Utc::now())?;
        if let Some(task) = receipt
            .result
            .tasks
            .iter()
            .find(|task| task.status != TaskSyncTaskStatus::Applied)
        {
            bail!(
                "the data import did not apply task {} ({}); resolve it through the existing folder workflow before publishing",
                task.task.0,
                task.reason.as_deref().unwrap_or("held or refused")
            );
        }
        Ok(())
    }

    async fn export_sync_folder(
        &self,
        data: &mut DataWorktree,
        profile: &ExportProfile,
    ) -> Result<bool> {
        self.export_task_sync_data_folder(data, profile, false, None, Utc::now())?;
        if data.folder_changes_admitted(false)?.is_empty() {
            return Ok(false);
        }
        stage_folder(data).await?;
        data.commit_folder("Export project task data").await?;
        Ok(true)
    }
}

fn folder_command(data: &DataWorktree, args: &[&str]) -> Result<Command> {
    let config = data.filter_overrides_within(Budget::PerQuery)?;
    let mut options = config.iter().map(String::as_str).collect::<Vec<_>>();
    options.extend_from_slice(args);
    Ok(git::command(&data.checkout, &options))
}

async fn clean_data(data: &DataWorktree) -> Result<bool> {
    if data.git_directory.join("MERGE_HEAD").try_exists()? {
        return Ok(false);
    }
    let command = folder_command(data, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    Ok(ordinary::checked(command, "check the data worktree")
        .await?
        .is_empty())
}

async fn stage_folder(data: &DataWorktree) -> Result<()> {
    data.check_head()?;
    ordinary::checked(
        folder_command(data, &["add", "--all", "--", ".engram"])?,
        "stage the data folder",
    )
    .await?;
    Ok(())
}

async fn fetch(data: &DataWorktree, remote: &str) -> Result<Option<String>> {
    let result = ordinary::run(
        ordinary::network(
            &data.checkout,
            &[
                "ls-remote",
                "--exit-code",
                "--refs",
                "--",
                remote,
                &data.reference,
            ],
        ),
        "observe the data branch",
    )
    .await?;
    if result.status.code() == Some(2) && result.stdout.is_empty() {
        return Ok(None);
    }
    ensure!(
        result.status.success(),
        "Git could not observe the configured data branch"
    );
    let row = std::str::from_utf8(&result.stdout)?.trim_end_matches(['\r', '\n']);
    let (oid, reference) = row
        .split_once('\t')
        .context("invalid Git remote data ref")?;
    ensure!(
        reference == data.reference,
        "unexpected Git remote data ref"
    );
    let incoming = ordinary::oid(oid.as_bytes())?;
    if data.head_commit() != Some(incoming.as_str()) {
        ordinary::checked(
            ordinary::network(
                &data.checkout,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    "--",
                    remote,
                    &incoming,
                ],
            ),
            "fetch the data commit",
        )
        .await?;
    }
    Ok(Some(incoming))
}

async fn merge(data: &mut DataWorktree, incoming: &str) -> Result<bool> {
    let local = data.head_commit().context("missing data commit")?;
    if local == incoming {
        return Ok(false);
    }
    data.check_commit(incoming)?;
    let base = ordinary::oid(
        &ordinary::checked(
            git::command(&data.checkout, &["merge-base", local, incoming]),
            "find the common data history",
        )
        .await?,
    )?;
    if base == incoming {
        return Ok(false);
    }
    data.check_head()?;
    if base == local {
        ordinary::checked(
            folder_command(data, &["merge", "--ff-only", "--", incoming])?,
            "fast-forward the data branch",
        )
        .await?;
        data.refresh_sync_tip().await?;
        return Ok(true);
    }

    let result = ordinary::run(
        folder_command(
            data,
            &[
                "-c",
                "user.name=Engram task sync",
                "-c",
                "user.email=task-sync@engram.invalid",
                "merge",
                "--no-ff",
                "--no-commit",
                "--no-gpg-sign",
                "--",
                incoming,
            ],
        )?,
        "merge the fetched data branch",
    )
    .await?;
    if !result.status.success() {
        ensure!(
            result.status.code() == Some(1),
            "Git data merge failed; inspect the data worktree"
        );
        let unmerged = ordinary::checked(
            git::command(
                &data.checkout,
                &["diff", "--name-only", "--diff-filter=U", "-z"],
            ),
            "read data merge conflicts",
        )
        .await?;
        // The exporter's envelope is local provenance. The importer reads
        // incoming task content by commit and recomputes the shared basis.
        ensure!(
            unmerged == b".engram/project.json\0",
            "the data merge has conflicts; resolve them through the existing folder workflow before publishing"
        );
        ordinary::checked(
            folder_command(data, &["checkout", "--ours", "--", ".engram/project.json"])?,
            "keep the local folder envelope",
        )
        .await?;
        stage_folder(data).await?;
    }
    data.commit_folder("Merge shared project task data").await?;
    Ok(true)
}

async fn push(data: &DataWorktree, remote: &str, local: &str) -> Result<bool> {
    let spec = format!("{local}:{}", data.reference);
    let output = ordinary::run(
        ordinary::network(
            &data.checkout,
            &[
                "push",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "--",
                remote,
                &spec,
            ],
        ),
        "push the data branch",
    )
    .await?;
    if output.status.success() {
        return Ok(true);
    }
    let text = std::str::from_utf8(&output.stdout)?;
    let rejection = format!("!\t{spec}\t");
    if output.status.code() == Some(1)
        && text.lines().any(|line| {
            line.strip_prefix(rejection.as_str()).is_some_and(|reason| {
                matches!(
                    reason,
                    "[rejected] (non-fast-forward)"
                        | "[rejected] (fetch first)"
                        | "[remote rejected] (failed to update ref)"
                )
            })
        })
    {
        return Ok(false);
    }
    bail!("Git could not push the data branch; the local commit is preserved")
}
