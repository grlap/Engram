//! Snapshot intake and immutable source-change notifications, never local sync.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::super::{SqliteStore, StoreError};
use super::feeds::{
    append_to_work_feeds, inspect_work_request, load_typed_work_object, replay_operation,
    validate_work_source_snapshot,
};
use super::planning::{create_root_on, normalize_strings, persist_operation_result};
use super::query::{latest_restored_record, load_work_item, parse_work_id};
use crate::domain::{
    ActorContext, ChildRequirement, CreateWorkRequest, ProjectId, SCHEMA_VERSION, WorkId,
    WorkImportEffect, WorkImportInput, WorkImportPreview, WorkImportReceipt, WorkItem,
    WorkItemKind, WorkObservationBasis, WorkOrigin, WorkSourceKey, WorkSourceLookup,
    WorkSourceNotice, WorkSourceProposal, WorkSourceSnapshot,
};
use crate::{CanonicalObject, ObjectHash, RestoredRecord, memory::Redactor};

#[cfg(test)]
mod tests;

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidWork(reason.into())
}

pub(super) fn key(snapshot: &WorkSourceSnapshot) -> WorkSourceKey {
    WorkSourceKey {
        adapter_kind: snapshot.adapter_kind.clone(),
        canonical_ref: snapshot.canonical_ref.clone(),
    }
}

pub(super) fn validate_key(source: &WorkSourceKey) -> Result<(), StoreError> {
    for value in [&source.adapter_kind, &source.canonical_ref] {
        if value.is_empty()
            || value.trim() != value
            || value.len() > 256
            || value
                .chars()
                .any(crate::domain::is_unsafe_rendered_text_char)
        {
            return Err(invalid(
                "source key fields must be normalized nonempty text, at most 256 UTF-8 bytes, without controls",
            ));
        }
    }
    Ok(())
}

fn validate_input(input: &WorkImportInput, now: DateTime<Utc>) -> Result<(), StoreError> {
    validate_work_source_snapshot(&input.snapshot, now)?;
    if let Some(draft) = &input.draft {
        if draft.acceptance.len() > 64 {
            return Err(invalid(
                "import draft admits at most 64 authored acceptance criteria",
            ));
        }
        for text in [&draft.title, &draft.outcome]
            .into_iter()
            .chain(&draft.acceptance)
        {
            if text.trim().is_empty()
                || text.trim() != text
                || text.len() > 8192
                || text.chars().any(|ch| {
                    crate::domain::is_unsafe_rendered_text_char(ch) && ch != '\n' && ch != '\t'
                })
            {
                return Err(invalid(
                    "import draft fields must be normalized nonblank prose of at most 8192 UTF-8 bytes",
                ));
            }
        }
    }
    Ok(())
}

