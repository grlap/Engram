//! The one-command workflow over the ordinary folder owners and Git.

use super::{FolderMemoryExport, LocalWorkService, MemoryImport};
use crate::{
    storage::TaskSyncTaskStatus,
    task_sync::{
        data_worktree::{Budget, DataWorktree},
        directory,
        git::{self, ordinary},
        memories::{ImportOutcome, KeyImportResult, WithheldKey, directory as memory_directory},
        progress::ExportProfile,
    },
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde::Serialize;
use std::{collections::BTreeMap, fmt::Write, path::Path, process::Command};

const DROPPED_MEMORY_PROFILE: &str =
    "the data history drops its memory profile. Restore the profile before syncing";

/// Parameters for an ordinary folder sync.
pub struct FolderSyncOptions<'a> {
    pub remote: &'a str,
    pub branch: &'a str,
    pub origin_machine: Option<&'a str>,
    pub no_push: bool,
    pub include_memories: bool,
}

/// Terminal observation of an ordinary folder sync.
#[derive(Debug, Serialize)]
pub struct FolderSyncReceipt {
    pub local_head: String,
    pub remote_head: Option<String>,
    pub pushed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld_memories: Vec<WithheldKey>,
}

impl LocalWorkService {
    /// Export and reconcile an enrolled data folder, then publish by fast-forward.
    /// A memory-capable folder exchanges memories automatically.
    /// Reports each applied memory retirement as soon as its import commits.
    ///
    /// # Errors
    /// Refuses a dirty data worktree, incomplete import, Git conflict or failed
    /// publication. Local commits stay available when publication fails.
    pub async fn sync_task_folder(
        &self,
        marker: &Path,
        options: &FolderSyncOptions<'_>,
        on_retirement: impl Fn(&ImportOutcome),
    ) -> Result<FolderSyncReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(Utc::now())?;
        let mut data = DataWorktree::observe_project(marker, &self.project_id, options.branch)?;
        ensure!(
            clean_data(&data).await?,
            "finish the existing data merge or preserve uncommitted data files before syncing"
        );
        ordinary::checked(
            git::command(
                &data.code_checkout,
                &["remote", "get-url", "--", options.remote],
            ),
            "read the configured data remote",
        )
        .await?;
        let basis = directory::read_folder_basis(&data.checkout.join(".engram/project.json"))
            .context("initialize and import the committed data folder before syncing")?;
        let (_, head) = self
            .task_sync_folder_head(&basis.profile, Utc::now())?
            .filter(|(_, head)| head.source.is_some())
            .context("import the committed data folder to enroll this host before syncing")?;
        let mut profile = head.profile;
        if let Some(alias) = options.origin_machine {
            ensure!(
                profile
                    .origin_machine
                    .as_deref()
                    .is_none_or(|recorded| recorded == alias),
                "use this host's recorded export alias for sync"
            );
            profile.origin_machine = Some(alias.to_owned());
        }
        ensure!(
            !profile.include_notes || profile.origin_machine.is_some(),
            "supply this host's export alias with --origin-machine to sync a notes-enabled folder"
        );
        profile.validate()?;

