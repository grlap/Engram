//! Admission and explicit initialization of the separate task/evidence checkout. Observing this
//! value grants no commit or push authority and never changes the code checkout.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{git, setup::PrivateDirectory};

/// The forward-only way past another host's commits merged in before this
/// host exported its own changes: a new commit sets their task files aside,
/// this host exports on top, and reverting the set-aside commit brings them
/// back for the import. The data branch only moves forward. `commit` is the
/// folder this host last recorded, when it was a commit.
pub(crate) fn set_aside_exit(commit: Option<&str>) -> String {
    let source = commit.map_or_else(
        || "COMMIT, the data branch's commit that holds this host's last export".to_owned(),
        str::to_owned,
    );
    format!(
        "set the other host's task changes aside without moving the branch back: in the data worktree run `git restore --source={source} --staged --worktree -- .engram/tasks` and commit that; import, then export and commit; then `git revert --no-edit` the set-aside commit, resolving any conflict on a task both hosts changed as an agent merge and committing it, and import"
    )
}

/// The fixed bound of one local administration query.
pub(crate) const QUERY_BOUND: Duration = crate::operation_time::MINIMUM;

/// How long the local queries of one observation may take. `PerQuery` keeps
/// each query's own fixed bound. `Until` caps every query at the time left in
/// the caller's operation and refuses with a deadline once that time is spent,
/// so an observation never outlasts the operation that requested it.
#[derive(Clone, Copy, Debug)]
pub enum Budget {
    PerQuery,
    Until(Instant),
    /// As `Until`, while an enclosing read owns the operation warning.
    ObservedUntil(Instant),
}

impl Budget {
    /// The budget of an operation that started at `started` with `deadline`.
    ///
    /// # Errors
    /// Refuses a deadline that cannot be represented.
    pub fn within(started: Instant, deadline: Duration) -> Result<Self> {
        started
            .checked_add(deadline)
            .map(Self::Until)
            .context("operation deadline cannot be represented")
    }

    pub(crate) fn observed_within(
        started: Instant,
        deadline: Duration,
        _warning: &crate::operation_time::Warning,
    ) -> Result<Self> {
        started
            .checked_add(deadline)
            .map(Self::ObservedUntil)
            .context("operation deadline cannot be represented")
    }

    fn bound(self) -> Result<Duration> {
        let (Self::Until(end) | Self::ObservedUntil(end)) = self else {
            return Ok(QUERY_BOUND);
        };
        let now = Instant::now();
        #[cfg(test)]
        let now = now + budget_probe::before_query();
        let left = end.saturating_duration_since(now);
        if left.is_zero() {
            return Err(git::QueryFailure::Deadline.into());
        }
        let bound = left.min(QUERY_BOUND);
        #[cfg(test)]
        budget_probe::record(bound);
        Ok(bound)
    }
}

mod advance;
mod commit;
mod evidence;
mod initialize;
mod project;
pub use commit::StagedCommitRequest;
pub use evidence::PackFile;
pub use initialize::{Initialization, initialize};

/// Persistent project metadata, outside build output and task-retention paths.
pub const DIRECTORY_NAME: &str = ".engram-data";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataWorktree {
    pub code_checkout: PathBuf,
    pub checkout: PathBuf,
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
    pub reference: String,
    pub head: Option<String>,
}

impl DataWorktree {
    /// Observe the checkout selected by a retained binding. The historical
    /// `main_checkout` field keeps its wire name; new bindings select the data
    /// checkout there, never the code checkout.
    ///
    /// # Errors
    /// Refuses code/foreign checkout selection and changed administration.
    pub fn from_binding(binding: &super::ledger::Binding) -> Result<Self> {
        Self::from_binding_within(binding, Budget::PerQuery)
    }

    /// [`Self::from_binding`] within an operation's remaining budget.
    ///
    /// # Errors
    /// As [`Self::from_binding`], and a deadline once the budget is spent.
    pub fn from_binding_within(binding: &super::ledger::Binding, budget: Budget) -> Result<Self> {
        let checkout = &binding.repository.main_checkout;
        ensure!(
            checkout.file_name() == Some(std::ffi::OsStr::new(DIRECTORY_NAME)),
            "exchange binding must select the dedicated .engram-data worktree"
        );
        let code = checkout
            .parent()
            .context("data checkout has no code parent")?;
        let branch = binding
            .branch
            .strip_prefix("refs/heads/")
            .context("binding requires a data branch ref")?;
        let observed = Self::observe_within(code, checkout, branch, budget)?;
        ensure!(
            observed.git_directory == std::fs::canonicalize(&binding.repository.git_directory)?,
            "data worktree administration differs from binding"
        );
        Ok(observed)
    }

