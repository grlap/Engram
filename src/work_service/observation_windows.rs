//! The explicit source-observation window of `show`: every execution
//! observation on the item's active run, or its latest run when none is
//! active, those on other workspaces included, newest first from the cursor's
//! boundary. A read that records nothing, on one read-only snapshot.

use super::*;
use crate::domain::WorkCatalogReadCut;

/// Observations decoded for one window page; verbs fit the rest.
const MAX_WINDOW_OBSERVATIONS: usize = 32;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservationCursor {
    project: ProjectId,
    work: WorkId,
    run: WorkRunId,
    cut: WorkCatalogReadCut,
    total: usize,
    position: i64,
    observation: ObjectId,
}

/// Newest-first rows of one run's source observations. Total and newer count
/// the whole run feed; only the emitted oldest row advances the cursor.
pub(crate) struct WorkObservationWindow {
    pub short_ref: String,
    /// The title as `show` compacts it; `title_truncated` says when the stored
    /// title is longer, and `title_bytes` how long it is.
    pub title: String,
    pub title_truncated: bool,
    pub title_bytes: usize,
    pub rows: Vec<WorkObservationRow>,
    pub total: usize,
    pub newer: usize,
    project: ProjectId,
    work: WorkId,
    run: Option<WorkRunId>,
    cut: WorkCatalogReadCut,
}

/// One source observation as the window lists it. `None` fields were not
/// recorded.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkObservationRow {
    pub observation: String,
    pub position: i64,
    /// `admitted` for a turn's own observation, `unadmitted` for one a host
    /// recorded without admission.
    pub admission: &'static str,
    /// Whether the host reported a source change. An unadmitted record that
    /// reports none says nothing about the source, so it carries `None`.
    pub source_changed: Option<bool>,
    pub workspace: Option<String>,
    pub revision: Option<String>,
    pub root_generation: Option<i64>,
    /// The reporting session as the display label `show` uses for sessions.
    pub reporting_session: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    /// What only an unadmitted observation carries.
    pub unadmitted: Option<UnadmittedObservationDetail>,
}

/// An unadmitted observation's own facts, worded so it never reads as a
/// turn, a pass or an established cause.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct UnadmittedObservationDetail {
    pub occurrence: String,
    pub observed_from: DateTime<Utc>,
    pub observed_through: DateTime<Utc>,
    pub cause: String,
    pub accounting: &'static str,
    pub checks: Vec<ObservedCheckRow>,
}

/// One check seen inside an unadmitted turn: always uncredited.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedCheckRow {
    pub host_check_id: String,
    pub kind: &'static str,
    pub result: &'static str,
    pub credit: &'static str,
    pub finished_at: Option<DateTime<Utc>>,
    /// The revision the host says it ran on; `None` when unknown.
    pub source_revision: Option<String>,
    /// The host's opaque reference, shortened for display only.
    pub evidence_ref: Option<String>,
}