fn source_item_on(
    connection: &Connection,
    project: &ProjectId,
    source: &WorkSourceKey,
) -> Result<Option<WorkItem>, StoreError> {
    validate_key(source)?;
    let mut statement = connection.prepare(
        "SELECT work.work_id, source.object_hash FROM work_items work
         JOIN objects source ON source.object_hash = work.source_snapshot_hash
         WHERE work.project_id = ?1 AND source.object_kind = 'work_source_snapshot'
           AND json_extract(source.canonical_json, '$.adapter_kind') = ?2
           AND json_extract(source.canonical_json, '$.canonical_ref') = ?3
         ORDER BY work.work_id LIMIT 2",
    )?;
    let rows = statement
        .query_map(
            params![project.0, source.adapter_kind, source.canonical_ref],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > 1 {
        return Err(invalid(
            "source key resolves to multiple local items; inspect the imported work before applying",
        ));
    }
    let Some((id, hash)) = rows.into_iter().next() else {
        return Ok(None);
    };
    let hash = ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
    let snapshot =
        load_typed_work_object::<WorkSourceSnapshot>(connection, &hash, "work_source_snapshot")?;
    let item = load_work_item(connection, parse_work_id(&id)?)?;
    if key(&snapshot) != *source
        || item.project_id != *project
        || item.origin != WorkOrigin::Imported
        || item.source_snapshot_id.as_ref() != Some(&hash)
    {
        return Err(StoreError::InvalidWorkProjection(
            "import source binding differs from canonical work".into(),
        ));
    }
    Ok(Some(item))
}

pub(in crate::storage) fn native_source_notices_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Vec<WorkSourceNotice>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object.object_hash FROM objects object
         JOIN work_feed_entries feed ON feed.object_hash = object.object_hash
         WHERE object.object_kind = 'work_source_proposal'
           AND json_extract(object.canonical_json, '$.work_id') = ?1
           AND feed.feed_kind = 'root_work' AND feed.feed_id = ?2
         ORDER BY feed.position",
    )?;
    let hashes = statement
        .query_map(
            params![item.work_id.0.to_string(), item.root_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    hashes
        .into_iter()
        .map(|hash| {
            let hash =
                ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
            let proposal = load_typed_work_object::<WorkSourceProposal>(
                connection,
                &hash,
                "work_source_proposal",
            )?;
            validate_proposal_on(connection, item, &proposal)?;
            Ok(proposal.notice)
        })
        .collect()
}

/// Full inherited closure audit. Ordinary selected-capture reads do not call it.
pub(in crate::storage) fn validate_restored_source_notices_on(
    connection: &Connection,
    item: &WorkItem,
    record: &RestoredRecord,
) -> Result<(), StoreError> {
    if record.project_id != item.project_id
        || record.work_id != item.work_id
        || record.item.work_id != item.work_id
        || record.item.root_id != item.root_id
        || record.item.source_snapshot_id != item.source_snapshot_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "restored source notice crosses its canonical item".into(),
        ));
    }
    for notice in &record.history.source_notices {
        validate_notice_on(connection, item, notice)?;
    }
    Ok(())
}

// Membership is a navigation probe, not an integrity audit. Select only a
// matching inherited container; omitted captures belong to doctor/export.
fn source_snapshot_known_on(
    connection: &Connection,
    item: &WorkItem,
    snapshot: &ObjectHash,
) -> Result<bool, StoreError> {
    if item.source_snapshot_id.as_ref() == Some(snapshot) {
        return Ok(true);
    }
    let native: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_work_source_proposal_work
         WHERE object_kind = 'work_source_proposal'
           AND json_extract(canonical_json, '$.work_id') = ?1
           AND json_extract(canonical_json, '$.notice.proposed_snapshot') = ?2)",
        params![item.work_id.0.to_string(), snapshot.as_str()],
        |row| row.get(0),
    )?;
    if native {
        return Ok(true);
    }
    let hash: Option<String> = connection
        .query_row(
            "SELECT record.record_hash FROM work_restored_records record
         JOIN objects object ON object.object_hash = record.record_hash
         WHERE record.work_id = ?1 AND EXISTS (
           SELECT 1 FROM json_each(object.canonical_json, '$.history.source_notices') notice
           WHERE json_extract(notice.value, '$.proposed_snapshot') = ?2)
         ORDER BY record.generation_index DESC LIMIT 1",
            params![item.work_id.0.to_string(), snapshot.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(hash) = hash else {
        return Ok(false);
    };
    let record = selected_restored_source_on(connection, item, hash)?;
    Ok(record
        .history
        .source_notices
        .iter()
        .any(|notice| &notice.proposed_snapshot == snapshot))
}

fn selected_restored_source_on(
    connection: &Connection,
    item: &WorkItem,
    hash: String,
) -> Result<RestoredRecord, StoreError> {
    let hash = ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
    let record =
        load_typed_work_object::<RestoredRecord>(connection, &hash, "work_restored_record")?;
    if record.work_id != item.work_id
        || record.project_id != item.project_id
        || record.item.work_id != item.work_id
        || record.item.root_id != item.root_id
        || record.item.source_snapshot_id != item.source_snapshot_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "restored source notice crosses its item".into(),
        ));
    }
    Ok(record)
}

