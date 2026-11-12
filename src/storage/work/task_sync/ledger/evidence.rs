//! Canonical evidence outcomes use the existing keyed exchange chain.

use rusqlite::{Connection, OptionalExtension, params};

use super::{Binding, StoreError, require};
use crate::task_sync::ledger::{EvidenceIntent, EvidenceReceipt};

mod import;
#[cfg(test)]
pub(super) use import::after_commit;

/// Historical receipts retain their original fingerprints. Validate canonical
/// manifest and admission links without reconstructing or hashing pack bodies;
/// the import writer compares the current selected payload before admission.
pub(super) fn validate_receipt_on(
    connection: &Connection,
    binding: &Binding,
    intent: &EvidenceIntent,
    receipt: &EvidenceReceipt,
) -> Result<(), StoreError> {
    receipt.validate(intent)?;
    for (pack_id, selected) in &intent.packs {
        let (publishing_origin, records) =
            super::super::evidence::retained_pack_header_on(connection, &binding.project, pack_id)
                .map_err(|error| super::super::invalid(error.to_string()))?;
        require(
            publishing_origin == selected.publishing_origin && records == selected.records,
            "evidence receipt pack differs from its frozen selection",
        )?;
        let admission: Option<(String, String)> = connection.query_row(
            "SELECT json_extract(canonical_json,'$.pack_id'),json_extract(canonical_json,'$.project_id')
             FROM objects WHERE object_id=?1 AND object_kind=?2",
            params![selected.admission.as_str(),super::super::evidence::ADMISSION_KIND],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        require(
            admission == Some((pack_id.to_string(), binding.project.0.clone())),
            "evidence receipt admission is missing or belongs to another pack",
        )?;
    }
    Ok(())
}