        // Check an already resolved memory commit against the fetched tip
        // before importing the resolution into SQLite.
        let mut first_fetch = if basis.memory_capable {
            Some(fetch(&data, options.remote).await?)
        } else {
            None
        };
        let mut admitted = None;
        if let Some(Some(incoming)) = &first_fetch {
            let local = data.head_commit().context("missing data commit")?;
            if incoming != local {
                let base = merge_base(&data, local, incoming).await?;
                if base == *incoming {
                    self.require_sync_memory_publication(&data, incoming, local)?;
                    admitted = Some((incoming.clone(), local.to_owned()));
                } else {
                    git::require_immutable_memory_revisions(
                        &data.checkout,
                        &self.project_id,
                        incoming,
                        local,
                    )?;
                    // The peer may have retired a revision after the local
                    // merge incorporated it. Its bytes are still in the base.
                    if base != local {
                        git::require_immutable_memory_revisions(
                            &data.checkout,
                            &self.project_id,
                            &base,
                            local,
                        )?;
                    }
                }
            }
        }
        // Finish a prior commit whose import did not complete. The same import
        // owner also refuses unresolved planning before a new publication.
        self.import_sync_folder(&data, basis.memory_capable, &on_retirement)?;
        let mut sync_memories = basis.memory_capable;
        let mut withheld_memories = self
            .export_sync_folder(&mut data, &profile, sync_memories, &on_retirement)
            .await?;
        for _ in 0..2 {
            let incoming = if let Some(incoming) = first_fetch.take() {
                incoming
            } else {
                let incoming = fetch(&data, options.remote).await?;
                if sync_memories
                    && let Some(remote_tip) = incoming.as_deref()
                    && let Some(local) = data.head_commit().filter(|local| *local != remote_tip)
                {
                    git::require_immutable_memory_revisions(
                        &data.checkout,
                        &self.project_id,
                        remote_tip,
                        local,
                    )?;
                }
                incoming
            };
            let merged_profile = match incoming.as_deref() {
                Some(incoming) => merge(&mut data, incoming, &basis, sync_memories).await?,
                None => None,
            };
            if let (Some(merged_memories), Some(incoming)) = (merged_profile, incoming.as_ref()) {
                sync_memories = merged_memories;
                let local = data.head_commit().context("missing data commit")?;
                if incoming != local {
                    self.require_sync_memory_publication(&data, incoming, local)?;
                    admitted = Some((incoming.clone(), local.to_owned()));
                }
                self.import_sync_folder(&data, sync_memories, &on_retirement)?;
            }
            // Check the committed tips before enabling the profile so an
            // opt-in cannot hide a merge that discarded an established one.
            if merged_profile.is_some() || (options.include_memories && !sync_memories) {
                sync_memories |= options.include_memories;
                withheld_memories = self
                    .export_sync_folder(&mut data, &profile, sync_memories, &on_retirement)
                    .await?;
            }
            let local = data.head_commit().context("missing data commit")?;
            let same = incoming.as_deref() == Some(local);
            // Reuse admission only for the same fetched and outgoing commits.
            if let Some(remote_tip) = incoming.as_deref().filter(|tip| *tip != local)
                && admitted
                    .as_ref()
                    .is_none_or(|(remote, candidate)| remote != remote_tip || candidate != local)
            {
                self.require_sync_memory_publication(&data, remote_tip, local)?;
            }
            if options.no_push || same {
                return Ok(FolderSyncReceipt {
                    local_head: local.into(),
                    remote_head: incoming,
                    pushed: false,
                    withheld_memories,
                });
            }
            data.check_head()?;
            if push(&data, options.remote, local).await? {
                return Ok(FolderSyncReceipt {
                    local_head: local.into(),
                    remote_head: Some(local.into()),
                    pushed: true,
                    withheld_memories,
                });
            }
        }
        bail!(
            "the data branch rejected two pushes; the local commit is preserved. Check the remote branch and run task-sync sync again"
        )
    }

    fn require_sync_memory_publication(
        &self,
        data: &DataWorktree,
        incoming: &str,
        local: &str,
    ) -> Result<()> {
        let retired =
            git::memory_publication_retirements(&data.checkout, &self.project_id, incoming, local)?;
        if !retired.is_empty() {
            self.read_store_at(Utc::now())?
                .require_known_memory_retirements(&self.project_id, &retired)?;
        }
        Ok(())
    }

    fn import_sync_folder(
        &self,
        data: &DataWorktree,
        include_memories: bool,
        on_retirement: &impl Fn(&ImportOutcome),
    ) -> Result<()> {
        let receipt = if include_memories {
            let fence = memory_directory::publication_lock(data)?;
            let head = data.head_commit().context("missing data commit")?;
            let (selected, bytes) =
                git::read_memory_snapshot_with_bytes(&data.checkout, &self.project_id, head)?;
            let memory = MemoryImport {
                fence: &fence,
                selected: &selected,
                options: &BTreeMap::new(),
                bytes: u64::try_from(bytes)?,
                on_retirement: Some(on_retirement),
            };
            self.import_task_sync_data_folder(data, None, false, false, Some(&memory), Utc::now())?
        } else {
            self.import_task_sync_data_folder(data, None, false, false, None, Utc::now())?
        };
        let mut memory_issues = String::new();
        for result in &receipt.memories {
            let (key, reason) = match result {
                KeyImportResult::Applied { .. } => continue,
                KeyImportResult::Refused { key, reason } => (key, reason),
                KeyImportResult::ConcurrentRetirement { key, remedy, .. } => (key, remedy),
            };
            writeln!(
                memory_issues,
                "{} ({})",
                super::terminal_error_line(key),
                super::terminal_error_line(reason)
            )?;
        }
        let mut task_issues = String::new();
        for task in &receipt.result.tasks {
            let status = match task.status {
                TaskSyncTaskStatus::Applied => continue,
                TaskSyncTaskStatus::Held => "held",
                TaskSyncTaskStatus::Refused => "refused",
            };
            write!(
                task_issues,
                "{} {status} ({})",
                task.task.0,
                task.reason.as_deref().unwrap_or("reason unavailable")
            )?;
            if let Some(detail) = &task.detail {
                write!(task_issues, " {}", super::terminal_error_line(detail))?;
            }
            task_issues.push('\n');
        }
        if !receipt.memory_partial && task_issues.is_empty() {
            return Ok(());
        }
        let mut unapplied = String::new();
        if receipt.memory_partial {
            write!(
                unapplied,
                "the data import did not apply these memories:\n{memory_issues}"
            )?;
        }
        if !task_issues.is_empty() {
            write!(
                unapplied,
                "the data import did not apply these tasks:\n{task_issues}"
            )?;
        }
        let separate_import = if include_memories {
            String::new()
        } else {
            format!(
                "To retry the import separately, use `engram task-sync import DIRECTORY` with the data folder at {} and the same branch. ",
                super::terminal_error_command(&data.checkout.join(".engram").to_string_lossy())
            )
        };
        bail!(
            "{unapplied}Resolve the listed issues. {separate_import}Rerun your original `engram task-sync sync` \
             invocation with all its options, including any `--no-push` and `--remote`."
        )
    }

    async fn export_sync_folder(
        &self,
        data: &mut DataWorktree,
        profile: &ExportProfile,
        include_memories: bool,
        on_retirement: &impl Fn(&ImportOutcome),
    ) -> Result<Vec<WithheldKey>> {
        let withheld = if include_memories {
            let fence = memory_directory::publication_lock(data)?;
            let basis = memory_directory::committed(data, &self.project_id)?;
            let resolutions = BTreeMap::new();
            let mut admitted = None;
            let export = FolderMemoryExport::publication(
                data,
                &self.project_id,
                &basis,
                &fence,
                &resolutions,
                None,
                &mut admitted,
            )?;
            self.export_task_sync_data_folder(data, profile, false, Some(export), Utc::now())?;
            admitted.context("memory capture missing")?.withheld
        } else {
            self.export_task_sync_data_folder(data, profile, false, None, Utc::now())?;
            Vec::new()
        };
        if data.folder_changes_admitted(include_memories)?.is_empty() {
            return Ok(withheld);
        }
        stage_folder(data).await?;
        data.commit_folder("Export project task data").await?;
        // Confirm this export while its single-parent commit is still HEAD.
        self.import_sync_folder(data, include_memories, on_retirement)?;
        Ok(withheld)
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
    // Relative remote paths are rooted in the main checkout. Both worktrees
    // share the object store, so the data checkout can read the fetched commit.
    let result = ordinary::run(
        ordinary::network(
            &data.code_checkout,
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
                &data.code_checkout,
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

async fn merge(
    data: &mut DataWorktree,
    incoming: &str,
    local_basis: &directory::FolderBasis,
    local_memories: bool,
) -> Result<Option<bool>> {
    let local = data.head_commit().context("missing data commit")?;
    if local == incoming {
        return Ok(None);
    }
    data.check_commit(incoming)?;
    let base = merge_base(data, local, incoming).await?;
    if base == incoming {
        // A manually resolved merge can keep the task-only envelope even
        // though the remote ancestor already enabled memories.
        ensure!(
            local_memories
                || !directory::committed_folder_basis(data, incoming)?
                    .is_some_and(|basis| basis.memory_capable),
            DROPPED_MEMORY_PROFILE
        );
        return Ok(None);
    }
    let incoming_basis = directory::committed_folder_basis(data, incoming)?
        .context("the fetched data commit has no project envelope")?;
    ensure!(
        incoming_basis.project_id == local_basis.project_id
            && incoming_basis.profile.include_notes == local_basis.profile.include_notes,
        "the fetched data commit belongs to a different project or uses a different notes profile"
    );
    let incoming_memories = incoming_basis.memory_capable;
    if local_memories != incoming_memories {
        // One side may still be working from before the first memory export.
        // A side that has removed the established capability must repair it.
        ensure!(
            !directory::committed_folder_basis(data, &base)?
                .is_some_and(|basis| basis.memory_capable),
            DROPPED_MEMORY_PROFILE
        );
    }
    data.check_head()?;
    if base == local {
        ordinary::checked(
            folder_command(
                data,
                &[
                    "merge",
                    "--ff-only",
                    "--no-overwrite-ignore",
                    "--",
                    incoming,
                ],
            )?,
            "fast-forward the data branch without overwriting local files",
        )
        .await?;
        data.refresh_sync_tip().await?;
        return Ok(Some(incoming_memories));
    }

    merge_divergent(data, incoming, incoming_memories && !local_memories).await?;
    Ok(Some(local_memories || incoming_memories))
}

async fn merge_base(data: &DataWorktree, local: &str, incoming: &str) -> Result<String> {
    ordinary::oid(
        &ordinary::checked(
            git::command(&data.checkout, &["merge-base", local, incoming]),
            "find the common data history",
        )
        .await?,
    )
}

async fn merge_divergent(
    data: &mut DataWorktree,
    incoming: &str,
    take_incoming_envelope: bool,
) -> Result<()> {
    // Git's three-way merge can overwrite ignored files despite its flag.
    let ignored = ordinary::checked(
        git::command(
            &data.checkout,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
                "--",
                ".engram",
            ],
        ),
        "check for ignored files in the data folder",
    )
    .await?;
    ensure!(
        ignored.is_empty(),
        "set aside ignored local files under .engram before merging divergent data branches"
    );
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
                "--no-overwrite-ignore",
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
        ensure!(
            !unmerged.is_empty(),
            "Git could not start the data merge; check the data worktree for obstructing local files"
        );
        // The importer recomputes the shared basis from the merged commit.
        // Keep the local envelope unless the incoming side enabled memories.
        ensure!(
            unmerged == b".engram/project.json\0",
            "the data merge has conflicts; resolve them through the existing folder workflow before publishing"
        );
        let envelope = if take_incoming_envelope {
            "--theirs"
        } else {
            "--ours"
        };
        ordinary::checked(
            folder_command(data, &["checkout", envelope, "--", ".engram/project.json"])?,
            "keep the selected folder envelope",
        )
        .await?;
        stage_folder(data).await?;
    }
    data.commit_folder("Merge shared project task data").await?;
    Ok(())
}

async fn push(data: &DataWorktree, remote: &str, local: &str) -> Result<bool> {
    let spec = format!("{local}:{}", data.reference);
    let output = ordinary::run(
        ordinary::network(
            &data.code_checkout,
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
