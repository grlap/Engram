use super::{
    Connection, DateTime, SqliteStore, StoreError, Utc, Value, WorkCatalogPage, WorkCatalogQuery,
    WorkClaim, WorkClaimState, load_work_claim_optional, normalize_work_catalog_key, params,
    push_catalog_parameter, work_catalog_page_on, work_catalog_sql,
};
use crate::domain::{FeedId, ProjectId, WorkCatalogReadCut};

impl SqliteStore {
    /// Shared transient cut for explicit listing and record-window readers.
    /// Call inside the same snapshot as the corresponding projection.
    pub(crate) fn work_read_cut(
        &self,
        project: &ProjectId,
        now: DateTime<Utc>,
    ) -> Result<WorkCatalogReadCut, StoreError> {
        catalog_cut(self, project, now)
    }
    /// Canonicalize filter identity with exactly the catalog's matching rules.
    pub(crate) fn normalize_catalog_filters(query: &mut WorkCatalogQuery) {
        for field in [&mut query.search, &mut query.label, &mut query.assigned_to] {
            *field = field
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(normalize_work_catalog_key);
        }
        query.after = None;
        query.limit = 0;
    }

    /// Count, prefix boundary, page, holders and continuation basis share one
    /// snapshot. Ambient catalogs never enter this counted reader.
    #[allow(
        clippy::type_complexity,
        reason = "internal count/page reader returns its shared snapshot basis"
    )]
    pub(crate) fn query_work_catalog_continuation(
        &self,
        project: &ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
        expected: Option<&WorkCatalogReadCut>,
    ) -> Result<
        (
            WorkCatalogPage,
            usize,
            usize,
            Vec<WorkClaim>,
            WorkCatalogReadCut,
        ),
        StoreError,
    > {
        self.work_read_snapshot(|store| {
            let cut = catalog_cut(store, project, now)?;
            if let Some(expected) = expected
                && (cut.project_position != expected.project_position
                    || now < expected.observed_at
                    || expected
                        .valid_until_ms
                        .is_some_and(|until| now.timestamp_millis() >= until))
            {
                return Err(cursor_invalid("catalog changed; start a fresh listing"));
            }
            let (total, preceding, anchor) =
                catalog_counts(&store.connection, project, now, query)?;
            if query.after.is_some() && !anchor {
                return Err(cursor_invalid(
                    "continuation item no longer matches this listing",
                ));
            }
            #[cfg(test)]
            super::tests::after_catalog_count();
            let page = work_catalog_page_on(&store.connection, project, now, query)?;
            let mut claims = Vec::new();
            for item in &page.items {
                if let Some(run_id) = item.work.active_run_id
                    && let Some(claim) = load_work_claim_optional(&store.connection, run_id)?
                    && claim.state == WorkClaimState::Active
                    && claim.expires_at > now
                {
                    claims.push(claim);
                }
            }
            Ok((page, total, preceding, claims, cut))
        })
    }
}

fn catalog_cut(
    store: &SqliteStore,
    project: &ProjectId,
    now: DateTime<Utc>,
) -> Result<WorkCatalogReadCut, StoreError> {
    let project_position = store.work_feed_head(&FeedId::Project(project.clone()))?;
    // Read-only time transitions can alter matching, holder words or detach
    // advice even when no new project event is appended. Projection columns
    // truncate to milliseconds while canonical holders retain finer precision.
    // Include the current millisecond conservatively: a cut observed during a
    // boundary millisecond cannot be continued; a fresh later listing can.
    let valid_until_ms = store.connection.query_row(
        "SELECT MIN(wake) FROM (
             SELECT deferred_until_ms AS wake FROM work_items
             WHERE project_id = ?1 AND lifecycle = 'open' AND deferred_until_ms >= ?2
             UNION ALL
             SELECT claim.expires_at_ms FROM work_claims claim
             JOIN work_items item ON item.active_run_id = claim.run_id
             WHERE item.project_id = ?1 AND claim.state = 'active' AND claim.expires_at_ms >= ?2
             UNION ALL
             SELECT offer.expires_at_ms FROM work_handoff_offers offer
             JOIN work_items item ON item.active_run_id = offer.run_id
             WHERE item.project_id = ?1 AND offer.state = 'offered' AND offer.expires_at_ms >= ?2
         )",
        params![project.0, now.timestamp_millis()],
        |row| row.get(0),
    )?;
    Ok(WorkCatalogReadCut {
        project_position,
        observed_at: now,
        valid_until_ms,
    })
}

fn catalog_counts(
    connection: &Connection,
    project: &ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
) -> Result<(usize, usize, bool), StoreError> {
    let (mut sql, mut parameters) = work_catalog_sql(project, now, query, false)?;
    let boundary = push_catalog_parameter(
        &mut parameters,
        query
            .after
            .map_or(Value::Null, |id| Value::Text(id.0.to_string())),
    );
    sql = sql.replace("SELECT COUNT(*) FROM classified", &format!(
        "SELECT COUNT(*), COALESCE(SUM(work_id <= {boundary}), 0), COALESCE(MAX(work_id = {boundary}), 0) FROM classified"));
    #[cfg(test)]
    crate::storage::work::WORK_CATALOG_COUNT_QUERIES.with(|count| count.set(count.get() + 1));
    let (total, preceding, anchor): (i64, i64, bool) =
        connection.query_row(&sql, rusqlite::params_from_iter(parameters.iter()), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let checked = |value| {
        usize::try_from(value).map_err(|_| {
            StoreError::InvalidWorkProjection("catalog count is outside the supported range".into())
        })
    };
    Ok((checked(total)?, checked(preceding)?, anchor))
}

fn cursor_invalid(reason: &str) -> StoreError {
    StoreError::WorkCatalogCursorInvalid {
        reason: reason.into(),
    }
}
