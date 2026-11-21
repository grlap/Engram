//! Run the ordinary export, Git merge, import and push in that order.

use super::{
    FolderImportReceipt, FolderMemoryAdmission, FolderMemoryExport, LocalWorkService, MemoryImport,
    task_sync_folder_export::SyncExport,
};
use crate::{
    storage::{FolderDisk, FolderHeadReason, FolderSource, TaskSyncTaskStatus},
    task_sync::{
        self,
        data_worktree::{Budget, DataWorktree},
        directory::{self, FolderBasis},
        exchange::{self, Cancellation, ExchangeTarget, RunError, RunLimits, TargetSelection},
        git,
        memories::directory as memory_files,
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

/// Selection for one ordinary data-branch synchronization.
pub struct DataSyncOptions {
    pub remote: String,
    pub branch: String,
    pub dry_run: bool,
    pub no_push: bool,
    pub reconciled: bool,
}

/// Completed and pending operations on the admitted data worktree.
#[derive(serde::Serialize)]
pub struct DataSyncReceipt {
    pub scope: &'static str,
    pub local_head: String,
    pub remote_head: Option<String>,
    pub exported: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memory_withheld: Vec<task_sync::memories::WithheldKey>,
    pub imported: Option<FolderImportReceipt>,
    pub pushed: bool,
    pub dry_run: bool,
    pub resume_command: Option<&'static str>,
    pub pending: Option<String>,
}

struct Sync<'a> {
    service: &'a LocalWorkService,
    marker: &'a Path,
    options: &'a DataSyncOptions,
    limits: RunLimits,
    started: Instant,
    cancel: &'a Cancellation,
}

struct MergeEnvelope<'a> {
    peer: &'a str,
    local: &'a [u8],
    fetched: &'a [u8],
    source: FolderSource,
}

const SYNC_AUTHOR: &str = "Engram task sync";
const SYNC_EMAIL: &str = "task-sync@engram.invalid";

impl LocalWorkService {
    /// Synchronize an initialized data worktree through the folder and Git
    /// owners. An exact committed import is the basis for publication.
    ///
    /// # Errors
    /// Refuses an unsafe checkout, unsupported profile, missing enrollment,
    /// incomplete Git operation, or a failed folder or transport operation.
    pub async fn sync_data_folder(
        &self,
        marker: &Path,
        options: &DataSyncOptions,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> Result<DataSyncReceipt> {
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        if !options.dry_run {
            self.require_task_sync_writer()?;
        }
        limits.validate_request()?;
        Sync {
            service: self,
            marker,
            options,
            limits,
            started: Instant::now(),
            cancel,
        }
        .run()
        .await
    }
}

impl Sync<'_> {
    fn remaining(&self) -> Result<RunLimits> {
        Ok(exchange::remaining(self.limits, self.started)?)
    }

    fn budget(&self) -> Result<Budget> {
        Budget::within(self.started, self.limits.deadline)
    }

    fn select(&self) -> Result<DataWorktree> {
        let data = DataWorktree::observe_project(
            self.marker,
            &self.service.project_id,
            &self.options.branch,
        )?;
        data.select_admitted_folder(&data.checkout.join(".engram"))?;
        Ok(data)
    }

    fn git(&self, data: &DataWorktree, args: &[&str], missing: bool) -> Result<Vec<u8>> {
        git::query(
            &data.checkout,
            args,
            missing,
            None,
            self.remaining()?.deadline,
        )
    }

    async fn effect(&self, data: &DataWorktree, args: &[&str]) -> Result<()> {
        let overrides = data.filter_overrides_within(self.budget()?)?;
        let args = overrides
            .iter()
            .map(String::as_str)
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        exchange::ordinary::local(&data.checkout, &args, self.remaining()?, self.cancel).await
    }

