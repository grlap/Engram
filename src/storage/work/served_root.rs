//! The named root a completed item's claim worked in, read from its seal so
//! completion can tell the agent the workspace has served this item. It
//! writes nothing and touches no file: removing a workspace stays the
//! agent's own act.

use super::named_root_state_on;
use crate::ObjectId;
use crate::domain::{NamedRootState, WorkClaimId, WorkRunId};
use crate::storage::{SqliteStore, StoreError};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// The workspace a completed claim had named as its root when it completed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServedNamedRoot {
    pub workspace_id: String,
    /// Another live claim of the same session still names this workspace, so
    /// it is not yet free to remove.
    pub shared: bool,
}

impl SqliteStore {
    /// The named root bound on `seal_id`'s claim at its completion cut, or
    /// `None` when the claim named none, and whether another live claim of
    /// the same holder session names the same workspace at `now`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the seal, its run or a named-root record
    /// cannot be read.
    pub(crate) fn served_named_root(
        &self,
        seal_id: &ObjectId,
        now: DateTime<Utc>,
    ) -> Result<Option<ServedNamedRoot>, StoreError> {
        self.work_read_snapshot(|store| {
            let seal = store.completion_seal_at(seal_id)?;
            let NamedRootState::Bound { workspace_id, .. } = named_root_state_on(
                &store.connection,
                seal.run_id,
                seal.claim_id,
                seal.completion_cut.position,
            )?
            else {
                return Ok(None);
            };
            // The holder of the exact claim the seal names.
            let holder: String = store.connection.query_row(
                "SELECT holder_session_id FROM work_claims WHERE claim_id = ?1",
                [seal.claim_id.0.to_string()],
                |row| row.get(0),
            )?;
            let shared = other_live_claim_names_on(
                &store.connection,
                &holder,
                seal.run_id,
                &workspace_id,
                now,
            )?;
            Ok(Some(ServedNamedRoot {
                workspace_id,
                shared,
            }))
        })
    }
}

/// Whether a live claim of `holder` on a run other than `completed` names
/// `workspace_id` as its bound root now.
fn other_live_claim_names_on(
    connection: &Connection,
    holder: &str,
    completed: WorkRunId,
    workspace_id: &str,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let mut statement = connection.prepare(
        "SELECT run_id, claim_id FROM work_claims
         WHERE holder_session_id = ?1 AND state = 'active' AND expires_at_ms > ?2
           AND run_id != ?3",
    )?;
    let claims = statement
        .query_map(
            params![holder, now.timestamp_millis(), completed.0.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for (run, claim) in claims {
        let (Ok(run), Ok(claim)) = (run.parse(), claim.parse()) else {
            return Err(StoreError::InvalidWorkProjection(
                "a live claim row holds a malformed id".into(),
            ));
        };
        let (run, claim) = (WorkRunId(run), WorkClaimId(claim));
        if let NamedRootState::Bound {
            workspace_id: other,
            ..
        } = named_root_state_on(connection, run, claim, i64::MAX)?
            && other == workspace_id
        {
            return Ok(true);
        }
    }
    Ok(false)
}
