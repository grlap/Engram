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

    /// The observed HEAD commit, when the folder's task files and envelope
    /// hold exactly its content: nothing staged, modified or untracked there.
    /// Policy files and other data beside them do not count. A HEAD that moved
    /// since the observation refuses rather than certifying another commit.
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
    /// deleted or untracked, as paths relative to the data checkout. An
    /// ignored task file or envelope counts too, so an ignore rule never
    /// hides a loose file at a folder path, which an export would otherwise
    /// count as committed; other ignored entries, which the folder's policy
    /// ignores by design, do not.
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
        // Both listings name every folder path, so they take the folder
        // listing limit rather than the general query output limit.
        let listing = |args: &[&str]| {
            let _warning = crate::operation_time::Warning::start("local Git query");
            super::git::query_listing(
                &self.checkout,
                args,
                deadline,
                super::git::MAX_LISTING_BYTES,
            )
        };
        // Status skips a file whose index entry is marked assume-unchanged or
        // skip-worktree, so such a file could hide an edit from this read.
        let paths: &[&str] = if include_memories {
            &[".engram/tasks", ".engram/project.json", ".engram/memories"]
        } else {
            &[".engram/tasks", ".engram/project.json"]
        };
        let mut index_args = vec!["ls-files", "-v", "-z", "--"];
        index_args.extend_from_slice(paths);
        for entry in listing(&index_args)?
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            ensure!(
                entry.len() > 2 && entry[1] == b' ',
                "malformed Git index result"
            );
            ensure!(
                entry[0] != b'S' && !entry[0].is_ascii_lowercase(),
                "index inspection is suppressed for {}: clear it with `git update-index --no-assume-unchanged --no-skip-worktree` on that path in the data worktree",
                String::from_utf8_lossy(&entry[2..])
            );
        }
        let configured = super::git::query(
            &self.checkout,
            &["config", "--name-only", "--get-regexp", r"^filter\."],
            true,
            None,
            deadline,
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
        let mut args = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        args.extend([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            // Each ignored file, also inside an ignored directory, which
            // `matching` would list as the directory alone.
            "--ignored=traditional",
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
            let path = String::from_utf8(path.to_vec())?;
            // The folder policy ignores entries such as desktop metadata.
            // An ignored document inside a shared component still counts.
            let folder_file = path == ".engram/project.json"
                || (include_memories
                    && path.starts_with(".engram/memories/")
                    && path.rsplit_once('.').is_some_and(|(_, ext)| ext == "json"))
                || path.strip_prefix(".engram/tasks/").is_some_and(|name| {
                    !name.contains('/')
                        && std::path::Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension == "json")
                });
            if record.starts_with(b"!! ") && !folder_file {
                continue;
            }
            paths.insert(path);
        }
        Ok(paths)
    }

    /// Recheck the bound branch and commit immediately before an effect.
    pub(crate) fn check_head(&self) -> Result<()> {
        PrivateDirectory::open(&self.checkout)?;
        ensure!(
            git_path(&self.checkout, "--git-dir")? == self.git_directory,
            "data worktree administration changed since observation"
        );
        let (branch, commit) = self.observe_tip()?;
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
        let branch = query(&self.checkout, &["symbolic-ref", "--quiet", "HEAD"], true)?;
        let branch = std::str::from_utf8(&branch)?
            .trim_end_matches('\n')
            .to_owned();
        let commit = query(
            &self.checkout,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            true,
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

    /// Re-observe the exact worktree, administration, branch and HEAD.
    ///
    /// # Errors
    /// Refuses any drift since this observation.
    pub fn revalidate(&self) -> Result<()> {
        self.revalidate_within(Budget::PerQuery)
    }

    /// [`Self::revalidate`] within an operation's remaining budget.
    ///
    /// # Errors
    /// As [`Self::revalidate`], and a deadline once the budget is spent.
    pub fn revalidate_within(&self, budget: Budget) -> Result<()> {
        let branch = self
            .reference
            .strip_prefix("refs/heads/")
            .context("data branch ref missing")?;
        let observed = Self::observe_within(&self.code_checkout, &self.checkout, branch, budget)?;
        self.require_expected_tip(&observed.reference, observed.head.as_deref())?;
        ensure!(observed == *self, "data worktree changed since observation");
        Ok(())
    }

    /// Bind an existing dedicated linked worktree, including an unborn orphan
    /// branch. Existing canonical repository observations are never rewritten.
    ///
    /// # Errors
    /// Refuses another repository, a code-history branch, a non-ignored location
    /// or any indexed/working-tree file outside the shared data directory.
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
        if let Some(head) = &observed.head {
            observed.check_commit_within(head, budget)?;
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
        self.check_root_tree_within(commit, budget)
    }

    fn check_root_tree(&self, tree: &str) -> Result<()> {
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

    /// Inspect names without comparing file content or invoking a clean filter.
    /// Ignored untracked files count too: an ignored code file must not become
    /// an unnoticed occupant of the data worktree.
    ///
    /// # Errors
    /// Refuses unreadable inventories and any file outside `.engram/`.
    pub fn check_data_paths(&self) -> Result<()> {
        self.check_data_paths_within(Budget::PerQuery)
    }

    fn check_data_paths_within(&self, budget: Budget) -> Result<()> {
        for args in [
            vec!["ls-files", "-z", "--cached"],
            vec!["ls-files", "-z", "--others", "--exclude-standard"],
            vec![
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
            ],
        ] {
            // Each inventory names every folder path, so it takes the folder
            // listing limit rather than the general query output limit.
            check_paths(&git::query_listing(
                &self.checkout,
                &args,
                budget.bound()?,
                git::MAX_LISTING_BYTES,
            )?)?;
        }
        Ok(())
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
