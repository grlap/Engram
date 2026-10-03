use super::{
    Connection, DateTime, SqliteStore, StoreError, Utc, WorkCatalogPage, WorkCatalogQuery,
    WorkClaim, WorkClaimState, current_work_priority, load_work_claim_optional,
    normalize_work_catalog_key, params, ready_listing_order, ready_seek_key, work_catalog_page_on,
    work_catalog_sql,
};
use crate::domain::{FeedId, ProjectId, WorkCatalogReadCut};
use sha2::{Digest, Sha256};

/// What a listing continuation is checked against.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ListingExpectation<'a> {
    /// An `ls` page: the time it was read and the fingerprint of the
    /// listing's complete selected sequence then.
    Membership {
        observed_at: DateTime<Utc>,
        fingerprint: &'a str,
    },
    /// Compact `next`'s ready navigation, which reads no complete sequence:
    /// its project cut, refused by any project write or the next time
    /// transition.
    ProjectCut(&'a WorkCatalogReadCut),
}

/// One ordered pass over a listing's complete selected sequence.
struct Membership {
    /// The content fingerprint of the sequence: every matching work id in
    /// listing order, with its priority under ready order.
    fingerprint: String,
    total: usize,
    /// Members at or before the continuation's anchor in listing order.
    preceding: usize,
    /// Whether the anchor is itself a member.
    anchor: bool,
}

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
        query.after_priority = None;
        query.limit = 0;
    }

    /// Count, prefix boundary, page, holders and continuation basis share one
    /// snapshot. Ambient catalogs never enter this counted reader. The
    /// classified catalog query runs twice: once for the ordered membership
    /// pass, which yields the fingerprint, the total, the preceding count and
    /// the anchor together, and once for the page.
    ///
    /// An `ls` continuation carries a membership fingerprint and is checked
    /// against the listing's complete selected sequence recomputed now: it is
    /// refused exactly when an item entered or left the filtered set or its
    /// order moved, including through a time transition, and never because
    /// of an unrelated write. Compact `next`'s ready navigation reads no
    /// complete sequence and carries its project cut instead: any project
    /// write or the next project-wide time transition refuses it. Neither
    /// check reads the project-wide expiry again; only minting a project cut
    /// does.
    #[allow(
        clippy::type_complexity,
        reason = "internal count/page reader returns its shared snapshot basis"
    )]
    pub(crate) fn query_work_catalog_continuation(
        &self,
        project: &ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
        expected: Option<ListingExpectation<'_>>,
    ) -> Result<(WorkCatalogPage, usize, usize, Vec<WorkClaim>, String), StoreError> {
        self.work_read_snapshot(|store| {
            let membership = catalog_membership(&store.connection, project, now, query)?;
            if let Some(expected) = expected {
                let changed = match expected {
                    ListingExpectation::Membership {
                        observed_at,
                        fingerprint,
                    } => now < observed_at || membership.fingerprint != fingerprint,
                    ListingExpectation::ProjectCut(cut) => {
                        now < cut.observed_at
                            || store.work_feed_head(&FeedId::Project(project.clone()))?
                                != cut.project_position
                            || cut
                                .valid_until_ms
                                .is_some_and(|until| now.timestamp_millis() >= until)
                    }
                };
                if changed {
                    return Err(cursor_invalid("catalog changed; start a fresh listing"));
                }
            }
            if let Some((encoded, after)) = ready_seek_key(query)?
                && current_work_priority(&store.connection, after)? != Some(encoded)
            {
                return Err(cursor_invalid(
                    "continuation item no longer matches this listing",
                ));
            }
            let Membership {
                fingerprint,
                total,
                preceding,
                anchor,
            } = membership;
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
            Ok((page, total, preceding, claims, fingerprint))
        })
    }
}

/// The listing's complete selected sequence at `now`, read in one ordered
/// pass: its content fingerprint over every matching work id in listing
/// order (with its priority under ready order, one `\n`-terminated line per
/// member), streamed into the hash rather than built as one string, and the
/// total, preceding count and anchor of `query.after`, as the old counting
/// query reported them. The fingerprint compares two readings of one
/// listing and is never stored.
fn catalog_membership(
    connection: &Connection,
    project: &ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
) -> Result<Membership, StoreError> {
    let (sql, parameters) = work_catalog_sql(project, now, query, false)?;
    let ready = ready_listing_order(query);
    let order = if ready {
        "priority, work_id"
    } else {
        "work_id"
    };
    let count = "SELECT COUNT(*) FROM classified";
    if !sql.contains(count) {
        return Err(StoreError::InvalidWorkProjection(
            "catalog query lost its count selection; the membership scan cannot be derived".into(),
        ));
    }
    let sql = format!(
        "{} ORDER BY {order}",
        sql.replace(count, "SELECT work_id, priority FROM classified")
    );
    #[cfg(test)]
    {
        crate::storage::work::WORK_CATALOG_COUNT_QUERIES.with(|count| count.set(count.get() + 1));
        crate::storage::work::WORK_CATALOG_CLASSIFIED_QUERIES
            .with(|count| count.set(count.get() + 1));
    }
    let after = query.after.map(|id| id.0.to_string());
    let after_priority = query.after_priority.map(i64::from);
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(parameters.iter()))?;
    let mut hasher = Sha256::new();
    let (mut total, mut preceding, mut anchor) = (0_usize, 0_usize, false);
    while let Some(row) = rows.next()? {
        let work: String = row.get(0)?;
        let priority: i64 = row.get(1)?;
        total += 1;
        if let Some(after) = after.as_deref() {
            // The old counting query's rule: by work id alone in ordinary
            // order, by priority then work id in ready order.
            let at_or_before = if ready {
                after_priority.is_some_and(|after_priority| {
                    priority < after_priority
                        || (priority == after_priority && work.as_str() <= after)
                })
            } else {
                work.as_str() <= after
            };
            preceding += usize::from(at_or_before);
            anchor |= work == after;
        }
        hasher.update(work.as_bytes());
        if ready {
            hasher.update(b":");
            hasher.update(priority.to_string().as_bytes());
        }
        hasher.update(b"\n");
    }
    Ok(Membership {
        fingerprint: format!("{:x}", hasher.finalize()),
        total,
        preceding,
        anchor,
    })
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
    #[cfg(test)]
    crate::storage::work::WORK_CATALOG_EXPIRY_QUERIES.with(|count| count.set(count.get() + 1));
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

fn cursor_invalid(reason: &str) -> StoreError {
    StoreError::WorkCatalogCursorInvalid {
        reason: reason.into(),
    }
}