    /// Capture a repository basis for a new binding without changing canonical
    /// historical bindings or granting exchange authority.
    pub fn repository_basis(&self, enrollment: uuid::Uuid) -> super::ledger::RepositoryBasis {
        super::ledger::RepositoryBasis {
            enrollment,
            main_checkout: self.checkout.clone(),
            git_directory: self.git_directory.clone(),
        }
    }

    /// The observed HEAD commit, when Git status reports nothing staged,
    /// modified or untracked among the folder's task files and envelope.
    /// Ignored files are not reported, so a complete import separately
    /// compares the commit's task files with those it reads. Policy files and
    /// other data beside them do not count. A HEAD that moved since the
    /// observation refuses rather than certifying another commit.
    ///
    /// # Errors
    /// Refuses a failed or unbounded Git status read, and drift.
    pub fn committed_folder(&self) -> Result<Option<String>> {
        let Some(head) = &self.head else {
            return Ok(None);
        };
        Ok(self.folder_changes()?.is_empty().then(|| head.clone()))
    }

    /// The observed HEAD commit, if the branch has one.
    #[must_use]
    pub fn head_commit(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// The task files and envelope that differ from HEAD: staged, modified,
    /// deleted or untracked, as paths relative to the data checkout, exactly
    /// as Git reports them. Ignored files are not listed; an export never
    /// overwrites a file whose bytes differ from what it planned over.
    ///
    /// Comparing worktree bytes lets Git apply attributes, so every configured
    /// filter driver is disabled for this read: no clean or process command
    /// runs, and a file a filter would have normalised reads as changed, never
    /// as clean. End-of-line conversion still applies, so a file that differs
    /// only in its line endings may read as clean; imports read the commit,
    /// never these bytes. The observation is checked again afterwards, so the
    /// result belongs to the HEAD it was observed with.
    ///
    /// # Errors
    /// Refuses a failed or unbounded Git read, and drift since observation.
    pub fn folder_changes(&self) -> Result<std::collections::BTreeSet<String>> {
        let paths = self.folder_changes_admitted(false)?;
        self.revalidate()?;
        Ok(paths)
    }

    /// Observe status under an existing admission. Combined exports include
    /// memories alongside the task files and envelope.
    pub(crate) fn folder_changes_admitted(
        &self,
        include_memories: bool,
    ) -> Result<std::collections::BTreeSet<String>> {
        // The same bound as every other folder listing: a status over a folder
        // at its task limit on a slow disk must finish, not time out.
        let deadline = QUERY_BOUND;
        // A folder whose every task changed is reported in full, so the status
        // takes the folder listing limit rather than the general query limit.
        let listing = |args: &[&str]| {
            let _warning = crate::operation_time::Warning::start("local Git query");
            super::git::query_listing(
                &self.checkout,
                args,
                deadline,
                super::git::MAX_LISTING_BYTES,
            )
        };
        // Only Git's own change report is read: an index entry an operator
        // marked assume-unchanged or skip-worktree is trusted as marked.
        let paths: &[&str] = if include_memories {
            &[".engram/tasks", ".engram/project.json", ".engram/memories"]
        } else {
            &[".engram/tasks", ".engram/project.json"]
        };
        let overrides = self.filter_overrides_within(Budget::PerQuery)?;
        let mut args = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        args.extend([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--",
        ]);
        args.extend_from_slice(paths);
        let status = listing(args.as_slice())?;
        let mut paths = std::collections::BTreeSet::new();
        for record in status
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let path = record
                .get(3..)
                .ok_or_else(|| anyhow::anyhow!("malformed Git status record"))?;
            paths.insert(String::from_utf8(path.to_vec())?);
        }
        Ok(paths)
    }