    fn profile(&self, data: &DataWorktree, commit: &str) -> Result<FolderBasis> {
        let evidence = self.git(data, &["ls-tree", commit, "--", ".engram/evidence"], false)?;
        ensure!(
            evidence.is_empty(),
            "sync_evidence_profile_unsupported: use the explicit folder and evidence exchange"
        );
        let bytes = self.git(
            data,
            &["show", &format!("{commit}:.engram/project.json")],
            false,
        )?;
        let basis = directory::folder_basis_from_bytes(&bytes)?;
        ensure!(
            basis.project_id == self.service.project_id
                && !basis.profile.include_notes
                && basis.profile.origin_machine.is_none(),
            "sync requires this project's committed task or task-and-memory profile without notes or activity"
        );
        Ok(basis)
    }

    fn admit_publication(
        &self,
        data: &DataWorktree,
        local: &str,
        observed: Option<&str>,
    ) -> Result<()> {
        ensure!(
            self.git(data, &["rev-parse", "--is-shallow-repository"], false)?
                .trim_ascii()
                == b"false",
            "publishing task data requires complete Git history"
        );
        // Exclude only history reachable from the exact tip that the push
        // will require. A new ref needs admission of its complete history.
        let mut args = vec!["rev-list", "--format=%T", local];
        if let Some(observed) = observed {
            args.extend(["--not", observed]);
        }
        args.push("--");
        let outgoing = self.git(data, &args, false)?;
        ensure!(
            !outgoing.is_empty(),
            "the outgoing data history could not be established"
        );
        let mut lines = std::str::from_utf8(&outgoing)?.lines();
        let mut commits = Vec::new();
        let mut trees = BTreeSet::new();
        while let Some(header) = lines.next() {
            let commit = header
                .strip_prefix("commit ")
                .context("invalid outgoing Git commit record")?;
            let tree = lines.next().context("missing outgoing Git tree record")?;
            task_sync::ledger::oid(commit)?;
            task_sync::ledger::oid(tree)?;
            commits.push(commit);
            trees.insert(tree);
        }
        let remote_policy = if let Some(observed) = observed {
            git::ordinary_data_policy(
                &data.checkout,
                &self.service.project_id,
                observed,
                self.remaining()?.deadline,
            )?
        } else {
            BTreeMap::new()
        };
        for tree in trees {
            git::admit_ordinary_data_tree(
                &data.checkout,
                &self.service.project_id,
                tree,
                self.remaining()?.deadline,
                Some(&remote_policy),
            )?;
        }
        data.require_owned_sync_commits(&commits)
    }

    fn record_owned_commit(
        &self,
        data: &DataWorktree,
        parent: &str,
        merge_parent: Option<&str>,
        tree: &str,
        message: &str,
    ) -> Result<()> {
        let head = self.git(data, &["rev-parse", "--verify", "HEAD"], false)?;
        let head = std::str::from_utf8(head.trim_ascii())?;
        task_sync::ledger::oid(head)?;
        let bytes = self.git(data, &["cat-file", "commit", head], false)?;
        let commit = git::observe_commit(head, &bytes)?;
        let (headers, actual_message) = std::str::from_utf8(&bytes)?
            .split_once("\n\n")
            .context("ordinary sync commit lacks a message boundary")?;
        let identity = format!("{SYNC_AUTHOR} <{SYNC_EMAIL}> ");
        let mut identities = headers.lines().skip(1 + commit.parents.len());
        let author = identities
            .next()
            .and_then(|line| line.strip_prefix("author "));
        let committer = identities
            .next()
            .and_then(|line| line.strip_prefix("committer "));
        ensure!(
            commit.tree == tree
                && commit
                    .parents
                    .iter()
                    .map(String::as_str)
                    .eq(std::iter::once(parent).chain(merge_parent))
                && author.is_some_and(|author| author.starts_with(&identity))
                && committer.is_some_and(|committer| committer.starts_with(&identity))
                && identities.next().is_none()
                && actual_message == format!("{message}\n"),
            "ordinary task sync could not prove ownership of the new commit; preserve and review it before publishing manually"
        );
        data.record_owned_sync_commit(head)
    }

