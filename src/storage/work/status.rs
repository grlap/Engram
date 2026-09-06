//! Candidate-scoped status selection over immutable notes, not a status ledger.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::feeds::load_typed_work_object;
use super::notes::{NOTE_OBJECTS, WorkNoteRecord, load_note};
use super::query::{load_work_claim_optional, restored_records_with_hash_for_item};
use crate::domain::{StatusNoteRole, status_note_role};
use crate::storage::{SqliteStore, StoreError};
use crate::{ObjectHash, WorkClaimState, WorkEvent, WorkItem};

/// Verified, immutable capture qualification; project-feed order selects native
/// notes, after inherited generation/member order. Capture time is not order.
pub(crate) struct SelectedStatusNote {
    pub note: WorkNoteRecord,
    /// Read-only native hash or inherited `RECORD_HASH:INDEX` detail address.
    pub locator: String,
}

impl SqliteStore {
    pub(crate) fn current_status_notes(
        &self,
        item: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<(Option<SelectedStatusNote>, Option<SelectedStatusNote>), StoreError> {
        let owner = self.status_owner(item, now)?;
        let mut current = owner
            .as_deref()
            .map(|actor| self.select_status_note(item, Some(actor)))
            .transpose()?
            .flatten();
        let mut peer = self.select_status_note(item, None)?;
        if (owner.is_some() && current.is_none()) || peer.is_none() {
            // The indexed empty result costs no canonical decode. Both
            // selections share verified history and its source hash.
            for (hash, record) in
                restored_records_with_hash_for_item(&self.connection, item.work_id)?
                    .into_iter()
                    .rev()
            {
                for (index, note) in record.history.notes.iter().enumerate().rev() {
                    if note.evidence_kind != crate::WorkEvidenceKind::Generic {
                        continue;
                    }
                    let target = match status_note_role(&note.actor) {
                        Some(StatusNoteRole::Owner)
                            if current.is_none()
                                && owner.as_deref() == Some(note.actor.actor_id.as_str()) =>
                        {
                            &mut current
                        }
                        Some(StatusNoteRole::Peer) if peer.is_none() => &mut peer,
                        _ => continue,
                    };
                    *target = Some(SelectedStatusNote {
                        locator: format!("{hash}:{}", index + 1),
                        note: WorkNoteRecord {
                            kind: note.evidence_kind,
                            summary: note.summary.clone(),
                            gate: None,
                            refs: note.refs.clone(),
                            actor: note.actor.clone(),
                            recorded_at: note.recorded_at,
                        },
                    });
                }
                if (owner.is_none() || current.is_some()) && peer.is_some() {
                    break;
                }
            }
        }
        Ok((current, peer))
    }

    fn status_owner(
        &self,
        item: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError> {
        let claim = item
            .active_run_id
            .map(|run| load_work_claim_optional(&self.connection, run))
            .transpose()?
            .flatten()
            .filter(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now);
        let Some(claim) = claim else {
            return Ok(item.assigned_to.clone());
        };
        // A session spelling is not an actor principal. Resolve the actor from
        // this claim epoch's verified claim/renewal/accepted-handoff event.
        let hash: Option<String> = self
            .connection
            .query_row(
                "SELECT entry.object_hash FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'project' AND entry.feed_id = ?1
               AND entry.work_id = ?2 AND entry.object_kind = 'work_event'
               AND json_extract(object.canonical_json, '$.claim.claim_id') = ?3
               AND json_extract(object.canonical_json, '$.claim.fence') = ?4
               AND json_extract(object.canonical_json, '$.transition.kind')
                   IN ('claimed', 'claim_renewed', 'handed_off')
             ORDER BY entry.position DESC LIMIT 1",
                params![
                    item.project_id.0,
                    item.work_id.0.to_string(),
                    claim.claim_id.0.to_string(),
                    claim.fence
                ],
                |row| row.get(0),
            )
            .optional()?;
        let hash = hash.ok_or_else(|| invalid("live claim has no accountable actor event"))?;
        let event: WorkEvent =
            load_typed_work_object(&self.connection, &parse_hash(hash)?, "work_event")?;
        if event.project_id != item.project_id
            || event.work_id != item.work_id
            || event.actor.session_id.as_ref() != Some(&claim.holder)
            || !event.claim.as_ref().is_some_and(|stored| {
                stored.claim_id == claim.claim_id && stored.fence == claim.fence
            })
        {
            return Err(invalid("status owner event differs from its claim binding"));
        }
        Ok(Some(event.actor.actor_id))
    }

    fn select_status_note(
        &self,
        item: &WorkItem,
        owner: Option<&str>,
    ) -> Result<Option<SelectedStatusNote>, StoreError> {
        let role = if owner.is_some() { "owner" } else { "peer" };
        let sql = format!(
            "WITH notes AS ({NOTE_OBJECTS})
             SELECT entry.object_hash, notes.family, entry.object_kind
             FROM notes CROSS JOIN work_feed_entries entry
               ON entry.feed_kind = 'project' AND entry.feed_id = ?2 AND entry.object_hash = notes.hash
             CROSS JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.object_kind IN ('work_evidence', 'work_observation', 'work_restored_evidence')
               AND (?3 IS NULL OR json_extract(object.canonical_json, '$.actor.actor_id') = ?3)
               AND EXISTS (SELECT 1 FROM json_each(object.canonical_json, '$.actor.provenance_chain') link
                 WHERE json_extract(link.value, '$.source') = ?4
                   AND json_extract(link.value, '$.reference') = ?5)
             ORDER BY entry.position DESC LIMIT 1"
        );
        let selected: Option<(String, String, String)> = self
            .connection
            .query_row(
                &sql,
                params![
                    item.work_id.0.to_string(),
                    item.project_id.0,
                    owner,
                    crate::domain::STATUS_NOTE_SOURCE,
                    role
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((hash, family, kind)) = selected {
            let hash = parse_hash(hash)?;
            let note = load_note(&self.connection, item.work_id, &hash, &family, &kind)?;
            validate_selection(&note, owner)?;
            return Ok(Some(SelectedStatusNote {
                locator: hash.as_str().into(),
                note,
            }));
        }
        Ok(None)
    }
}

fn validate_selection(note: &WorkNoteRecord, owner: Option<&str>) -> Result<(), StoreError> {
    let expected = if owner.is_some() {
        StatusNoteRole::Owner
    } else {
        StatusNoteRole::Peer
    };
    if note.kind != crate::WorkEvidenceKind::Generic
        || note.gate.is_some()
        || status_note_role(&note.actor) != Some(expected)
        || owner.is_some_and(|owner| owner != note.actor.actor_id)
    {
        return Err(invalid(
            "status selection differs from canonical note qualification",
        ));
    }
    Ok(())
}

fn parse_hash(value: String) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_stored(value.clone()).ok_or(StoreError::InvalidStoredHash(value))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
