//! Dense work delivery pages and retained attribution checks.

use super::{
    FeedId, ProjectId, SCHEMA_VERSION, SessionId, SqliteStore, StagedWorkChangePage, StoreError,
    WorkChange, WorkChangeProjection, WorkFeedEntry, WorkId, agent_change_object, change_context,
    history_display, sealed_source, views,
};

pub(super) fn work_delivery_boundary(
    store: &SqliteStore,
    project_id: &ProjectId,
    _session_id: &SessionId,
    focused_work_id: Option<WorkId>,
) -> Result<Option<WorkId>, StoreError> {
    let focused_root_id = focused_work_id
        .map(|work_id| {
            let work = store.get_work_item(work_id)?;
            if work.project_id != *project_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "focused work {} does not belong to project {:?}",
                    work.work_id.0, project_id.0
                )));
            }
            Ok(work.root_id)
        })
        .transpose()?;
    Ok(focused_root_id)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the receiving session and visibility boundary bind the exact budgeted delivery page"
)]
pub(super) fn verified_bounded_work_changes(
    store: &SqliteStore,
    project_id: &ProjectId,
    session_id: &SessionId,
    focused_root_id: Option<WorkId>,
    entries: Vec<WorkFeedEntry>,
    confirmed_through: i64,
    budget: usize,
) -> Result<Vec<WorkChange>, StoreError> {
    let has_entries = !entries.is_empty();
    let mut changes = Vec::new();
    let mut sealed_sources = sealed_source::ViewCache::new();
    for (offset, entry) in entries.into_iter().enumerate() {
        let expected = confirmed_through
            + i64::try_from(offset).map_err(|_| {
                StoreError::InvalidWorkProjection("work delivery offset overflowed".into())
            })?
            + 1;
        if entry.position.position != expected {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work delivery expected position {expected} but found {}",
                entry.position.position
            )));
        }
        let object = store
            .get::<serde_json::Value>(&entry.object_id)?
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "project-feed object {} is missing",
                    entry.object_id
                ))
            })?;
        let from_current_session = source_is_from_session(&entry.object_kind, &object, session_id);
        let display_producer = source_display_producer(&entry.object_kind, &object);
        let (capture, completion_checkpoint) = change_context::hydrate(store, &entry, &object)?;
        let history_display = history_display::hydrate(store, &entry, &object)?;
        let sealed_source = sealed_source::hydrate(store, &entry, project_id, &mut sealed_sources)?;
        // A work event's display and its delivered summary share one read of
        // its facts; every other kind is projected as before.
        let delivery = match &history_display {
            Some(display) => WorkChangeProjection::Visible(display.stored_summary()),
            None => agent_change_object(
                store,
                project_id,
                focused_root_id,
                &entry.object_kind,
                object,
                Some(&entry.position),
            )?,
        };
        changes.push(WorkChange {
            sealed_source,
            history_display,
            capture,
            completion_checkpoint,
            display_producer,
            from_current_session: matches!(&delivery, WorkChangeProjection::Visible(_))
                && from_current_session,
            delivery,
            entry,
        });
        if serde_json::to_vec(&changes)?.len() > budget {
            changes.pop();
            break;
        }
    }
    if has_entries && changes.is_empty() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "one compact work change exceeds its {budget}-byte section budget"
        )));
    }
    Ok(changes)
}

/// Validates one retained pending delivery page against current source state,
/// exactly as the next core retry reads it.
///
/// A whole-store transfer reads a page this way so that it either carries the
/// page or refuses it, rather than publishing a store whose next core retry
/// would fail on it.
///
/// # Errors
///
/// Returns [`StoreError`] when the page does not bind its exact dense source
/// interval or its attribution disagrees with the receiving session.
pub(crate) fn read_pending_delivery(
    store: &SqliteStore,
    session: &SessionId,
    project: &ProjectId,
    confirmed: i64,
    through: i64,
    payload: &[u8],
) -> Result<(), StoreError> {
    decode_staged_work_change_page(store, session, project, confirmed, through, payload)?;
    Ok(())
}

