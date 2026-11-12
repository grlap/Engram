//! Exact private construction for a new request following a deferred import.

use super::{audit, container, missing, write_new};
use crate::ObjectId;
use crate::task_sync::{
    delta::{DeltaCandidatePayload, PlanningSuccessorRecipe, PlanningSuccessorRequest},
    ledger::{Binding, Event},
    setup::PrivateDirectory,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

const ROOT: &str = "engram-task-sync-planning-successors";

/// Local construction only. The original source remains owned by its original
/// preparation; this payload is the newly resolved local planning document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSuccessor {
    pub binding: ObjectId,
    pub recipe: PlanningSuccessorRecipe,
    pub payload: DeltaCandidatePayload,
}

impl RetainedSuccessor {
    /// Reconstruct the exact event without allocating any new identities.
    ///
    /// # Errors
    /// Refuses a mismatched binding, malformed payload or missing envelope.
    pub fn prepared_event(&self, binding: &Binding) -> Result<Event> {
        Ok(self
            .recipe
            .prepared_event(&self.binding, binding, self.payload.clone())?)
    }

    pub(crate) fn request_fingerprint(&self) -> Result<ObjectId> {
        Ok(
            crate::CanonicalObject::freeze(&("planning_successor", self))?
                .key()
                .clone(),
        )
    }

    pub(crate) fn matches_outcome(&self, binding: &Binding, event: &Event) -> Result<bool> {
        let crate::task_sync::ledger::Transition::PlanningSuccessorApplied { prepared, receipt } =
            &event.transition
        else {
            return Ok(false);
        };
        Ok(prepared == &self.recipe.prepared
            && self
                .recipe
                .applied_event(&self.binding, binding, receipt.as_ref().clone())?
                == *event)
    }
}

/// The exact request a fresh construction is selected for, before any chain
/// position is chosen. It names the reservation, never retained bytes.
pub(crate) fn construction_request(
    binding_id: &ObjectId,
    request: &PlanningSuccessorRequest,
    payload: &DeltaCandidatePayload,
) -> Result<ObjectId> {
    Ok(crate::CanonicalObject::freeze(&(
        "planning_successor_construction",
        binding_id,
        request,
        payload,
    ))?
    .key()
    .clone())
}

/// Reserve complete construction bytes before entering the SQL writer. A
/// partial or divergent prior attempt is preserved and refused.
///
/// # Errors
/// Refuses unsafe files, malformed requests, competing custody or changed bytes.
pub fn retain(binding: &Binding, retained: &RetainedSuccessor) -> Result<()> {
    retained.prepared_event(binding)?;
    let bytes = crate::canonical::canonical_bytes(retained)?;
    ensure!(
        bytes.len() <= super::MAX_IMPORT_PLAN,
        "successor construction exceeds byte bound"
    );
    let _guard = audit::AuditReservation::acquire(&binding.repository.git_directory)?;
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let directory = container(&git, Path::new(ROOT))?;
    let name = format!("{}.json", retained.recipe.request.intent.operation);
    if let Some(previous) = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
    {
        ensure!(
            previous == bytes,
            "retained successor differs or is partial; preserve exact request"
        );
        return Ok(());
    }
    write_new(&directory, Path::new(&name), &bytes)?;
    #[cfg(unix)]
    audit::sync(&directory)?;
    Ok(())
}

/// Recover the original local construction before selecting any replacement ids.
/// This does not admit the destination or claim that SQL committed.
///
/// # Errors
/// Refuses noncanonical, partial, unsafe or differently selected construction.
pub fn read(
    binding_id: &ObjectId,
    binding: &Binding,
    request: &PlanningSuccessorRequest,
) -> Result<Option<RetainedSuccessor>> {
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let name = format!("{}.json", request.intent.operation);
    let Some(bytes) = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
    else {
        return Ok(None);
    };
    let retained = decode(&bytes)?;
    ensure!(
        &retained.binding == binding_id && &retained.recipe.request == request,
        "retained successor selection differs from exact retry"
    );
    retained.prepared_event(binding)?;
    Ok(Some(retained))
}

fn decode(bytes: &[u8]) -> Result<RetainedSuccessor> {
    let retained: RetainedSuccessor = serde_json::from_slice(bytes)?;
    retained.recipe.validate()?;
    ensure!(
        crate::canonical::canonical_bytes(&retained)? == bytes,
        "retained successor is noncanonical or partial"
    );
    Ok(retained)
}

pub(super) fn inventory(git: &PrivateDirectory) -> Result<BTreeMap<uuid::Uuid, RetainedSuccessor>> {
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let (mut result, mut total) = (BTreeMap::new(), 0_usize);
    for name in directory.names(super::MAX_TASKS)? {
        let text = name.to_str().context("non-UTF8 successor name")?;
        let operation = uuid::Uuid::parse_str(
            text.strip_suffix(".json")
                .context("unknown successor name")?,
        )?;
        ensure!(
            !operation.is_nil() && text == format!("{operation}.json"),
            "noncanonical successor name"
        );
        let bytes = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
            .context("successor construction disappeared")?;
        total = total
            .checked_add(bytes.len())
            .context("successor inventory byte overflow")?;
        ensure!(
            total <= super::MAX_TREE,
            "successor inventory exceeds byte bound"
        );
        let retained = decode(&bytes)?;
        ensure!(
            retained.recipe.request.intent.operation == operation,
            "successor names another operation"
        );
        result.insert(operation, retained);
    }
    Ok(result)
}
