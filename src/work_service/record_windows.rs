//! Explicit show windows: select newest records, retain canonical membership
//! and feed ordering, and let verbs fit the final emitted representation.

use super::*;
use crate::domain::WorkCatalogReadCut;
use crate::storage::{
    WorkRecordAddress, WorkRecordContent, WorkRecordFamily, WorkRecordIndex, WorkRecordKind,
    WorkRecordOrder,
};
use std::collections::BTreeMap;

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
    /// Complete item-family totals, including gates excluded by default.
    /// Only rows are fitted; these snapshot totals never change during fitting.
    pub families: BTreeMap<WorkRecordFamily, usize>,
    project: ProjectId,
    work: WorkId,
    cut: WorkCatalogReadCut,
}

pub(crate) struct WorkRecordRow {
    pub gate: Option<crate::GateEvidenceRecord>,
    pub family: WorkRecordFamily,
    pub locator: String,
    pub kind: String,
    pub summary: String,
    pub body_bytes: usize,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
    pub body_omitted: bool,
    pub summary_truncated: bool,
    /// The typed facts of a native verification record.
    pub verification: Option<crate::storage::VerificationFacts>,
    /// The complete inherited event or completion member, every field as the
    /// record stores it; only its detail read carries it.
    pub member: Option<serde_json::Value>,
    address: WorkRecordAddress,
    order: WorkRecordOrder,
}

impl WorkRecordRow {
    pub(crate) fn project_position(&self) -> Option<i64> {
        self.order.project_position()
    }
}

impl WorkRecordWindow {
    /// The read cut already validated and used by this window, not a delivery
    /// acknowledgement or authority token. Verbs may reflect it in the header.
    pub(crate) fn read_cut(&self) -> &WorkCatalogReadCut {
        &self.cut
    }

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
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            if cursor
                .as_ref()
                .is_some_and(|cursor| cursor.work != item.work_id)
            {
                return Err(invalid(
                    "continuation belongs to another item, project or window kind",
                ));
            }
            let cut = store.work_read_cut(&self.project_id, now)?;
            let mut index = store.work_record_index(&self.project_id, item.work_id, kind)?;
            let mut families = BTreeMap::new();
            for entry in &index {
                *families.entry(entry.record_family).or_insert(0) += 1;
            }
            if kind == WorkRecordKind::Notes {
                index.retain(|entry| entry.record_family != WorkRecordFamily::Gates);
            }
            // The window's records are immutable and only ever appended, so
            // the selected total and the boundary record decide whether the
            // remaining rows are still the ones the first page announced; a
            // write elsewhere in the project does not move them.
            let end = if let Some(cursor) = &cursor {
                if now < cursor.cut.observed_at || cursor.total != index.len() {
                    return Err(invalid("the window changed; start a fresh window"));
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
                false,
                true,
                super::service::FocusText::Full,
                now,
            )?;
            let mut rows = Vec::new();
            let mut bytes = 0;
            for entry in index[..end].iter().rev().take(64) {
                let mut row =
                    project_record(store, &self.project_id, item.work_id, entry, kind, false)?;
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
                    families,
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
    /// Record-id prefixes resolve only within this item's note membership.
    /// A native verification record also carries a page of its reconstructed
    /// obligation assessment; `after` continues that page and is refused for
    /// any other note.
    pub(crate) fn work_note_detail(
        &self,
        work_ref: &str,
        locator: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(String, WorkRecordRow, Option<VerificationAssessmentPage>), StoreError> {
        let cursor = after
            .map(|token| {
                super::continuation::decode::<AssessmentCursor>("v1-", token)
                    .ok_or_else(|| invalid("invalid assessment cursor; read the note detail again"))
            })
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.project != self.project_id)
        {
            return Err(invalid(
                "continuation belongs to another item, project or record",
            ));
        }
        let (prefix, member) = locator
            .split_once(':')
            .map_or((locator, None), |(prefix, member)| (prefix, Some(member)));
        // An inherited member is a note (INDEX), an event (event-INDEX) or
        // the completion; indexes are one-based immutable member positions.
        let positive = |index: &str| {
            !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && index.parse::<usize>().is_ok_and(|index| index > 0)
        };
        let history_member = member.is_some_and(|member| {
            member == "completion" || member.strip_prefix("event-").is_some_and(positive)
        });
        if !(8..=64).contains(&prefix.len())
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
            || member.is_some_and(|member| !history_member && !positive(member))
        {
            return Err(reference_invalid(
                "use at least eight hex digits and, for inherited members, :INDEX for a note, :event-INDEX for an event, or :completion",
                Vec::new(),
            ));
        }
        let kind = if history_member {
            WorkRecordKind::History
        } else {
            WorkRecordKind::NotesWithGates
        };
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            let index = store.work_record_index(&self.project_id, item.work_id, kind)?;
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
            let row = project_record(
                store,
                &self.project_id,
                item.work_id,
                matches[0],
                WorkRecordKind::NotesWithGates,
                true,
            )?;
            if cursor.as_ref().is_some_and(|cursor| {
                cursor.work != item.work_id || cursor.record != row.address.hash
            }) {
                return Err(invalid(
                    "continuation belongs to another item, project or record",
                ));
            }
            let assessment = if row.verification.is_some() && row.address.member.is_none() {
                store.verification_assessment(
                    item.work_id,
                    &row.address.hash,
                    cursor
                        .as_ref()
                        .map(|cursor| crate::storage::AssessmentBoundary {
                            trigger_position: cursor.trigger_position,
                            obligation_id: cursor.obligation,
                        }),
                    MAX_ASSESSMENT_ROWS,
                )?
            } else {
                None
            };
            let page = match (assessment, cursor) {
                // A cursor is issued only for a verification record and is
                // bound to it, so real input meets the record check above;
                // this refuses a cursor that names a note with no assessment.
                (None, Some(_)) => {
                    return Err(invalid(
                        "only a verification record's assessment continues; drop --after",
                    ));
                }
                (None, None) => None,
                (Some(assessment), cursor) => Some(assessment_page(
                    &self.project_id,
                    item.work_id,
                    &row.address.hash,
                    assessment,
                    cursor.as_ref(),
                )?),
            };
            Ok((item.short_ref.clone(), row, page))
        })
    }
}

