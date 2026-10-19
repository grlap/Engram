use std::time::{Duration, Instant};

use super::super::load::GRAPH_VALIDATION_WORK;
use super::*;
use crate::{
    WorkGraphSnapshotDocument, WorkGraphSnapshotEvent, WorkGraphSnapshotHistory,
    WorkGraphSnapshotItem, WorkGraphSnapshotRecordPayload, WorkGraphSnapshotSectionCounts, WorkId,
};

const CONTAINER_LIMIT: usize = 128 * 1024 * 1024;
const VALIDATION_BUDGET: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum ForestShape {
    Independent,
    Mixed,
    TerminalFanout,
    Chained,
}

#[derive(Clone, Copy, Default)]
/// Structural input/output units used by the graph-validation counter bound.
/// `relations` (P) counts item row markers and relation fields, per-native-
/// record relation-basis materialization, and optional event relation
/// endpoints. `events` (E) counts each event row; source/memory validation,
/// parsing, sorting comparisons, text bytes, and payload/equality copies are
/// outside this element-visit measure.
struct StructuralCounts {
    items: usize,
    records: usize,
    blockers: usize,
    events: usize,
    relations: usize,
}

impl StructuralCounts {
    fn denominator(self) -> usize {
        self.items + self.records + self.blockers + self.events
    }

    fn total(self) -> usize {
        self.denominator() + self.relations
    }
}

fn short_ref(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}

