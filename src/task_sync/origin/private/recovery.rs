//! Recovery retains predecessor bytes before installing a successor witness.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Directory, PrivateFile, WITNESS_FORMAT, Witness, directory};
use crate::task_sync::settings::files::OperationLock;
use crate::task_sync::{
    invalid,
    origin::{
        Enrollment, ProducingOrigin,
        recovery::{RecoveryCustody, RecoveryExchangeAnchor, RecoveryRecord, RecoveryWitness},
    },
    progress::ExportCut,
};
use crate::{CanonicalObject, ObjectId, ProjectId, storage::StoreError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryRecipe {
    pub schema_version: u32,
    pub enrollment: Enrollment,
    pub recovery_id: ObjectId,
    pub recovery: RecoveryRecord,
}

impl RecoveryRecipe {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        self.recovery.validate()?;
        if self.schema_version != 1
            || self.enrollment.project_id != self.recovery.project_id
            || self.enrollment.predecessor_origin_id.as_ref()
                != Some(&self.recovery.request.predecessor)
            || self.enrollment.enrollment_operation_id != self.recovery.request.operation
            || self.enrollment.actor != self.recovery.actor
            || self.enrollment.enrolled_at != self.recovery.recovered_at
            || self.recovery_id == self.recovery.successor_origin
        {
            return Err(invalid(
                "origin recovery recipe differs from its exact transition",
            ));
        }
        Ok(())
    }

    pub(crate) fn enrollment_object(&self) -> Result<CanonicalObject, StoreError> {
        self.validate()?;
        CanonicalObject::identified(&self.recovery.successor_origin, &self.enrollment)
    }

    pub(crate) fn recovery_object(&self) -> Result<CanonicalObject, StoreError> {
        self.validate()?;
        CanonicalObject::identified(&self.recovery_id, &self.recovery)
    }
}

struct CustodyBytes {
    pending: Option<Vec<u8>>,
    registration: Option<Vec<u8>>,
    audit: Option<Vec<u8>>,
}

impl CustodyBytes {
    fn read(directory: &Directory) -> Result<Self, StoreError> {
        let read = |slot| {
            directory
                .read_slot(slot)
                .map_err(|_| invalid("origin recovery cannot read bounded predecessor custody"))
        };
        Ok(Self {
            pending: read(PrivateFile::OriginPending)?,
            registration: read(PrivateFile::OriginRegistration)?,
            audit: read(PrivateFile::OriginAuditPending)?,
        })
    }

    fn comparisons(&self) -> Result<RecoveryCustody, StoreError> {
        let fingerprint = |bytes: &Option<Vec<u8>>| {
            bytes
                .as_ref()
                .map(|bytes| CanonicalObject::freeze(bytes).map(|object| object.key().clone()))
                .transpose()
        };
        let witness = self
            .registration
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<Witness>(bytes).ok())
            .filter(|witness| witness.format == WITNESS_FORMAT);
        let registration_uninterpretable = self.registration.is_some() && witness.is_none();
        Ok(RecoveryCustody {
            enrollment_intent: fingerprint(&self.pending)?,
            registration: fingerprint(&self.registration)?,
            audit_intent: fingerprint(&self.audit)?,
            witness: witness.map(|witness| RecoveryWitness {
                project: witness.project,
                origin: witness.origin,
                enrollment_operation: witness.enrollment_operation,
                cut: witness.cut,
                exchange: witness.exchange.map(|exchange| RecoveryExchangeAnchor {
                    binding: exchange.binding,
                    head: exchange.head,
                    sequence: exchange.sequence,
                    cut: exchange.cut,
                }),
                pending_exchange: witness.pending_exchange,
            }),
            registration_uninterpretable,
        })
    }
}

/// The same installation-local lock serializes enrollment, exchange and recovery.
pub(crate) struct RecoveryReservation {
    database: PathBuf,
    project: ProjectId,
    directory: Directory,
    _lock: OperationLock,
}

impl RecoveryReservation {
    pub(crate) fn acquire(database: &Path, project: &ProjectId) -> Result<Self, StoreError> {
        let directory = directory(database, project, false)?
            .ok_or_else(|| invalid("origin recovery requires an existing project home"))?;
        let lock = directory
            .operation_lock()
            .map_err(|_| invalid("origin recovery is busy or its reservation is unsafe"))?;
        Ok(Self {
            database: database.to_owned(),
            project: project.clone(),
            directory,
            _lock: lock,
        })
    }