    fn audit_snapshot(&self, data: &DataWorktree, object: &str) -> Result<()> {
        git::admit_ordinary_data_tree(
            &data.checkout,
            &self.service.project_id,
            object,
            self.remaining()?.deadline,
            None,
        )
    }

    fn admit_incoming(&self, data: &DataWorktree, local: &str, peer: &str) -> Result<()> {
        ensure!(
            self.git(
                data,
                &[
                    "diff-tree",
                    "-r",
                    "--name-only",
                    "--no-renames",
                    local,
                    peer,
                    "--",
                    ".engram/.gitattributes",
                    ".engram/.gitignore",
                ],
                false,
            )?
            .is_empty(),
            "the fetched commit changes shared Git policy; inspect it through the explicit folder exchange"
        );
        self.audit_snapshot(data, peer)
    }

    fn export(
        &self,
        data: &DataWorktree,
        profile: &FolderBasis,
    ) -> Result<Vec<task_sync::memories::WithheldKey>> {
        let now = chrono::Utc::now();
        if !profile.memory_capable {
            self.service
                .export_task_sync_data_folder(data, &profile.profile, false, None, now)?;
            return Ok(Vec::new());
        }
        let lock = memory_files::publication_lock(data)?;
        let project = &self.service.project_id;
        let before = memory_files::committed(data, project)?;
        let before = &before;
        let lock = &lock;
        let mut withheld = None;
        let accepted = &mut withheld;
        self.service.export_task_sync_data_folder(
            data,
            &profile.profile,
            false,
            Some(FolderMemoryExport {
                selected: before,
                resolutions: &BTreeMap::new(),
                recovery_basis_matches: None,
                admit: Box::new(move |captured| {
                    let conflicts = captured
                        .withheld
                        .iter()
                        .filter(|held| before.contains_key(&held.key))
                        .map(|held| format!("{}: {}", held.key, held.reason))
                        .collect::<Vec<_>>();
                    ensure!(
                        conflicts.is_empty(),
                        "sync_memory_conflict: {}",
                        conflicts.join(", ")
                    );
                    let mut after = before.clone();
                    after.extend(captured.keys);
                    let bytes = memory_files::preflight_component(
                        data, before, &after, false, project, None,
                    )?;
                    let files = memory_files::publication_changes(data, before, &after, false)?;
                    Ok(FolderMemoryAdmission {
                        bytes,
                        files,
                        clear_capability_staging: None,
                        publish: Box::new(move |task_bytes| {
                            memory_files::write_admitted(
                                data,
                                before,
                                &after,
                                false,
                                lock,
                                project,
                                usize::try_from(task_bytes)?,
                            )?;
                            *accepted = Some(captured.withheld);
                            Ok(())
                        }),
                    })
                }),
            }),
            now,
        )?;
        withheld.context("memory export did not select its files")
    }

    fn import(&self, data: &DataWorktree, profile: &FolderBasis) -> Result<FolderImportReceipt> {
        let lock = profile
            .memory_capable
            .then(|| memory_files::publication_lock(data))
            .transpose()?;
        let (keys, bytes) = if lock.is_some() {
            data.revalidate_within(self.budget()?)?;
            let commit = data
                .head_commit()
                .context("data head missing before memory import")?;
            let selected = git::read_memory_snapshot_with_bytes(
                &data.checkout,
                &self.service.project_id,
                commit,
                self.remaining()?.deadline,
            )?;
            data.revalidate_within(self.budget()?)?;
            selected
        } else {
            (BTreeMap::new(), 0)
        };
        let options = BTreeMap::new();
        self.service.import_task_sync_data_folder(
            data,
            None,
            false,
            false,
            lock.as_ref()
                .map(|fence| MemoryImport {
                    fence,
                    selected: &keys,
                    options: &options,
                    bytes,
                })
                .as_ref(),
            chrono::Utc::now(),
        )
    }

