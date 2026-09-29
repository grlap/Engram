//! The landings completion seals record, read for the doctor's on-request
//! check. Seals without a landing, including every seal written before the
//! field existed, are skipped as stored.

use crate::domain::{CompletionLanding, CompletionSeal, WorkId};
use crate::storage::{SqliteStore, StoreError};

/// One landing a completion seal records, with the item it completed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedLanding {
    pub work_id: WorkId,
    /// The item's short reference, as the words print it.
    pub work_ref: String,
    pub landing: CompletionLanding,
}

impl SqliteStore {
    /// Every landing the store's completion seals record, in the order the
    /// seals were written.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the seals cannot be read or decoded.
    pub fn recorded_landings(&self) -> Result<Vec<RecordedLanding>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT seal_json FROM work_completion_seals ORDER BY rowid")?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut landings = Vec::new();
        for bytes in rows {
            let seal: CompletionSeal = serde_json::from_slice(&bytes?)?;
            if let Some(landing) = seal.landing {
                landings.push(RecordedLanding {
                    work_id: seal.work_id,
                    work_ref: super::super::planning::short_ref(seal.work_id),
                    landing,
                });
            }
        }
        Ok(landings)
    }
}
