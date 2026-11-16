//! Construct exact operation-owned Git bytes without advancing the data branch.

use super::{DataWorktree, QUERY_BOUND, Result, ensure, git, oid_line, query};
use crate::task_sync::{ledger::RetainedCommit, retention};
use chrono::{DateTime, Utc};
use std::fmt::Write;

/// Complete caller selection. A fixed whole-second date and exact message keep
/// the same Git object across a lost response; no user/global Git identity is
/// consulted. This transient request grants no commit or publication authority.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct StagedCommitRequest {
    pub operation: uuid::Uuid,
    pub tree: String,
    pub expected_head: Option<String>,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: DateTime<Utc>,
    pub message: String,
}

impl StagedCommitRequest {
    fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(!self.operation.is_nil(), "missing data commit operation");
        super::super::ledger::oid(&self.tree)?;
        ensure!(self.parents.len() <= 2, "data commit has too many parents");
        ensure!(
            match &self.expected_head {
                Some(head) => self.parents.first() == Some(head),
                None => self.parents.is_empty(),
            },
            "data commit must retain the selected local head as first parent"
        );
        for (index, parent) in self.parents.iter().enumerate() {
            super::super::ledger::oid(parent)?;
            ensure!(
                parent.len() == self.tree.len() && !self.parents[..index].contains(parent),
                "data commit has mixed object formats or repeated parents"
            );
        }
        for text in [&self.author_name, &self.author_email] {
            ensure!(
                !text.is_empty()
                    && text.trim() == text
                    && text.len() <= 256
                    && !text.chars().any(char::is_control)
                    && !text.contains(['<', '>']),
                "data commit author must be bounded single-line identity text"
            );
        }
        ensure!(
            !self.author_email.chars().any(char::is_whitespace),
            "data commit email contains whitespace"
        );
        ensure!(
            self.authored_at.timestamp() >= 0 && self.authored_at.timestamp_subsec_nanos() == 0,
            "data commit requires a fixed whole-second Git date"
        );
        ensure!(
            !self.message.trim().is_empty()
                && self.message.len() <= 64 * 1024
                && self.message.ends_with('\n')
                && !self.message.contains('\0'),
            "data commit requires a bounded nonempty message ending with newline"
        );
        let mut bytes = format!("tree {}\n", self.tree);
        for parent in &self.parents {
            writeln!(bytes, "parent {parent}")?;
        }
        let identity = format!(
            "{} <{}> {} +0000",
            self.author_name,
            self.author_email,
            self.authored_at.timestamp()
        );
        write!(
            bytes,
            "author {identity}\ncommitter {identity}\n\n{}",
            self.message
        )?;
        Ok(bytes.into_bytes())
    }
}

impl DataWorktree {
    /// Create and retain the caller's exact staged data commit, without staging
    /// files, changing the index or advancing any branch. The later explicit
    /// `advance_retained` step remains separate. No remote is contacted.
    ///
    /// Replay returns the original retained commit after verifying its pin and
    /// raw bytes. Missing or partial retention is refused rather than repaired.
    /// Only the selected operation's private ref can be created. Invocation
    /// grants no commit or push authority; the caller must already hold it.
    ///
    /// # Errors
    /// Refuses changed requests, index or branch, non-data trees or parents,
    /// foreign administration, incomplete retention and bounded Git failures.
    pub fn create_retained(&self, request: &StagedCommitRequest) -> Result<RetainedCommit> {
        let bytes = request.bytes()?;
        let oid = oid_line(&git::query(
            &self.checkout,
            &["hash-object", "-t", "commit", "--stdin"],
            false,
            Some(&bytes),
            QUERY_BOUND,
        )?)?;
        let observed = git::observe_commit(&oid, &bytes)?;
        let selected = RetainedCommit {
            oid,
            tree: observed.tree,
            parents: observed.parents,
            retention_ref: format!(
                "refs/engram/task-sync/exchange/{}/commit",
                request.operation
            ),
            raw_bytes_fingerprint: observed.raw_bytes_fingerprint,
        };
        selected.validate()?;
        let current = self.observe_advance(&selected, request.expected_head.as_deref())?;
        let guard = retention::audit::AuditReservation::acquire(&current.git_directory)?;
        current.check_staged_commit(request)?;
        if let Some(retained) =
            retention::commit_selection(&current.git_directory, request.operation)?
        {
            ensure!(
                retained == selected,
                "data commit request differs from its retained selection"
            );
            current.verify_created(&retained, &bytes)?;
            return Ok(retained);
        }
        ensure!(
            query(
                &current.checkout,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(symref)",
                    &selected.retention_ref
                ],
                false
            )?
            .is_empty(),
            "unrecorded data commit pin exists; preserve it"
        );
        guard.reserve_artifact(
            request.operation,
            retention::audit::journal::Family::Commit,
            &selected,
        )?;
        let created = git::query(
            &current.checkout,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            false,
            Some(&bytes),
            QUERY_BOUND,
        )?;
        ensure!(
            oid_line(&created)? == selected.oid,
            "created data commit differs from exact input"
        );
        current.check_commit(&selected.oid)?;
        let current = current.observe_advance(&selected, request.expected_head.as_deref())?;
        current.check_staged_commit(request)?;
        retention::reserve_commit(&current.git_directory, request.operation, &selected, &guard)?;
        query(
            &current.checkout,
            &[
                "update-ref",
                "--no-deref",
                &selected.retention_ref,
                &selected.oid,
                &"0".repeat(selected.oid.len()),
            ],
            false,
        )?;
        current.verify_created(&selected, &bytes)?;
        Ok(selected)
    }

    fn check_staged_commit(&self, request: &StagedCommitRequest) -> Result<()> {
        ensure!(
            query(&self.checkout, &["cat-file", "-t", &request.tree], false)? == b"tree\n",
            "data commit requires an exact tree object"
        );
        self.check_root_tree(&request.tree)?;
        for parent in &request.parents {
            self.check_commit(parent)?;
        }
        ensure!(
            self.has_staged_tree(&request.tree)?,
            "staged data tree changed; preserve the selected commit request"
        );
        Ok(())
    }

    fn verify_created(&self, retained: &RetainedCommit, bytes: &[u8]) -> Result<()> {
        ensure!(
            query(
                &self.checkout,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(symref)",
                    &retained.retention_ref
                ],
                false
            )? == format!("{} \n", retained.oid).as_bytes(),
            "data commit pin is missing or changed; preserve uncertainty"
        );
        ensure!(
            query(
                &self.checkout,
                &["cat-file", "commit", &retained.oid],
                false
            )? == bytes,
            "data commit raw bytes differ from the selected request"
        );
        Ok(())
    }
}