    fn status(&self, data: &DataWorktree) -> Result<Vec<u8>> {
        let overrides = data.filter_overrides_within(self.budget()?)?;
        let mut args = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        args.extend([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ]);
        git::query_listing(
            &data.checkout,
            &args,
            self.remaining()?.deadline,
            git::MAX_LISTING_BYTES,
        )
    }

    async fn commit(
        &self,
        data: &DataWorktree,
        message: &str,
        expected: Option<&BTreeMap<String, Option<String>>>,
        merge: Option<MergeEnvelope<'_>>,
    ) -> Result<()> {
        data.revalidate_within(self.budget()?)?;
        let parent = data.head_commit().context("sync commit parent missing")?;
        let verify_staged = |expected: &BTreeMap<String, Option<String>>, require_all| {
            git::matches_staged_export(
                &data.checkout,
                parent,
                expected,
                self.remaining()?.deadline,
                require_all,
            )
        };
        if let Some(expected) = expected {
            ensure!(
                verify_staged(expected, false)?,
                "the existing index differs from the retained export; preserve the staged changes before syncing"
            );
        }
        self.effect(data, &["add", "--all", "--", ".engram"])
            .await?;
        let mut tree = self.git(data, &["write-tree"], false)?;
        if let Some(merge) = &merge {
            let before = std::str::from_utf8(tree.trim_ascii())?;
            let (root, count) = git::merged_task_membership(
                &data.checkout,
                &self.service.project_id,
                &merge.source.commit,
                merge.source.inventory_root,
                merge.source.task_count,
                before,
                self.remaining()?.deadline,
            )?;
            let envelope = directory::merged_envelope(merge.local, merge.fetched, root, count)?;
            directory::replace_merge_envelope(data, &envelope)?;
            self.effect(data, &["add", "--", ".engram/project.json"])
                .await?;
            tree = self.git(data, &["write-tree"], false)?;
        }
        let tree_id = std::str::from_utf8(tree.trim_ascii())?;
        if let Some(expected) = expected {
            ensure!(
                verify_staged(expected, true)?,
                "the staged files do not match the retained export; preserve the staged files before syncing"
            );
        } else {
            self.audit_snapshot(data, tree_id)?;
        }
        data.revalidate_within(self.budget()?)?;
        let author_name = format!("author.name={SYNC_AUTHOR}");
        let author_email = format!("author.email={SYNC_EMAIL}");
        let committer_name = format!("committer.name={SYNC_AUTHOR}");
        let committer_email = format!("committer.email={SYNC_EMAIL}");
        self.effect(
            data,
            &[
                "-c",
                &author_name,
                "-c",
                &author_email,
                "-c",
                &committer_name,
                "-c",
                &committer_email,
                "-c",
                "i18n.commitEncoding=UTF-8",
                "commit",
                "--quiet",
                "--no-verify",
                "--no-gpg-sign",
                "--cleanup=verbatim",
                "-m",
                message,
            ],
        )
        .await?;
        self.record_owned_commit(
            data,
            parent,
            merge.as_ref().map(|merge| merge.peer),
            tree_id,
            message,
        )
    }

