use super::{
    Connection, DateTime, ReadyWork, SqliteStore, StoreError, Utc, Value, WorkAvailability,
    WorkBlocker, WorkCatalogPage, WorkCatalogQuery, WorkClaim, WorkClaimState, WorkId,
    WorkPrerequisiteState, derive_projected_work_availability, encode_state,
    load_work_claim_optional, load_work_item_projection, normalize_work_catalog_key, params,
    parse_work_id, projected_prerequisite_state,
};

#[cfg(test)]
mod tests;

const PROJECTED_WORK_AVAILABILITY_SQL: &str = r"
    CASE
        WHEN candidate.lifecycle != 'open' THEN 'closed'
        WHEN (candidate.active_run_id IS NOT NULL AND NOT EXISTS (
            SELECT 1
            FROM work_runs run
            JOIN work_root_executions execution
              ON execution.root_execution_id = run.root_execution_id
            WHERE run.run_id = candidate.active_run_id
              AND run.work_id = candidate.work_id
              AND execution.project_id = candidate.project_id
              AND execution.root_id = candidate.root_id
              AND execution.state = 'active'
        )) OR (candidate.active_run_id IS NULL
               AND COALESCE(json_extract(candidate.item_json, '$.restored'), 0) != 1)
          OR EXISTS (
            WITH RECURSIVE ancestors(work_id, parent_id, lifecycle) AS (
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                WHERE parent.work_id = candidate.parent_id
                UNION ALL
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                JOIN ancestors ON parent.work_id = ancestors.parent_id
            )
            SELECT 1 FROM ancestors WHERE lifecycle != 'open'
        ) THEN 'blocked'
        WHEN candidate.deferred_until_ms IS NOT NULL
          AND candidate.deferred_until_ms > ?2 THEN 'deferred'
        WHEN EXISTS (
            SELECT 1 FROM work_blockers blocker
            WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
        ) OR EXISTS (
            SELECT 1
            FROM work_prerequisites edge
            LEFT JOIN work_items prerequisite
              ON prerequisite.work_id = edge.prerequisite_id
            LEFT JOIN work_items replacement
              ON replacement.work_id = prerequisite.superseded_by
            WHERE edge.work_id = candidate.work_id
              AND (
                  prerequisite.work_id IS NULL
                  OR NOT (
                      prerequisite.lifecycle = 'completed'
                      OR (
                          prerequisite.lifecycle = 'superseded'
                          AND replacement.lifecycle = 'completed'
                      )
                  )
              )
        ) THEN 'blocked'
        WHEN EXISTS (
            SELECT 1 FROM work_claims claim
            WHERE claim.run_id = candidate.active_run_id
              AND claim.state = 'active'
              AND claim.expires_at_ms > ?2
        ) THEN CASE WHEN EXISTS (
            SELECT 1 FROM work_runs run
            WHERE run.run_id = candidate.active_run_id
              AND run.last_checkpoint_hash IS NOT NULL
        ) THEN 'active' ELSE 'claimed' END
        ELSE 'ready'
    END
";
const PROJECTED_WORK_HAS_BLOCKER_SQL: &str = r"
    EXISTS (
        SELECT 1 FROM work_blockers blocker
        WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
    ) OR EXISTS (
        SELECT 1
        FROM work_prerequisites edge
        LEFT JOIN work_items prerequisite
          ON prerequisite.work_id = edge.prerequisite_id
        LEFT JOIN work_items replacement
          ON replacement.work_id = prerequisite.superseded_by
        WHERE edge.work_id = candidate.work_id
          AND (
              prerequisite.work_id IS NULL
              OR NOT (
                  prerequisite.lifecycle = 'completed'
                  OR (
                      prerequisite.lifecycle = 'superseded'
                      AND replacement.lifecycle = 'completed'
                  )
              )
          )
    )
";

