//! Pending writes inside the existing repository custody reservation. A journal
//! records intent, never a SQL commit or permission to dispatch transport.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{AuditAcknowledgement, AuditReservation, read_bytes, read_head};
use crate::task_sync::retention::write_new;
use crate::task_sync::setup::PrivateDirectory;
use crate::{CanonicalObject, ObjectId};

pub(super) const FILE: &str = "managed.json";
pub(super) const PENDING: &str = "managed.pending";
const LIMIT: usize = super::TOTAL_LIMIT;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Family {
    Candidate,
    Delta,
    Commit,
    Import,
    Publication,
    Evidence,
    Successor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteAhead {
    pub operation: uuid::Uuid,
    pub family: Family,
    /// Content comparison of the canonical typed request, separate from the
    /// minted operation id. Payload bytes remain with their artifact owner.
    pub request_fingerprint: ObjectId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    pub checkpoint: AuditAcknowledgement,
    pub revision: uuid::Uuid,
    pub previous_revision: Option<uuid::Uuid>,
    pub writes: BTreeMap<String, WriteAhead>,
}

impl WriteAhead {
    fn key(&self) -> Result<String> {
        ensure!(!self.operation.is_nil(), "missing custody write operation");
        Ok(format!(
            "{}:{}",
            self.operation,
            serde_json::to_string(&self.family)?
        ))
    }

    fn validate(&self) -> Result<()> {
        self.key()?;
        Ok(())
    }
}

impl Journal {
    fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.revision.is_nil() && self.writes.len() <= super::MAX_AUDITS,
            "invalid custody journal scope or capacity"
        );
        for (key, write) in &self.writes {
            write.validate()?;
            ensure!(*key == write.key()?, "custody write key differs");
        }
        let bytes = crate::canonical::canonical_bytes(self)?;
        ensure!(bytes.len() <= LIMIT, "custody journal exceeds byte bound");
        Ok(bytes)
    }
}

impl AuditReservation {
    pub(crate) fn activate_audit_journal(&self, audit: &super::RetainedAudit) -> Result<()> {
        let acknowledgement = audit.acknowledgement()?;
        if acknowledgement.mode == super::AuditMode::Managed {
            self.activate_journal(&acknowledgement)?;
        }
        Ok(())
    }

    pub(crate) fn resume_audit_journal(&self, audit: &super::RetainedAudit) -> Result<()> {
        let acknowledgement = audit.acknowledgement()?;
        if acknowledgement.mode == super::AuditMode::Managed
            && read_head(&self.directory)?.as_ref() == Some(&acknowledgement)
        {
            self.activate_journal(&acknowledgement)?;
        }
        Ok(())
    }

    /// Observe the managed checkpoint and pending writes without listing a
    /// directory. A partial publication is uncertainty, never a cold fallback.
    pub(crate) fn journal(&self) -> Result<Option<Journal>> {
        self.recover_pending_journal()?;
        self.journal_state()
    }

    /// Admit ordinary sync only when no managed write holds this repository.
    /// A standard sync never repairs or settles an explicit custody operation.
    pub(crate) fn require_idle_for_ordinary_sync(&self) -> Result<()> {
        let recovery = "managed task-sync custody is pending or unproved; preserve the repository and recover the exact managed_audit request before running sync";
        ensure!(pending_journal(&self.directory)?.is_none(), "{recovery}");
        let head = read_head(&self.directory)?;
        let journal = self.journal_state()?;
        if head.is_some_and(|head| head.mode == super::AuditMode::Managed) {
            let journal = journal.context(recovery)?;
            crate::task_sync::retention::validate_journal(&self.git_directory, &journal)?;
            ensure!(journal.writes.is_empty(), "{recovery}");
        } else {
            ensure!(journal.is_none(), "{recovery}");
        }
        Ok(())
    }

    fn journal_state(&self) -> Result<Option<Journal>> {
        journal_state(&self.directory)
    }

    pub(crate) fn require_audit_mode(&self, managed: bool) -> Result<()> {
        audit_mode(&self.directory, managed)
    }