    /// Refuse folder files, named relative to `.engram/`, that an ignore rule
    /// would keep out of `git add`, naming the first matching rule. Only these
    /// paths are asked; a tracked file is not subject to ignore rules.
    ///
    /// # Errors
    /// Refuses an ignored file and a failed Git query.
    pub(crate) fn refuse_ignored_files<'a>(
        &self,
        files: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut input = Vec::new();
        for file in files {
            input.extend_from_slice(b".engram/");
            input.extend_from_slice(file.as_bytes());
            input.push(0);
        }
        if input.is_empty() {
            return Ok(());
        }
        // Plain check-ignore names only paths a rule effectively excludes; a
        // path the folder's own policy re-includes with `!` is not reported.
        let ignored = git::ignored_paths(&self.checkout, &input, QUERY_BOUND)?;
        let Some(path) = ignored
            .split(|byte| *byte == 0)
            .find(|path| !path.is_empty())
        else {
            return Ok(());
        };
        // Only for the refusal: which rule excludes that one path.
        let mut first = path.to_vec();
        first.push(0);
        let verbose = git::query(
            &self.checkout,
            &["check-ignore", "-v", "-z", "--stdin"],
            true,
            Some(&first),
            QUERY_BOUND,
        )?;
        // The match is its rule's source, line and pattern, then the path.
        let fields = verbose
            .split(|byte| *byte == 0)
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>();
        let path = String::from_utf8_lossy(path);
        let rule = match fields.as_slice() {
            [source, line, pattern, ..] => format!("`{pattern}` at {source}:{line}"),
            _ => "an ignore rule".to_owned(),
        };
        Err(super::TaskSyncRefusal::TaskIgnorePolicyExcludesFile
            .invalid(format!(
                "task_sync_ignore_policy_excludes_file: {path} is ignored by {rule}, so committing the export would leave it out; remove or narrow that rule, then export again"
            ))
            .into())
    }

    /// The envelope and task file paths a data commit holds, for a complete
    /// import that records the folder on disk as that commit.
    ///
    /// # Errors
    /// Refuses a failed or unbounded Git read.
    pub(crate) fn committed_task_files(
        &self,
        commit: &str,
    ) -> Result<std::collections::BTreeSet<String>> {
        super::ledger::oid(commit)?;
        let listing = git::query_listing(
            &self.checkout,
            &[
                "ls-tree",
                "-z",
                "--name-only",
                commit,
                "--",
                ".engram/project.json",
                ".engram/tasks/",
            ],
            QUERY_BOUND,
            git::MAX_LISTING_BYTES,
        )?;
        listing
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| Ok(String::from_utf8(path.to_vec())?))
            .collect()
    }

    /// Prevent worktree reads and effects from invoking configured Git filters.
    pub(crate) fn filter_overrides_within(&self, budget: Budget) -> Result<Vec<String>> {
        let configured = query_within(
            &self.checkout,
            &["config", "--name-only", "--get-regexp", r"^filter\."],
            true,
            budget,
        )?;
        let drivers = String::from_utf8(configured)?
            .lines()
            .filter_map(|key| key.strip_prefix("filter.")?.rsplit_once('.'))
            .map(|(driver, _)| driver.to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let mut overrides = Vec::new();
        for driver in &drivers {
            for (key, value) in [
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ] {
                overrides.push("-c".to_owned());
                overrides.push(format!("filter.{driver}.{key}={value}"));
            }
        }
        Ok(overrides)
    }

    /// Recheck the bound branch and commit immediately before an effect.
    pub(crate) fn check_head(&self) -> Result<()> {
        self.check_head_within(Budget::PerQuery)
    }

    fn check_head_within(&self, budget: Budget) -> Result<()> {
        PrivateDirectory::open(&self.checkout)?;
        ensure!(
            git_path_within(&self.checkout, "--git-dir", budget)? == self.git_directory,
            "data worktree administration changed since observation"
        );
        let (branch, commit) = self.observe_tip_within(budget)?;
        self.require_expected_tip(&branch, commit.as_deref())
    }

    /// Recheck the checked-out ref and tip after syncing the publication.
    /// The write phase already checked the administration directory.
    pub(crate) fn publication_tip_matches(&self) -> Result<bool> {
        if let Some(commit) = &self.head {
            let current = query(
                &self.checkout,
                &[
                    "for-each-ref",
                    "--format=%(HEAD) %(refname) %(objectname)",
                    &self.reference,
                ],
                false,
            )?;
            return Ok(current == format!("* {} {commit}\n", self.reference).as_bytes());
        }
        // An unborn branch has no ref record to list.
        let (branch, commit) = self.observe_tip()?;
        Ok(branch == self.reference && commit.is_none())
    }

    fn observe_tip(&self) -> Result<(String, Option<String>)> {
        self.observe_tip_within(Budget::PerQuery)
    }

    fn observe_tip_within(&self, budget: Budget) -> Result<(String, Option<String>)> {
        let branch = query_within(
            &self.checkout,
            &["symbolic-ref", "--quiet", "HEAD"],
            true,
            budget,
        )?;
        let branch = std::str::from_utf8(&branch)?
            .trim_end_matches('\n')
            .to_owned();
        let commit = query_within(
            &self.checkout,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            true,
            budget,
        )?;
        let commit = if commit.is_empty() {
            None
        } else {
            Some(oid_line(&commit)?)
        };
        Ok((branch, commit))
    }

    fn require_expected_tip(&self, branch: &str, commit: Option<&str>) -> Result<()> {
        if branch != self.reference || commit != self.head.as_deref() {
            let expected_tip = self.head.as_deref().unwrap_or("<unborn>");
            let observed_tip = commit.unwrap_or("<unborn>");
            let observed_branch = if branch.is_empty() {
                "<detached>"
            } else {
                branch
            };
            return Err(super::TaskSyncRefusal::IncrementalBasisUnavailable
                .invalid(format!(
                    "data worktree HEAD changed since observation: expected {} at {expected_tip}, observed {observed_branch} at {observed_tip}; verify the data branch and rerun import at the new tip. When exporting, import the new tip first",
                    self.reference,
                ))
                .into());
        }
        Ok(())
    }

    /// Recheck the worktree's administration, attached branch and HEAD before
    /// an effect. The admission itself is not repeated: one session syncs a
    /// data folder at a time, and the tip check catches any other writer.
    ///
    /// # Errors
    /// Refuses a changed administration, branch or HEAD.
    pub fn revalidate(&self) -> Result<()> {
        self.revalidate_within(Budget::PerQuery)
    }

    /// [`Self::revalidate`] within an operation's remaining budget.
    ///
    /// # Errors
    /// As [`Self::revalidate`], and a deadline once the budget is spent.
    pub fn revalidate_within(&self, budget: Budget) -> Result<()> {
        self.check_head_within(budget)
    }

    /// Bind an existing dedicated linked worktree, including an unborn orphan
    /// branch. Existing canonical repository observations are never rewritten.
    ///
    /// # Errors
    /// Refuses another repository, a code-history branch, a non-ignored
    /// location, or a staged, changed or untracked file outside the shared
    /// data directory. A committed entry outside it is refused where a commit
    /// is made, taken in, recorded or pushed, not here.
    pub fn observe(code_checkout: &Path, checkout: &Path, branch: &str) -> Result<Self> {
        Self::observe_within(code_checkout, checkout, branch, Budget::PerQuery)
    }

    fn observe_within(
        code_checkout: &Path,
        checkout: &Path,
        branch: &str,
        budget: Budget,
    ) -> Result<Self> {
        // Every query below spends the same operation budget.
        let query =
            |root: &Path, args: &[&str], missing: bool| query_within(root, args, missing, budget);
        let git_path = |root: &Path, flag: &str| git_path_within(root, flag, budget);
        PrivateDirectory::open(code_checkout)?;
        PrivateDirectory::open(checkout)?;
        let code_checkout = std::fs::canonicalize(code_checkout)?;
        let checkout = std::fs::canonicalize(checkout)?;
        ensure!(
            code_checkout != checkout,
            "data and code checkouts must be separate"
        );
        ensure!(
            checkout == code_checkout.join(DIRECTORY_NAME),
            "data worktree must use the fixed .engram-data directory in the main checkout"
        );
        ensure!(
            git_path(&code_checkout, "--show-toplevel")? == code_checkout
                && git_path(&checkout, "--show-toplevel")? == checkout,
            "select exact code and data worktree roots"
        );
        let common_directory = git_path(&code_checkout, "--git-common-dir")?;
        ensure!(
            git_path(&code_checkout, "--git-dir")? == common_directory,
            "select the main code checkout, not a linked code worktree"
        );
        check_code_path_untracked_within(&code_checkout, budget)?;
        let git_directory = git_path(&checkout, "--git-dir")?;
        ensure!(
            query(
                &checkout,
                &["config", "--bool", "--get", "core.sparseCheckout"],
                true
            )? != b"true\n",
            "data worktree must be complete; sparse checkout is not supported"
        );
        ensure!(
            git_path(&checkout, "--git-common-dir")? == common_directory
                && git_directory != common_directory,
            "data worktree must have its own administration in the code repository"
        );
        ensure!(
            !branch.is_empty() && !branch.starts_with('-'),
            "select an ordinary data branch name"
        );
        let reference = format!("refs/heads/{branch}");
        query(&checkout, &["check-ref-format", &reference], false)?;
        ensure!(
            query(&checkout, &["symbolic-ref", "--quiet", "HEAD"], false)?
                == format!("{reference}\n").as_bytes(),
            "selected branch changed; data worktree is not attached to the selected data branch"
        );
        ensure!(
            !query(
                &code_checkout,
                &["check-ignore", "--no-index", "--", DIRECTORY_NAME],
                true
            )?
            .is_empty(),
            "data worktree must be ignored by the code checkout"
        );
        let head = query(
            &checkout,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            true,
        )?;
        let head = if head.is_empty() {
            None
        } else {
            let head = oid_line(&head)?;
            Some(head)
        };
        let observed = Self {
            code_checkout,
            checkout,
            git_directory,
            common_directory,
            reference,
            head,
        };
        // HEAD's tree is not listed here: every commit this host makes or
        // takes in has its root tree checked when it is admitted.
        if let Some(head) = &observed.head {
            observed.check_orphan_within(head, budget)?;
        }
        observed.check_data_paths_within(budget)?;
        Ok(observed)
    }

    /// Admit an exact incoming or outgoing commit, not only the current HEAD.
    /// Inspect the root tree instead of enumerating every unchanged task.
    ///
    /// # Errors
    /// Refuses code ancestry and any root other than the ordinary `.engram` tree.
    pub fn check_commit(&self, commit: &str) -> Result<()> {
        self.check_commit_within(commit, Budget::PerQuery)
    }

    /// [`Self::check_commit`] within an operation's remaining budget.
    ///
    /// # Errors
    /// As [`Self::check_commit`], and a deadline once the budget is spent.
    pub fn check_commit_within(&self, commit: &str, budget: Budget) -> Result<()> {
        self.check_orphan_within(commit, budget)?;
        self.check_root_tree_within(commit, budget)
    }

    fn check_orphan_within(&self, commit: &str, budget: Budget) -> Result<()> {
        super::ledger::oid(commit)?;
        let code_head = oid_line(&query_within(
            &self.code_checkout,
            &["rev-parse", "--verify", "HEAD"],
            false,
            budget,
        )?)?;
        ensure!(
            query_within(
                &self.checkout,
                &["merge-base", commit, &code_head],
                true,
                budget
            )?
            .is_empty(),
            "data branch must have orphan history separate from code"
        );
        Ok(())
    }

    /// Refuse a commit or tree whose root holds anything but `.engram/`.
    ///
    /// # Errors
    /// Refuses another root entry and a failed Git read.
    pub(crate) fn check_root_tree(&self, tree: &str) -> Result<()> {
        self.check_root_tree_within(tree, Budget::PerQuery)
    }

    fn check_root_tree_within(&self, tree: &str, budget: Budget) -> Result<()> {
        let entries = query_within(
            &self.checkout,
            &["ls-tree", "-z", "--full-tree", tree],
            false,
            budget,
        )?;
        ensure!(
            entries.is_empty() || entries.last() == Some(&0),
            "unterminated data tree"
        );
        for entry in entries
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let split = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid data tree")?;
            let header = std::str::from_utf8(&entry[..split])?
                .split(' ')
                .collect::<Vec<_>>();
            ensure!(
                header.len() == 3
                    && header[0] == "040000"
                    && header[1] == "tree"
                    && &entry[split + 1..] == b".engram",
                "data commit contains an entry outside the .engram/ directory"
            );
            super::ledger::oid(header[2])?;
        }
        Ok(())
    }

    /// Refuse a staged, modified, deleted or untracked path outside `.engram/`,
    /// as Git reports it. Unchanged index entries and ignored files are not
    /// listed: Git never commits an ignored file, and the root tree of every
    /// commit is checked when it is admitted.
    ///
    /// # Errors
    /// Refuses an unreadable status and any changed path outside `.engram/`.
    pub fn check_data_paths(&self) -> Result<()> {
        self.check_data_paths_within(Budget::PerQuery)
    }

    fn check_data_paths_within(&self, budget: Budget) -> Result<()> {
        // Comparing worktree bytes must not run a configured clean filter.
        let overrides = self.filter_overrides_within(budget)?;
        let mut args = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        args.extend([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ]);
        let status = git::query_listing(
            &self.checkout,
            &args,
            budget.bound()?,
            git::MAX_LISTING_BYTES,
        )?;
        ensure!(
            status.is_empty() || status.last() == Some(&0),
            "unterminated data path inventory"
        );
        let mut paths = Vec::with_capacity(status.len());
        for record in status
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            paths.extend_from_slice(
                record
                    .get(3..)
                    .ok_or_else(|| anyhow::anyhow!("malformed Git status record"))?,
            );
            paths.push(0);
        }
        check_paths(&paths)
    }
}