    pub(crate) fn recipe(&self) -> Result<Option<RecoveryRecipe>, StoreError> {
        let Some(bytes) = self
            .directory
            .read_slot(PrivateFile::OriginRecoveryPending)
            .map_err(|_| invalid("origin recovery intent is unreadable"))?
        else {
            return Ok(None);
        };
        let recipe: RecoveryRecipe = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("origin recovery intent has an invalid shape"))?;
        recipe.validate()?;
        if recipe.recovery.project_id != self.project {
            return Err(invalid("origin recovery intent belongs to another project"));
        }
        Ok(Some(recipe))
    }

    pub(crate) fn custody(&self) -> Result<RecoveryCustody, StoreError> {
        CustodyBytes::read(&self.directory)?.comparisons()
    }

    fn archive(&self, operation: uuid::Uuid) -> Result<Directory, StoreError> {
        // The only variable path member is a typed UUID, never a caller path.
        let parent = self
            .database
            .parent()
            .ok_or_else(|| invalid("origin recovery has no project home"))?;
        Directory::open(
            &parent
                .join("task-sync-origin-recovery")
                .join(operation.to_string()),
            true,
        )
        .map_err(|_| invalid("origin recovery archive is unavailable"))?
        .ok_or_else(|| invalid("origin recovery archive is missing"))
    }

    pub(crate) fn retain(&self, recipe: &RecoveryRecipe) -> Result<(), StoreError> {
        recipe.validate()?;
        let bytes = serde_json::to_vec(recipe)?;
        if bytes.len() > crate::task_sync::settings::files::MAX_BYTES {
            return Err(invalid(
                "origin recovery intent exceeds its retained byte bound",
            ));
        }
        if let Some(old) = self.recipe()? {
            return if old == *recipe {
                Ok(())
            } else {
                Err(invalid("another origin recovery intent remains pending"))
            };
        }
        self.check_predecessor(recipe)?;
        let source = CustodyBytes::read(&self.directory)?;
        let archive = self.archive(recipe.recovery.request.operation)?;
        for (slot, bytes) in [
            (PrivateFile::OriginPending, source.pending),
            (PrivateFile::OriginRegistration, source.registration),
            (PrivateFile::OriginAuditPending, source.audit),
        ] {
            let existing = archive
                .read_slot(slot)
                .map_err(|_| invalid("origin recovery archive cannot be read"))?;
            match (existing, bytes) {
                (Some(existing), Some(bytes)) if existing == bytes => (),
                (None, Some(bytes)) => archive.replace_slot(slot, &bytes).map_err(|_| {
                    invalid("origin recovery predecessor bytes could not be retained")
                })?,
                (None, None) => (),
                _ => {
                    return Err(invalid(
                        "origin recovery archive differs from predecessor custody",
                    ));
                }
            }
        }
        self.check_predecessor(recipe)?;
        self.directory
            .replace_slot(PrivateFile::OriginRecoveryPending, &bytes)
            .map_err(|_| invalid("origin recovery intent could not be retained"))
    }

    pub(crate) fn check_predecessor(&self, recipe: &RecoveryRecipe) -> Result<(), StoreError> {
        if recipe.recovery.project_id != self.project || self.custody()? != recipe.recovery.custody
        {
            return Err(invalid("origin recovery predecessor custody changed"));
        }
        Ok(())
    }

    /// The caller's writer has proved canonical ID/operation absence. Keep all
    /// predecessor bytes and archives; release only this exact uncommitted slot.
    pub(crate) fn cancel_after_absence_check(
        &self,
        recipe: &RecoveryRecipe,
    ) -> Result<(), StoreError> {
        if self.recipe()?.as_ref() != Some(recipe) {
            return Err(invalid(
                "origin recovery intent changed before cancellation",
            ));
        }
        self.check_predecessor(recipe)?;
        self.directory
            .remove_slot(PrivateFile::OriginRecoveryPending)
            .map_err(|_| {
                invalid("uncommitted origin recovery cleanup failed; retain exact request")
            })
    }

    /// Caller has validated the exact canonical commit under its writer.
    pub(crate) fn finish(
        &self,
        recipe: &RecoveryRecipe,
        origin: &ProducingOrigin,
        cut: ExportCut,
    ) -> Result<(), StoreError> {
        if self.recipe()?.as_ref() != Some(recipe)
            || origin.origin_id != recipe.recovery.successor_origin
            || origin.enrollment != recipe.enrollment
        {
            return Err(invalid(
                "origin recovery activation differs from retained transition",
            ));
        }
        let archive = self.archive(recipe.recovery.request.operation)?;
        let prior = CustodyBytes::read(&archive)?;
        if prior.comparisons()? != recipe.recovery.custody {
            return Err(invalid("origin recovery predecessor archive differs"));
        }
        let witness = Witness {
            format: WITNESS_FORMAT.into(),
            project: self.project.clone(),
            origin: origin.origin_id.clone(),
            enrollment_operation: recipe.recovery.request.operation,
            cut,
            exchange: None,
            pending_exchange: None,
        };
        let witness_bytes = serde_json::to_vec(&witness)?;
        let current = CustodyBytes::read(&self.directory)?;
        // An exact committed retry may observe any prefix of post-commit cleanup.
        if (current.pending.is_some() && current.pending != prior.pending)
            || (current.audit.is_some() && current.audit != prior.audit)
            || (current.registration != prior.registration
                && current.registration.as_ref() != Some(&witness_bytes))
        {
            return Err(invalid("origin recovery activation custody changed"));
        }
        // Original bytes remain in the immutable operation-qualified archive.
        for slot in [PrivateFile::OriginPending, PrivateFile::OriginAuditPending] {
            self.directory.remove_slot(slot).map_err(|_| {
                invalid("origin recovery cleanup is uncertain; retry exact operation")
            })?;
        }
        self.directory
            .replace_slot(PrivateFile::OriginRegistration, &witness_bytes)
            .map_err(|_| {
                invalid("origin recovery activation is uncertain; retry exact operation")
            })?;
        self.directory
            .remove_slot(PrivateFile::OriginRecoveryPending)
            .map_err(|_| {
                invalid("origin recovery acknowledgement is uncertain; retry exact operation")
            })
    }
}