pub(super) fn decode_staged_work_change_page(
    store: &SqliteStore,
    session: &SessionId,
    project: &ProjectId,
    confirmed: i64,
    through: i64,
    payload: &[u8],
) -> Result<StagedWorkChangePage, StoreError> {
    let mut page: StagedWorkChangePage = serde_json::from_slice(payload)?;
    verify_staged_work_change_page(
        store,
        session,
        &FeedId::Project(project.clone()),
        confirmed,
        through,
        &mut page,
    )?;
    Ok(page)
}

pub(super) fn verify_staged_work_change_page(
    store: &SqliteStore,
    session_id: &SessionId,
    feed: &FeedId,
    confirmed_through: i64,
    delivered_through: i64,
    page: &mut StagedWorkChangePage,
) -> Result<(), StoreError> {
    if page.schema_version != SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "staged work delivery schema {} is unsupported",
            page.schema_version
        )));
    }
    let entries = store.work_feed_between(feed, confirmed_through, delivered_through)?;
    if entries.len() != page.changes.len() {
        return Err(StoreError::InvalidWorkProjection(
            "staged work delivery payload does not bind its exact dense source interval".into(),
        ));
    }
    for (entry, change) in entries.iter().zip(&page.changes) {
        if entry.position != change.entry.position
            || entry.object_kind != change.entry.object_kind
            || entry.object_id != change.entry.object_id
        {
            return Err(StoreError::InvalidWorkProjection(
                "staged work delivery payload does not bind its exact dense source interval".into(),
            ));
        }
    }
    let mut sealed_sources = sealed_source::ViewCache::new();
    for (entry, change) in entries.into_iter().zip(&mut page.changes) {
        let object = store
            .get::<serde_json::Value>(&entry.object_id)?
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "staged project-feed object {} is missing",
                    entry.object_id
                ))
            })?;
        let expected = matches!(&change.delivery, WorkChangeProjection::Visible(_))
            && source_is_from_session(&entry.object_kind, &object, session_id);
        if change.from_current_session != expected {
            return Err(StoreError::InvalidWorkProjection(
                "staged work attribution differs from the receiving session".into(),
            ));
        }
        change.history_display = history_display::hydrate(store, &entry, &object)?;
        change.sealed_source =
            sealed_source::hydrate_feed(store, &entry, feed, &mut sealed_sources)?;
        change.display_producer = source_display_producer(&entry.object_kind, &object);
        (change.capture, change.completion_checkpoint) =
            change_context::hydrate(store, &entry, &object)?;
    }
    Ok(())
}

fn source_actor<'a>(kind: &str, object: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    // An unadmitted observation is attributed to the session that observed
    // it, never to the cause it may assert.
    if kind == "unadmitted_execution_observation" {
        return object.get("observer");
    }
    object.get("actor").or_else(|| {
        (kind == "work_source_proposal")
            .then(|| object.get("notice")?.get("actor"))
            .flatten()
    })
}

pub(crate) fn source_is_from_session(
    kind: &str,
    object: &serde_json::Value,
    session_id: &SessionId,
) -> bool {
    source_actor(kind, object)
        .and_then(|actor| actor.get("session_id"))
        .and_then(serde_json::Value::as_str)
        == Some(session_id.0.as_str())
}

pub(super) fn source_display_producer(
    kind: &str,
    object: &serde_json::Value,
) -> Option<views::DisplayProducer> {
    let actor = source_actor(kind, object)?;
    Some(views::DisplayProducer {
        actor_id: actor.get("actor_id")?.as_str()?.into(),
        // A source without a recorded kind is never labelled as the reader.
        actor_kind: actor
            .get("actor_kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .into(),
        session_id: actor
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(|session| SessionId(session.into())),
    })
}
