//! Preserve exact construction metadata before an evidence import can mutate SQL.

use super::{audit, container, missing, write_new};
use crate::ObjectId;
use crate::task_sync::{
    ledger::{Binding, EvidenceImportRecipe, EvidenceIntent},
    setup::PrivateDirectory,
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, path::Path};

const ROOT: &str = "engram-task-sync-evidence-imports";

pub(crate) fn request_fingerprint(recipe: &EvidenceImportRecipe) -> Result<ObjectId> {
    Ok(
        crate::CanonicalObject::freeze(&("evidence_import", recipe))?
            .key()
            .clone(),
    )
}

/// The exact request a fresh construction is selected for, before any chain
/// position is chosen. It names the reservation, never a retained recipe.
pub(crate) fn construction_request(
    binding_id: &ObjectId,
    intent: &EvidenceIntent,
) -> Result<ObjectId> {
    Ok(
        crate::CanonicalObject::freeze(&("evidence_construction", binding_id, intent))?
            .key()
            .clone(),
    )
}

/// Retain event ids, attribution, selected admissions and time without minting
/// replacements. Existing partial/different bytes refuse and remain in place.
///
/// # Errors
/// Refuses malformed/oversized recipes, competing custody or unsafe/changed files.
pub fn retain_recipe(
    binding_id: &ObjectId,
    binding: &Binding,
    recipe: &EvidenceImportRecipe,
) -> Result<()> {
    recipe.validate(binding_id, binding)?;
    let bytes = crate::canonical::canonical_bytes(recipe)?;
    let _guard = audit::AuditReservation::acquire(&binding.repository.git_directory)?;
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let directory = container(&git, Path::new(ROOT))?;
    let name = format!("{}.json", recipe.intent()?.operation);
    if let Some(previous) = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
    {
        ensure!(
            previous == bytes,
            "retained evidence recipe differs or is partial; preserve exact intent"
        );
        return Ok(());
    }
    write_new(&directory, Path::new(&name), &bytes)?;
    #[cfg(unix)]
    audit::sync(&directory)?;
    Ok(())
}

/// Read the exact earlier construction recipe before selecting any new ids.
/// This establishes neither completion nor permission to admit its payload.
///
/// # Errors
/// Refuses partial, noncanonical, unsafe or differently attributed metadata.
pub fn read_recipe(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
) -> Result<Option<EvidenceImportRecipe>> {
    intent.validate(binding_id, binding)?;
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let name = format!("{}.json", intent.operation);
    let Some(bytes) = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
    else {
        return Ok(None);
    };
    let recipe = decode(&bytes)?;
    recipe.validate(binding_id, binding)?;
    ensure!(
        recipe.intent()? == intent,
        "retained evidence request differs from exact retry"
    );
    Ok(Some(recipe))
}

fn decode(bytes: &[u8]) -> Result<EvidenceImportRecipe> {
    let recipe: EvidenceImportRecipe = serde_json::from_slice(bytes)?;
    ensure!(
        crate::canonical::canonical_bytes(&recipe)? == bytes,
        "retained evidence recipe is noncanonical or partial"
    );
    Ok(recipe)
}

pub(super) fn inventory(
    git: &PrivateDirectory,
) -> Result<BTreeMap<uuid::Uuid, EvidenceImportRecipe>> {
    let directory = match git.child(Path::new(ROOT)) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let (mut result, mut total) = (BTreeMap::new(), 0_usize);
    for name in directory.names(super::MAX_TASKS)? {
        let text = name.to_str().context("non-UTF8 evidence recipe name")?;
        let operation = uuid::Uuid::parse_str(
            text.strip_suffix(".json")
                .context("unknown evidence recipe name")?,
        )?;
        ensure!(
            !operation.is_nil() && text == format!("{operation}.json"),
            "noncanonical evidence recipe name"
        );
        let bytes = audit::read_bytes(&directory, Path::new(&name), super::MAX_IMPORT_PLAN)?
            .context("evidence recipe disappeared")?;
        total = total
            .checked_add(bytes.len())
            .context("evidence recipe inventory byte overflow")?;
        ensure!(
            total <= super::MAX_TREE,
            "evidence recipe inventory exceeds byte bound"
        );
        let recipe = decode(&bytes)?;
        ensure!(
            recipe.intent()?.operation == operation,
            "evidence recipe names another operation"
        );
        result.insert(operation, recipe);
    }
    Ok(result)
}
