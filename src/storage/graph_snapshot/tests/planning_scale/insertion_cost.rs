use super::*;
use crate::storage::graph_snapshot::load::capture_load_item_sql;
use crate::storage::work_cost::sqlite::{self, Counters};

fn root_last_document(
    template: &WorkGraphSnapshotDocument,
    shape: ForestShape,
    count: usize,
) -> WorkGraphSnapshotDocument {
    let ids: Vec<_> = (0..count)
        .map(|index| {
            WorkId(uuid::Uuid::from_u128(
                u128::try_from(count - index).unwrap(),
            ))
        })
        .collect();
    let document = build_document_with_ids(template, &ids, shape, 1, 0, true);
    let positions: std::collections::HashMap<_, _> = document
        .body
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.work_id, index))
        .collect();
    for item in &document.body.items {
        if let Some(parent) = item.parent_id {
            assert!(positions[&item.work_id] < positions[&parent]);
        }
    }
    document
}

fn sample(
    label: &str,
    document: &WorkGraphSnapshotDocument,
    foreign_keys: bool,
) -> (Counters, Vec<crate::WorkItem>, SqliteStore) {
    let mut destination = SqliteStore::open_in_memory().unwrap();
    assert!(destination.connection.is_autocommit());
    destination
        .connection
        .pragma_update(None, "foreign_keys", foreign_keys)
        .unwrap();
    let enabled: bool = destination
        .connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();
    assert_eq!(enabled, foreign_keys);
    let bytes = document.container_bytes().unwrap();
    let (result, timings) = capture_load_item_sql(|| {
        destination.load_work_graph_snapshot(
            &document.body.summary.project_id,
            &actor("small-forest-loader"),
            &bytes,
            false,
            at(900),
            &DevelopmentNoopRedactor,
        )
    });
    let result = result.unwrap();
    assert!(result.loaded);
    assert_eq!(
        result.preview.refs,
        document
            .body
            .items
            .iter()
            .map(|item| item.short_ref.clone())
            .collect::<Vec<_>>()
    );
    let metrics = timings.item_insert_sql.unwrap();
    assert_eq!(
        metrics.total.statements,
        u64::try_from(document.body.items.len()).unwrap()
    );
    assert_eq!(metrics.by_kind.len(), 1);
    assert_eq!(metrics.by_kind["insert"], metrics.total);
    eprintln!(
        "restore_item_sql family={label} items={} foreign_keys={foreign_keys} statements={} fullscan_steps={} vm_steps={} sorts={}",
        document.body.items.len(),
        metrics.total.statements,
        metrics.total.fullscan_steps,
        metrics.total.vm_steps,
        metrics.total.sorts,
    );
    let violations: i64 = destination
        .connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    let items = document
        .body
        .items
        .iter()
        .map(|item| destination.get_work_item(item.work_id).unwrap())
        .collect();
    (metrics.total, items, destination)
}

#[test]
#[ignore = "focused before/after FK attribution diagnostic"]
fn graph_snapshot_small_forest_fk_insertion_baseline() {
    sqlite::assert_calibrated();
    let template = root_template();
    for (label, shape) in [
        ("independent", ForestShape::Independent),
        ("mixed", ForestShape::Mixed),
        ("terminal-fanout", ForestShape::TerminalFanout),
    ] {
        for count in [256, 512] {
            let document = root_last_document(&template, shape, count);
            let (enabled, enabled_items, _) = sample(label, &document, true);
            let (disabled, disabled_items, _) = sample(label, &document, false);
            assert_eq!(enabled.statements, disabled.statements);
            assert_eq!(enabled_items, disabled_items);
        }
    }
}

#[test]
fn graph_snapshot_small_forest_item_insertion_avoids_quadratic_fk_scans() {
    sqlite::assert_calibrated();
    let template = root_template();
    let independent = root_last_document(&template, ForestShape::Independent, 512);
    let (control, _, _) = sample("independent-control", &independent, true);
    assert_eq!(control.fullscan_steps, 0);
    for (label, shape) in [
        ("mixed", ForestShape::Mixed),
        ("terminal-fanout", ForestShape::TerminalFanout),
    ] {
        let small = root_last_document(&template, shape, 256);
        let large = root_last_document(&template, shape, 512);
        let (small_cost, _, _) = sample(label, &small, true);
        let (large_cost, _, _) = sample(label, &large, true);
        assert_eq!(small_cost.fullscan_steps, control.fullscan_steps);
        assert_eq!(large_cost.fullscan_steps, control.fullscan_steps);
        assert!(large_cost.vm_steps <= small_cost.vm_steps * 3);
    }
}

fn physical_ids(store: &SqliteStore) -> Vec<String> {
    store
        .connection
        .prepare("SELECT work_id FROM work_items ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn graph_snapshot_parent_first_insertion_preserves_the_snapshot_and_depth_limit() {
    let mut document = root_last_document(&root_template(), ForestShape::Independent, 5);
    let ids: Vec<_> = document
        .body
        .items
        .iter()
        .map(|item| item.work_id)
        .collect();
    let root = *ids.last().unwrap();
    for (index, item) in document.body.items.iter_mut().enumerate() {
        item.root_id = root;
        item.parent_id = ids.get(index + 1).copied();
        item.child_requirement = if index % 2 == 0 {
            ChildRequirement::Required
        } else {
            ChildRequirement::Optional
        };
    }
    rebind_snapshot_body(&mut document);
    let (_, _, mut destination) = sample("depth-limit", &document, true);
    assert_eq!(
        physical_ids(&destination),
        ids.iter()
            .rev()
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>()
    );
    let saved = save_document(&mut destination, &document.body.summary.project_id, 901);
    assert_eq!(saved.body.items, document.body.items);
}

#[test]
fn graph_snapshot_supersession_cycle_keeps_the_whole_original_insertion_order() {
    let mut document = root_last_document(&root_template(), ForestShape::Mixed, 3);
    let child = document
        .body
        .items
        .iter()
        .find(|item| item.parent_id.is_some())
        .unwrap();
    let parent_id = child.parent_id.unwrap();
    let child_id = child.work_id;
    assert_eq!(child.child_requirement, ChildRequirement::Optional);
    let reason = "optional child replaces its parent";
    let parent = document
        .body
        .items
        .iter_mut()
        .find(|item| item.work_id == parent_id)
        .unwrap();
    parent.lifecycle = WorkLifecycle::Superseded;
    parent.superseded_by = Some(child_id);
    parent.disposal_reason = Some(reason.into());
    let record = document
        .body
        .records
        .iter_mut()
        .find(|record| record.work_id == parent_id)
        .unwrap();
    let WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload else {
        panic!("fixture uses native history");
    };
    history.events.push(WorkGraphSnapshotEvent {
        kind: "disposed".into(),
        work_revision: 1,
        occurred_at: at(1),
        reason: Some(reason.into()),
        lifecycle: Some(WorkLifecycle::Superseded),
        related_work_id: Some(child_id),
        related_work_revision: None,
        actor: actor("planner-session"),
    });
    rebind_snapshot_body(&mut document);
    let (_, _, mut destination) = sample("supersession-cycle", &document, true);
    assert_eq!(
        physical_ids(&destination),
        document
            .body
            .items
            .iter()
            .map(|item| item.work_id.0.to_string())
            .collect::<Vec<_>>()
    );
    let saved = save_document(&mut destination, &document.body.summary.project_id, 901);
    assert_eq!(saved.body.items, document.body.items);
}
