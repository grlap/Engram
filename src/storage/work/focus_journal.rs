//! How one word moved this session's focus, recorded as it happens.
//!
//! A word installs a [`FocusJournal`] on its thread before it acts and takes
//! the net [`FocusChange`] when it ends. Storage appends to it only after a
//! transaction commits: a focus move from the transaction that wrote the new
//! focus, a claim's binding from the transaction that took or renewed the
//! claim, and an ended claim from the completion that ended it. Nothing here
//! is stored, so a retried word reports only what that retry itself did.
//!
//! The journal is thread-local because every word runs synchronously on one
//! thread from its start to its end. A word body must never await while its
//! journal is installed; another call could then run on the same thread and
//! write into it.

use std::cell::RefCell;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;

use super::super::StoreError;
use super::completion::feed_head;
use super::feeds::named_root_state_on;
use super::planning::validate_control_work_binding_on;
use super::query::{load_work_claim_optional, load_work_run};
use crate::domain::{
    ControlWorkBinding, FeedId, NamedRootState, ProjectId, SessionId, WorkClaim, WorkClaimId,
    WorkClaimState, WorkId, WorkItem, WorkLifecycle, WorkRun, WorkRunState,
};

/// Bytes a word that can move focus leaves free when it fits its receipt, so
/// its focus disclosure always fits beside it. The disclosure is bounded: two
/// short refs, a claim id and fence, a generation and a workspace name of at
/// most `MAX_DISCLOSED_WORKSPACE_JSON_BYTES` bytes on either surface, in JSON
/// or as one text line; a test measures the largest one against this reserve.
pub(crate) const FOCUS_CHANGE_RESERVE: usize = 640;

/// The longest workspace name a disclosure carries, measured both as encoded
/// JSON and as escaped terminal text; a longer one is named by its UTF-8
/// length instead.
pub(crate) const MAX_DISCLOSED_WORKSPACE_JSON_BYTES: usize = 192;

/// What the host could bind for the session's focused item from its next
/// turn: the live claim this session holds, and the claim's named source
/// root when one is bound.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FocusBinding {
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
}

/// One word's net change of focus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusChange {
    pub from: Option<String>,
    pub to: Option<String>,
    /// The binding captured for `to` in this word's own transactions, when it
    /// still held after the last of them.
    pub binding: Option<FocusBinding>,
}

#[derive(Debug)]
enum Event {
    Moved {
        from: Option<(WorkId, String)>,
        to: (WorkId, String),
        binding: Option<FocusBinding>,
    },
    Bound {
        work: WorkId,
        binding: Option<FocusBinding>,
    },
    Ended {
        work: WorkId,
    },
}

struct Installed {
    project_id: ProjectId,
    session_id: SessionId,
    events: Vec<Event>,
}

