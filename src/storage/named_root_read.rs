//! A host's read of one claim's named-root lifecycle on its run, for any run
//! and claim the store holds, current or long finished. It reads one
//! snapshot, writes nothing and needs no live holder: the host reconciles
//! the roots it named for claims it no longer holds.

use super::{Connection, SessionId, SqliteStore, StoreError, work};
use crate::domain::{
    FeedId, FeedPosition, NamedRootEventReference, NamedRootRead, NamedRootReadClaim,
    NamedRootReadRun, ProjectId, WorkClaimId, WorkRunId,
};

impl SqliteStore {
    /// Reads `claim_id`'s named-root lifecycle on `run_id` for the bound host
    /// session. The session's own work binding plays no part: any run and
    /// claim of its project can be read. The state is the one the session
    /// status and the turn receipts derive, at this read's run-feed cut.
    ///
    /// # Errors
    ///
    /// Returns the control session's own refusals for a superseded
    /// connection, an unbound session or a mismatched project or routing
    /// token; [`StoreError::NamedRootReadRefused`] when the run is unknown,
    /// the claim does not belong to it, or it belongs to another project; and
    /// other [`StoreError`] values when a record fails its canonical
    /// association or cannot be read. A refusal is never a named-root state.
    pub(crate) fn read_named_root(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        run_id: WorkRunId,
        claim_id: WorkClaimId,
    ) -> Result<NamedRootRead, StoreError> {
        work::on_one_snapshot(&self.connection, |connection| {
            Self::verify_control_connection(connection, session_id, connection_token)?;
            let session = Self::load_control_session_on(connection, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
            Self::verify_control_session(&session, project_id, routing_token)?;
            read_named_root_on(connection, project_id, run_id, claim_id)
        })
    }
}

fn read_named_root_on(
    connection: &Connection,
    project_id: &ProjectId,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
) -> Result<NamedRootRead, StoreError> {
    let refused = |reason: &str| StoreError::NamedRootReadRefused(reason.into());
    // An unknown run is the caller's mistake; a run whose feed exists without
    // its row is a damaged store, and says so.
    let (run_row, run_feed): (bool, bool) = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_runs WHERE run_id = ?1),
                EXISTS(SELECT 1 FROM work_feed_entries
                       WHERE feed_kind = 'run_execution' AND feed_id = ?1)",
        [run_id.0.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if !run_row {
        if run_feed {
            return Err(StoreError::InvalidWorkProjection(format!(
                "run {run_id:?} has a run feed but no run row"
            )));
        }
        return Err(refused("the run is unknown in this store"));
    }
    let run = work::load_work_run(connection, run_id)?;
    let claim = work::load_work_claim_optional(connection, run_id)?
        .filter(|claim| claim.claim_id == claim_id)
        .ok_or_else(|| refused("the claim does not belong to the run"))?;
    if claim.run_id != run.run_id || claim.work_id != run.work_id {
        return Err(StoreError::InvalidWorkProjection(
            "a claim crosses its canonical run".into(),
        ));
    }
    let item = work::load_work_item(connection, run.work_id)?;
    if item.project_id != *project_id {
        return Err(refused("the run belongs to another project"));
    }
    let read_cut = work::current_run_feed_cut_on(connection, run_id)?;
    let named_root = work::named_root_state_on(connection, run_id, claim_id, read_cut.position)?;
    let latest_event =
        work::latest_named_root_event_record_on(connection, run_id, claim_id, read_cut.position)?
            .map(|(position, event_id, event)| {
                if event.project_id != *project_id
                    || event.work_id != run.work_id
                    || event.root_execution_id != run.root_execution_id
                    || event.run_id != run_id
                    || event.claim_id != claim_id
                {
                    return Err(StoreError::InvalidWorkProjection(
                        "a named-root event crosses its claim's run".into(),
                    ));
                }
                Ok(NamedRootEventReference {
                    event: event_id,
                    position: FeedPosition {
                        feed: FeedId::RunExecution(run_id),
                        position,
                    },
                    generation: event.generation,
                    kind: event.kind,
                    workspace_id: event.workspace_id,
                    named_at: event.named_at,
                    end_reason: event.end_reason,
                })
            })
            .transpose()?;
    Ok(NamedRootRead {
        project_id: project_id.clone(),
        work_id: run.work_id,
        root_execution_id: run.root_execution_id,
        run_id,
        claim_id,
        run: NamedRootReadRun {
            state: run.state,
            generation: run.generation,
        },
        claim: NamedRootReadClaim {
            state: claim.state,
            holder: claim.holder,
            expires_at: claim.expires_at,
            revision: claim.revision,
            fence: claim.fence,
        },
        named_root,
        latest_event,
        read_cut,
    })
}
