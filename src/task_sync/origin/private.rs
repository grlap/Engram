//! Bounded installation-local custody. These files never enter store export.
//! The exchange owner serializes writes using its existing operation lock.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{Enrollment, ProducingOrigin};
use crate::task_sync::settings::files::{Directory, PrivateFile};
use crate::task_sync::{invalid, progress::ExportCut};
use crate::{CanonicalObject, ObjectId, ProjectId, project_home_of, storage::StoreError};

const PENDING_FORMAT: &str = "engram-task-sync-origin-pending-v1";
const WITNESS_FORMAT: &str = "engram-task-sync-origin-registration-v1";
const AUDIT_FORMAT: &str = "engram-task-sync-custody-audit-pending-v1";

pub(crate) mod recovery;

/// Exact canonical E1 references, compared under the caller's SQLite writer.
/// These are progress references, not a second source of progress authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExchangeAnchor {
    pub binding: ObjectId,
    pub head: ObjectId,
    pub sequence: u64,
    pub cut: ExportCut,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Witness {
    format: String,
    pub project: ProjectId,
    pub origin: ObjectId,
    pub enrollment_operation: uuid::Uuid,
    pub cut: ExportCut,
    pub exchange: Option<ExchangeAnchor>,
    /// A retained E1 request comparison, never a second progress authority.
    pub pending_exchange: Option<ObjectId>,
}