    pub(crate) fn recover_pending_journal(&self) -> Result<()> {
        let Some((bytes, _)) = pending_journal(&self.directory)? else {
            return Ok(());
        };
        self.directory
            .dir()
            .rename(PENDING, self.directory.dir(), FILE)?;
        #[cfg(unix)]
        super::sync(&self.directory)?;
        ensure!(
            read_bytes(&self.directory, Path::new(FILE), LIMIT)?.as_ref() == Some(&bytes),
            "custody journal changed during recovery; recover the exact managed_audit request"
        );
        Ok(())
    }

    /// The storage owner calls this only after a complete cold audit, canonical
    /// checkpoint commit and exact acknowledgement. This method grants no SQL
    /// admission and does not infer activation from the presence of artifacts.
    pub(crate) fn activate_journal(&self, checkpoint: &AuditAcknowledgement) -> Result<()> {
        ensure!(
            checkpoint.mode == super::AuditMode::Managed
                && read_head(&self.directory)?.as_ref() == Some(checkpoint),
            "managed custody requires the acknowledged cold checkpoint"
        );
        let retained = self
            .read(&checkpoint.checkpoint)?
            .context("managed custody checkpoint event is missing")?;
        ensure!(
            retained.acknowledgement()? == *checkpoint,
            "managed custody checkpoint acknowledgement differs"
        );
        if let Some(bytes) = read_bytes(&self.directory, Path::new(FILE), LIMIT)? {
            let previous: Journal = serde_json::from_slice(&bytes)?;
            ensure!(
                previous.bytes()? == bytes,
                "previous custody journal is not canonical"
            );
            if previous.checkpoint == *checkpoint {
                self.journal()?;
                return Ok(());
            }
        }
        let journal = Journal {
            checkpoint: checkpoint.clone(),
            revision: uuid::Uuid::new_v4(),
            previous_revision: None,
            writes: BTreeMap::new(),
        };
        self.publish_journal(&journal)
    }

    /// Reserve complete first-attempt input before creating an operation's
    /// files or updating its Git pin. Audit-only repositories retain the old
    /// exhaustive contract until storage explicitly activates this journal.
    pub(crate) fn reserve_artifact(
        &self,
        operation: uuid::Uuid,
        family: Family,
        request: &impl Serialize,
    ) -> Result<()> {
        let Some(mut journal) = self.journal()? else {
            ensure!(
                read_bytes(&self.directory, Path::new(PENDING), LIMIT)?.is_none()
                    && read_head(&self.directory)?
                        .is_none_or(|head| head.mode != super::AuditMode::Managed),
                "custody activation is pending or missing; recover its exact cold checkpoint"
            );
            return Ok(());
        };
        let write = WriteAhead {
            operation,
            family,
            request_fingerprint: CanonicalObject::freeze(request)?.key().clone(),
        };
        write.validate()?;
        let key = write.key()?;
        if let Some(previous) = journal.writes.get(&key) {
            ensure!(
                previous == &write,
                "custody write differs from its exact pending request"
            );
            self.journal()?;
            return Ok(());
        }
        // A settled operation's artifact stays retained after its write leaves
        // the journal. A different request for it is refused here, before a
        // write that no artifact could ever match is published.
        if let Some(retained) = crate::task_sync::retention::retained_request_fingerprint(
            &self.git_directory,
            family,
            operation,
        )? {
            ensure!(
                retained == write.request_fingerprint,
                "the retained {family:?} artifact for operation {operation} holds request {retained}, not this request {}; preserve both inputs and retry with the exact original request, or use a new operation",
                write.request_fingerprint
            );
        }
        journal.writes.insert(key, write);
        journal.previous_revision = Some(journal.revision);
        journal.revision = uuid::Uuid::new_v4();
        self.publish_journal(&journal)
    }