fn observation_row(
    identity: &super::identity::DisplayIdentity,
    position: i64,
    hash: &ObjectId,
    record: crate::storage::SourceObservationRecord,
) -> WorkObservationRow {
    use super::unadmitted::{
        accounting_words, cause_words, check_kind_word, check_result_word, displayed,
        occurrence_words,
    };
    match record {
        crate::storage::SourceObservationRecord::Admitted(observation) => {
            let basis = observation.source_basis.as_ref();
            WorkObservationRow {
                observation: hash.as_str().to_owned(),
                position,
                admission: "admitted",
                source_changed: Some(observation.source_changed),
                workspace: basis.map(|basis| basis.workspace_id.clone()),
                revision: basis.map(|basis| basis.source_revision.clone()),
                root_generation: basis.and_then(|basis| basis.source_root_generation),
                reporting_session: identity.session(&observation.session_id),
                observed_at: observation.observed_at,
                recorded_at: observation.recorded_at,
                unadmitted: None,
            }
        }
        crate::storage::SourceObservationRecord::Unadmitted(observation) => {
            let change = observation.occurrence.source_change();
            let sighting = change.and_then(|change| change.sighting());
            let checks = observation
                .occurrence
                .checks()
                .into_iter()
                .map(|recorded| ObservedCheckRow {
                    host_check_id: displayed(&recorded.check.host_check_id),
                    kind: check_kind_word(recorded.check.check_kind),
                    result: check_result_word(recorded.check.observed_result),
                    credit: "uncredited",
                    finished_at: recorded.check.finished_at,
                    source_revision: recorded
                        .check
                        .source_basis
                        .as_ref()
                        .map(|basis| displayed(&basis.source_revision)),
                    evidence_ref: recorded.check.host_evidence_ref.as_deref().map(displayed),
                })
                .collect();
            WorkObservationRow {
                observation: hash.as_str().to_owned(),
                position,
                admission: "unadmitted",
                source_changed: change.is_some().then_some(true),
                workspace: change.map(|change| displayed(change.workspace_id())),
                revision: sighting
                    .map(|sighting| displayed(&sighting.source_basis.source_revision)),
                root_generation: sighting
                    .and_then(|sighting| sighting.source_basis.source_root_generation),
                reporting_session: identity.session(&observation.observing_session),
                observed_at: Some(observation.observed_interval.through),
                recorded_at: observation.recorded_at,
                unadmitted: Some(UnadmittedObservationDetail {
                    occurrence: occurrence_words(&observation.occurrence),
                    observed_from: observation.observed_interval.from,
                    observed_through: observation.observed_interval.through,
                    cause: cause_words(&observation.causality),
                    accounting: accounting_words(&observation.accounting),
                    checks,
                }),
            }
        }
    }
}

impl WorkObservationWindow {
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
        let observation = ObjectId::from_stored(row.observation.clone())
            .ok_or_else(|| invalid("show continuation names an invalid record"))?;
        super::continuation::encode(
            "o1-",
            &ObservationCursor {
                project: self.project.clone(),
                work: self.work,
                run,
                cut: self.cut.clone(),
                total: self.total,
                position: row.position,
                observation,
            },
        )
        .map(Some)
        .ok_or_else(|| invalid("show continuation metadata exceeds its budget"))
    }
}

impl LocalWorkService {
    /// The source observations of the item's active run, or of its latest
    /// run when none is active, newest first from the cursor's boundary. One
    /// read snapshot; a cursor from another cut, run or window is refused.
    pub(crate) fn work_observation_window(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkObservationWindow, StoreError> {
        let cursor = after
            .map(|token| {
                super::continuation::decode::<ObservationCursor>("o1-", token)
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
            let (run, entries) = match store.source_observation_entries(item.work_id)? {
                Some((run, entries)) => (Some(run), entries),
                None => (None, Vec::new()),
            };
            let end = if let Some(cursor) = &cursor {
                if run != Some(cursor.run) {
                    return Err(invalid(
                        "the item's run changed; start a fresh observations window",
                    ));
                }
                // The run's source records are only ever appended: its
                // total and the boundary record decide the remaining rows,
                // whatever was written elsewhere in the project.
                if now < cursor.cut.observed_at || cursor.total != entries.len() {
                    return Err(invalid("the window changed; start a fresh window"));
                }
                entries
                    .iter()
                    .position(|(position, observation)| {
                        *position == cursor.position && *observation == cursor.observation
                    })
                    .ok_or_else(|| invalid("continuation boundary no longer matches this window"))?
            } else {
                entries.len()
            };
            let selected = entries[..end]
                .iter()
                .rev()
                .take(MAX_WINDOW_OBSERVATIONS)
                .cloned()
                .collect::<Vec<_>>();
            let rows = store
                .source_observations(&selected)?
                .into_iter()
                .map(|(position, hash, record)| observation_row(&identity, position, &hash, record))
                .collect();
            let title = compact_text(&item.title);
            Ok(WorkObservationWindow {
                short_ref: item.short_ref,
                title_truncated: title != item.title,
                title_bytes: item.title.len(),
                title,
                rows,
                total: entries.len(),
                newer: entries.len() - end,
                project: self.project_id.clone(),
                work: item.work_id,
                run,
                cut,
            })
        })
    }
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkShowCursorInvalid {
        reason: reason.into(),
    }
}
