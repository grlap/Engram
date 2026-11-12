//! Advance only to an existing operation-owned commit. Creating the commit and
//! recording the ledger outcome remain separate, explicit caller operations.

use super::{Context, DataWorktree, Result, ensure, oid_line, query};
use crate::task_sync::{ledger::RetainedCommit, retention};

impl DataWorktree {
    /// Fast-forward this data branch to an already retained staged snapshot.
    /// Repeating the same selection after a lost response observes the original
    /// commit; it never recreates commit metadata. No files are staged, no
    /// worktree bytes are changed and no remote is contacted.
    ///
    /// Invocation grants no commit or publication authority. The caller owns
    /// that authority and records the existing ledger transition afterwards.
    ///
    /// # Errors
    /// Refuses missing/changed retention, a different staged tree or branch,
    /// non-data content and a commit that does not descend from the selected
    /// previous head. An agent-resolved commit may retain both data parents.
    /// Ref changes and later observations are not a filesystem/SQLite transaction;
    /// preserve the selection if a failure leaves the outcome uncertain.
    pub fn advance_retained(
        &self,
        retained: &RetainedCommit,
        expected_head: Option<&str>,
    ) -> Result<Self> {
        retained.validate()?;
        if let Some(head) = expected_head {
            super::super::ledger::oid(head)?;
        }
        let current = self.observe_advance(retained, expected_head)?;
        let _reservation = retention::audit::AuditReservation::acquire(&current.git_directory)?;
        let operation = retained
            .retention_ref
            .strip_prefix("refs/engram/task-sync/exchange/")
            .and_then(|value| value.strip_suffix("/commit"))
            .context("invalid retained data commit ref")?;
        let operation = uuid::Uuid::parse_str(operation)?;
        ensure!(
            retained.retention_ref == format!("refs/engram/task-sync/exchange/{operation}/commit")
                && retention::commit_selection(&current.git_directory, operation)?.as_ref()
                    == Some(retained),
            "data commit selection is missing or changed; preserve the original operation"
        );
        let pin = query(
            &current.checkout,
            &[
                "for-each-ref",
                "--format=%(objectname) %(symref)",
                &retained.retention_ref,
            ],
            false,
        )?;
        ensure!(
            pin == format!("{} \n", retained.oid).as_bytes(),
            "retained data commit pin changed"
        );
        let bytes = query(
            &current.checkout,
            &["cat-file", "commit", &retained.oid],
            false,
        )?;
        let observed = super::super::git::observe_commit(&retained.oid, &bytes)?;
        ensure!(
            observed.tree == retained.tree
                && observed.parents == retained.parents
                && observed.raw_bytes_fingerprint == retained.raw_bytes_fingerprint,
            "retained data commit bytes changed"
        );
        current.check_commit(&retained.oid)?;
        if let Some(head) = expected_head {
            ensure!(
                oid_line(&query(
                    &current.checkout,
                    &["merge-base", head, &retained.oid],
                    false
                )?)? == head,
                "retained data commit is not a fast-forward of the selected previous head"
            );
        }
        ensure!(
            current.has_staged_tree(&retained.tree)?,
            "staged data tree differs from the retained commit; preserve local changes"
        );
        let current = current.observe_advance(retained, expected_head)?;
        if current.head.as_deref() != Some(&retained.oid) {
            let absent = "0".repeat(retained.oid.len());
            query(
                &current.checkout,
                &[
                    "update-ref",
                    "--no-deref",
                    &current.reference,
                    &retained.oid,
                    expected_head.unwrap_or(&absent),
                ],
                false,
            )
            .context(
                "data branch advance refused; inspect the original selection before retrying",
            )?;
        }
        let after = current.observe_advance(retained, expected_head)?;
        ensure!(
            after.head.as_deref() == Some(&retained.oid),
            "data branch advance is unconfirmed"
        );
        ensure!(
            after.has_staged_tree(&retained.tree)?,
            "data index changed after branch advance; preserve the original operation"
        );
        Ok(after)
    }

    pub(super) fn has_staged_tree(&self, tree: &str) -> Result<bool> {
        // write-tree populates the real index's cache-tree even when the
        // subsequent comparison refuses. Compare cached object entries only;
        // intent-to-add entries are unstaged and remain untouched.
        Ok(query(
            &self.checkout,
            &[
                "diff-index",
                "--cached",
                "--raw",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ita-invisible-in-index",
                tree,
                "--",
            ],
            false,
        )?
        .is_empty())
    }

    pub(super) fn observe_advance(
        &self,
        retained: &RetainedCommit,
        expected_head: Option<&str>,
    ) -> Result<Self> {
        let branch = self
            .reference
            .strip_prefix("refs/heads/")
            .context("data branch ref missing")?;
        let current = Self::observe(&self.code_checkout, &self.checkout, branch)?;
        ensure!(
            current.git_directory == self.git_directory
                && current.common_directory == self.common_directory,
            "data worktree administration changed before branch advance"
        );
        ensure!(
            current.head.as_deref() == expected_head
                || current.head.as_deref() == Some(&retained.oid),
            "data branch changed before advance; preserve the competing head"
        );
        Ok(current)
    }
}