    async fn merge(&self, data: &DataWorktree, local: &str, remote: &str) -> Result<()> {
        let common = self.git(data, &["merge-base", local, remote], false)?;
        if common.trim_ascii() == remote.as_bytes() {
            return Ok(());
        }
        self.admit_incoming(data, local, remote)?;
        data.revalidate_within(self.budget()?)?;
        if common.trim_ascii() == local.as_bytes() {
            return self
                .effect(
                    data,
                    &["merge", "--ff-only", "--no-overwrite-ignore", remote],
                )
                .await;
        }
        let common = std::str::from_utf8(common.trim_ascii())?;
        let ours =
            git::changed_task_ids(&data.checkout, common, local, self.remaining()?.deadline)?;
        let theirs =
            git::changed_task_ids(&data.checkout, common, remote, self.remaining()?.deadline)?;
        let overlap = ours
            .intersection(&theirs)
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>();
        data.revalidate_within(self.budget()?)?;
        ensure!(
            self.git(
                data,
                &[
                    "config",
                    "--name-only",
                    "--get-regexp",
                    r"^merge\..*\.driver$"
                ],
                true,
            )?
            .is_empty(),
            "ordinary task sync cannot run configured custom Git merge drivers; resolve and commit the data merge yourself, then rerun sync"
        );
        let overrides = data.filter_overrides_within(self.budget()?)?;
        let mut args = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        args.extend([
            "-c",
            "user.name=Engram task sync",
            "-c",
            "user.email=task-sync@engram.invalid",
            "merge",
            "--no-commit",
            "--no-ff",
            "--no-edit",
            "--no-overwrite-ignore",
            remote,
        ]);
        let conflicted =
            exchange::ordinary::merge(&data.checkout, &args, self.remaining()?, self.cancel)
                .await?;
        let merge_head = self.git(
            data,
            &["rev-parse", "--quiet", "--verify", "MERGE_HEAD"],
            true,
        )?;
        ensure!(
            merge_head.trim_ascii() == remote.as_bytes()
                && (!conflicted
                    || !self
                        .git(
                            data,
                            &["diff", "--name-only", "--diff-filter=U", "-z"],
                            false
                        )?
                        .is_empty()),
            "Git did not complete the selected data merge; preserve the checkout before retrying"
        );
        ensure!(
            overlap.is_empty(),
            "both data tips changed {}; select the task files and run engram task-sync sync --reconciled",
            overlap.join(", ")
        );
        self.finish_merge(data, local, remote, false).await
    }