pub(super) fn check_code_path_untracked(code: &Path) -> Result<()> {
    check_code_path_untracked_within(code, Budget::PerQuery)
}

fn check_code_path_untracked_within(code: &Path, budget: Budget) -> Result<()> {
    ensure!(
        query_within(
            code,
            &["ls-files", "-z", "--cached", "--", DIRECTORY_NAME],
            false,
            budget
        )?
        .is_empty(),
        "code index tracks the reserved .engram-data path; preserve it and resolve before exchange"
    );
    let head = query_within(
        code,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        true,
        budget,
    )?;
    if !head.is_empty() {
        let head = oid_line(&head)?;
        ensure!(
            query_within(
                code,
                &["ls-tree", "-z", &head, "--", DIRECTORY_NAME],
                false,
                budget
            )?
            .is_empty(),
            "code HEAD tracks the reserved .engram-data path; preserve it and resolve before exchange"
        );
    }
    Ok(())
}

fn check_paths(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "unterminated data path inventory"
    );
    for path in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path).context("data path must be UTF-8")?;
        ensure!(
            path.starts_with(".engram/")
                && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
                && !path.contains(['\\', '\r', '\n']),
            "data worktree contains a path outside .engram/; preserve it and resolve before exchange"
        );
    }
    Ok(())
}

