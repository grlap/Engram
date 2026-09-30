//! The explicit evaluation-history window of `show` and the complete detail
//! of one record. Ordinary `show` and `show --full` keep reading only the
//! newest record; completion is untouched.

use super::focus::WorkAuthoredVerdict;
use super::*;
use crate::domain::WorkCatalogReadCut;
use crate::storage::AssessedAcceptanceEvaluation;

/// Records decoded for one window page. Each record may be large, so a page
/// decodes at most this many, newest first, and verbs fit the rest.
const MAX_WINDOW_RECORDS: usize = 16;

/// Verdict words shown per row; the rest are counted exactly and read
/// through the record's complete detail.
pub(crate) const MAX_ROW_VERDICTS: usize = 16;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvaluationCursor {
    project: ProjectId,
    work: WorkId,
    run: WorkRunId,
    cut: WorkCatalogReadCut,
    total: usize,
    /// Stale reasons are judged under the policy, which a policy change moves
    /// without moving the project feed.
    policy: crate::domain::AcceptanceEvaluationPolicy,
    position: i64,
    evaluation: ObjectId,
}

/// Newest-first rows of one run's evaluation records. Total and newer count
/// the whole run feed; only the emitted oldest row advances the cursor.
pub(crate) struct WorkEvaluationWindow {
    pub short_ref: String,
    /// The title as `show` compacts it; `title_truncated` says when the stored
    /// title is longer, and `title_bytes` how long it is.
    pub title: String,
    pub title_truncated: bool,
    pub title_bytes: usize,
    /// False when the history run has ended, so its records are not judged.
    pub judged: bool,
    pub rows: Vec<WorkEvaluationRow>,
    pub total: usize,
    pub newer: usize,
    project: ProjectId,
    work: WorkId,
    run: Option<WorkRunId>,
    cut: WorkCatalogReadCut,
    policy: crate::domain::AcceptanceEvaluationPolicy,
}

/// One evaluation record as the window lists it.
pub(crate) struct WorkEvaluationRow {
    pub evaluation: String,
    pub position: i64,
    pub mode: &'static str,
    /// The evaluator's session as the display label `show` uses for
    /// sessions; `None` when the record names no session.
    pub evaluator_session: Option<String>,
    pub attempt_key: String,
    pub created_at: DateTime<Utc>,
    pub work_revision: i64,
    /// Verdict words by criterion position, at most [`MAX_ROW_VERDICTS`].
    pub verdicts: Vec<(usize, &'static str)>,
    pub verdicts_total: usize,
    pub stale: Option<&'static str>,
    /// The source observation that decided the move the record reads stale
    /// for, when one did.
    pub stale_observation: Option<super::ShownDecidingObservation>,
    /// Whether the stale reason was judged: false once the run has ended.
    pub judged: bool,
    pub supersedes: Option<String>,
    pub newest: bool,
}

/// One record complete: every verdict with its criterion, rationale and
/// citations.
pub(crate) struct WorkEvaluationDetail {
    pub short_ref: String,
    pub row: WorkEvaluationRow,
    pub evaluated_cut: i64,
    pub verdicts: Vec<WorkAuthoredVerdict>,
    /// `provider/model[@version]`, when the evaluator named its model.
    pub evaluator_model: Option<String>,
    /// A sub-agent evaluator's asserted execution identity.
    pub execution_identity: Option<String>,
    /// A sub-agent evaluator's attested parent session, as its display label.
    pub parent_session: Option<String>,
    /// The judged source fingerprint the record declares.
    pub source_fingerprint: Option<String>,
}

impl WorkEvaluationWindow {
    /// The read cut this window was selected at.
    pub(crate) fn read_cut(&self) -> &WorkCatalogReadCut {
        &self.cut
    }

