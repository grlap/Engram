//! Explicit show windows: select newest records, retain canonical membership
//! and feed ordering, and let verbs fit the final emitted representation.

use super::*;
use crate::domain::WorkCatalogReadCut;
use crate::storage::{
    WorkRecordAddress, WorkRecordContent, WorkRecordFamily, WorkRecordIndex, WorkRecordKind,
    WorkRecordOrder,
};
use std::collections::BTreeMap;

/// A continuation cursor given for a record that has no assessment to page.
pub(crate) const ASSESSMENT_CONTINUATION_REFUSAL: crate::argument_names::Twin =
    crate::argument_names::Twin {
        cli: "only a verification record's assessment continues; drop --after",
        mcp: "only a verification record's assessment continues; drop after",
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
            let mut view = self.focus_view_for_projection(
                store,
                item.work_id,
                false,
                true,
                super::service::FocusText::Full,
                now,
            )?;
            view.session_focus = Some(self.session_focus(store, now)?);
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
                super::continuation::decode::<AssessmentCursor>(ASSESSMENT_CURSOR_PREFIX, token)
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
            let view = cursor
                .as_ref()
                .map_or(AssessmentView::Summary, |cursor| cursor.view);
            let assessment = if row.verification.is_some() && row.address.member.is_none() {
                // The summary classifies every candidate; the history pages
                // through them in order.
                let (after, limit) = match view {
                    AssessmentView::Summary => (None, usize::MAX),
                    AssessmentView::History => (
                        cursor
                            .as_ref()
                            .and_then(|cursor| cursor.boundary)
                            .map(|boundary| crate::storage::AssessmentBoundary {
                                trigger_position: boundary.trigger_position,
                                obligation_id: boundary.obligation,
                            }),
                        MAX_ASSESSMENT_ROWS,
                    ),
                };
                store.verification_assessment(item.work_id, &row.address.hash, after, limit)?
            } else {
                None
            };
            let page = match (assessment, cursor) {
                // A cursor is issued only for a verification record and is
                // bound to it, so real input meets the record check above;
                // this refuses a cursor that names a note with no assessment.
                (None, Some(_)) => {
                    return Err(invalid(ASSESSMENT_CONTINUATION_REFUSAL.cli));
                }
                (None, None) => None,
                (Some(assessment), cursor) => Some(assessment_page(
                    &self.project_id,
                    item.work_id,
                    &row.address.hash,
                    view,
                    assessment,
                    cursor.as_ref(),
                )?),
            };
            Ok((item.short_ref.clone(), row, page))
        })
    }
}

/// Obligation assessments shown per page of a verification record's history.
pub(crate) const MAX_ASSESSMENT_ROWS: usize = 8;

/// The prefix of an assessment continuation: both views, one cursor type.
const ASSESSMENT_CURSOR_PREFIX: &str = "v2-";

/// Which view of a verification record's assessment a page shows: the
/// default summary, or the exhaustive history it names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssessmentView {
    /// Exact counts by status and reason, and every row a reader must act
    /// on: it matches or mismatches at the record's cut, or its obligation
    /// is still recorded open.
    Summary,
    /// Every candidate in trigger-position and id order, a page at a time.
    History,
}

/// The last obligation a page showed, in trigger-position and id order.
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssessmentCursorBoundary {
    trigger_position: i64,
    obligation: crate::domain::WorkObligationId,
}

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
    view: AssessmentView,
    /// The last row shown; none starts the view from its first row.
    boundary: Option<AssessmentCursorBoundary>,
}

/// One exact count of the summary: candidates with this status and reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssessmentCount {
    /// `matches`, `mismatch` or `left_out`.
    pub status: String,
    /// The mismatch or left-out reason; none for `matches`.
    pub reason: Option<String>,
    pub count: usize,
}

/// One page of a verification record's reconstructed assessment, in either
/// view, at one cut: exact counts, the rows from here on, and what a
/// continuation needs to resume the same view at the same cut.
pub(crate) struct VerificationAssessmentPage {
    pub view: AssessmentView,
    pub record_position: i64,
    pub cut_position: i64,
    /// Every candidate of the record's check kind on the run.
    pub total: usize,
    pub check_kind: crate::domain::VerificationKind,
    /// Summary only: exact counts over every candidate.
    pub counts: Vec<AssessmentCount>,
    /// Summary: every row that must be shown, on any page. History: `total`.
    pub view_total: usize,
    /// Rows of this view on earlier pages.
    pub earlier: usize,
    /// Rows of this view from here on. A summary carries all that remain, for
    /// the renderer to fit; a history page carries at most one page.
    pub rows: Vec<crate::storage::VerificationObligationAssessment>,
    cursor: AssessmentCursor,
    /// Where a bound record's check ran and how it was bound here.
    pub bound: Option<crate::domain::AcceptanceBoundVerification>,
}

