//! Transient diagnostics and historical navigation; never staged as durable
//! delivery bytes and never fed back into completion or accounting.

use super::*;

pub(super) type ViewCache = std::collections::HashMap<ObjectId, SealedSourceView>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SealedSourceCursor {
    project: ProjectId,
    work: WorkId,
    run: WorkRunId,
    seal: ObjectId,
    horizon: i64,
    newer: usize,
    boundary: Option<(i64, ObjectId)>,
}

pub(super) fn view(
    store: &SqliteStore,
    seal: &ObjectId,
    project: &ProjectId,
    expected: Option<(WorkId, WorkRunId)>,
) -> SealedSourceView {
    match store.sealed_source_page(seal, None, None, 0) {
        Ok(page) => {
            let diagnostic = page.diagnostic;
            if &diagnostic.project != project
                || expected
                    .is_some_and(|(work, run)| work != diagnostic.work || run != diagnostic.run)
            {
                return SealedSourceView {
                    seal: seal.clone(),
                    diagnostic: None,
                    unavailable: Some("seal_binding_mismatch"),
                    continuation: None,
                    details_omitted: false,
                };
            }
            let continuation = super::continuation::encode(
                "sd1-",
                &SealedSourceCursor {
                    project: project.clone(),
                    work: diagnostic.work,
                    run: diagnostic.run,
                    seal: diagnostic.seal.clone(),
                    horizon: diagnostic.horizon,
                    newer: 0,
                    boundary: None,
                },
            );
            if continuation.is_none() {
                return SealedSourceView {
                    seal: seal.clone(),
                    diagnostic: None,
                    unavailable: Some("historical_navigation_exceeds_budget"),
                    continuation: None,
                    details_omitted: false,
                };
            }
            SealedSourceView {
                seal: seal.clone(),
                diagnostic: Some(diagnostic),
                unavailable: None,
                continuation,
                details_omitted: false,
            }
        }
        Err(error) => SealedSourceView {
            seal: seal.clone(),
            diagnostic: None,
            unavailable: Some(super::advisory_error_class(&error)),
            continuation: None,
            details_omitted: false,
        },
    }
}

pub(super) fn hydrate(
    store: &SqliteStore,
    entry: &WorkFeedEntry,
    project: &ProjectId,
    cache: &mut ViewCache,
) -> Result<Option<SealedSourceView>, StoreError> {
    if !matches!(
        entry.object_kind.as_str(),
        "execution_observation" | "unadmitted_execution_observation"
    ) {
        return Ok(None);
    }
    Ok(store
        .source_observation_seal(&entry.object_id)?
        .map(|seal| {
            cache
                .entry(seal.clone())
                .or_insert_with(|| view(store, &seal, project, None))
                .clone()
        }))
}

pub(super) fn hydrate_feed(
    store: &SqliteStore,
    entry: &WorkFeedEntry,
    feed: &FeedId,
    cache: &mut ViewCache,
) -> Result<Option<SealedSourceView>, StoreError> {
    match feed {
        FeedId::Project(project) => hydrate(store, entry, project, cache),
        _ => Err(StoreError::InvalidWorkProjection(
            "delivery is not a project feed".into(),
        )),
    }
}

impl SealedSourceCursor {
    pub(super) fn continuation(
        &self,
        rows: &[super::WorkObservationRow],
        visible: usize,
        total: usize,
    ) -> Result<Option<String>, StoreError> {
        if visible == 0 || self.newer + visible >= total {
            return Ok(None);
        }
        let row = rows
            .get(visible - 1)
            .ok_or_else(|| invalid("invalid sealed window boundary"))?;
        let observation = ObjectId::from_stored(row.observation.clone())
            .ok_or_else(|| invalid("invalid observation locator"))?;
        let mut next = self.clone();
        next.boundary = Some((row.position, observation));
        next.newer += visible;
        super::continuation::encode("sd1-", &next)
            .map(Some)
            .ok_or_else(|| invalid("sealed source continuation exceeds its budget"))
    }
}

impl LocalWorkService {
    pub(super) fn sealed_source_window(
        &self,
        work_ref: &str,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<super::WorkObservationWindow, StoreError> {
        let cursor = super::continuation::decode::<SealedSourceCursor>("sd1-", token)
            .ok_or_else(|| invalid("invalid sealed source continuation"))?;
        let store = self.read_store_at(now)?;
        let identity = self.display_identity();
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            if cursor.project != self.project_id || cursor.work != item.work_id {
                return Err(invalid(
                    "sealed source continuation belongs to another item or project",
                ));
            }
            let page = store.sealed_source_page(
                &cursor.seal,
                Some(cursor.horizon),
                cursor.boundary.as_ref().map(|b| b.0),
                32,
            )?;
            if page.diagnostic.project != cursor.project
                || page.diagnostic.work != cursor.work
                || page.diagnostic.run != cursor.run
                || cursor.newer != page.newer
            {
                return Err(invalid(
                    "sealed source continuation differs from its original seal",
                ));
            }
            if let Some((position, observation)) = &cursor.boundary {
                let boundary = store.sealed_source_page(
                    &cursor.seal,
                    Some(cursor.horizon),
                    position.checked_add(1),
                    1,
                )?;
                if !boundary
                    .rows
                    .first()
                    .is_some_and(|row| row.position == *position && row.observation == *observation)
                {
                    return Err(invalid(
                        "sealed source continuation boundary does not match",
                    ));
                }
            }
            let entries = page
                .rows
                .iter()
                .map(|row| (row.position, row.observation.clone()))
                .collect::<Vec<_>>();
            let rows = store
                .source_observations(&entries)?
                .into_iter()
                .zip(&page.rows)
                .map(|((position, id, record), fact)| {
                    let mut row = super::observation_windows::observation_row(
                        &identity, position, &id, record,
                    );
                    row.sealed_comparison = Some(fact.comparison.clone());
                    row
                })
                .collect::<Vec<_>>();
            let title = compact_text(&item.title);
            Ok(super::WorkObservationWindow {
                short_ref: item.short_ref,
                title_truncated: title != item.title,
                title_bytes: item.title.len(),
                title,
                rows,
                total: page.diagnostic.total,
                newer: cursor.newer,
                project: self.project_id.clone(),
                work: item.work_id,
                run: Some(cursor.run),
                cut: store.work_read_cut(&self.project_id, now)?,
                sealed: Some((cursor, page.diagnostic)),
            })
        })
    }
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkShowCursorInvalid {
        reason: reason.into(),
    }
}
