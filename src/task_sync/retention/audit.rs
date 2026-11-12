//! Exact cold-audit custody files. Retaining or acknowledging bytes never
//! appends a canonical event, admits a source, or activates managed writers.

use std::{collections::BTreeMap, io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{container, missing, write_new};
use crate::{
    CanonicalObject, ObjectId,
    task_sync::{
        file_lock::HeldLock,
        ledger::{Event, Transition},
        setup::PrivateDirectory,
    },
};

const ROOT: &str = "engram-task-sync-custody-audits";
const EVENT_LIMIT: usize = 16 * 1024 * 1024;
const SMALL_LIMIT: usize = 16 * 1024;
const TOTAL_LIMIT: usize = 128 * 1024 * 1024;
const MAX_AUDITS: usize = 100_000;
const MAX_ENTRIES: usize = MAX_AUDITS * 2 + 3;

/// A minted event retained before its owning SQL transaction is attempted.
/// Possession proves no commit or admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedAudit {
    pub event_id: ObjectId,
    pub event: Event,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    AuditOnly,
}

/// Filesystem acknowledgement of an exact canonical checkpoint. The caller
/// must establish the SQL commit before publishing it. It grants no authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditAcknowledgement {
    pub binding: ObjectId,
    pub origin: ObjectId,
    pub restore_occurrence: Option<uuid::Uuid>,
    pub checkpoint: ObjectId,
    pub event_fingerprint: ObjectId,
    pub mode: AuditMode,
}

impl RetainedAudit {
    /// Check the exact serialized retention size before creating pending files.
    ///
    /// # Errors
    /// Refuses another transition, malformed event scope or the event byte limit.
    pub fn preflight(&self) -> Result<Vec<u8>> {
        let Transition::CustodyCheckpoint { checkpoint } = &self.event.transition else {
            anyhow::bail!("audit retention requires a custody checkpoint");
        };
        ensure!(
            self.event.schema_version == 1
                && self.event.sequence > 1
                && self.event.predecessor.is_some()
                && !checkpoint.operation.is_nil()
                && self.event.binding == checkpoint.local_cut.binding,
            "audit retention event scope is invalid"
        );
        crate::task_sync::ledger::attribution(&self.event.actor)?;
        let bytes = crate::canonical::canonical_bytes(self)?;
        ensure!(
            bytes.len() <= EVENT_LIMIT,
            "custody checkpoint exceeds retained event byte bound"
        );
        Ok(bytes)
    }

    /// Content comparison only; the minted event id remains its identity.
    ///
    /// # Errors
    /// Refuses an event that cannot be canonically encoded.
    pub fn event_fingerprint(&self) -> Result<ObjectId> {
        Ok(CanonicalObject::freeze(&self.event)?.key().clone())
    }

    fn acknowledgement(&self) -> Result<AuditAcknowledgement> {
        let Transition::CustodyCheckpoint { checkpoint } = &self.event.transition else {
            anyhow::bail!("audit retention requires a custody checkpoint");
        };
        Ok(AuditAcknowledgement {
            binding: self.event.binding.clone(),
            origin: checkpoint.local_cut.origin.clone(),
            restore_occurrence: checkpoint.local_cut.restore_occurrence,
            checkpoint: self.event_id.clone(),
            event_fingerprint: self.event_fingerprint()?,
            mode: AuditMode::AuditOnly,
        })
    }
}

/// Installation-local serialization using the existing no-follow capability
/// and file-lock owner. B2 must connect all managed writers before activation.
pub struct AuditReservation {
    directory: PrivateDirectory,
    _lock: HeldLock,
}

impl AuditReservation {
    pub(crate) fn require_root(&self, git_directory: &Path) -> Result<()> {
        let root = PrivateDirectory::open(git_directory)?.child(Path::new(ROOT))?;
        ensure!(
            root.identity()? == self.directory.identity()?,
            "custody reservation belongs to another repository"
        );
        Ok(())
    }
    /// Read one exact retained event under this reservation.
    ///
    /// # Errors
    /// Refuses unsafe, oversized, partial or changed bytes.
    pub fn read(&self, id: &ObjectId) -> Result<Option<RetainedAudit>> {
        read_at(&self.directory, id)
    }

