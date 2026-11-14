//! Pending writes inside the existing repository custody reservation. A journal
//! records intent, never a SQL commit or permission to dispatch transport.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{AuditAcknowledgement, AuditReservation, read_bytes, read_head};
use crate::task_sync::retention::write_new;

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
    /// The complete typed request, canonically encoded before any artifact
    /// effect. Content comparison is separate from the minted operation id.
    pub request: serde_json::Value,
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
        ensure!(
            crate::canonical::canonical_bytes(&self.request)?.len() <= LIMIT,
            "custody write request exceeds byte bound"
        );
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
        ensure!(
            read_bytes(&self.directory, Path::new(PENDING), LIMIT)?.is_none(),
            "custody journal publication is pending; recover its exact request"
        );
        self.journal_state()
    }

    fn journal_state(&self) -> Result<Option<Journal>> {
        let Some(bytes) = read_bytes(&self.directory, Path::new(FILE), LIMIT)? else {
            return Ok(None);
        };
        let journal: Journal = serde_json::from_slice(&bytes)?;
        ensure!(
            journal.bytes()? == bytes,
            "custody journal is not canonical"
        );
        ensure!(
            read_head(&self.directory)?.as_ref() == Some(&journal.checkpoint),
            "managed custody checkpoint moved; run the named cold audit"
        );
        let acknowledgement = read_bytes(
            &self.directory,
            Path::new(&format!("{}.ack.json", journal.checkpoint.checkpoint)),
            super::SMALL_LIMIT,
        )?
        .context("managed custody checkpoint acknowledgement missing")?;
        ensure!(
            acknowledgement == crate::canonical::canonical_bytes(&journal.checkpoint)?,
            "managed custody immutable acknowledgement differs"
        );
        Ok(Some(journal))
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
        let Some(mut journal) = self.journal_state()? else {
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
            request: serde_json::to_value(request)?,
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
        journal.writes.insert(key, write);
        journal.previous_revision = Some(journal.revision);
        journal.revision = uuid::Uuid::new_v4();
        self.publish_journal(&journal)
    }

    /// Remove only the writes whose exact captured journal the storage owner
    /// has checked after SQL commit. A concurrent writer or uncertain effect
    /// cannot be discarded by acknowledging an older observation.
    pub(crate) fn settle_journal(&self, observed: &Journal) -> Result<()> {
        let current = self.journal_state()?;
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

/// Read only the checkpoint-selected journal. Audit-only repositories keep the
/// old contract; a managed head with missing journal never falls back to it.
pub(crate) fn observe(root: &Path) -> Result<Option<Journal>> {
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
    let reservation = AuditReservation::acquire(root)?;
    Ok(Some(reservation.journal()?.context(
        "managed custody journal is missing; run the named cold audit",
    )?))
}
