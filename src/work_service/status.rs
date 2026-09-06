//! Bounded advisory status projection; no claim is inferred from a read.

use super::*;

/// Advisory projection of an immutable, capture-qualified note, not authority.
/// Selection follows project-feed order (inherited members precede native
/// appends), never `recorded_at`. Rendering may shorten only the preview.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCurrentStatus {
    /// Complete source text, or a bounded first nonblank line/prefix.
    pub body_or_first_line: String,
    /// False requires explicit omission disclosure and full-detail navigation.
    pub complete: bool,
    /// Asserted capture time, not selection order.
    pub recorded_at: DateTime<Utc>,
    /// Read-only native hash or inherited `RECORD_HASH:INDEX` detail address.
    pub locator: String,
    /// Relative asserted actor/session context; not authenticated identity.
    pub by: String,
}

impl WorkCurrentStatus {
    /// Called only after the enclosing serialized/terminal receipt fails its
    /// budget. Progressively shorten recoverable text without losing metadata.
    pub(crate) fn shorten_preview(&mut self) -> bool {
        // Small commitments are already useful bounded context. Unrelated
        // verbose metadata pressure must not erase them into an ellipsis.
        if serde_json::to_vec(&self.body_or_first_line).is_ok_and(|bytes| bytes.len() <= 128)
            && super::terminal_safe_multiline(&self.body_or_first_line).len() <= 128
        {
            return false;
        }
        if !self.complete && self.body_or_first_line == "…" {
            return false;
        }
        let line = self
            .body_or_first_line
            .split('\n')
            .find(|line| !line.trim().is_empty())
            .unwrap_or("…");
        let line = line.trim_end_matches('…');
        let mut end = line.len() / 2;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        self.body_or_first_line = format!("{}…", &line[..end]);
        self.complete = false;
        true
    }
}

pub(crate) fn shorten_status_previews(
    current: &mut Option<WorkCurrentStatus>,
    peer: &mut Option<WorkCurrentStatus>,
) -> bool {
    peer.as_mut()
        .is_some_and(WorkCurrentStatus::shorten_preview)
        || current
            .as_mut()
            .is_some_and(WorkCurrentStatus::shorten_preview)
}

impl LocalWorkService {
    pub(crate) fn status_for_item(
        &self,
        store: &SqliteStore,
        id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<(Option<WorkCurrentStatus>, Option<WorkCurrentStatus>), StoreError> {
        let item = store.get_work_item(id)?;
        if item.project_id != self.project_id {
            return Err(StoreError::InvalidWorkProjection(
                "status read crosses its project".into(),
            ));
        }
        let (current, peer) = store.current_status_notes(&item, now)?;
        Ok((
            current.map(|note| self.status_summary(note)),
            peer.map(|note| self.status_summary(note)),
        ))
    }

    fn status_summary(&self, selected: crate::storage::SelectedStatusNote) -> WorkCurrentStatus {
        let note = selected.note;
        let complete = note.summary.len() <= 768;
        let body = if complete {
            note.summary.as_str()
        } else {
            note.summary
                .split('\n')
                .find(|line| !line.trim().is_empty())
                .unwrap_or("…")
        };
        let body = if body.len() <= 768 {
            body.to_owned()
        } else {
            let mut end = 768;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &body[..end])
        };
        WorkCurrentStatus {
            body_or_first_line: body,
            complete,
            recorded_at: note.recorded_at,
            locator: selected.locator,
            by: if note.actor.actor_id == self.actor_id
                && note.actor.session_id.as_ref() == Some(&self.session_id)
            {
                "you"
            } else {
                "another session"
            }
            .into(),
        }
    }
}