    /// Observe the reserved namespace completely.
    ///
    /// # Errors
    /// Refuses unknown, partial, unsafe or oversized entries.
    pub fn inventory(&self) -> Result<AuditInventory> {
        inventory_at(&self.directory)
    }
    /// Reserve this repository's audit namespace without opening SQLite.
    ///
    /// # Errors
    /// Refuses unsafe directories, partial locks or a competing reservation.
    pub fn acquire(git_directory: &Path) -> Result<Self> {
        ensure!(
            git_directory.is_absolute(),
            "Git admin path must be absolute"
        );
        let git = PrivateDirectory::open(git_directory)?;
        let directory = container(&git, Path::new(ROOT))?;
        let name = Path::new("prepare.lock");
        let file = match directory.open_file(name, true, true) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                directory.open_file(name, true, false)?
            }
            Err(error) => return Err(error),
        };
        let lock = HeldLock::try_acquire(file).context("custody audit reservation is busy")?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// Preserve exact first-attempt bytes. Partial files are never recreated.
    /// The storage owner publishes its small pending descriptor first.
    ///
    /// # Errors
    /// Refuses capacity, a different retained event or partial existing bytes.
    pub fn retain(&self, audit: &RetainedAudit) -> Result<()> {
        let bytes = audit.preflight()?;
        let name = format!("{}.json", audit.event_id);
        if let Some(old) = read_bytes(&self.directory, Path::new(&name), EVENT_LIMIT)? {
            ensure!(
                old == bytes,
                "retained audit event differs or is partial; preserve uncertainty"
            );
            return Ok(());
        }
        write_new(&self.directory, Path::new(&name), &bytes)?;
        #[cfg(unix)]
        sync(&self.directory)?;
        ensure!(
            read_bytes(&self.directory, Path::new(&name), EVENT_LIMIT)?.as_ref() == Some(&bytes),
            "retained audit event changed during publication"
        );
        Ok(())
    }

    /// Publish only the acknowledged checkpoint selected by the SQL owner.
    /// Exact replay never resets a later repository head.
    ///
    /// # Errors
    /// Refuses changed predecessor acknowledgement, missing event or drift.
    pub fn acknowledge(
        &self,
        audit: &RetainedAudit,
        previous: Option<&AuditAcknowledgement>,
    ) -> Result<AuditAcknowledgement> {
        ensure!(
            read_at(&self.directory, &audit.event_id)?.as_ref() == Some(audit),
            "audit acknowledgement requires its exact retained event"
        );
        let next = audit.acknowledgement()?;
        let current = read_head(&self.directory)?;
        ensure!(
            current.as_ref() == previous || current.as_ref() == Some(&next),
            "custody audit head changed; preserve exact pending acknowledgement"
        );
        let Transition::CustodyCheckpoint { checkpoint } = &audit.event.transition else {
            anyhow::bail!("audit retention requires a custody checkpoint");
        };
        ensure!(
            checkpoint.previous_checkpoint.as_ref()
                == previous
                    .filter(|ack| ack.binding == audit.event.binding)
                    .map(|ack| &ack.checkpoint),
            "custody audit predecessor acknowledgement differs"
        );
        let bytes = crate::canonical::canonical_bytes(&next)?;
        ensure!(
            bytes.len() <= SMALL_LIMIT,
            "custody audit acknowledgement exceeds byte bound"
        );
        let immutable = format!("{}.ack.json", audit.event_id);
        match read_bytes(&self.directory, Path::new(&immutable), SMALL_LIMIT)? {
            Some(old) => ensure!(old == bytes, "custody audit acknowledgement differs"),
            None => write_new(&self.directory, Path::new(&immutable), &bytes)?,
        }
        if current.as_ref() != Some(&next) {
            let pending = format!("head-{}.pending", audit.event_id);
            match read_bytes(&self.directory, Path::new(&pending), SMALL_LIMIT)? {
                Some(old) => ensure!(
                    old == bytes,
                    "pending audit acknowledgement differs or is partial"
                ),
                None => write_new(&self.directory, Path::new(&pending), &bytes)?,
            }
            self.directory
                .dir()
                .rename(&pending, self.directory.dir(), "head.json")?;
        }
        #[cfg(unix)]
        sync(&self.directory)?;
        ensure!(
            read_head(&self.directory)?.as_ref() == Some(&next),
            "custody audit head changed during acknowledgement"
        );
        Ok(next)
    }
}

