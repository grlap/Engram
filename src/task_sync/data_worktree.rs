//! Admission and explicit initialization of the separate task/evidence checkout. Observing this
//! value grants no commit or push authority and never changes the code checkout.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{git, setup::PrivateDirectory};

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
        ensure!(
            Self::observe_within(&self.code_checkout, &self.checkout, branch, budget)? == *self,
            "data worktree changed since observation"
        );
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
                && git_directory != common_directory
                && git_directory != git_path(&code_checkout, "--git-dir")?,
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
            check_paths(&query_within(&self.checkout, &args, false, budget)?)?;
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