// Reads and apply validate only the latest selected capture. SQL counts omitted
// native notices and inherited members without canonical body decoding. The
// selected inherited container is still verified in full as one canonical object;
// doctor/export alone follow every historical source closure.
fn source_notice_summary_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(usize, Option<WorkSourceNotice>), StoreError> {
    let inherited_count: i64 = connection.query_row(
        "SELECT COALESCE(SUM(json_array_length(object.canonical_json, '$.history.source_notices')), 0)
         FROM work_restored_records record JOIN objects object ON object.object_hash = record.record_hash
         WHERE record.work_id = ?1",
        [item.work_id.0.to_string()], |row| row.get(0),
    )?;
    let mut count = usize::try_from(inherited_count)
        .map_err(|_| invalid("source notice count exceeds the supported range"))?;
    let native_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM objects object
         JOIN work_feed_entries feed ON feed.object_hash = object.object_hash
         WHERE object.object_kind = 'work_source_proposal'
           AND json_extract(object.canonical_json, '$.work_id') = ?1
           AND feed.feed_kind = 'root_work' AND feed.feed_id = ?2",
        params![item.work_id.0.to_string(), item.root_id.0.to_string()],
        |row| row.get(0),
    )?;
    let native_count = usize::try_from(native_count)
        .map_err(|_| invalid("source notice count exceeds the supported range"))?;
    count = count
        .checked_add(native_count)
        .ok_or_else(|| invalid("source notice count exceeds the supported range"))?;
    Ok((count, latest_source_notice_on(connection, item)?))
}

