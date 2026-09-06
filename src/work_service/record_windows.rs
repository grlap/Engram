//! Explicit show windows: select newest records, retain canonical membership
//! and feed ordering, and let verbs fit the final emitted representation.

use super::*;
use crate::domain::WorkCatalogReadCut;
use crate::storage::{
    WorkRecordAddress, WorkRecordContent, WorkRecordIndex, WorkRecordKind, WorkRecordOrder,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordCursor {
    project: ProjectId,
    work: WorkId,
    kind: WorkRecordKind,
    cut: WorkCatalogReadCut,
    total: usize,
    address: WorkRecordAddress,
    order: WorkRecordOrder,
}

/// Newest-first candidates. Total and newer are measured over the complete
/// immutable-member/native-feed index; only the emitted oldest row advances.
pub(crate) struct WorkRecordWindow {
    pub kind: WorkRecordKind,
    pub rows: Vec<WorkRecordRow>,
    pub total: usize,
    pub newer: usize,
    project: ProjectId,
    work: WorkId,
    cut: WorkCatalogReadCut,
}

pub(crate) struct WorkRecordRow {
    pub locator: String,
    pub kind: String,
    pub summary: String,
    pub body_bytes: usize,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
    pub body_omitted: bool,
    pub summary_truncated: bool,
    address: WorkRecordAddress,
    order: WorkRecordOrder,
}

impl WorkRecordWindow {
    pub(crate) fn continuation(&self, visible: usize) -> Result<Option<String>, StoreError> {
        if visible == 0 || self.newer + visible == self.total {
            return Ok(None);
        }
        let row = &self.rows[visible - 1];
        super::continuation::encode(
            "s1-",
            &RecordCursor {
                project: self.project.clone(),
                work: self.work,
                kind: self.kind,
                cut: self.cut.clone(),
                total: self.total,
                address: row.address.clone(),
                order: row.order.clone(),
            },
        )
        .map(Some)
        .ok_or_else(|| invalid("show continuation metadata exceeds its budget"))
    }
}

impl LocalWorkService {
    pub(crate) fn work_record_window(
        &self,
        work_ref: &str,
        kind: WorkRecordKind,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(WorkFocusView, WorkRecordWindow), StoreError> {
        let cursor = after
            .map(|token| {
                super::continuation::decode::<RecordCursor>("s1-", token)
                    .ok_or_else(|| invalid("invalid show cursor; start a fresh window"))
            })
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.project != self.project_id || cursor.kind != kind)
        {
            return Err(invalid(
                "continuation belongs to another item, project or window kind",
            ));
        }
        let mut store = self.store_at(now)?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.work != item.work_id)
        {
            return Err(invalid(
                "continuation belongs to another item, project or window kind",
            ));
        }
        store.focus_work_session(&self.project_id, &self.session_id, item.work_id, now)?;
        store.work_read_snapshot(|store| {
            let cut = store.work_read_cut(&self.project_id, now)?;
            let index = store.work_record_index(&self.project_id, item.work_id, kind)?;
            let end = if let Some(cursor) = &cursor {
                if cut.project_position != cursor.cut.project_position
                    || now < cursor.cut.observed_at
                    || cursor
                        .cut
                        .valid_until_ms
                        .is_some_and(|until| now.timestamp_millis() >= until)
                    || cursor.total != index.len()
                {
                    return Err(invalid(
                        "show read cut changed or expired; start a fresh window",
                    ));
                }
                index
                    .iter()
                    .position(|row| row.address == cursor.address && row.order == cursor.order)
                    .ok_or_else(|| invalid("continuation boundary no longer matches this window"))?
            } else {
                index.len()
            };
            let view = self.focus_view_for_projection(
                store,
                item.work_id,
                true,
                true,
                super::service::FocusText::Full,
                now,
            )?;
            let mut rows = Vec::new();
            let mut bytes = 0;
            for entry in index[..end].iter().rev().take(64) {
                let mut row = project_record(store, &self.project_id, item.work_id, entry, kind)?;
                let size = row.summary.len() + row.refs.iter().map(String::len).sum::<usize>();
                if size > MAX_AGENT_WORK_RESPONSE_BYTES {
                    row.summary.clear();
                    row.refs.clear();
                    row.body_omitted = true;
                }
                bytes += row.summary.len() + row.refs.iter().map(String::len).sum::<usize>() + 64;
                rows.push(row);
                if bytes >= MAX_AGENT_WORK_RESPONSE_BYTES {
                    break;
                }
            }
            Ok((
                view,
                WorkRecordWindow {
                    kind,
                    rows,
                    total: index.len(),
                    newer: index.len() - end,
                    project: self.project_id.clone(),
                    work: item.work_id,
                    cut,
                },
            ))
        })
    }

    /// Complete immutable note detail; deliberately no window byte limit.
    /// Hash prefixes resolve only within this item's note membership.
    pub(crate) fn work_note_detail(
        &self,
        work_ref: &str,
        locator: &str,
        now: DateTime<Utc>,
    ) -> Result<(String, WorkRecordRow), StoreError> {
        let (prefix, member) = locator
            .split_once(':')
            .map_or((locator, None), |(hash, member)| (hash, Some(member)));
        if !(8..=64).contains(&prefix.len())
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
            || member
                .is_some_and(|member| member.parse::<usize>().ok().is_none_or(|index| index == 0))
        {
            return Err(reference_invalid(
                "use at least eight hex digits and, for inherited notes, :INDEX",
                Vec::new(),
            ));
        }
        let mut store = self.store_at(now)?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, item.work_id, now)?;
        store.work_read_snapshot(|store| {
            let index =
                store.work_record_index(&self.project_id, item.work_id, WorkRecordKind::Notes)?;
            let prefix = prefix.to_ascii_lowercase();
            let matches = index
                .iter()
                .filter(|row| {
                    row.address.hash.as_str().starts_with(&prefix)
                        && member.is_none_or(|member| {
                            row.locator
                                .split_once(':')
                                .is_some_and(|(_, suffix)| suffix == member)
                        })
                })
                .collect::<Vec<_>>();
            if matches.len() != 1 || (member.is_none() && matches[0].address.member.is_some()) {
                return Err(reference_invalid(
                    if matches.is_empty() {
                        "note locator does not belong to this item"
                    } else {
                        "note locator is ambiguous or needs its immutable member index"
                    },
                    matches.iter().map(|row| row.locator.clone()).collect(),
                ));
            }
            Ok((
                item.short_ref.clone(),
                project_record(
                    store,
                    &self.project_id,
                    item.work_id,
                    matches[0],
                    WorkRecordKind::Notes,
                )?,
            ))
        })
    }
}

