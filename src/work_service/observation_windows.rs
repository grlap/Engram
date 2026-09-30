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
    pub source_changed: bool,
    pub workspace: Option<String>,
    pub revision: Option<String>,
    pub root_generation: Option<i64>,
    /// The reporting session as the display label `show` uses for sessions.
    pub reporting_session: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
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
                .map(|(position, hash, observation)| {
                    let basis = observation.source_basis.as_ref();
                    WorkObservationRow {
                        observation: hash.as_str().to_owned(),
                        position,
                        source_changed: observation.source_changed,
                        workspace: basis.map(|basis| basis.workspace_id.clone()),
                        revision: basis.map(|basis| basis.source_revision.clone()),
                        root_generation: basis.and_then(|basis| basis.source_root_generation),
                        reporting_session: identity.session(&observation.session_id),
                        observed_at: observation.observed_at,
                        recorded_at: observation.recorded_at,
                    }
                })
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