// File contents are flushed on every host. Only Unix flushes directory
// metadata here; other hosts intentionally omit these directory-sync calls.
#[cfg(unix)]
pub(super) fn sync(directory: &PrivateDirectory) -> Result<()> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    directory
        .dir()
        .open_with(".", &options)?
        .into_std()
        .sync_all()?;
    Ok(())
}

// Keep the generic setup reader at 2 MiB. Only this retention owner opens its
// known event file with the existing ledger's explicit 16 MiB bound.
pub(in crate::task_sync) fn read_bytes(
    directory: &PrivateDirectory,
    name: &Path,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    match directory.dir().symlink_metadata(name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(meta) => ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit as u64,
            "custody audit file is unsafe or exceeds byte bound"
        ),
    }
    let file = directory.open_file(name, false, false)?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "custody audit file exceeds byte bound"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "custody audit file grew past byte bound"
    );
    Ok(Some(bytes))
}

fn read_at(directory: &PrivateDirectory, id: &ObjectId) -> Result<Option<RetainedAudit>> {
    read_bytes(directory, Path::new(&format!("{id}.json")), EVENT_LIMIT)?
        .map(|bytes| {
            let audit: RetainedAudit = serde_json::from_slice(&bytes)?;
            ensure!(
                audit.event_id == *id && audit.preflight()? == bytes,
                "retained audit event has different identity or noncanonical bytes"
            );
            Ok(audit)
        })
        .transpose()
}

fn read_head(directory: &PrivateDirectory) -> Result<Option<AuditAcknowledgement>> {
    read_bytes(directory, Path::new("head.json"), SMALL_LIMIT)?
        .map(|bytes| {
            let head = serde_json::from_slice(&bytes)?;
            ensure!(
                crate::canonical::canonical_bytes(&head)? == bytes,
                "custody audit head is not canonical"
            );
            Ok(head)
        })
        .transpose()
}

/// A full cold observation, never proof that SQL committed these events.
#[derive(Clone, Debug, Default)]
pub struct AuditInventory {
    pub events: BTreeMap<ObjectId, RetainedAudit>,
    pub acknowledgements: BTreeMap<ObjectId, AuditAcknowledgement>,
    pub head: Option<AuditAcknowledgement>,
    pub pending_acknowledgements: BTreeMap<ObjectId, AuditAcknowledgement>,
}

/// Enumerate every retained audit file, rejecting partial or unknown entries.
///
/// # Errors
/// Refuses unsafe files, missing acknowledged events, drift and capacity.
pub fn inventory(git_directory: &Path) -> Result<AuditInventory> {
    let git = PrivateDirectory::open(git_directory)?;
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(AuditInventory::default()),
        Err(error) => return Err(error),
    };
    inventory_at(&directory)
}

