use super::super::load::HIERARCHY_PASS_VISITS;
use super::*;
use crate::WorkGraphSnapshotLoadResult;

const OPEN_LIMIT: usize = super::super::super::work::MAX_OPEN_WORK_DESCENDANTS as usize;
const OPEN_LIMIT_REFUSAL: &str = "open descendant count exceeds the work limit";

fn short_ref_of(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}

fn open_children(
    store: &mut SqliteStore,
    root: &crate::WorkItem,
    count: usize,
    prefix: &str,
    second: i64,
) -> Vec<crate::WorkItem> {
    let mut created = Vec::with_capacity(count);
    let mut batch = 0_usize;
    while created.len() < count {
        let parent = store.get_work_item(root.work_id).expect("current parent");
        let size = (count - created.len()).min(16);
        let children = (0..size)
            .map(|index| ChildWorkDraft {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                local_key: format!("{prefix}-{batch:03}-{index:02}"),
                child_requirement: ChildRequirement::Optional,
                title: format!("{prefix} child {batch:03}-{index:02}"),
                outcome: "restore validation stays linear".into(),
                acceptance: Vec::new(),
                kind: WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
            })
            .collect();
        let offset = i64::try_from(batch).expect("batch count fits");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: parent.revision,
                    children,
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("{prefix}-batch-{batch}"),
                    created_at: at(second + offset),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose children");
        created.extend(decomposition.children);
        batch += 1;
    }
    created
}

fn cancel(store: &mut SqliteStore, child: &crate::WorkItem, second: i64) {
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "closed history".into(),
                actor: actor("planner-session"),
                idempotency_key: format!("cancel-{}", child.short_ref),
                disposed_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel child");
}