fn project_record(
    store: &SqliteStore,
    project: &ProjectId,
    work: WorkId,
    entry: &WorkRecordIndex,
    kind: WorkRecordKind,
) -> Result<WorkRecordRow, StoreError> {
    let mut note_body_bytes = None;
    let mut summary_truncated = false;
    let (label, summary, refs, actor, recorded_at) =
        match store.work_record_content(project, work, entry)? {
            WorkRecordContent::Note(mut note) => {
                super::projection::project_full_note(&mut note)?;
                note_body_bytes = Some(note.summary.len());
                let label = serde_json::to_value(note.kind)?
                    .as_str()
                    .unwrap_or("note")
                    .to_owned();
                let summary = if kind == WorkRecordKind::History {
                    let summary = compact_text(&note.summary);
                    summary_truncated = summary != note.summary;
                    summary
                } else {
                    note.summary
                };
                let refs = if kind == WorkRecordKind::History {
                    Vec::new()
                } else {
                    note.refs
                };
                (label, summary, refs, note.actor, note.recorded_at)
            }
            WorkRecordContent::Event(event, position) => {
                let projected = project_work_event(store, &event, &position)?;
                (
                    projected.change_kind,
                    projected.summary,
                    Vec::new(),
                    event.actor.clone(),
                    event.created_at,
                )
            }
            WorkRecordContent::InheritedHistory {
                kind,
                summary,
                actor,
                recorded_at,
            } => (kind, compact_text(&summary), Vec::new(), actor, recorded_at),
        };
    Ok(WorkRecordRow {
        locator: entry.locator.clone(),
        body_bytes: note_body_bytes.unwrap_or(summary.len()),
        summary,
        kind: label,
        refs,
        actor,
        recorded_at,
        body_omitted: false,
        summary_truncated,
        address: entry.address.clone(),
        order: entry.order.clone(),
    })
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkShowCursorInvalid {
        reason: reason.into(),
    }
}
fn reference_invalid(reason: &str, mut candidates: Vec<String>) -> StoreError {
    let more = candidates.len().saturating_sub(16);
    candidates.truncate(16);
    StoreError::WorkNoteReferenceInvalid {
        reason: reason.into(),
        candidates,
        more,
    }
}