fn latest_source_notice_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<WorkSourceNotice>, StoreError> {
    // Even an empty latest layer has required current-build history fields.
    // This is one selected record, never an open-time or full-history scan.
    latest_restored_record(connection, item.work_id)?;
    let mut latest = None;
    let hash: Option<String> = connection
        .query_row(
            "SELECT object.object_hash FROM objects object
         JOIN work_feed_entries feed ON feed.object_hash = object.object_hash
         WHERE object.object_kind = 'work_source_proposal'
           AND json_extract(object.canonical_json, '$.work_id') = ?1
           AND feed.feed_kind = 'root_work' AND feed.feed_id = ?2
         ORDER BY feed.position DESC LIMIT 1",
            params![item.work_id.0.to_string(), item.root_id.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(hash) = hash {
        let hash =
            ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
        let proposal = load_typed_work_object::<WorkSourceProposal>(
            connection,
            &hash,
            "work_source_proposal",
        )?;
        validate_proposal_on(connection, item, &proposal)?;
        latest = Some(proposal.notice);
    } else {
        let hash: Option<String> = connection
            .query_row(
                "SELECT record.record_hash FROM work_restored_records record
             JOIN objects object ON object.object_hash = record.record_hash
             WHERE record.work_id = ?1
               AND json_array_length(object.canonical_json, '$.history.source_notices') > 0
             ORDER BY record.generation_index DESC LIMIT 1",
                [item.work_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(hash) = hash {
            latest = selected_restored_source_on(connection, item, hash)?
                .history
                .source_notices
                .pop();
            if let Some(notice) = &latest {
                validate_notice_on(connection, item, notice)?;
            }
        }
    }
    Ok(latest)
}

fn validate_notice_on(
    connection: &Connection,
    item: &WorkItem,
    notice: &WorkSourceNotice,
) -> Result<(), StoreError> {
    let cited = load_typed_work_object::<WorkSourceSnapshot>(
        connection,
        &notice.cited_snapshot,
        "work_source_snapshot",
    )?;
    let proposed = load_typed_work_object::<WorkSourceSnapshot>(
        connection,
        &notice.proposed_snapshot,
        "work_source_snapshot",
    )?;
    if item.source_snapshot_id.as_ref() != Some(&notice.cited_snapshot)
        || key(&cited) != key(&proposed)
        || notice.work_revision < 1
        || notice.cited_snapshot == notice.proposed_snapshot
    {
        return Err(StoreError::InvalidWorkProjection(
            "source notice has invalid source bindings".into(),
        ));
    }
    validate_work_source_snapshot(&proposed, notice.recorded_at)?;
    validate_work_source_snapshot(&cited, notice.recorded_at)?;
    if notice.actor.actor_id.trim().is_empty()
        || notice
            .actor
            .session_id
            .as_ref()
            .is_none_or(|session| session.0.trim().is_empty())
        || notice.actor.validate_attribution_context().is_err()
    {
        return Err(StoreError::InvalidWorkProjection(
            "source notice has invalid attribution".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_proposal_on(
    connection: &Connection,
    item: &WorkItem,
    proposal: &WorkSourceProposal,
) -> Result<(), StoreError> {
    if proposal.schema_version != SCHEMA_VERSION
        || proposal.project_id != item.project_id
        || proposal.work_id != item.work_id
        || proposal.root_id != item.root_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "source proposal crosses its canonical work".into(),
        ));
    }
    let basis = match &proposal.basis {
        WorkObservationBasis::NativeEvent { event } => {
            let event =
                load_typed_work_object::<crate::WorkEvent>(connection, event, "work_event")?;
            (
                event.work_id,
                event.project_id,
                event.work.revision,
                event.work.source_snapshot_id,
            )
        }
        WorkObservationBasis::RestoredRecord { record } => {
            let record = load_typed_work_object::<RestoredRecord>(
                connection,
                record,
                "work_restored_record",
            )?;
            (
                record.work_id,
                record.project_id,
                1,
                record.item.source_snapshot_id,
            )
        }
    };
    if basis
        != (
            item.work_id,
            item.project_id.clone(),
            proposal.notice.work_revision,
            Some(proposal.notice.cited_snapshot.clone()),
        )
    {
        return Err(StoreError::InvalidWorkProjection(
            "source proposal differs from its canonical local basis".into(),
        ));
    }
    validate_notice_on(connection, item, &proposal.notice)
}

fn preview_on(
    connection: &Connection,
    project: &ProjectId,
    input: &WorkImportInput,
) -> Result<WorkImportPreview, StoreError> {
    let snapshot = CanonicalObject::freeze(&input.snapshot)?;
    let source_key = key(&input.snapshot);
    let item = source_item_on(connection, project, &source_key)?;
    let effect = if let Some(item) = &item {
        let known = source_snapshot_known_on(connection, item, snapshot.hash())?;
        if input.draft.is_some() {
            return Err(invalid(
                "source refresh takes no local draft; omit draft and author local changes separately with work update",
            ));
        }
        if known {
            WorkImportEffect::AlreadyKnown
        } else {
            WorkImportEffect::Notify
        }
    } else {
        if input.draft.is_none() {
            return Err(invalid(
                "first import requires an explicit local draft title and outcome; absent acceptance stays empty",
            ));
        }
        WorkImportEffect::Create
    };
    let token = CanonicalObject::freeze(&(project, input, &item, effect))?;
    Ok(WorkImportPreview {
        project_id: project.clone(),
        source_key,
        snapshot: snapshot.hash().clone(),
        effect,
        work_id: item.as_ref().map(|item| item.work_id),
        work_ref: item.as_ref().map(|item| item.short_ref.clone()),
        work_revision: item.as_ref().map(|item| item.revision),
        cited_snapshot: item
            .as_ref()
            .and_then(|item| item.source_snapshot_id.clone()),
        draft: input.draft.clone().map(|mut draft| {
            draft.acceptance = normalize_strings(&draft.acceptance);
            draft
        }),
        preview_token: token.hash().clone(),
    })
}

impl SqliteStore {
    pub(crate) fn work_source_detail(
        &self,
        project: &ProjectId,
        source: &WorkSourceKey,
    ) -> Result<Option<crate::domain::WorkSourceDetail>, StoreError> {
        self.work_read_snapshot(|store| {
            store
                .lookup_work_source(project, source)?
                .map(|lookup| {
                    let cited_source = load_typed_work_object(
                        &store.connection,
                        &lookup.cited_snapshot,
                        "work_source_snapshot",
                    )?;
                    let latest_proposed_source = lookup
                        .latest_notice
                        .as_ref()
                        .map(|notice| {
                            load_typed_work_object(
                                &store.connection,
                                &notice.proposed_snapshot,
                                "work_source_snapshot",
                            )
                        })
                        .transpose()?;
                    Ok(crate::domain::WorkSourceDetail {
                        notices_omitted: lookup.notice_count.saturating_sub(1),
                        lookup,
                        cited_source,
                        latest_proposed_source,
                    })
                })
                .transpose()
        })
    }
    pub(crate) fn work_source_for_item(
        &self,
        id: WorkId,
    ) -> Result<Option<WorkSourceLookup>, StoreError> {
        let item = load_work_item(&self.connection, id)?;
        let Some(hash) = item.source_snapshot_id.clone() else {
            return Ok(None);
        };
        let source = load_typed_work_object::<WorkSourceSnapshot>(
            &self.connection,
            &hash,
            "work_source_snapshot",
        )?;
        let (notice_count, latest_notice) = source_notice_summary_on(&self.connection, &item)?;
        Ok(Some(WorkSourceLookup {
            source_key: key(&source),
            work_id: id,
            work_ref: item.short_ref,
            work_revision: item.revision,
            cited_snapshot: hash,
            notice_count,
            latest_notice,
        }))
    }
    /// Reads the proposed effect without persisting a snapshot or changing work.
    ///
    /// # Errors
    /// Refuses invalid input, ambiguous source bindings or invalid stored work.
    pub fn preview_work_import(
        &self,
        project: &ProjectId,
        input: &WorkImportInput,
        now: DateTime<Utc>,
    ) -> Result<WorkImportPreview, StoreError> {
        validate_input(input, now)?;
        self.work_read_snapshot(|store| preview_on(&store.connection, project, input))
    }

    /// Finds the exact source key and reports citation separately from notices.
    ///
    /// # Errors
    /// Refuses invalid keys, ambiguous bindings or corrupt selected captures.
    /// Omitted captures are counted, not fully verified by this ordinary read.
    pub fn lookup_work_source(
        &self,
        project: &ProjectId,
        source: &WorkSourceKey,
    ) -> Result<Option<WorkSourceLookup>, StoreError> {
        self.work_read_snapshot(|store| {
            source_item_on(&store.connection, project, source)?
                .map(|item| {
                    let (notice_count, latest_notice) =
                        source_notice_summary_on(&store.connection, &item)?;
                    Ok(WorkSourceLookup {
                        source_key: source.clone(),
                        work_id: item.work_id,
                        work_ref: item.short_ref,
                        work_revision: item.revision,
                        cited_snapshot: item
                            .source_snapshot_id
                            .ok_or_else(|| invalid("imported work has no source citation"))?,
                        notice_count,
                        latest_notice,
                    })
                })
                .transpose()
        })
    }

    /// Applies exactly a previewed intake. Refresh records a notification only.
    ///
    /// # Errors
    /// Refuses invalid or redactor-rejected input, invalid attribution, stale
    /// preview tokens, or storage/integrity failures. Transactional effects roll
    /// back on refusal; an exact committed intent returns its original receipt.
    pub fn apply_work_import<R: Redactor>(
        &mut self,
        project: &ProjectId,
        input: &WorkImportInput,
        preview_token: &ObjectHash,
        actor: &ActorContext,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<WorkImportReceipt, StoreError> {
        validate_input(input, now)?;
        inspect_work_request(redactor, &(project, input, preview_token, actor), actor)?;
        if actor.actor_id.trim().is_empty()
            || actor
                .session_id
                .as_ref()
                .is_none_or(|session| session.0.trim().is_empty())
        {
            return Err(invalid(
                "import requires asserted actor and session attribution",
            ));
        }
        let intent =
            CanonicalObject::freeze(&(project, input, preview_token, actor.retry_stable()))?;
        let transaction = self.begin_work_mutation()?;
        if let Some(receipt) = replay_operation(
            &transaction,
            "import_work",
            intent.hash().as_str(),
            intent.hash(),
        )? {
            transaction.commit()?;
            return Ok(receipt);
        }
        let preview = preview_on(&transaction, project, input)?;
        if &preview.preview_token != preview_token {
            return Err(invalid(
                "work import preview changed; run import preview again before applying",
            ));
        }
        if let Some(id) = preview.work_id {
            let item = load_work_item(&transaction, id)?;
            latest_source_notice_on(&transaction, &item)?;
        }
        let snapshot = CanonicalObject::freeze(&input.snapshot)?;
        Self::insert_object(&transaction, "work_source_snapshot", &snapshot)?;
        let item = if let Some(id) = preview.work_id {
            load_work_item(&transaction, id)?
        } else {
            let draft = input
                .draft
                .as_ref()
                .ok_or_else(|| invalid("first import requires a local draft"))?;
            let request = CreateWorkRequest {
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: draft.title.clone(),
                outcome: draft.outcome.clone(),
                acceptance: draft.acceptance.clone(),
                kind: WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                external_ref: None,
                notes: Vec::new(),
                origin: WorkOrigin::Imported,
                source_snapshot_id: Some(snapshot.hash().clone()),
                actor: actor.clone(),
                idempotency_key: intent.hash().as_str().into(),
                created_at: now,
            };
            create_root_on(&transaction, &request, &[], redactor)?
        };
        let cited_snapshot = item
            .source_snapshot_id
            .clone()
            .ok_or_else(|| invalid("import has no cited snapshot"))?;
        let proposal = if preview.effect == WorkImportEffect::Notify {
            let event: Option<String> = transaction.query_row(
                "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
                [item.work_id.0.to_string()],
                |row| row.get(0),
            )?;
            let basis = if let Some(hash) = event {
                WorkObservationBasis::NativeEvent {
                    event: ObjectHash::from_stored(hash.clone())
                        .ok_or(StoreError::InvalidStoredHash(hash))?,
                }
            } else {
                WorkObservationBasis::RestoredRecord {
                    record: latest_restored_record(&transaction, item.work_id)?
                        .ok_or_else(|| invalid("restored import has no canonical basis"))?
                        .0,
                }
            };
            let proposal = WorkSourceProposal {
                schema_version: SCHEMA_VERSION,
                project_id: project.clone(),
                work_id: item.work_id,
                root_id: item.root_id,
                basis,
                notice: WorkSourceNotice {
                    work_revision: item.revision,
                    cited_snapshot: cited_snapshot.clone(),
                    proposed_snapshot: snapshot.hash().clone(),
                    actor: actor.clone(),
                    recorded_at: now,
                },
            };
            validate_proposal_on(&transaction, &item, &proposal)?;
            let object = CanonicalObject::freeze(&proposal)?;
            Self::insert_object(&transaction, "work_source_proposal", &object)?;
            append_to_work_feeds(
                &transaction,
                project,
                item.root_id,
                None,
                None,
                "work_source_proposal",
                &object,
            )?;
            Some(object.hash().clone())
        } else {
            None
        };
        let receipt = WorkImportReceipt {
            effect: preview.effect,
            source_key: preview.source_key,
            snapshot: snapshot.hash().clone(),
            cited_snapshot,
            work_id: item.work_id,
            work_ref: item.short_ref,
            work_revision: item.revision,
            proposal,
        };
        persist_operation_result(
            &transaction,
            "import_work",
            intent.hash().as_str(),
            intent.hash(),
            &receipt,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }
}