fn save_document(
    store: &mut SqliteStore,
    project: &ProjectId,
    second: i64,
) -> WorkGraphSnapshotDocument {
    store
        .save_work_graph_snapshot(
            project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("save planning scale snapshot")
        .document
}

fn root_template() -> WorkGraphSnapshotDocument {
    let project = ProjectId("planning-scale-template".into());
    let mut source = SqliteStore::open_in_memory().expect("source store");
    create_root(&mut source, &project, "template", "planning-scale-template");
    let mut document = save_document(&mut source, &project, 10);
    let item = &mut document.body.items[0];
    item.title = "x".into();
    item.outcome = "x".into();
    item.acceptance.clear();
    item.labels.clear();
    item.priority = 0;
    item.assigned_to = None;
    let WorkGraphSnapshotRecordPayload::Native { history } = &mut document.body.records[0].payload
    else {
        panic!("new item has a native history record");
    };
    let history: &mut WorkGraphSnapshotHistory = history.as_mut();
    *history = WorkGraphSnapshotHistory {
        source_notices: Vec::new(),
        notes: Vec::new(),
        events: Vec::new(),
        completion: None,
    };
    rebind_snapshot_body(&mut document);
    document
}

fn set_section_counts(document: &mut WorkGraphSnapshotDocument) {
    document.body.summary.section_counts = WorkGraphSnapshotSectionCounts {
        items: document.body.items.len(),
        blockers: document.body.blockers.len(),
        sources: document.body.sources.len(),
        records: document.body.records.len(),
        memories: document.body.memories.len(),
    };
    document.manifest.summary = document.body.summary.clone();
}

fn build_document(
    template: &WorkGraphSnapshotDocument,
    item_count: usize,
    shape: ForestShape,
    records_per_item: usize,
    events_per_record: usize,
    bind_digest: bool,
) -> WorkGraphSnapshotDocument {
    assert!(item_count > 0);
    assert!(records_per_item > 0);
    let template_item = template.body.items[0].clone();
    let template_record = template.body.records[0].clone();
    let mut ids = Vec::with_capacity(item_count);
    let mut unique = std::collections::HashSet::with_capacity(item_count);
    let mut unique_refs = std::collections::HashSet::with_capacity(item_count);
    while ids.len() < item_count {
        let id = WorkId::new();
        if !unique.contains(&id) && unique_refs.insert(short_ref(id)) {
            unique.insert(id);
            ids.push(id);
        }
    }

    let mut items = Vec::with_capacity(item_count);
    let mut records = Vec::with_capacity(item_count.saturating_mul(records_per_item));
    let mixed_groups = [1_usize, 2, 8, 32, 128, 256];
    let mut root_id = ids[0];
    let mut next_group_start = 0;
    let mut group_number = 0;
    for (index, &id) in ids.iter().enumerate() {
        if index == next_group_start {
            root_id = id;
            let group_size = match shape {
                ForestShape::Independent | ForestShape::Chained => 1,
                ForestShape::Mixed => mixed_groups[group_number % mixed_groups.len()],
                ForestShape::TerminalFanout => 556,
            };
            next_group_start = index + group_size.min(item_count - index);
            group_number += 1;
        }

        let mut item: WorkGraphSnapshotItem = template_item.clone();
        let terminal_fanout_offset = index % 556;
        let terminal_descendant = matches!(shape, ForestShape::TerminalFanout)
            && (1..=300).contains(&terminal_fanout_offset);
        item.work_id = id;
        item.short_ref = short_ref(id);
        item.root_id = root_id;
        item.parent_id = if id == root_id { None } else { Some(root_id) };
        item.child_requirement = if id == root_id {
            ChildRequirement::Required
        } else {
            ChildRequirement::Optional
        };
        if terminal_descendant {
            item.lifecycle = WorkLifecycle::Cancelled;
            item.disposal_reason = Some("planning-scale terminal fanout".into());
        }
        item.prerequisites.clear();
        if matches!(shape, ForestShape::Chained) && index > 0 {
            item.prerequisites.push(ids[index - 1]);
        }
        items.push(item);

        for generation in 0..records_per_item {
            let mut record = template_record.clone();
            record.work_id = id;
            record.generation_index = generation;
            let WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload else {
                panic!("new item has a native history record");
            };
            let history: &mut WorkGraphSnapshotHistory = history.as_mut();
            let mut events: Vec<_> = (0..events_per_record)
                .map(|event_index| WorkGraphSnapshotEvent {
                    kind: "revised".into(),
                    work_revision: i64::try_from(event_index + 1).expect("small fixture revision"),
                    occurred_at: at(i64::try_from(event_index + 1).expect("small time")),
                    reason: None,
                    lifecycle: None,
                    related_work_id: None,
                    related_work_revision: None,
                    actor: actor("planner-session"),
                })
                .collect();
            if terminal_descendant {
                events.push(WorkGraphSnapshotEvent {
                    kind: "disposed".into(),
                    work_revision: i64::try_from(events_per_record + generation + 1)
                        .expect("small fixture revision"),
                    occurred_at: at(
                        i64::try_from(events_per_record + generation + 1).expect("small time")
                    ),
                    reason: Some("planning-scale terminal fanout".into()),
                    lifecycle: Some(WorkLifecycle::Cancelled),
                    related_work_id: None,
                    related_work_revision: None,
                    actor: actor("planner-session"),
                });
            }
            *history = WorkGraphSnapshotHistory {
                source_notices: Vec::new(),
                notes: Vec::new(),
                events,
                completion: None,
            };
            records.push(record);
        }
    }

    items.sort_by(|left, right| left.short_ref.cmp(&right.short_ref));
    records.sort_by(|left, right| {
        let left_ref = short_ref(left.work_id);
        let right_ref = short_ref(right.work_id);
        left_ref
            .cmp(&right_ref)
            .then(left.generation_index.cmp(&right.generation_index))
    });
    let mut document = template.clone();
    document.body.items = items;
    document.body.records = records;
    set_section_counts(&mut document);
    if bind_digest {
        rebind_snapshot_body(&mut document);
    }
    document
}

fn serialized_size(
    template: &WorkGraphSnapshotDocument,
    item_count: usize,
    shape: ForestShape,
    records_per_item: usize,
    events_per_record: usize,
) -> usize {
    build_document(
        template,
        item_count,
        shape,
        records_per_item,
        events_per_record,
        false,
    )
    .container_bytes()
    .expect("serialize planning scale candidate")
    .len()
}

fn largest_item_count(
    template: &WorkGraphSnapshotDocument,
    shape: ForestShape,
    records_per_item: usize,
    events_per_record: usize,
) -> usize {
    let size = |item_count| {
        serialized_size(
            template,
            item_count,
            shape,
            records_per_item,
            events_per_record,
        )
    };
    largest_count_by_container_size(size)
}

fn largest_count_by_container_size(mut size: impl FnMut(usize) -> usize) -> usize {
    let first_size = size(1);
    assert!(first_size <= CONTAINER_LIMIT, "one family member must fit");

    // Bracket the boundary exponentially, then locate it with logarithmic
    // rebuilds. A fixed-size sample can misestimate families whose per-item
    // serialized growth changes with group composition (for example, added
    // lifecycle events in terminal-fanout groups).
    let mut fitting = 1;
    let mut exceeding = 2;
    while size(exceeding) <= CONTAINER_LIMIT {
        fitting = exceeding;
        exceeding = exceeding
            .checked_mul(2)
            .expect("container boundary must fit in usize item counts");
    }

    while exceeding - fitting > 1 {
        let midpoint = fitting + (exceeding - fitting) / 2;
        if size(midpoint) <= CONTAINER_LIMIT {
            fitting = midpoint;
        } else {
            exceeding = midpoint;
        }
    }
    fitting
}

#[test]
fn nmax_search_uses_logarithmic_size_rebuilds_and_proves_boundary() {
    const EXPECTED_NMAX: usize = 8_191;
    let mut calls = 0;
    let nmax = largest_count_by_container_size(|items| {
        calls += 1;
        // The periodic increment models family composition changing at group
        // boundaries while keeping serialized size monotonic in item count.
        items * 16_384 + items / 17
    });

    assert_eq!(nmax, EXPECTED_NMAX);
    assert!(nmax * 16_384 + nmax / 17 <= CONTAINER_LIMIT);
    assert!((nmax + 1) * 16_384 + (nmax + 1) / 17 > CONTAINER_LIMIT);
    assert!(calls <= usize::BITS as usize * 2 + 2);
}

fn add_blockers(
    mut document: WorkGraphSnapshotDocument,
    blockers_per_item: usize,
    bind_digest: bool,
) -> WorkGraphSnapshotDocument {
    let item_ids = document
        .body
        .items
        .iter()
        .map(|item| item.work_id)
        .collect::<Vec<_>>();
    for (index, work_id) in item_ids.into_iter().enumerate() {
        for blocker_index in 0..blockers_per_item {
            document
                .body
                .blockers
                .push(crate::WorkGraphSnapshotBlocker {
                    work_id,
                    blocker_id: format!("scale-blocker-{index:08}-{blocker_index:03}"),
                    kind: WorkBlockerKind::Manual,
                    detail: "x".into(),
                    created_by: actor("planner-session"),
                    created_at: at(1),
                });
        }
    }
    let refs = document
        .body
        .items
        .iter()
        .map(|item| (item.work_id, item.short_ref.as_str()))
        .collect::<std::collections::HashMap<_, _>>();
    document.body.blockers.sort_by(|left, right| {
        refs.get(&left.work_id)
            .cmp(&refs.get(&right.work_id))
            .then(left.blocker_id.cmp(&right.blocker_id))
    });
    set_section_counts(&mut document);
    if bind_digest {
        rebind_snapshot_body(&mut document);
    }
    document
}

fn largest_blocker_item_count(
    template: &WorkGraphSnapshotDocument,
    blockers_per_item: usize,
) -> usize {
    let size = |items| {
        let roots = build_document(template, items, ForestShape::Independent, 1, 0, false);
        add_blockers(roots, blockers_per_item, false)
            .container_bytes()
            .expect("serialize blocker fixture candidate")
            .len()
    };
    largest_count_by_container_size(size)
}

fn structural_counts(document: &WorkGraphSnapshotDocument) -> StructuralCounts {
    let items_by_id = document
        .body
        .items
        .iter()
        .map(|item| (item.work_id, item))
        .collect::<std::collections::HashMap<_, _>>();
    let mut blockers_by_item = std::collections::HashMap::<WorkId, usize>::new();
    for blocker in &document.body.blockers {
        *blockers_by_item.entry(blocker.work_id).or_default() += 1;
    }
    let mut counts = StructuralCounts {
        items: document.body.items.len(),
        records: document.body.records.len(),
        blockers: document.body.blockers.len(),
        ..StructuralCounts::default()
    };
    for item in &document.body.items {
        counts.relations += 1
            + present(item.parent_id.is_some())
            + present(item.superseded_by.is_some())
            + item.prerequisites.len()
            + item.prerequisites.len();
    }
    for record in &document.body.records {
        let WorkGraphSnapshotRecordPayload::Native { history } = &record.payload else {
            continue;
        };
        counts.events += history.events.len();
        counts.relations += 1 + history
            .events
            .iter()
            .map(|event| {
                present(event.related_work_id.is_some())
                    + present(event.related_work_revision.is_some())
            })
            .sum::<usize>();
        let item = items_by_id.get(&record.work_id).expect("record item");
        counts.relations += present(item.parent_id.is_some())
            + present(item.superseded_by.is_some())
            + item.prerequisites.len();
        counts.relations += item.prerequisites.len();
        counts.blockers += blockers_by_item
            .get(&record.work_id)
            .copied()
            .unwrap_or_default();
    }
    counts
}

fn present(value: bool) -> usize {
    usize::from(value)
}

fn validate_one_sample(label: &str, document: &WorkGraphSnapshotDocument) {
    let bytes = document.container_bytes().expect("snapshot container");
    assert!(
        bytes.len() <= CONTAINER_LIMIT,
        "{label}: input over container limit"
    );
    let counts = structural_counts(document);
    let fingerprint = document.manifest.body_sha256.as_str();
    eprintln!(
        "planning_scale fixture={label} I={} R={} B={} E={} P={} P/(I+R+B+E)={}/{} bytes={} fingerprint={fingerprint}",
        counts.items,
        counts.records,
        counts.blockers,
        counts.events,
        counts.relations,
        counts.relations,
        counts.denominator().max(1),
        bytes.len(),
    );
    let project = document.body.summary.project_id.clone();
    let preparation_project = document.body.summary.project_id.clone();
    let preparation_document = document.clone();
    GRAPH_VALIDATION_WORK.with(|work| work.set(0));
    let start = Instant::now();
    super::super::load::prepare_snapshot_for_scale_test(preparation_document, &preparation_project)
        .expect("planning scale snapshot prepares");
    let prepare_elapsed = start.elapsed();
    let validation_work = GRAPH_VALIDATION_WORK.with(std::cell::Cell::get);
    let bound = counts.total().saturating_mul(20).saturating_add(64);
    eprintln!(
        "planning_scale fixture={label} prepare_load_ms={} graph_validation_work={validation_work} graph_validation_bound={bound}",
        prepare_elapsed.as_millis(),
    );
    let mut validation_destination =
        SqliteStore::open_in_memory().expect("validation destination store");
    GRAPH_VALIDATION_WORK.with(|work| work.set(0));
    let start = Instant::now();
    validation_destination
        .load_work_graph_snapshot(
            &project,
            &actor("planning-scale-loader"),
            &bytes,
            true,
            at(900),
            &DevelopmentNoopRedactor,
        )
        .expect("planning scale snapshot validates without writes");
    let validation_elapsed = start.elapsed();
    let validation_work = GRAPH_VALIDATION_WORK.with(std::cell::Cell::get);
    eprintln!(
        "planning_scale fixture={label} validation_ms={} prepare_load_ms={} graph_validation_work={validation_work} graph_validation_bound={bound}",
        validation_elapsed.as_millis(),
        prepare_elapsed.as_millis(),
    );
    assert!(
        validation_work <= bound,
        "{label}: validation work exceeded its structural bound"
    );
    assert!(
        validation_elapsed <= VALIDATION_BUDGET,
        "{label}: full validation exceeded the 120-second budget"
    );

    let mut commit_destination = SqliteStore::open_in_memory().expect("commit destination store");
    let start = Instant::now();
    let result = commit_destination.load_work_graph_snapshot(
        &project,
        &actor("planning-scale-loader"),
        &bytes,
        false,
        at(900),
        &DevelopmentNoopRedactor,
    );
    let commit_elapsed = start.elapsed();
    eprintln!(
        "planning_scale fixture={label} commit_load_ms={}",
        commit_elapsed.as_millis(),
    );
    result.expect("planning scale snapshot validates and loads");
}

#[test]
fn compact_mixed_forest_graph_validation_work_is_bounded() {
    let template = root_template();
    let mixed = build_document(&template, 300, ForestShape::Mixed, 1, 0, true);
    let mixed_with_blockers = add_blockers(mixed, 1, true);
    validate_one_sample("compact-mixed-forest", &mixed_with_blockers);
}

fn validate_independent_fraction(label: &str, divisor: usize) {
    let template = root_template();
    let nmax = largest_item_count(&template, ForestShape::Independent, 1, 0);
    let count = (nmax / divisor).max(1);
    let document = build_document(&template, count, ForestShape::Independent, 1, 0, true);
    validate_one_sample(label, &document);
}

#[test]
#[ignore = "one bounded independent-root scaling diagnostic sample"]
fn graph_snapshot_diagnostic_independent_1_64() {
    validate_independent_fraction("independent-1-64", 64);
}

#[test]
#[ignore = "one bounded independent-root scaling diagnostic sample"]
fn graph_snapshot_diagnostic_independent_1_32() {
    validate_independent_fraction("independent-1-32", 32);
}

#[test]
#[ignore = "one bounded independent-root scaling diagnostic sample"]
fn graph_snapshot_diagnostic_independent_1_16() {
    validate_independent_fraction("independent-1-16", 16);
}

#[test]
#[ignore = "one bounded independent-root scaling diagnostic sample"]
fn graph_snapshot_diagnostic_independent_1_8() {
    validate_independent_fraction("independent-1-8", 8);
}

#[test]
#[ignore = "quiet-slot only; this constructs the Nmax matrix and near-limit containers"]
#[allow(
    clippy::too_many_lines,
    reason = "one ignored scale scenario records the full declared fixture matrix"
)]
// Quiet slot: cargo test --lib nmax_validation_quiet_slot -- --ignored --nocapture
fn graph_snapshot_nmax_validation_quiet_slot() {
    const TERMINAL_FANOUT_GROUP_SIZE: usize = 556;
    const TERMINAL_CHILDREN_PER_ROOT: usize = 300;

    let template = root_template();
    let nmax = largest_item_count(&template, ForestShape::Independent, 1, 0);
    assert!(nmax > 0, "minimal family fits at least one root");
    let largest = build_document(&template, nmax, ForestShape::Independent, 1, 0, true);
    let next = build_document(&template, nmax + 1, ForestShape::Independent, 1, 0, true);
    let largest_bytes = largest.container_bytes().expect("Nmax bytes");
    let largest_bytes_len = largest_bytes.len();
    let next_bytes = next.container_bytes().expect("Nmax+1 bytes");
    let next_bytes_len = next_bytes.len();
    assert!(largest_bytes_len <= CONTAINER_LIMIT);
    assert!(next_bytes_len > CONTAINER_LIMIT);
    drop(largest_bytes);
    drop(next_bytes);
    drop(next);
    eprintln!(
        "planning_scale family=minimal-valid-independent-open-roots Nmax={nmax} bytes={} Nplus1_bytes={} fingerprint={}",
        largest_bytes_len,
        next_bytes_len,
        largest.manifest.body_sha256.as_str(),
    );

    let quarter = (nmax / 4).max(1);
    let half = (nmax / 2).max(1);
    for (label, count) in [("independent-quarter", quarter), ("independent-half", half)] {
        let document = build_document(&template, count, ForestShape::Independent, 1, 0, true);
        validate_one_sample(label, &document);
    }
    validate_one_sample("independent-Nmax", &largest);

    let mixed_count = largest_item_count(&template, ForestShape::Mixed, 1, 0);
    let mixed = build_document(&template, mixed_count, ForestShape::Mixed, 1, 0, true);
    validate_one_sample("mixed-forest-Nmax", &mixed);

    // Each complete root group has one root, 300 terminal children, and 255
    // open children. A final partial group is retained only when it still has
    // all 300 terminal children, so every root meets both lifecycle bounds.
    let fanout_max_count = largest_item_count(&template, ForestShape::TerminalFanout, 1, 0);
    let complete_groups = fanout_max_count / TERMINAL_FANOUT_GROUP_SIZE;
    let remainder = fanout_max_count % TERMINAL_FANOUT_GROUP_SIZE;
    let fanout_count = complete_groups * TERMINAL_FANOUT_GROUP_SIZE
        + if remainder > TERMINAL_CHILDREN_PER_ROOT {
            remainder
        } else {
            0
        };
    assert!(fanout_count >= TERMINAL_FANOUT_GROUP_SIZE);
    let fanout = build_document(
        &template,
        fanout_count,
        ForestShape::TerminalFanout,
        1,
        0,
        true,
    );
    let mut descendants_per_root = std::collections::HashMap::<WorkId, (usize, usize)>::new();
    for item in fanout
        .body
        .items
        .iter()
        .filter(|item| item.parent_id.is_some())
    {
        match item.lifecycle {
            WorkLifecycle::Cancelled | WorkLifecycle::Superseded => {
                descendants_per_root.entry(item.root_id).or_default().0 += 1;
            }
            WorkLifecycle::Open => descendants_per_root.entry(item.root_id).or_default().1 += 1,
            lifecycle => panic!("unexpected terminal-fanout descendant lifecycle: {lifecycle:?}"),
        }
    }
    let min_terminal_descendants_per_root = descendants_per_root
        .values()
        .map(|(terminal, _)| *terminal)
        .min()
        .unwrap_or_default();
    let max_open_descendants_per_root = descendants_per_root
        .values()
        .map(|(_, open)| *open)
        .max()
        .unwrap_or_default();
    assert!(
        min_terminal_descendants_per_root
            > crate::storage::work::MAX_OPEN_WORK_DESCENDANTS as usize,
        "every terminal-fanout-Nmax root must exceed the terminal-descendant limit"
    );
    assert!(
        max_open_descendants_per_root <= crate::storage::work::MAX_OPEN_WORK_DESCENDANTS as usize,
        "terminal-fanout-Nmax must stay within the open-descendant limit for each root"
    );
    eprintln!(
        "planning_scale family=terminal-fanout-Nmax items={} roots={} min_terminal_descendants_per_root={min_terminal_descendants_per_root} max_open_descendants_per_root={max_open_descendants_per_root}",
        fanout.body.items.len(),
        descendants_per_root.len(),
    );
    validate_one_sample("terminal-fanout-Nmax", &fanout);

    let history_count = largest_item_count(&template, ForestShape::Chained, 4, 4);
    let history = build_document(&template, history_count, ForestShape::Chained, 4, 4, true);
    validate_one_sample("concentrated-history-relation-Nmax", &history);

    let blocker_count = largest_blocker_item_count(&template, 4);
    let blocker_roots = build_document(
        &template,
        blocker_count,
        ForestShape::Independent,
        1,
        0,
        false,
    );
    let blockers = add_blockers(blocker_roots, 4, true);
    validate_one_sample("independent-blockers-Nmax", &blockers);
}