    /// Remove only the writes whose exact captured journal the storage owner
    /// has checked after SQL commit. A concurrent writer or uncertain effect
    /// cannot be discarded by acknowledging an older observation.
    pub(crate) fn settle_journal(&self, observed: &Journal) -> Result<()> {
        let current = self.journal()?;
        if current.as_ref().is_some_and(|current| {
            current.checkpoint == observed.checkpoint
                && current.previous_revision == Some(observed.revision)
                && current.writes.is_empty()
        }) {
            self.journal()?;
            return Ok(());
        }
        ensure!(
            current.as_ref() == Some(observed),
            "custody journal changed before acknowledgement; retry exact request"
        );
        if observed.writes.is_empty() {
            return Ok(());
        }
        let mut next = observed.clone();
        next.writes.clear();
        next.previous_revision = Some(observed.revision);
        next.revision = uuid::Uuid::new_v4();
        self.publish_journal(&next)
    }

    fn publish_journal(&self, journal: &Journal) -> Result<()> {
        let mut bytes = journal.bytes()?;
        match read_bytes(&self.directory, Path::new(PENDING), LIMIT)? {
            Some(previous) => {
                let pending: Journal = serde_json::from_slice(&previous)?;
                ensure!(
                    pending.bytes()? == previous
                        && pending.checkpoint == journal.checkpoint
                        && pending.previous_revision == journal.previous_revision
                        && pending.writes == journal.writes,
                    "partial or different custody journal publication; preserve uncertainty"
                );
                bytes = previous;
            }
            None => write_new(&self.directory, Path::new(PENDING), &bytes)?,
        }
        self.directory
            .dir()
            .rename(PENDING, self.directory.dir(), FILE)?;
        #[cfg(unix)]
        super::sync(&self.directory)?;
        ensure!(
            read_bytes(&self.directory, Path::new(FILE), LIMIT)?.as_ref() == Some(&bytes),
            "custody journal changed during publication"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// How long a reader keeps re-reading a `managed.pending` that a live writer
/// is still publishing before it refuses as busy: the operation floor.
const PUBLICATION_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read only the checkpoint-selected journal. Audit-only repositories keep the
/// old contract; a managed head with missing journal never falls back to it.
///
/// A writer publishes `managed.pending` under the custody reservation and
/// renames it within that reservation, so a reader that meets it unreadable
/// while the reservation is held reads it again until it becomes readable or
/// leaves. A committed caller, such as a first `PushStarted`, then keeps its
/// result instead of failing on another writer's short publication.
pub(crate) fn observe(root: &Path) -> Result<Option<Journal>> {
    let started = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(5);
    observe_waiting(root, &mut || {
        if started.elapsed() >= PUBLICATION_WAIT {
            return false;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(250));
        true
    })
}

/// `wait` runs while another writer holds the reservation over an unreadable
/// `managed.pending`; it returns false to stop waiting and refuse as busy.
fn observe_waiting(root: &Path, wait: &mut dyn FnMut() -> bool) -> Result<Option<Journal>> {
    use crate::task_sync::{retention::missing, setup::PrivateDirectory};
    let git = PrivateDirectory::open(root)?;
    let directory = match git.child(Path::new(super::ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(head) = read_head(&directory)? else {
        return Ok(None);
    };
    if head.mode == super::AuditMode::AuditOnly {
        ensure!(
            read_bytes(&directory, Path::new(FILE), LIMIT)?.is_none(),
            "managed custody was replaced by an audit-only head; run named recovery"
        );
        return Ok(None);
    }
    // managed.json is atomically replaced before any reserved artifact effect.
    // A writer still holding the reservation must not block a post-commit read.
    loop {
        let unreadable = match pending_journal(&directory) {
            Ok(Some((_, pending))) => return Ok(Some(pending)),
            Ok(None) => {
                return Ok(Some(journal_state(&directory)?.context(
                    "managed custody journal is missing; run the named cold audit",
                )?));
            }
            Err(error) => error,
        };
        // A reader can arrive between create and sync of managed.pending.
        // Inspect a failed read under the writer's reservation before
        // treating it as interrupted custody that needs recovery.
        match AuditReservation::acquire(root) {
            Ok(reservation) => {
                if let Some((_, pending)) = pending_journal(&reservation.directory)? {
                    return Ok(Some(pending));
                }
                return Ok(Some(reservation.journal_state()?.context(
                    "managed custody journal is missing; run the named cold audit",
                )?));
            }
            Err(error)
                if error
                    .downcast_ref::<std::fs::TryLockError>()
                    .is_some_and(|error| matches!(error, std::fs::TryLockError::WouldBlock)) =>
            {
                if !wait() {
                    anyhow::bail!(
                        "custody journal publication stayed busy for {} s while another writer held the custody reservation; managed.pending could not be read: {unreadable:#}. Retry the exact request; if no Engram writer is running, preserve managed.pending and recover the exact managed_audit request",
                        PUBLICATION_WAIT.as_secs()
                    );
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_journal(directory: &PrivateDirectory) -> Result<Option<Journal>> {
    read_bytes(directory, Path::new(FILE), LIMIT)?
        .map(|bytes| {
            let journal: Journal = serde_json::from_slice(&bytes)?;
            ensure!(
                journal.bytes()? == bytes,
                "custody journal is not canonical"
            );
            Ok(journal)
        })
        .transpose()
}

fn check_checkpoint(directory: &PrivateDirectory, journal: &Journal) -> Result<()> {
    ensure!(
        journal.checkpoint.mode == super::AuditMode::Managed
            && read_head(directory)?.as_ref() == Some(&journal.checkpoint),
        "managed custody checkpoint moved; run a custody audit with reason managed_audit"
    );
    let acknowledgement = read_bytes(
        directory,
        Path::new(&format!("{}.ack.json", journal.checkpoint.checkpoint)),
        super::SMALL_LIMIT,
    )?
    .context("managed custody checkpoint acknowledgement missing")?;
    ensure!(
        acknowledgement == crate::canonical::canonical_bytes(&journal.checkpoint)?,
        "managed custody immutable acknowledgement differs"
    );
    Ok(())
}

fn pending_journal(directory: &PrivateDirectory) -> Result<Option<(Vec<u8>, Journal)>> {
    let Some(bytes) = read_bytes(directory, Path::new(PENDING), LIMIT)? else {
        return Ok(None);
    };
    let recovery = "partial or divergent custody journal; preserve it and recover the exact managed_audit request";
    let pending: Journal = serde_json::from_slice(&bytes).context(recovery)?;
    ensure!(pending.bytes()? == bytes, "{recovery}");
    check_checkpoint(directory, &pending)?;
    let current = read_journal(directory)?;
    let follows = match current.as_ref() {
        Some(current) if current == &pending => true,
        Some(current) if current.checkpoint == pending.checkpoint => {
            pending.previous_revision == Some(current.revision)
        }
        // A separately acknowledged cold audit starts a fresh empty journal.
        _ => pending.previous_revision.is_none() && pending.writes.is_empty(),
    };
    ensure!(follows, "{recovery}");
    Ok(Some((bytes, pending)))
}

fn journal_state(directory: &PrivateDirectory) -> Result<Option<Journal>> {
    let journal = read_journal(directory)?;
    if let Some(journal) = &journal {
        check_checkpoint(directory, journal)?;
    }
    Ok(journal)
}

fn audit_mode(directory: &PrivateDirectory, managed: bool) -> Result<()> {
    ensure!(
        managed || read_head(directory)?.is_none_or(|head| head.mode != super::AuditMode::Managed),
        "managed custody is active; run a custody audit with reason managed_audit"
    );
    Ok(())
}

pub(crate) fn require_audit_mode(root: &Path, managed: bool) -> Result<()> {
    use crate::task_sync::retention::missing;
    let git = PrivateDirectory::open(root)?;
    match git.child(Path::new(super::ROOT)) {
        Ok(directory) => audit_mode(&directory, managed),
        Err(error) if missing(&error) => Ok(()),
        Err(error) => Err(error),
    }
}