fn save(store: &mut SqliteStore, project: &ProjectId, second: i64) -> WorkGraphSnapshotDocument {
    store
        .save_work_graph_snapshot(
            project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("save graph")
        .document
}

/// Loads one document into a fresh store and reports how many snapshot items
/// the open-descendant pass visited; the counter is reset right before the
/// load and read right after, so nothing else on this thread is measured.
fn load_counting_visits(
    project: &ProjectId,
    document: &WorkGraphSnapshotDocument,
) -> (
    Result<WorkGraphSnapshotLoadResult, StoreError>,
    usize,
    SqliteStore,
) {
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    HIERARCHY_PASS_VISITS.with(|visits| visits.set(0));
    let result = destination.load_work_graph_snapshot(
        project,
        &actor("load-session"),
        &snapshot_bytes(document),
        false,
        at(900),
        &DevelopmentNoopRedactor,
    );
    let visits = HIERARCHY_PASS_VISITS.with(std::cell::Cell::get);
    (result, visits, destination)
}

fn destination_work_count(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .expect("count destination work")
}

fn injected_child(
    template: &crate::WorkGraphSnapshotItem,
    parent: &crate::WorkGraphSnapshotItem,
    title: &str,
) -> crate::WorkGraphSnapshotItem {
    let work_id = WorkId::new();
    crate::WorkGraphSnapshotItem {
        external_ref: None,
        work_id,
        short_ref: short_ref_of(work_id),
        root_id: parent.root_id,
        parent_id: Some(parent.work_id),
        child_requirement: ChildRequirement::Optional,
        title: title.into(),
        outcome: template.outcome.clone(),
        acceptance: Vec::new(),
        kind: WorkItemKind::Task,
        priority: 2,
        labels: Vec::new(),
        origin: WorkOrigin::Local,
        source_snapshot_id: None,
        lifecycle: WorkLifecycle::Open,
        prerequisites: Vec::new(),
        superseded_by: None,
        assigned_to: None,
        deferred_until: None,
        evaluation_mode: None,
        disposal_reason: None,
    }
}

/// Appends items to a saved document the way a hand-edited snapshot would
/// have to: in ref order, with the section count and the digest rebound.
fn inject(document: &mut WorkGraphSnapshotDocument, items: Vec<crate::WorkGraphSnapshotItem>) {
    document.body.summary.section_counts.items += items.len();
    document.body.items.extend(items);
    document
        .body
        .items
        .sort_by(|left, right| left.short_ref.cmp(&right.short_ref));
    rebind_snapshot_body(document);
}

fn assert_refused(
    name: &str,
    project: &ProjectId,
    document: &WorkGraphSnapshotDocument,
    message: &str,
) -> usize {
    let (result, visits, destination) = load_counting_visits(project, document);
    match result {
        Err(StoreError::InvalidGraphSnapshot(actual)) => {
            assert_eq!(actual, message, "{name}: refusal text");
        }
        other => panic!("{name}: expected the refusal {message:?}, got {other:?}"),
    }
    assert_eq!(
        destination_work_count(&destination),
        0,
        "{name}: a refused load left partial work"
    );
    visits
}

struct Shape {
    name: &'static str,
    roots: usize,
    open_per_root: usize,
    cancelled_per_root: usize,
}

// The bound: the open-descendant pass of restore validation visits each
// snapshot item exactly once, whatever the number of roots and whatever the
// items' lifecycles, so every shape below reads exactly its item count.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario compares every shape against the same bound"
)]
fn restore_validation_visits_each_item_once_whatever_the_root_count() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut source =
        SqliteStore::open(directory.path().join("shapes-source.db")).expect("source store");
    let shapes = [
        Shape {
            name: "wide-forest",
            roots: 64,
            open_per_root: 3,
            cancelled_per_root: 0,
        },
        Shape {
            name: "single-root",
            roots: 1,
            open_per_root: OPEN_LIMIT,
            cancelled_per_root: 0,
        },
        Shape {
            name: "roots-only",
            roots: 256,
            open_per_root: 0,
            cancelled_per_root: 0,
        },
        Shape {
            name: "closed-history",
            roots: 1,
            open_per_root: 0,
            cancelled_per_root: OPEN_LIMIT,
        },
        Shape {
            name: "wide-forest-half",
            roots: 32,
            open_per_root: 3,
            cancelled_per_root: 0,
        },
        Shape {
            name: "single-root-half",
            roots: 1,
            open_per_root: 127,
            cancelled_per_root: 0,
        },
    ];
    for shape in &shapes {
        let project = ProjectId(format!("snapshot-shape-{}", shape.name));
        for root_index in 0..shape.roots {
            let root = create_root(
                &mut source,
                &project,
                &format!("{} root {root_index:03}", shape.name),
                &format!("{}-root-{root_index:03}", shape.name),
            );
            let prefix = format!("{}-{root_index:03}", shape.name);
            open_children(&mut source, &root, shape.open_per_root, &prefix, 2);
            let closed = open_children(
                &mut source,
                &root,
                shape.cancelled_per_root,
                &format!("{prefix}-closed"),
                40,
            );
            for child in &closed {
                cancel(&mut source, child, 80);
            }
        }
        let document = save(&mut source, &project, 200);
        let items = document.body.items.len();
        let expected_items = shape.roots * (1 + shape.open_per_root + shape.cancelled_per_root);
        assert_eq!(items, expected_items, "{}: fixture size", shape.name);

        let (result, visits, _destination) = load_counting_visits(&project, &document);
        let preview = result.expect("every shape loads").preview;
        assert_eq!(
            preview.lifecycle_counts.cancelled,
            shape.roots * shape.cancelled_per_root,
            "{}: cancelled rows restored",
            shape.name
        );
        assert_eq!(
            preview.lifecycle_counts.open + preview.lifecycle_counts.proposed,
            shape.roots * (1 + shape.open_per_root),
            "{}: live rows restored",
            shape.name
        );
        // A scan per root would read roots times items here: 16384 for the
        // wide forest and 65536 for the roots-only shape, never 256.
        assert_eq!(
            visits, items,
            "{}: the open-descendant pass visits each snapshot item exactly once",
            shape.name
        );
    }
}