fn query(root: &Path, args: &[&str], missing: bool) -> Result<Vec<u8>> {
    query_within(root, args, missing, Budget::PerQuery)
}

fn query_within(root: &Path, args: &[&str], missing: bool, budget: Budget) -> Result<Vec<u8>> {
    let bound = budget.bound()?;
    if matches!(budget, Budget::ObservedUntil(_)) {
        git::query_observed(root, args, missing, None, bound)
    } else {
        git::query(root, args, missing, None, bound)
    }
}

fn git_path(root: &Path, flag: &str) -> Result<PathBuf> {
    git_path_within(root, flag, Budget::PerQuery)
}

fn git_path_within(root: &Path, flag: &str, budget: Budget) -> Result<PathBuf> {
    let bytes = query_within(
        root,
        &["rev-parse", "--path-format=absolute", flag],
        false,
        budget,
    )?;
    let path = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context("unterminated Git administration path")?;
    ensure!(
        !path.contains(['\r', '\n', '\0']),
        "ambiguous Git administration path"
    );
    let path = root.join(path);
    PrivateDirectory::open(&path)?;
    Ok(std::fs::canonicalize(path)?)
}

fn oid_line(bytes: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(bytes)?
        .strip_suffix('\n')
        .context("unterminated Git object id")?;
    super::ledger::oid(value)?;
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests;

/// Observe the budgeted queries of this test thread. At query `index`, the
/// operation loses `stall` of its budget as if that query had been slow, so a
/// test needs no real delay to exhaust a deadline at an exact point.
#[cfg(test)]
pub(crate) mod budget_probe {
    use std::{cell::RefCell, time::Duration};

    #[derive(Default)]
    struct Probe {
        queries: usize,
        stall: Option<(usize, Duration)>,
        stalled: bool,
        skew: Duration,
        bounds: Vec<Duration>,
    }

    thread_local! {
        static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    pub(crate) fn install(stall: Option<(usize, Duration)>) {
        PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Probe {
                stall,
                ..Probe::default()
            });
        });
    }

    /// The bounds given to each budgeted query and whether the stall was
    /// reached. Removes the probe.
    pub(crate) fn finish() -> (Vec<Duration>, bool) {
        PROBE.with(|probe| {
            probe
                .borrow_mut()
                .take()
                .map_or((Vec::new(), false), |probe| (probe.bounds, probe.stalled))
        })
    }

    pub(super) fn before_query() -> Duration {
        PROBE.with(|probe| {
            let mut probe = probe.borrow_mut();
            let Some(probe) = probe.as_mut() else {
                return Duration::ZERO;
            };
            if let Some((index, stall)) = probe.stall
                && index == probe.queries
            {
                probe.skew += stall;
                probe.stalled = true;
            }
            probe.queries += 1;
            probe.skew
        })
    }

    pub(super) fn record(bound: Duration) {
        PROBE.with(|probe| {
            if let Some(probe) = probe.borrow_mut().as_mut() {
                probe.bounds.push(bound);
            }
        });
    }
}