fn inventory_at(directory: &PrivateDirectory) -> Result<AuditInventory> {
    let names = directory.names(MAX_ENTRIES)?;
    let mut result = AuditInventory::default();
    let mut total = 0_usize;
    for name in &names {
        let name = name.to_str().context("non-UTF8 custody audit filename")?;
        if name == "prepare.lock" {
            continue;
        }
        let pending = name
            .strip_prefix("head-")
            .and_then(|name| name.strip_suffix(".pending"));
        let acknowledgement = name.ends_with(".ack.json");
        let limit = if acknowledgement || pending.is_some() || name == "head.json" {
            SMALL_LIMIT
        } else {
            EVENT_LIMIT
        };
        let bytes = read_bytes(directory, Path::new(name), limit)?
            .context("custody audit file disappeared")?;
        total = total
            .checked_add(bytes.len())
            .context("custody audit inventory exceeds byte bound")?;
        ensure!(
            total <= TOTAL_LIMIT,
            "custody audit inventory exceeds byte bound"
        );
        if name == "head.json" {
            result.head = read_head(directory)?;
        } else if let Some(id) = pending {
            let id = ObjectId::from_stored(id.to_owned())
                .context("unknown pending audit acknowledgement")?;
            let ack: AuditAcknowledgement = serde_json::from_slice(&bytes)?;
            ensure!(
                ack.checkpoint == id && crate::canonical::canonical_bytes(&ack)? == bytes,
                "invalid pending audit acknowledgement"
            );
            result.pending_acknowledgements.insert(id, ack);
        } else {
            let id = name
                .strip_suffix(if acknowledgement {
                    ".ack.json"
                } else {
                    ".json"
                })
                .and_then(|id| ObjectId::from_stored(id.to_owned()))
                .context("unknown custody audit entry")?;
            if acknowledgement {
                let ack: AuditAcknowledgement = serde_json::from_slice(&bytes)?;
                ensure!(
                    ack.checkpoint == id && crate::canonical::canonical_bytes(&ack)? == bytes,
                    "invalid custody audit acknowledgement"
                );
                result.acknowledgements.insert(id, ack);
            } else {
                let audit: RetainedAudit = serde_json::from_slice(&bytes)?;
                ensure!(
                    audit.event_id == id && audit.preflight()? == bytes,
                    "invalid retained custody audit event"
                );
                result.events.insert(id, audit);
            }
        }
    }
    ensure!(
        result.events.len() <= MAX_AUDITS,
        "custody audit count bound exceeded"
    );
    for (id, ack) in &result.acknowledgements {
        let audit = result
            .events
            .get(id)
            .context("acknowledged custody audit event missing")?;
        ensure!(
            *ack == audit.acknowledgement()?,
            "custody audit acknowledgement scope differs"
        );
    }
    ensure!(
        result.pending_acknowledgements.len() <= 1,
        "multiple pending audit acknowledgements"
    );
    for (id, ack) in &result.pending_acknowledgements {
        ensure!(
            result.acknowledgements.get(id) == Some(ack),
            "pending audit acknowledgement has no exact retained acknowledgement"
        );
    }
    if let Some(head) = &result.head {
        ensure!(
            result.acknowledgements.get(&head.checkpoint) == Some(head),
            "custody audit head has no exact acknowledgement"
        );
    }
    ensure!(
        directory.names(MAX_ENTRIES)? == names,
        "custody audit inventory changed during observation"
    );
    Ok(result)
}

/// Observe only this checkpoint's immutable acknowledgement, without changing
/// a newer repository head or enumerating unrelated history.
///
/// # Errors
/// Refuses unsafe, partial or contradictory acknowledgement bytes.
pub fn acknowledged(git_directory: &Path, audit: &RetainedAudit) -> Result<bool> {
    let git = PrivateDirectory::open(git_directory)?;
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let bytes = read_bytes(
        &directory,
        Path::new(&format!("{}.ack.json", audit.event_id)),
        SMALL_LIMIT,
    )?;
    let Some(bytes) = bytes else {
        return Ok(false);
    };
    let expected = crate::canonical::canonical_bytes(&audit.acknowledgement()?)?;
    ensure!(bytes == expected, "custody audit acknowledgement differs");
    Ok(true)
}