    async fn finish_merge(
        &self,
        data: &DataWorktree,
        local: &str,
        remote: &str,
        edited: bool,
    ) -> Result<()> {
        let unresolved = self.git(
            data,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
            false,
        )?;
        let conflicts = unresolved
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty() && *path != b".engram/project.json")
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>();
        ensure!(
            edited || conflicts.is_empty(),
            "data merge conflicts in {}; edit the data files and run engram task-sync sync --reconciled",
            conflicts.join(", ")
        );
        let before = self.git(
            data,
            &["show", &format!("{local}:.engram/project.json")],
            false,
        )?;
        let fetched = self.git(
            data,
            &["show", &format!("{remote}:.engram/project.json")],
            false,
        )?;
        let profile = directory::folder_basis_from_bytes(&before)?;
        ensure!(
            profile.project_id == self.service.project_id,
            "task merge basis belongs to a different project"
        );
        if profile.incremental.is_none() {
            return Err(directory::merge_requires_incremental_basis().into());
        }
        let source = self
            .service
            .task_sync_folder_head(&profile.profile, chrono::Utc::now())?
            .and_then(|(_, head)| head.source)
            .ok_or_else(directory::merge_requires_incremental_basis)?;
        if unresolved
            .split(|byte| *byte == 0)
            .any(|path| path == b".engram/project.json")
        {
            directory::replace_merge_envelope(data, &before)?;
        }
        self.commit(
            data,
            "Reconcile project data changes",
            None,
            Some(MergeEnvelope {
                peer: remote,
                local: &before,
                fetched: &fetched,
                source,
            }),
        )
        .await
    }

    fn record_import(receipt: &mut DataSyncReceipt, imported: FolderImportReceipt) -> bool {
        let partial = imported.memory_partial
            || imported
                .result
                .tasks
                .iter()
                .any(|task| task.status != TaskSyncTaskStatus::Applied);
        receipt.imported = Some(imported);
        if partial {
            receipt.pending =
                Some("Resolve the held or refused folder changes and rerun sync.".into());
            receipt.resume_command = Some("engram task-sync sync");
        }
        partial
    }

    async fn run(&self) -> Result<DataSyncReceipt> {
        let mut selected = self.select()?;
        let data = &selected;
        let head = data.head_commit().context(
            "commit an initialized data folder and enrol it through task-sync import before sync",
        )?;
        let profile = self.profile(data, head)?;
        let target = ExchangeTarget::capture_ordinary_remaining(
            &TargetSelection {
                repository: data.checkout.clone(),
                remote: self.options.remote.clone(),
                reference: data.reference.clone(),
            },
            self.remaining()?,
            self.cancel,
        )
        .await?;
        let scope = |profile: &FolderBasis| {
            if profile.memory_capable {
                "tasks_and_memories"
            } else {
                "task_planning_only"
            }
        };
        let mut receipt = DataSyncReceipt {
            scope: scope(&profile),
            local_head: head.into(),
            remote_head: None,
            exported: false,
            memory_withheld: Vec::new(),
            imported: None,
            pushed: false,
            dry_run: self.options.dry_run,
            resume_command: None,
            pending: None,
        };
        if self.options.dry_run {
            receipt.remote_head =
                exchange::ordinary::observe(&target, self.remaining()?, self.cancel).await?;
            return Ok(receipt);
        }
        let custody = task_sync::retention::audit::AuditReservation::acquire(&data.git_directory)?;
        custody.require_idle_for_ordinary_sync()?;
        self.service
            .read_store_at(chrono::Utc::now())?
            .require_settled_task_sync_steps(&self.service.project_id, &data.git_directory)?;
        let merge_head = self.git(
            data,
            &["rev-parse", "--quiet", "--verify", "MERGE_HEAD"],
            true,
        )?;
        let mut imported_tip = None;
        if !merge_head.is_empty() {
            ensure!(
                self.options.reconciled,
                "finish the data merge with engram task-sync sync --reconciled"
            );
            let remote = std::str::from_utf8(&merge_head)?.trim();
            data.check_commit_within(remote, self.budget()?)?;
            self.finish_merge(data, head, remote, true).await?;
            selected = self.select()?;
        } else {
            ensure!(
                !self.options.reconciled,
                "there is no data merge to reconcile"
            );
            let (_, recorded) = self
                .service
                .task_sync_folder_head(&profile.profile, chrono::Utc::now())?
                .context(
                    "import the committed folder once to enroll this host before running sync",
                )?;
            let last_source = recorded.source.as_ref().context(
                "establish the first import basis with task-sync import before running sync",
            )?;
            let now = chrono::Utc::now();
            let mut export = self.service.sync_export_state(data, now)?;
            if matches!(export, SyncExport::None) {
                ensure!(
                    self.status(data)?.is_empty(),
                    "preserve uncommitted data-worktree files before syncing"
                );
                if last_source.commit == head {
                    receipt.memory_withheld = self.export(data, &profile)?;
                    export = self.service.sync_export_state(data, chrono::Utc::now())?;
                }
            }
            if let SyncExport::Commit(postimages) = export {
                let status = self.status(data)?;
                let paths_on_disk = status
                    .split(|byte| *byte == 0)
                    .filter(|entry| !entry.is_empty())
                    .map(|entry| {
                        let path = entry.get(3..).context("malformed Git status")?;
                        Ok(std::str::from_utf8(path)?)
                    })
                    .collect::<Result<BTreeSet<_>>>()?;
                ensure!(
                    paths_on_disk.len() == postimages.len()
                        && paths_on_disk.iter().all(|path| path
                            .strip_prefix(".engram/")
                            .is_some_and(|path| postimages.contains_key(path))),
                    "preserve other uncommitted data-worktree files before syncing"
                );
                self.commit(data, "Export project data changes", Some(&postimages), None)
                    .await?;
                receipt.exported = true;
                selected = self.select()?;
                export = self
                    .service
                    .sync_export_state(&selected, chrono::Utc::now())?;
                ensure!(
                    matches!(export, SyncExport::Import),
                    "the committed export no longer matches its retained publication"
                );
            }
            if matches!(export, SyncExport::Import) {
                let local = selected
                    .head_commit()
                    .context("local export commit is missing")?;
                let profile = self.profile(&selected, local)?;
                receipt.local_head = local.to_owned();
                if Self::record_import(&mut receipt, self.import(&selected, &profile)?) {
                    return Ok(receipt);
                }
                imported_tip = Some(local.to_owned());
            }
        }
        let mut previous_remote = None;
        for attempt in 0..2 {
            let data = &selected;
            let local = data
                .head_commit()
                .context("data head missing after export")?;
            let remote =
                exchange::ordinary::observe(&target, self.remaining()?, self.cancel).await?;
            receipt.remote_head = remote.clone();
            if attempt != 0 && (remote.is_none() || remote == previous_remote) {
                return Ok(Self::publication_basis_changed(receipt));
            }
            if let Some(remote) = remote.as_deref().filter(|remote| *remote != local) {
                exchange::ordinary::fetch(&target, remote, self.remaining()?, self.cancel).await?;
            }
            if let (Some(previous), Some(current)) = (previous_remote.as_deref(), remote.as_deref())
                && !exchange::ordinary::is_ancestor(
                    &data.checkout,
                    previous,
                    current,
                    self.remaining()?,
                    self.cancel,
                )
                .await?
            {
                return Ok(Self::publication_basis_changed(receipt));
            }
            if let Some(remote) = remote.as_deref().filter(|remote| *remote != local) {
                data.check_commit_within(remote, self.budget()?)?;
                self.profile(data, remote)?;
                self.merge(data, local, remote).await?;
                selected = self.select()?;
            }
            let data = &selected;
            let local = data
                .head_commit()
                .context("data head missing after merge")?;
            let profile = self.profile(data, local)?;
            receipt.scope = scope(&profile);
            receipt.local_head = local.to_owned();
            let recorded = self
                .service
                .task_sync_folder_head(&profile.profile, chrono::Utc::now())?;
            let already_imported = recorded.is_some_and(|(_, head)| {
                head.reason == FolderHeadReason::Import
                    && head.pending.is_empty()
                    && matches!(&head.disk, FolderDisk::Committed { commit } if commit == local)
                    && head.source.is_some_and(|source| source.commit == local)
            });
            if imported_tip.as_deref() != Some(local)
                && (!already_imported || profile.memory_capable)
            {
                if Self::record_import(&mut receipt, self.import(data, &profile)?) {
                    return Ok(receipt);
                }
                imported_tip = Some(local.to_owned());
            }
            if self.options.no_push || remote.as_deref() == Some(local) {
                if remote.as_deref() != Some(local) {
                    receipt.resume_command = Some("engram task-sync sync");
                }
                return Ok(receipt);
            }
            data.revalidate_within(self.budget()?)?;
            ensure!(
                self.status(data)?.is_empty(),
                "the imported data worktree changed; preserve its edits before pushing"
            );
            self.admit_publication(data, local, remote.as_deref())?;
            match exchange::ordinary::push(
                &target,
                local,
                remote.as_deref(),
                self.remaining()?,
                self.cancel,
            )
            .await
            {
                Ok(()) => {
                    receipt.pushed = true;
                    return Ok(receipt);
                }
                Err(RunError::NonFastForward | RunError::RefUpdateRejected) if attempt == 0 => {
                    previous_remote = remote;
                }
                Err(error) => {
                    receipt.pending = Some(error.to_string());
                    receipt.resume_command = Some("engram task-sync sync");
                    return Ok(receipt);
                }
            }
        }
        anyhow::bail!(
            "the data branch advanced again; preserve the imported commit and rerun engram task-sync sync"
        )
    }

    fn publication_basis_changed(mut receipt: DataSyncReceipt) -> DataSyncReceipt {
        receipt.pending = Some(
            "the destination did not advance from the audited data tip; inspect the remote data branch before retrying"
                .into(),
        );
        receipt.resume_command = Some("engram task-sync sync");
        receipt
    }
}