// Restore still refuses a root over the open-descendant limit, counts
// proposed items with open ones, leaves closed items out of the count while
// still visiting them, and leaves no partial state behind a refusal.
#[test]
fn restore_refuses_a_root_over_the_open_descendant_limit_and_admits_the_limit() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-open-limit".into());
    let mut source =
        SqliteStore::open(directory.path().join("limit-source.db")).expect("source store");
    let root = create_root(&mut source, &project, "Full root", "full-root");
    let closed = open_children(&mut source, &root, 16, "closed", 2);
    for child in &closed {
        cancel(&mut source, child, 30);
    }
    let live = open_children(&mut source, &root, OPEN_LIMIT, "live", 40);
    assert_eq!(live.len(), OPEN_LIMIT);
    let document = save(&mut source, &project, 200);
    assert_eq!(document.body.items.len(), 1 + 16 + OPEN_LIMIT);

    let (result, visits, destination) = load_counting_visits(&project, &document);
    let preview = result.expect("exactly the limit is admitted").preview;
    assert_eq!(preview.lifecycle_counts.cancelled, 16);
    assert_eq!(
        preview.lifecycle_counts.open + preview.lifecycle_counts.proposed,
        1 + OPEN_LIMIT
    );
    assert_eq!(
        visits,
        1 + 16 + OPEN_LIMIT,
        "closed items are visited once, like every other item"
    );
    assert_eq!(
        destination
            .work_children(root.work_id)
            .expect("restored children")
            .len(),
        16 + OPEN_LIMIT
    );

    let root_item = document
        .body
        .items
        .iter()
        .find(|item| item.work_id == root.work_id)
        .expect("root is in the snapshot")
        .clone();
    let template = document
        .body
        .items
        .iter()
        .find(|item| item.parent_id == Some(root.work_id) && item.lifecycle == WorkLifecycle::Open)
        .expect("an open child is in the snapshot")
        .clone();

    let mut one_over = document.clone();
    let extra = injected_child(&template, &root_item, "Injected open child over the limit");
    inject(&mut one_over, vec![extra.clone()]);
    let visits = assert_refused("open-over-limit", &project, &one_over, OPEN_LIMIT_REFUSAL);
    assert_eq!(visits, one_over.body.items.len());

    let mut proposed_over = document.clone();
    let mut proposed = extra;
    proposed.lifecycle = WorkLifecycle::Proposed;
    proposed.title = "Injected proposed child over the limit".into();
    inject(&mut proposed_over, vec![proposed]);
    assert_refused(
        "proposed-over-limit",
        &project,
        &proposed_over,
        OPEN_LIMIT_REFUSAL,
    );
}

// The earlier validation phases keep their refusals: a document that is both
// too deep and over the open-descendant limit is refused for its depth, a
// malformed root binding is refused by the item's shape check, and a child
// whose root differs from its parent's by the relation pass, each before the
// count runs.
#[test]
fn restore_keeps_the_earlier_refusal_when_a_document_has_several_defects() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-defect-order".into());
    let mut source =
        SqliteStore::open(directory.path().join("order-source.db")).expect("source store");
    let root = create_root(&mut source, &project, "Ordered root", "ordered-root");
    let other = create_root(&mut source, &project, "Other root", "other-root");
    let live = open_children(&mut source, &root, OPEN_LIMIT, "ordered", 2);
    assert_eq!(live.len(), OPEN_LIMIT);
    let document = save(&mut source, &project, 200);
    let item = |work_id: WorkId| {
        document
            .body
            .items
            .iter()
            .find(|item| item.work_id == work_id)
            .expect("item is in the snapshot")
            .clone()
    };
    let root_item = item(root.work_id);
    let other_item = item(other.work_id);
    let first_child = item(live[0].work_id);

    let mut too_deep_and_over = document.clone();
    let mut chain = Vec::new();
    let mut parent = first_child.clone();
    for depth in 2..=5 {
        let next = injected_child(&first_child, &parent, &format!("Depth {depth}"));
        parent = next.clone();
        chain.push(next);
    }
    chain.push(injected_child(
        &first_child,
        &root_item,
        "The 256th open child",
    ));
    inject(&mut too_deep_and_over, chain);
    assert_refused(
        "depth-before-count",
        &project,
        &too_deep_and_over,
        "parent hierarchy exceeds the work depth limit",
    );

    let mut malformed_root = document.clone();
    let mut self_rooted = injected_child(&first_child, &root_item, "Self-rooted child");
    self_rooted.root_id = self_rooted.work_id;
    inject(&mut malformed_root, vec![self_rooted]);
    assert_refused(
        "malformed-root-binding",
        &project,
        &malformed_root,
        "work root and parent bindings disagree",
    );

    let mut crossing = document.clone();
    let mut crossed = injected_child(&first_child, &root_item, "Child bound to another root");
    crossed.root_id = other_item.work_id;
    inject(&mut crossing, vec![crossed]);
    assert_refused(
        "crossing-root",
        &project,
        &crossing,
        "child crosses its root binding",
    );
}

// A project with no work saves and restores an empty item section; the pass
// visits nothing.
#[test]
fn an_empty_graph_restores_without_a_hierarchy_visit() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-empty-graph".into());
    let mut source =
        SqliteStore::open(directory.path().join("empty-source.db")).expect("source store");
    let document = save(&mut source, &project, 2);
    assert_eq!(document.body.items.len(), 0);
    let (result, visits, _destination) = load_counting_visits(&project, &document);
    result.expect("an empty graph loads");
    assert_eq!(visits, 0);
}