thread_local! {
    static JOURNAL: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

/// The guard of one word's journal. Dropping it clears the journal, so a
/// return, an error or a panic never leaves one installed.
pub struct FocusJournal {
    owner: bool,
}

impl FocusJournal {
    /// Installs a journal for `session_id` on this thread. A word running
    /// inside another word's journal shares it: only the outer guard owns
    /// and takes it.
    #[must_use]
    pub fn begin(project_id: &ProjectId, session_id: &SessionId) -> Self {
        let owner = JOURNAL.with(|journal| {
            let mut journal = journal.borrow_mut();
            if journal.is_some() {
                return false;
            }
            *journal = Some(Installed {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                events: Vec::new(),
            });
            true
        });
        Self { owner }
    }

    /// The net change: the first move's origin and the last move's target,
    /// with the binding last captured for that target. `None` when focus did
    /// not move, or moved back to where it started.
    #[must_use]
    pub fn finish(self) -> Option<FocusChange> {
        if !self.owner {
            return None;
        }
        let installed = JOURNAL.with(|journal| journal.borrow_mut().take())?;
        net(installed.events)
    }
}

impl Drop for FocusJournal {
    fn drop(&mut self) {
        if self.owner {
            JOURNAL.with(|journal| journal.borrow_mut().take());
        }
    }
}

fn net(events: Vec<Event>) -> Option<FocusChange> {
    let mut origin: Option<Option<(WorkId, String)>> = None;
    let mut target: Option<(WorkId, String)> = None;
    let mut binding = None;
    for event in events {
        match event {
            Event::Moved {
                from,
                to,
                binding: captured,
            } => {
                if origin.is_none() {
                    origin = Some(from);
                }
                target = Some(to);
                binding = captured;
            }
            Event::Bound {
                work,
                binding: captured,
            } => {
                if target.as_ref().is_some_and(|(id, _)| *id == work) {
                    binding = captured;
                }
            }
            Event::Ended { work } => {
                if target.as_ref().is_some_and(|(id, _)| *id == work) {
                    binding = None;
                }
            }
        }
    }
    let (to_id, to_ref) = target?;
    let from = origin.flatten();
    if from.as_ref().is_some_and(|(id, _)| *id == to_id) {
        return None;
    }
    Some(FocusChange {
        from: from.map(|(_, short_ref)| short_ref),
        to: Some(to_ref),
        binding,
    })
}

/// Appends `event` when this thread's journal belongs to `session_id` and,
/// when one is named, to `project_id`. A work id names one item store-wide,
/// so an event that carries no project still concerns only its item.
fn record(project_id: Option<&ProjectId>, session_id: &SessionId, event: Event) {
    JOURNAL.with(|journal| {
        if let Some(installed) = journal.borrow_mut().as_mut()
            && project_id.is_none_or(|project_id| installed.project_id == *project_id)
            && installed.session_id == *session_id
        {
            installed.events.push(event);
        }
    });
}

/// Whether a journal is installed on this thread, so a caller can skip the
/// reads that only a journal needs.
pub(super) fn recording() -> bool {
    JOURNAL.with(|journal| journal.borrow().is_some())
}

/// A committed focus move.
pub(super) fn record_move(
    project_id: &ProjectId,
    session_id: &SessionId,
    from: Option<(WorkId, String)>,
    to: (WorkId, String),
    binding: Option<FocusBinding>,
) {
    record(
        Some(project_id),
        session_id,
        Event::Moved { from, to, binding },
    );
}

/// The binding a committed claim, renewal or recovery left on `work`.
pub(super) fn record_binding(
    project_id: &ProjectId,
    session_id: &SessionId,
    work: WorkId,
    binding: Option<FocusBinding>,
) {
    record(Some(project_id), session_id, Event::Bound { work, binding });
}

/// A committed completion: the holder's claim on `work` has ended.
pub(super) fn record_ended(session_id: &SessionId, work: WorkId) {
    record(None, session_id, Event::Ended { work });
}

/// The control binding a session's live claim on `work` would carry: the
/// open item's active run, claimed or active, under this session's active,
/// unexpired claim accepted at the item's revision.
pub(crate) fn owned_control_work_binding(
    work: &WorkItem,
    run: &WorkRun,
    claim: Option<&WorkClaim>,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Option<ControlWorkBinding> {
    let claim = claim?;
    (work.lifecycle == WorkLifecycle::Open
        && work.active_run_id == Some(run.run_id)
        && run.work_id == work.work_id
        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        && claim.work_id == work.work_id
        && claim.run_id == run.run_id
        && claim.accepted_work_revision == work.revision
        && claim.holder == *session_id
        && claim.state == WorkClaimState::Active
        && claim.expires_at > now)
        .then_some(ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: work.revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        })
}

/// What the host could bind for `work` from the session's next turn, read on
/// `connection` (the caller's transaction): this session's live claim, as
/// session bind would accept it, with the claim's named root when bound. The
/// disclosure never refuses the word it describes: a read that fails names
/// no binding, and the word's own reads report any damage.
pub(super) fn focus_binding_on(
    connection: &Connection,
    project_id: &ProjectId,
    session_id: &SessionId,
    work: &WorkItem,
    now: DateTime<Utc>,
) -> Option<FocusBinding> {
    binding_on(connection, project_id, session_id, work, now)
        .ok()
        .flatten()
}

fn binding_on(
    connection: &Connection,
    project_id: &ProjectId,
    session_id: &SessionId,
    work: &WorkItem,
    now: DateTime<Utc>,
) -> Result<Option<FocusBinding>, StoreError> {
    let Some(run_id) = work.active_run_id else {
        return Ok(None);
    };
    let run = load_work_run(connection, run_id)?;
    let claim = load_work_claim_optional(connection, run_id)?;
    let Some(binding) = owned_control_work_binding(work, &run, claim.as_ref(), session_id, now)
    else {
        return Ok(None);
    };
    match validate_control_work_binding_on(connection, project_id, session_id, &binding, now) {
        Ok(()) => {}
        Err(
            StoreError::ControlWorkBindingStale { .. }
            | StoreError::WorkClaimMismatch { .. }
            | StoreError::WorkClaimLapsed { .. }
            | StoreError::WorkRevisionConflict { .. }
            | StoreError::WorkNotFound(_)
            | StoreError::InvalidWork(_),
        ) => return Ok(None),
        Err(error) => return Err(error),
    }
    let head = feed_head(connection, &FeedId::RunExecution(run_id))?;
    let (workspace_id, generation) =
        match named_root_state_on(connection, run_id, binding.claim_id, head)? {
            NamedRootState::Bound {
                workspace_id,
                generation,
                ..
            } => (Some(workspace_id), Some(generation)),
            _ => (None, None),
        };
    Ok(Some(FocusBinding {
        claim_id: binding.claim_id,
        claim_fence: binding.claim_fence,
        workspace_id,
        generation,
    }))
}

#[cfg(test)]
mod tests;
