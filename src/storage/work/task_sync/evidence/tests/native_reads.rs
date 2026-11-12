//! Retention preserves history without changing ordinary local lookup results.
use super::*;
use crate::DevelopmentNoopRedactor;
use crate::domain::{WorkImportDraft, WorkImportEffect, WorkImportInput, WorkSourceKey};
use crate::storage::test_support::open_work_host;
use crate::storage::work::test_support::{claim, root_request};
use crate::{NamedRootBindingKind, NamedRootEndReason};
use std::collections::BTreeMap;

fn copy_store(source: &SqliteStore, home: &crate::test_support::TempHome) -> SqliteStore {
    let path = home.path().join("shared-before-retention.db");
    source
        .connection
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    SqliteStore::open(&path).unwrap()
}

fn retained_closure(source: &SqliteStore, root: &ObjectId) -> EvidencePack {
    let (origin, body) = origin(None);
    let mut records = BTreeMap::from([(origin.clone(), enrollment(&origin, &body))]);
    let mut pending = vec![root.clone()];
    while let Some(id) = pending.pop() {
        if records.contains_key(&id) {
            continue;
        }
        let (kind, bytes): (String, Vec<u8>) = source
            .connection
            .query_row(
                "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let record =
            RecordEnvelope::from_canonical(origin.clone(), id.clone(), &kind, bytes).unwrap();
        for dependency in crate::task_sync::evidence::inventory::dependencies(&record).unwrap() {
            pending.push(dependency.object_id);
        }
        records.insert(id, record);
    }
    pack(&origin, records.into_values().collect())
}

fn unchanged_originals(store: &SqliteStore, input: &EvidencePack) {
    for record in input.records() {
        let (kind, bytes): (String, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
                [record.object_id().as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, record.kind().as_str());
        assert_eq!(bytes, record.canonical_bytes());
    }
}

fn source_input() -> WorkImportInput {
    let payload = CanonicalObject::freeze(&json!({"plan":"source"})).unwrap();
    WorkImportInput {
        snapshot: crate::WorkSourceSnapshot {
            schema_version: crate::schema::SCHEMA_VERSION,
            adapter_kind: "planner".into(),
            canonical_ref: "shared/item".into(),
            projected: crate::WorkSourceProjection {
                title: Some("first".into()),
                body: None,
                status: None,
                owner: None,
            },
            captured_at: at(1),
            source_revision: Some("1".into()),
            fingerprint: "source revision one".into(),
            canonical_url: None,
            payload_hash: payload.key().clone(),
            raw: BTreeMap::new(),
        },
        draft: Some(WorkImportDraft {
            title: "local authored title".into(),
            outcome: "local outcome".into(),
            acceptance: Vec::new(),
        }),
    }
}

#[test]
fn retained_source_proposal_does_not_suppress_a_native_import_notice() {
    let mut destination = SqliteStore::open_in_memory().unwrap();
    let mut input = source_input();
    let first = destination
        .preview_work_import(&project(), &input, at(2))
        .unwrap();
    let created = destination
        .apply_work_import(
            &project(),
            &input,
            &first.preview_token,
            &actor("planner"),
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let home = crate::test_support::temp_home().unwrap();
    let mut source = copy_store(&destination, &home);
    input.draft = None;
    input.snapshot.source_revision = Some("2".into());
    input.snapshot.projected.title = Some("second".into());
    let foreign_preview = source
        .preview_work_import(&project(), &input, at(3))
        .unwrap();
    let foreign = source
        .apply_work_import(
            &project(),
            &input,
            &foreign_preview.preview_token,
            &actor("planner"),
            at(3),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let input_pack = retained_closure(&source, foreign.proposal.as_ref().unwrap());
    retain(&mut destination, &input_pack);
    let key = WorkSourceKey {
        adapter_kind: "planner".into(),
        canonical_ref: "shared/item".into(),
    };
    let lookup = destination
        .lookup_work_source(&project(), &key)
        .unwrap()
        .unwrap();
    assert_eq!(lookup.notice_count, 0);
    assert!(lookup.latest_notice.is_none());
    let preview = destination
        .preview_work_import(&project(), &input, at(4))
        .unwrap();
    assert_eq!(preview.effect, WorkImportEffect::Notify);
    let native = destination
        .apply_work_import(
            &project(),
            &input,
            &preview.preview_token,
            &actor("planner"),
            at(4),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_ne!(native.proposal, foreign.proposal);
    assert_eq!(native.work_id, created.work_id);
    let lookup = destination
        .lookup_work_source(&project(), &key)
        .unwrap()
        .unwrap();
    assert_eq!(lookup.notice_count, 1);
    assert_eq!(
        lookup.latest_notice.unwrap().proposed_snapshot,
        native.snapshot
    );
    assert_eq!(
        destination
            .preview_work_import(&project(), &input, at(5))
            .unwrap()
            .effect,
        WorkImportEffect::AlreadyKnown
    );
    unchanged_originals(&destination, &input_pack);
    healthy(&destination);
}

#[test]
fn named_root_end_uses_the_native_bound_record_after_foreign_retention() {
    let mut destination = SqliteStore::open_in_memory().unwrap();
    let item = destination
        .create_work(
            &root_request(&project().0, "native root", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut destination, &item, "runner", "claim", 2, 300);
    let host = open_work_host(&mut destination, "runner", at(3));
    let home = crate::test_support::temp_home().unwrap();
    let mut source = copy_store(&destination, &home);
    let foreign = source
        .bind_named_root(
            &project(),
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "foreign-workspace",
            1,
            at(3),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "foreign-bound",
            at(4),
        )
        .unwrap();
    let input_pack = retained_closure(&source, &foreign.event);
    retain(&mut destination, &input_pack);
    // The foreign row precedes the native row in the same claim/generation lookup.
    let native = destination
        .bind_named_root(
            &project(),
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "native-workspace",
            1,
            at(3),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "native-bound",
            at(4),
        )
        .unwrap();
    let ended = destination
        .bind_named_root(
            &project(),
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "native-workspace",
            1,
            at(3),
            NamedRootBindingKind::Ended,
            Some(NamedRootEndReason::ExplicitClear),
            &mut actor("runner"),
            "native-ended",
            at(5),
        )
        .unwrap();
    assert!(ended.position.position > native.position.position);
    assert_eq!(ended.workspace_id, "native-workspace");
    unchanged_originals(&destination, &input_pack);
    healthy(&destination);
}

#[test]
fn retained_work_history_does_not_make_an_empty_graph_destination_nonempty() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let item = source
        .create_work(
            &root_request(&project().0, "graph work", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let event: String = source.connection.query_row(
        "SELECT object_id FROM work_feed_entries WHERE feed_kind='root_work' AND feed_id=?1 AND object_kind='work_event' ORDER BY position DESC LIMIT 1",
        [item.work_id.0.to_string()], |row| row.get(0),
    ).unwrap();
    let input_pack = retained_closure(&source, &ObjectId::from_stored(event).unwrap());
    let saved = source
        .save_work_graph_snapshot(
            &project(),
            &actor("planner"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let bytes = serde_json::to_vec(&saved.document).unwrap();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    retain(&mut destination, &input_pack);
    assert!(matches!(
        destination.get_work_item(item.work_id),
        Err(StoreError::WorkNotFound(_))
    ));
    let loaded = destination
        .load_work_graph_snapshot(
            &project(),
            &actor("planner"),
            &bytes,
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(loaded.loaded);
    assert_eq!(
        destination.get_work_item(item.work_id).unwrap().title,
        item.title
    );
    assert!(matches!(
        destination.load_work_graph_snapshot(
            &project(),
            &actor("planner"),
            &bytes,
            false,
            at(5),
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::GraphDestinationNotEmpty)
    ));
    unchanged_originals(&destination, &input_pack);
    healthy(&destination);
}
