//! Monotonic shared information; none of these values grants local authority.

use super::{TaskDocument, activity::ActivityNotice};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Conflict {
    pub reason: &'static str,
    pub detail: String,
}

fn conflict(reason: &'static str, detail: impl Into<String>) -> Conflict {
    Conflict {
        reason,
        detail: detail.into(),
    }
}

/// One episode's immutable binding and monotonically advancing assertion.
/// A handoff may use one event to close an episode and start another, so event
/// identities are deliberately not required to be unique across episodes.
pub(crate) fn advance(before: &ActivityNotice, after: &ActivityNotice) -> Result<(), Conflict> {
    if before == after {
        return Ok(());
    }
    if before.activity_id != after.activity_id
        || before.task_id != after.task_id
        || before.origin_machine != after.origin_machine
        || before.actor != after.actor
    {
        return Err(conflict(
            "shared_identity",
            "activity identity or attribution differs",
        ));
    }
    if after.state_revision < before.state_revision {
        return Err(conflict(
            "activity_rollback",
            "activity revision moved backwards",
        ));
    }
    if after.state_revision == before.state_revision || after.event_id == before.event_id {
        return Err(conflict(
            "activity_conflict",
            "activity revision or transition identity conflicts",
        ));
    }
    Ok(())
}

/// Validate the retained subset. New entries pass their ordinary structural,
/// disclosure and store-wide ownership checks separately.
pub(crate) fn retains(before: &TaskDocument, after: &TaskDocument) -> Result<(), Conflict> {
    for note in &before.notes {
        let next = after
            .notes
            .iter()
            .find(|next| next.id == note.id)
            .ok_or_else(|| conflict("note_removal", "a retained shared note is missing"))?;
        if next != note {
            return Err(conflict(
                "shared_identity",
                "a shared note's immutable content differs",
            ));
        }
    }
    for (machine, entries) in &before.activity {
        for (id, notice) in entries {
            let next = after
                .activity
                .get(machine)
                .and_then(|entries| entries.get(id))
                .ok_or_else(|| {
                    conflict("activity_removal", "a retained activity episode is missing")
                })?;
            advance(notice, next)?;
        }
    }
    Ok(())
}

/// Governing equality excludes only inert shared members.
pub(crate) fn governing_equal(before: &TaskDocument, after: &TaskDocument) -> bool {
    let mut before = before.clone();
    before.notes.clone_from(&after.notes);
    before.activity.clone_from(&after.activity);
    before == *after
}