impl SqliteStore {
    /// Returns ready work ordered by priority, unblocking value, age, and stable id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when readiness projections cannot be read or verified.
    pub fn ready_work(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ReadyWork>, StoreError> {
        let sql = format!(
            "WITH classified AS (
                 SELECT candidate.work_id, candidate.priority,
                        candidate.created_at_ms,
                        ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
                 FROM work_items candidate
                 WHERE candidate.project_id = ?1
                   AND candidate.lifecycle = 'open'
             )
             SELECT work_id FROM classified
             WHERE availability = 'ready'
             ORDER BY priority,
                      (SELECT COUNT(*)
                       FROM work_prerequisites dependency
                       JOIN work_items dependant
                         ON dependant.work_id = dependency.work_id
                       WHERE dependency.prerequisite_id = classified.work_id
                         AND dependant.lifecycle = 'open') DESC,
                      created_at_ms, work_id
             LIMIT ?3"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let ids = statement
            .query_map(
                params![
                    project_id.0.as_str(),
                    now.timestamp_millis(),
                    i64::from(limit.clamp(1, 1_000))
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| parse_work_id(&row?))
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| inspect_work_catalog_on(&self.connection, id, now))
            .filter_map(|result| match result {
                Ok(view) if view.availability == WorkAvailability::Ready => Some(Ok(view)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Queries every lifecycle/availability class without mutating focus.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical work projections cannot be read.
    pub fn query_work_catalog(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<WorkCatalogPage, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let page = work_catalog_page_on(&transaction, project_id, now, query)?;
        transaction.commit()?;
        Ok(page)
    }

    /// The counted agent list shares its count, page and displayed holders in
    /// one read snapshot. Ambient catalog callers never pay for this count.
    pub(crate) fn query_work_catalog_listing(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<(WorkCatalogPage, usize, Vec<WorkClaim>), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let (sql, parameters) = work_catalog_sql(project_id, now, query, false)?;
        #[cfg(test)]
        super::WORK_CATALOG_COUNT_QUERIES.with(|count| count.set(count.get() + 1));
        let total: i64 =
            transaction.query_row(&sql, rusqlite::params_from_iter(parameters.iter()), |row| {
                row.get(0)
            })?;
        let total = usize::try_from(total).map_err(|_| {
            StoreError::InvalidWorkProjection("catalog count is outside the supported range".into())
        })?;
        #[cfg(test)]
        tests::after_catalog_count();
        let page = work_catalog_page_on(&transaction, project_id, now, query)?;
        let mut claims = Vec::new();
        for item in &page.items {
            if let Some(run_id) = item.work.active_run_id
                && let Some(claim) = load_work_claim_optional(&transaction, run_id)?
                && claim.state == WorkClaimState::Active
                && claim.expires_at > now
            {
                claims.push(claim);
            }
        }
        transaction.commit()?;
        Ok((page, total, claims))
    }
}

fn push_catalog_parameter(parameters: &mut Vec<Value>, value: Value) -> String {
    parameters.push(value);
    format!("?{}", parameters.len())
}

pub(in crate::storage::work) fn catalog_literal_fts_query(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn work_catalog_page_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
) -> Result<WorkCatalogPage, StoreError> {
    let (sql, parameters) = work_catalog_sql(project_id, now, query, true)?;
    let mut statement = connection.prepare(&sql)?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| parse_work_id(&row?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let limit = query.limit.clamp(1, 1_000) as usize;
    let mut items = Vec::with_capacity(ids.len());
    for work_id in ids {
        items.push(inspect_work_catalog_on(connection, work_id, now)?);
    }
    let next_after = (items.len() > limit).then(|| items[limit - 1].work.work_id);
    items.truncate(limit);
    Ok(WorkCatalogPage { items, next_after })
}

fn work_catalog_sql(
    project_id: &crate::domain::ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
    page: bool,
) -> Result<(String, Vec<Value>), StoreError> {
    let mut parameters = vec![
        Value::Text(project_id.0.clone()),
        Value::Integer(now.timestamp_millis()),
    ];
    let mut candidate_filters = vec!["candidate.project_id = ?1".to_owned()];
    if !query.lifecycles.is_empty() {
        let placeholders = query
            .lifecycles
            .iter()
            .map(|lifecycle| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*lifecycle)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        candidate_filters.push(format!(
            "candidate.lifecycle IN ({})",
            placeholders.join(", ")
        ));
    }
    let mut ownership_filters = Vec::new();
    if let Some(assigned_to) = query
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(assigned_to)),
        );
        ownership_filters.push(format!("candidate.assigned_to_key = {parameter}"));
    }
    if let Some(session) = &query.held_by {
        let parameter = push_catalog_parameter(&mut parameters, Value::Text(session.0.clone()));
        ownership_filters.push(format!(
            "EXISTS (SELECT 1 FROM work_claims claim
                     WHERE claim.run_id = candidate.active_run_id
                       AND claim.holder_session_id = {parameter}
                       AND claim.state = 'active' AND claim.expires_at_ms > ?2)"
        ));
    }
    if !ownership_filters.is_empty() {
        candidate_filters.push(format!("({})", ownership_filters.join(" OR ")));
    }
    if let Some(label) = query
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(label)),
        );
        candidate_filters.push(format!(
            "EXISTS (
                 SELECT 1 FROM work_item_labels label
                 WHERE label.work_id = candidate.work_id
                   AND label.label_key = {parameter}
             )"
        ));
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(normalize_work_catalog_key)
        .filter(|value| !value.is_empty())
    {
        let search_characters = search.chars().count();
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(if search_characters >= 3 {
                catalog_literal_fts_query(&search)
            } else {
                search
            }),
        );
        if search_characters >= 3 {
            candidate_filters.push(format!(
                "candidate.work_id IN (
                     SELECT work_id FROM work_catalog_fts
                     WHERE work_catalog_fts MATCH {parameter}
                 )"
            ));
        } else {
            candidate_filters.push(format!("instr(candidate.search_text_key, {parameter}) > 0"));
        }
    }

    let mut classified_filters = Vec::new();
    if !query.availabilities.is_empty() {
        let placeholders = query
            .availabilities
            .iter()
            .map(|availability| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*availability)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        classified_filters.push(format!("availability IN ({})", placeholders.join(", ")));
    }
    if query.blocked_only {
        candidate_filters.push(format!("({PROJECTED_WORK_HAS_BLOCKER_SQL})"));
    }
    if page && let Some(after) = query.after {
        let parameter = push_catalog_parameter(&mut parameters, Value::Text(after.0.to_string()));
        candidate_filters.push(format!("candidate.work_id > {parameter}"));
    }
    let classified_where = if classified_filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", classified_filters.join(" AND "))
    };
    let selection = if page {
        let limit = i64::from(query.limit.clamp(1, 1_000)).saturating_add(1);
        let parameter = push_catalog_parameter(&mut parameters, Value::Integer(limit));
        format!("work_id FROM classified {classified_where} ORDER BY work_id LIMIT {parameter}")
    } else {
        format!("COUNT(*) FROM classified {classified_where}")
    };
    let sql = format!(
        "WITH classified AS (
             SELECT candidate.work_id,
                    ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
             FROM work_items candidate
             WHERE {}
         )
         SELECT {selection}",
        candidate_filters.join(" AND ")
    );
    Ok((sql, parameters))
}

fn inspect_work_catalog_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item_projection(connection, work_id)?;
    let blockers = load_active_blocker_catalog_rows(connection, work_id)?;
    let prerequisites = classified_prerequisite_catalog_rows(connection, work_id)?;
    derive_projected_work_availability(connection, work, blockers, prerequisites, now)
}

fn load_active_blocker_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?))
        })?
        .map(|row| {
            let (bytes, scalar_bound) = row?;
            let blocker: WorkBlocker = serde_json::from_slice(&bytes)?;
            if !scalar_bound || blocker.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "active blocker {} differs from its scalar binding",
                    blocker.blocker_id
                )));
            }
            Ok(blocker)
        })
        .collect()
}

fn classified_prerequisite_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1
         ORDER BY edge.prerequisite_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|row| {
            let (prerequisite_id, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&prerequisite_id)?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, state))
        })
        .collect()
}