    pub(crate) fn continuation(&self, visible: usize) -> Result<Option<String>, StoreError> {
        let Some(run) = self.run else {
            return Ok(None);
        };
        if visible == 0 || self.newer + visible == self.total {
            return Ok(None);
        }
        let row = self
            .rows
            .get(visible - 1)
            .ok_or_else(|| invalid("show continuation past the selected rows"))?;
        let evaluation = ObjectId::from_stored(row.evaluation.clone())
            .ok_or_else(|| invalid("show continuation names an invalid record"))?;
        super::continuation::encode(
            "e1-",
            &EvaluationCursor {
                project: self.project.clone(),
                work: self.work,
                run,
                cut: self.cut.clone(),
                total: self.total,
                policy: self.policy.clone(),
                position: row.position,
                evaluation,
            },
        )
        .map(Some)
        .ok_or_else(|| invalid("show continuation metadata exceeds its budget"))
    }
}

impl LocalWorkService {
    /// The evaluation records of the item's active run, or of its latest run
    /// when none is active, newest first from the cursor's boundary. One read
    /// snapshot; a cursor from another cut, run or window is refused.
    pub(crate) fn work_evaluation_window(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkEvaluationWindow, StoreError> {
        let cursor = after
            .map(|token| {
                super::continuation::decode::<EvaluationCursor>("e1-", token)
                    .ok_or_else(|| invalid("invalid show cursor; start a fresh window"))
            })
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.project != self.project_id)
        {
            return Err(invalid(
                "continuation belongs to another item, project or window kind",
            ));
        }
        let identity = self.display_identity();
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
            let policy = store.acceptance_evaluation_policy()?;
            let (run, active, entries) = match store.acceptance_evaluation_entries(item.work_id)? {
                Some(history) => (Some(history.run_id), history.active, history.entries),
                None => (None, true, Vec::new()),
            };
            let end = if let Some(cursor) = &cursor {
                if run != Some(cursor.run) {
                    return Err(invalid(
                        "the item's run changed; start a fresh evaluations window",
                    ));
                }
                if cursor.policy != policy {
                    return Err(invalid(
                        "the acceptance policy changed; start a fresh evaluations window",
                    ));
                }
                if cut.project_position != cursor.cut.project_position
                    || now < cursor.cut.observed_at
                    || cursor
                        .cut
                        .valid_until_ms
                        .is_some_and(|until| now.timestamp_millis() >= until)
                    || cursor.total != entries.len()
                {
                    return Err(invalid(
                        "show read cut changed or expired; start a fresh window",
                    ));
                }
                entries
                    .iter()
                    .position(|entry| {
                        entry.position == cursor.position && entry.evaluation == cursor.evaluation
                    })
                    .ok_or_else(|| invalid("continuation boundary no longer matches this window"))?
            } else {
                entries.len()
            };
            let rows = match (run, entries.last()) {
                (Some(run), Some(newest)) => {
                    let selected = entries[..end]
                        .iter()
                        .rev()
                        .take(MAX_WINDOW_RECORDS)
                        .cloned()
                        .collect::<Vec<_>>();
                    store
                        .assess_acceptance_evaluations(
                            item.work_id,
                            run,
                            &selected,
                            &newest.evaluation,
                        )?
                        .into_iter()
                        .map(|assessed| window_row(&assessed, &identity))
                        .collect()
                }
                _ => Vec::new(),
            };
            let title = compact_text(&item.title);
            Ok(WorkEvaluationWindow {
                short_ref: item.short_ref,
                title_truncated: title != item.title,
                title_bytes: item.title.len(),
                title,
                judged: active,
                rows,
                total: entries.len(),
                newer: entries.len() - end,
                project: self.project_id.clone(),
                work: item.work_id,
                run,
                cut,
                policy,
            })
        })
    }

    /// One evaluation record of the item complete, by its full record id.
    pub(crate) fn work_evaluation_detail(
        &self,
        work_ref: &str,
        record: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkEvaluationDetail, StoreError> {
        let unknown = || {
            StoreError::InvalidWork(
                "no evaluation of this item has that record id; list them with --evaluations"
                    .into(),
            )
        };
        let evaluation = ObjectId::from_stored(record.to_owned()).ok_or_else(unknown)?;
        let identity = self.display_identity();
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            let assessed = store
                .acceptance_evaluation_of_item(item.work_id, &evaluation)?
                .ok_or_else(unknown)?;
            let verdicts = assessed
                .record
                .verdicts
                .iter()
                .enumerate()
                .map(|(index, verdict)| WorkAuthoredVerdict {
                    position: index + 1,
                    criterion: verdict.criterion.clone(),
                    verdict: verdict.verdict.word(),
                    basis: verdict.basis.word(),
                    rationale: verdict.rationale.clone(),
                    citations: verdict
                        .evidence
                        .iter()
                        .map(|hash| hash.as_str().to_owned())
                        .collect(),
                })
                .collect();
            let record = &assessed.record;
            Ok(WorkEvaluationDetail {
                short_ref: item.short_ref,
                evaluated_cut: record.evaluated_cut.position,
                evaluator_model: record.evaluator_model.as_ref().map(|model| {
                    let mut named = format!("{}/{}", model.provider, model.model);
                    if let Some(version) = &model.version {
                        named.push('@');
                        named.push_str(version);
                    }
                    named
                }),
                execution_identity: record.execution_identity.clone(),
                parent_session: record
                    .parent_session
                    .as_ref()
                    .map(|session| identity.session(session)),
                source_fingerprint: record
                    .source_basis
                    .as_ref()
                    .map(|basis| basis.fingerprint.clone()),
                row: window_row(&assessed, &identity),
                verdicts,
            })
        })
    }
}

fn window_row(
    assessed: &AssessedAcceptanceEvaluation,
    identity: &super::identity::DisplayIdentity<'_>,
) -> WorkEvaluationRow {
    let record = &assessed.record;
    WorkEvaluationRow {
        evaluation: assessed.evaluation.as_str().to_owned(),
        position: assessed.position,
        mode: record.mode.word(),
        evaluator_session: record
            .evaluator
            .session_id
            .as_ref()
            .map(|session| identity.session(session)),
        attempt_key: record.attempt_key.clone(),
        created_at: record.created_at,
        work_revision: record.work_revision,
        verdicts: record
            .verdicts
            .iter()
            .enumerate()
            .take(MAX_ROW_VERDICTS)
            .map(|(index, verdict)| (index + 1, verdict.verdict.word()))
            .collect(),
        verdicts_total: record.verdicts.len(),
        stale: assessed.stale.map(crate::AcceptanceStaleReason::word),
        stale_observation: assessed
            .stale_observation
            .as_ref()
            .map(|observation| super::ShownDecidingObservation::new(observation, identity)),
        judged: assessed.judged,
        supersedes: record.supersedes.as_ref().map(|id| id.as_str().to_owned()),
        newest: assessed.newest,
    }
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkShowCursorInvalid {
        reason: reason.into(),
    }
}