impl Witness {
    fn matches_enrollment(&self, pending: &PendingEnrollment) -> bool {
        self.format == WITNESS_FORMAT
            && self.project == pending.record.project_id
            && self.origin == pending.origin
            && self.enrollment_operation == pending.record.enrollment_operation_id
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingEnrollment {
    format: String,
    pub origin: ObjectId,
    pub record: Enrollment,
    pub expected_witness: Option<Witness>,
}

impl PendingEnrollment {
    /// Mint once before retaining the intent; retries read these same bytes.
    pub(crate) fn new(record: Enrollment, expected_witness: Option<Witness>) -> Self {
        Self {
            format: PENDING_FORMAT.into(),
            origin: ObjectId::mint(),
            record,
            expected_witness,
        }
    }

    pub(crate) fn object(&self) -> Result<CanonicalObject, StoreError> {
        self.validate()?;
        CanonicalObject::identified(&self.origin, &self.record)
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.format != PENDING_FORMAT
            || self.record.enrollment_operation_id.is_nil()
            || self.record.project_id.0.trim().is_empty()
            || self.expected_witness.as_ref().is_some_and(|prior| {
                prior.format != WITNESS_FORMAT || prior.project != self.record.project_id
            })
            || self.expected_witness.as_ref().map(|prior| &prior.origin)
                != self.record.predecessor_origin_id.as_ref()
        {
            return Err(invalid(
                "invalid origin enrollment intent or predecessor witness",
            ));
        }
        Ok(())
    }

    pub(crate) fn witness(&self, cut: ExportCut) -> Witness {
        Witness {
            format: WITNESS_FORMAT.into(),
            project: self.record.project_id.clone(),
            origin: self.origin.clone(),
            enrollment_operation: self.record.enrollment_operation_id,
            cut,
            // New enrollment is not an assertion about an old binding's progress.
            exchange: None,
            pending_exchange: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateState {
    pub pending: Option<PendingEnrollment>,
    pub witness: Option<Witness>,
    pub audit: Option<PendingAudit>,
}

/// Small write-ahead custody only. The complete event is retained by the
/// repository owner, and only SQLite can establish its canonical commit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingAudit {
    format: String,
    pub project: ProjectId,
    pub audit_request: crate::task_sync::delta::CustodyAuditRequest,
    pub binding: ObjectId,
    pub operation: uuid::Uuid,
    pub event_id: ObjectId,
    pub request: ObjectId,
    pub event_fingerprint: ObjectId,
    pub expected_tail: ObjectId,
    pub previous_checkpoint: Option<ObjectId>,
    pub expected_witness: Witness,
    pub repository_head: Option<crate::task_sync::retention::audit::AuditAcknowledgement>,
}

impl PendingAudit {
    pub(crate) fn new(
        audit: &crate::task_sync::retention::audit::RetainedAudit,
        request: &crate::task_sync::delta::CustodyAuditRequest,
        expected_witness: Witness,
        repository_head: Option<crate::task_sync::retention::audit::AuditAcknowledgement>,
    ) -> Result<Self, StoreError> {
        let crate::task_sync::ledger::Transition::CustodyCheckpoint { checkpoint } =
            &audit.event.transition
        else {
            return Err(invalid("audit descriptor requires a custody checkpoint"));
        };
        let value = Self {
            format: AUDIT_FORMAT.into(),
            project: expected_witness.project.clone(),
            audit_request: request.clone(),
            binding: audit.event.binding.clone(),
            operation: checkpoint.operation,
            event_id: audit.event_id.clone(),
            request: checkpoint.request_fingerprint.clone(),
            event_fingerprint: audit
                .event_fingerprint()
                .map_err(|error| invalid(error.to_string()))?,
            expected_tail: audit
                .event
                .predecessor
                .clone()
                .ok_or_else(|| invalid("audit predecessor missing"))?,
            previous_checkpoint: checkpoint.previous_checkpoint.clone(),
            expected_witness,
            repository_head,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.audit_request.fingerprint()? != self.request
            || self.audit_request.operation != self.operation
            || self.audit_request.binding != self.binding
            || self.audit_request.expected_tail != self.expected_tail
            || self.audit_request.previous_checkpoint != self.previous_checkpoint
            || self.format != AUDIT_FORMAT
            || self.project.0.trim().is_empty()
            || self.operation.is_nil()
            || self.event_id == self.expected_tail
            || self.previous_checkpoint.as_ref() == Some(&self.event_id)
            || self.request.as_str().len() != 64
            || self.event_fingerprint.as_str().len() != 64
            || self.expected_witness.format != WITNESS_FORMAT
            || self.expected_witness.project != self.project
            || self.expected_witness.pending_exchange.is_some()
            || self
                .expected_witness
                .exchange
                .as_ref()
                .is_none_or(|anchor| {
                    anchor.binding != self.binding || anchor.head != self.expected_tail
                })
            || self
                .repository_head
                .as_ref()
                .is_some_and(|head| head.event_fingerprint.as_str().len() != 64)
        {
            return Err(invalid("invalid retained custody audit descriptor"));
        }
        Ok(())
    }

    pub(crate) fn matches_retained(
        &self,
        audit: &crate::task_sync::retention::audit::RetainedAudit,
    ) -> Result<bool, StoreError> {
        let crate::task_sync::ledger::Transition::CustodyCheckpoint { checkpoint } =
            &audit.event.transition
        else {
            return Ok(false);
        };
        Ok(self.event_id == audit.event_id
            && self.binding == audit.event.binding
            && self.event_fingerprint
                == audit
                    .event_fingerprint()
                    .map_err(|error| invalid(error.to_string()))?
            && audit.event.predecessor.as_ref() == Some(&self.expected_tail)
            && self.operation == checkpoint.operation
            && self.request == checkpoint.request_fingerprint
            && self.previous_checkpoint == checkpoint.previous_checkpoint
            && self.expected_witness.origin == checkpoint.local_cut.origin
            && self.audit_request.expected_local_cut == checkpoint.local_cut
            && self.audit_request.reason == checkpoint.reason
            && self.audit_request.key == checkpoint.key
            && checkpoint.local_cut.position.feed == crate::FeedId::Project(self.project.clone()))
    }
}

pub(crate) fn pending_audit_error(audit: &PendingAudit) -> StoreError {
    invalid(format!(
        "custody audit operation {} remains pending; retry that exact custody audit before another exchange or enrollment",
        audit.operation
    ))
}

/// Recovery information for the retained operation, never a new intent.
pub(crate) fn pending_enrollment_error(intent: &PendingEnrollment) -> StoreError {
    invalid(format!(
        "origin enrollment intent remains pending; retry origin-enroll operation {} with its original predecessor, or use origin-cancel for that exact uncommitted intent before exchange",
        intent.record.enrollment_operation_id
    ))
}

fn directory(
    database: &Path,
    project: &ProjectId,
    create: bool,
) -> Result<Option<Directory>, StoreError> {
    project_home_of(database, project)
        .ok_or_else(|| invalid("origin witness requires the project's local store home"))?;
    let parent = database
        .parent()
        .ok_or_else(|| invalid("origin store has no project home"))?;
    Directory::open(parent, create).map_err(|_| invalid("origin private directory is unavailable"))
}

/// Read without creating files or adopting a database's retained identity.
pub(crate) fn read(database: &Path, project: &ProjectId) -> Result<PrivateState, StoreError> {
    let Some(directory) = directory(database, project, false)? else {
        return Ok(PrivateState {
            pending: None,
            witness: None,
            audit: None,
        });
    };
    if directory
        .read_slot(PrivateFile::OriginRecoveryPending)
        .map_err(|_| invalid("origin recovery intent is unreadable"))?
        .is_some()
    {
        return Err(invalid(
            "origin recovery is pending; retry its exact recovery operation",
        ));
    }
    let pending = directory
        .read_slot(PrivateFile::OriginPending)
        .map_err(|_| invalid("origin enrollment intent is unreadable"))?
        .map(|bytes| serde_json::from_slice::<PendingEnrollment>(&bytes))
        .transpose()?;
    let witness = directory
        .read_slot(PrivateFile::OriginRegistration)
        .map_err(|_| invalid("origin continuity witness is unreadable"))?
        .map(|bytes| serde_json::from_slice::<Witness>(&bytes))
        .transpose()?;
    let audit = directory
        .read_slot(PrivateFile::OriginAuditPending)
        .map_err(|_| invalid("custody audit intent is unreadable"))?
        .map(|bytes| serde_json::from_slice::<PendingAudit>(&bytes))
        .transpose()?;
    if let Some(audit) = &audit {
        audit.validate()?;
        if &audit.project != project {
            return Err(invalid("custody audit descriptor project differs"));
        }
    }
    if pending
        .as_ref()
        .is_some_and(|intent| intent.validate().is_err() || &intent.record.project_id != project)
        || witness.as_ref().is_some_and(|witness| {
            witness.format != WITNESS_FORMAT
                || &witness.project != project
                || witness.enrollment_operation.is_nil()
        })
    {
        return Err(invalid("origin private enrollment scope or format differs"));
    }
    Ok(PrivateState {
        pending,
        witness,
        audit,
    })
}

/// Retain a new intent only against its exact prior installation state.
/// The existing exchange operation lock must cover this read/replace pair.
pub(crate) fn retain(database: &Path, intent: &PendingEnrollment) -> Result<(), StoreError> {
    intent.validate()?;
    let project = &intent.record.project_id;
    let state = read(database, project)?;
    if let Some(pending) = &state.pending {
        if pending == intent {
            if state.witness == intent.expected_witness
                || state
                    .witness
                    .as_ref()
                    .is_some_and(|witness| witness.matches_enrollment(intent))
            {
                return Ok(());
            }
            return Err(invalid(
                "origin enrollment retry has a different private witness",
            ));
        }
        // A prior completed enrollment's pending bytes are kept until the next
        // explicit operation. Unsettled custody is never silently replaced.
        if !state
            .witness
            .as_ref()
            .is_some_and(|witness| witness.matches_enrollment(pending))
        {
            return Err(pending_enrollment_error(pending));
        }
    }
    if let Some(audit) = &state.audit {
        return Err(pending_audit_error(audit));
    }
    if state.witness != intent.expected_witness {
        return Err(invalid(
            "origin continuity witness changed before enrollment",
        ));
    }
    if state
        .witness
        .as_ref()
        .is_some_and(|witness| witness.pending_exchange.is_some())
    {
        return Err(invalid(
            "origin exchange intent remains pending; recover it before retaining enrollment",
        ));
    }
    let bytes = serde_json::to_vec(intent)?;
    directory(database, project, true)?
        .ok_or_else(|| invalid("origin private directory missing"))?
        .replace_slot(PrivateFile::OriginPending, &bytes)
        .map_err(|_| invalid("origin enrollment intent could not be retained"))
}

/// Hold the project operation lock and preflight the complete event first.
pub(crate) fn retain_audit(database: &Path, audit: &PendingAudit) -> Result<(), StoreError> {
    audit.validate()?;
    let bytes = serde_json::to_vec(audit)?;
    if bytes.len() > crate::task_sync::settings::files::MAX_BYTES {
        return Err(invalid(
            "custody audit descriptor exceeds private byte bound",
        ));
    }
    let state = read(database, &audit.project)?;
    if state.pending.is_some()
        || state.witness.as_ref() != Some(&audit.expected_witness)
        || state.audit.as_ref().is_some_and(|old| old != audit)
    {
        return Err(invalid(
            "custody audit private basis changed before retention",
        ));
    }
    if state.audit.as_ref() == Some(audit) {
        return Ok(());
    }
    directory(database, &audit.project, false)?
        .ok_or_else(|| invalid("origin private directory missing"))?
        .replace_slot(PrivateFile::OriginAuditPending, &bytes)
        .map_err(|_| invalid("custody audit descriptor retention failed; preserve exact request"))
}

/// Clear only after the SQL owner proved this event and acknowledged both
/// repository and origin witnesses. Never clear another request or reservation.
pub(crate) fn finish_audit(
    database: &Path,
    audit: &PendingAudit,
    witnessed: &Witness,
) -> Result<(), StoreError> {
    let state = read(database, &audit.project)?;
    if state.pending.is_some()
        || state.audit.as_ref() != Some(audit)
        || state.witness.as_ref() != Some(witnessed)
        || witnessed.pending_exchange.is_some()
        || witnessed
            .exchange
            .as_ref()
            .is_none_or(|anchor| anchor.binding != audit.binding || anchor.head != audit.event_id)
    {
        return Err(invalid(
            "custody audit acknowledgement differs from exact pending event",
        ));
    }
    directory(database, &audit.project, false)?
        .ok_or_else(|| invalid("origin private directory missing"))?
        .remove_slot(PrivateFile::OriginAuditPending)
        .map_err(|_| invalid("custody audit cleanup pending; retry the exact request"))
}

/// Remove only a newly retained intent whose caller has NOT attempted commit.
/// The caller still holds the operation lock and has dropped its SQL writer.
/// Never use this to discard a pre-existing or ambiguously committed intent.
pub(crate) fn cancel_uncommitted(
    database: &Path,
    intent: &PendingEnrollment,
) -> Result<(), StoreError> {
    remove_exact_pending(database, intent)
}

/// The explicit cancellation owner holds the project lock and SQL writer,
/// proving canonical ID/operation absence and unchanged predecessor anchors.
/// This removes no exchange reservation, witness or retained artifact.
pub(crate) fn cancel_after_absence_check(
    database: &Path,
    intent: &PendingEnrollment,
) -> Result<(), StoreError> {
    remove_exact_pending(database, intent)
}

fn remove_exact_pending(database: &Path, intent: &PendingEnrollment) -> Result<(), StoreError> {
    let project = &intent.record.project_id;
    let state = read(database, project)?;
    if state.pending.as_ref() != Some(intent) || state.witness != intent.expected_witness {
        return Err(invalid("origin intent changed before exact cleanup"));
    }
    directory(database, project, false)?
        .ok_or_else(|| invalid("origin private directory missing"))?
        .remove_slot(PrivateFile::OriginPending)
        .map_err(|_| invalid("uncommitted origin intent cleanup failed; retain exact operation"))
}

/// Activate only the exact record committed for the retained intent. This is
/// a post-commit step; failure leaves the committed record and intent intact.
pub(crate) fn finish(
    database: &Path,
    intent: &PendingEnrollment,
    committed: &ProducingOrigin,
    cut: ExportCut,
) -> Result<(), StoreError> {
    intent.validate()?;
    if committed.origin_id != intent.origin || committed.enrollment != intent.record {
        return Err(invalid(
            "committed origin differs from retained enrollment intent",
        ));
    }
    let project = &intent.record.project_id;
    let state = read(database, project)?;
    if state.pending.as_ref() != Some(intent) {
        return Err(invalid(
            "origin enrollment intent changed before activation",
        ));
    }
    let witness = intent.witness(cut);
    if state
        .witness
        .as_ref()
        .is_some_and(|current| current.matches_enrollment(intent))
    {
        // Preserve any later E1 progress; a replay must never reset its anchor.
        return directory(database, project, false)?
            .ok_or_else(|| invalid("origin private directory missing"))?
            .remove_slot(PrivateFile::OriginPending)
            .map_err(|_| invalid("origin enrollment cleanup pending; retry the same operation"));
    }
    if state.witness != intent.expected_witness {
        return Err(invalid(
            "origin continuity witness changed before activation",
        ));
    }
    let directory = directory(database, project, false)?
        .ok_or_else(|| invalid("origin private directory missing"))?;
    directory
        .replace_slot(
            PrivateFile::OriginRegistration,
            &serde_json::to_vec(&witness)?,
        )
        .map_err(|_| invalid("origin activation is uncertain; retain its original intent"))?;
    directory
        .remove_slot(PrivateFile::OriginPending)
        .map_err(|_| invalid("origin enrollment cleanup pending; retry the same operation"))
}

pub(crate) fn advance(
    database: &Path,
    expected: &Witness,
    next: &Witness,
) -> Result<(), StoreError> {
    if expected.project != next.project
        || expected.origin != next.origin
        || expected.enrollment_operation != next.enrollment_operation
        || read(database, &expected.project)?.witness.as_ref() != Some(expected)
    {
        return Err(invalid(
            "origin witness changed; preserve committed exchange for reconciliation",
        ));
    }
    directory(database, &expected.project, false)?
        .ok_or_else(|| invalid("origin private directory missing"))?
        .replace_slot(PrivateFile::OriginRegistration, &serde_json::to_vec(next)?)
        .map_err(|_| invalid("origin witness update failed; committed exchange remains uncertain"))
}
