//! Locate existing catalog rows through their already-indexed short reference.

use super::{Connection, StoreError, WorkItem, normalize_work_catalog_key, params};

#[cfg(test)]
mod tests;

const MATCHING_ROWIDS: &str =
    "SELECT rowid FROM work_catalog_fts WHERE work_catalog_fts MATCH ?1 AND work_id = ?2";

pub(super) fn remove_prior_rows(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    // The immutable reference is already part of the stored search text.
    // MATCH cost depends on trigram posting lists and candidate rows, with
    // worst-case O(N) in catalog size. Other items may mention the reference;
    // the exact UUID predicate prevents deleting their rows. This is not a
    // constant-time or bounded locator.
    let phrase = format!(
        "\"{}\"",
        normalize_work_catalog_key(&item.short_ref).replace('"', "\"\"")
    );
    let work_id = item.work_id.0.to_string();
    let rowids = {
        let mut statement = connection.prepare(MATCHING_ROWIDS)?;
        statement
            .query_map(params![phrase, work_id], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if rowids.is_empty() {
        // An existing item's row whose text lost its reference must still
        // be replaced. Missing rows also take this O(N) UUID-scan fallback.
        connection.execute("DELETE FROM work_catalog_fts WHERE work_id = ?1", [work_id])?;
    } else {
        let mut delete = connection.prepare("DELETE FROM work_catalog_fts WHERE rowid = ?1")?;
        // Keep all exact-UUID matches, including duplicate indexed rows.
        for rowid in rowids {
            delete.execute([rowid])?;
        }
    }
    Ok(())
}