impl VerificationAssessmentPage {
    /// Candidates not on this page, earlier pages included (history view).
    pub(crate) fn omitted(&self) -> usize {
        self.total - self.rows.len()
    }

    /// Whether rows of this view remain after the first `shown` rows here.
    pub(crate) fn more_after(&self, shown: usize) -> bool {
        self.earlier + shown < self.view_total
    }

    /// The continuation that resumes this view after `row`, at the same cut.
    pub(crate) fn continuation_after(
        &self,
        row: &crate::storage::VerificationObligationAssessment,
    ) -> Result<String, StoreError> {
        self.encode(
            self.view,
            Some(AssessmentCursorBoundary {
                trigger_position: row.trigger_position,
                obligation: row.obligation_id,
            }),
        )
    }

    /// The continuation that starts the exhaustive history at the same cut.
    pub(crate) fn history_start(&self) -> Result<String, StoreError> {
        self.encode(AssessmentView::History, None)
    }

    fn encode(
        &self,
        view: AssessmentView,
        boundary: Option<AssessmentCursorBoundary>,
    ) -> Result<String, StoreError> {
        super::continuation::encode(
            ASSESSMENT_CURSOR_PREFIX,
            &AssessmentCursor {
                view,
                boundary,
                ..self.cursor.clone()
            },
        )
        .ok_or_else(|| invalid("assessment continuation exceeds its budget"))
    }
}

/// The serde word of a unit enum variant.
fn serde_word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn assessment_page(
    project: &ProjectId,
    work: WorkId,
    record: &ObjectId,
    view: AssessmentView,
    assessment: crate::storage::VerificationAssessment,
    cursor: Option<&AssessmentCursor>,
) -> Result<VerificationAssessmentPage, StoreError> {
    use crate::control::ObligationAssessment;
    use crate::storage::RecordedObligationEnd;
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
    let base = AssessmentCursor {
        project: project.clone(),
        work,
        run: assessment.run_id,
        record: record.clone(),
        record_position: assessment.record_position,
        head: assessment.head_position,
        total,
        view,
        boundary: None,
    };
    let (counts, view_total, earlier, rows) = match view {
        AssessmentView::History => (Vec::new(), total, assessment.earlier, assessment.rows),
        AssessmentView::Summary => {
            let mut counts = BTreeMap::<(String, Option<String>), usize>::new();
            for row in &assessment.rows {
                let key = match row.assessment {
                    ObligationAssessment::Matches => ("matches".to_owned(), None),
                    ObligationAssessment::Mismatch(mismatch) => {
                        ("mismatch".to_owned(), Some(serde_word(&mismatch)))
                    }
                    ObligationAssessment::Skipped(skip) => {
                        ("left_out".to_owned(), Some(serde_word(&skip)))
                    }
                };
                *counts.entry(key).or_insert(0) += 1;
            }
            // A row a reader must act on is shown in full, in trigger-position
            // order: one that matches or mismatches, or whose obligation is
            // still recorded open. Every other row is left out before
            // matching and appears only in the counts.
            let must_show = assessment
                .rows
                .into_iter()
                .filter(|row| {
                    !matches!(row.assessment, ObligationAssessment::Skipped(_))
                        || row.recorded == RecordedObligationEnd::Open
                })
                .collect::<Vec<_>>();
            let earlier = match cursor.and_then(|cursor| cursor.boundary) {
                None => 0,
                Some(boundary) => {
                    must_show
                        .iter()
                        .position(|row| {
                            row.trigger_position == boundary.trigger_position
                                && row.obligation_id == boundary.obligation
                        })
                        .ok_or_else(|| {
                            invalid("continuation boundary no longer matches this record")
                        })?
                        + 1
                }
            };
            let view_total = must_show.len();
            let rows = must_show.into_iter().skip(earlier).collect();
            let counts = counts
                .into_iter()
                .map(|((status, reason), count)| AssessmentCount {
                    status,
                    reason,
                    count,
                })
                .collect();
            (counts, view_total, earlier, rows)
        }
    };
    Ok(VerificationAssessmentPage {
        view,
        record_position: assessment.record_position,
        cut_position: assessment.cut_position,
        total,
        check_kind: assessment.check_kind,
        counts,
        view_total,
        earlier,
        rows,
        cursor: base,
        bound: assessment.bound,
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
    let (label, summary, refs, actor, recorded_at) = match store
        .work_record_content(project, work, entry)?
    {
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
            // One read of the event's facts serves the change kind and the
            // displayed summary.
            let display = super::history_display::HistoryDisplay::load(store, &event, &position)?;
            (
                display.stored_summary().change_kind,
                display.summary(false),
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