/// Obligation assessments shown per page of a verification record's detail.
pub(crate) const MAX_ASSESSMENT_ROWS: usize = 8;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssessmentCursor {
    project: ProjectId,
    work: WorkId,
    run: WorkRunId,
    record: ObjectId,
    record_position: i64,
    /// The run feed's head when the page was read: any append since, such as
    /// a new obligation or a resolution, makes the continuation stale.
    head: i64,
    total: usize,
    /// The last obligation shown, in trigger-position and id order.
    trigger_position: i64,
    obligation: crate::domain::WorkObligationId,
}

/// One page of a verification record's reconstructed assessment: exact
/// counts over every candidate, the rows of this page, and the continuation
/// to the rest.
pub(crate) struct VerificationAssessmentPage {
    pub record_position: i64,
    pub cut_position: i64,
    pub total: usize,
    /// Candidates shown on earlier pages.
    pub earlier: usize,
    pub rows: Vec<crate::storage::VerificationObligationAssessment>,
    pub continuation: Option<String>,
}

impl VerificationAssessmentPage {
    /// Candidates not on this page, earlier pages included.
    pub(crate) fn omitted(&self) -> usize {
        self.total - self.rows.len()
    }
}

fn assessment_page(
    project: &ProjectId,
    work: WorkId,
    record: &ObjectId,
    assessment: crate::storage::VerificationAssessment,
    cursor: Option<&AssessmentCursor>,
) -> Result<VerificationAssessmentPage, StoreError> {
    let total = assessment.total;
    if let Some(cursor) = cursor {
        if cursor.run != assessment.run_id {
            return Err(invalid(
                "continuation belongs to another item, project or record",
            ));
        }
        if cursor.record_position != assessment.record_position
            || cursor.head != assessment.head_position
            || cursor.total != total
        {
            return Err(invalid(
                "the run changed since this page was read; read the note detail again",
            ));
        }
        if !assessment.boundary_found {
            return Err(invalid(
                "continuation boundary no longer matches this record",
            ));
        }
    }
    let shown = assessment.earlier + assessment.rows.len();
    let continuation = match assessment.rows.last() {
        Some(last) if shown < total => Some(
            super::continuation::encode(
                "v1-",
                &AssessmentCursor {
                    project: project.clone(),
                    work,
                    run: assessment.run_id,
                    record: record.clone(),
                    record_position: assessment.record_position,
                    head: assessment.head_position,
                    total,
                    trigger_position: last.trigger_position,
                    obligation: last.obligation_id,
                },
            )
            .ok_or_else(|| invalid("assessment continuation exceeds its budget"))?,
        ),
        _ => None,
    };
    Ok(VerificationAssessmentPage {
        record_position: assessment.record_position,
        cut_position: assessment.cut_position,
        total,
        earlier: assessment.earlier,
        rows: assessment.rows,
        continuation,
    })
}

/// One row of a window, or with `detail` the complete record a locator
/// names: an inherited event or completion then keeps its whole summary and
/// carries the complete member.
fn project_record(
    store: &SqliteStore,
    project: &ProjectId,
    work: WorkId,
    entry: &WorkRecordIndex,
    kind: WorkRecordKind,
    detail: bool,
) -> Result<WorkRecordRow, StoreError> {
    let mut note_body_bytes = None;
    let mut summary_truncated = false;
    let mut verification = None;
    let mut gate = None;
    let mut complete_member = None;
    let (label, summary, refs, actor, recorded_at) =
        match store.work_record_content(project, work, entry)? {
            WorkRecordContent::Note(mut note) => {
                super::projection::project_full_note(&mut note)?;
                note_body_bytes = Some(note.summary.len());
                verification = note.verification.take();
                gate = note.gate.take();
                let label = if crate::domain::status_note_role(&note.actor).is_some() {
                    "status".into()
                } else {
                    serde_json::to_value(note.kind)?
                        .as_str()
                        .unwrap_or("note")
                        .to_owned()
                };
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
                    super::history_display::HistoryDisplay::load(store, &event, &position)?
                        .summary(false),
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
                member,
            } => {
                // The window shows a compact summary but keeps the original
                // size, and says when it shortened it so the row offers its
                // detail; the detail read keeps the whole text.
                note_body_bytes = Some(summary.len());
                let shown = if detail {
                    complete_member = Some(member);
                    summary
                } else {
                    let compact = compact_text(&summary);
                    summary_truncated = compact != summary;
                    compact
                };
                (kind, shown, Vec::new(), actor, recorded_at)
            }
        };
    Ok(WorkRecordRow {
        gate,
        family: entry.record_family,
        locator: entry.locator.clone(),
        body_bytes: note_body_bytes.unwrap_or(summary.len()),
        summary,
        kind: label,
        refs,
        actor,
        recorded_at,
        body_omitted: false,
        summary_truncated,
        verification,
        member: complete_member,
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
