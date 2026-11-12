//! Real canonical history at the aggregate discovery boundaries.

use super::super::super::super::{MAX_BYTES, MAX_RECORD_BYTES, MAX_RECORDS, view};
use super::*;
use crate::task_sync::evidence::limits::{self, Dimension, LimitKind, Oversize};
use crate::task_sync::ledger::{BINDING_KIND, EvidenceStop};
use rusqlite::params;

fn totals(connection: &rusqlite::Connection) -> (usize, usize) {
    let (count, bytes): (i64, i64) = connection
        .query_row(
            "SELECT count(*),coalesce(sum(length(canonical_json)),0) FROM objects
             WHERE object_kind IN (?1,?2)",
            params![BINDING_KIND, EVENT_KIND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    (
        usize::try_from(count).unwrap(),
        usize::try_from(bytes).unwrap(),
    )
}

fn insert(
    connection: &rusqlite::Connection,
    kind: &str,
    value: &impl serde::Serialize,
) -> ObjectId {
    let object = CanonicalObject::mint(value).unwrap();
    assert!(object.bytes().len() <= MAX_RECORD_BYTES);
    SqliteStore::insert_object(connection, kind, &object).unwrap();
    object.key().clone()
}

fn filler(input: &InspectedEvidence) -> Binding {
    let mut binding = input.binding.clone();
    binding.project = crate::ProjectId(format!("capacity-{}", uuid::Uuid::new_v4()));
    binding.repository.enrollment = uuid::Uuid::new_v4();
    binding.store.origin = Some(ObjectId::mint());
    binding.store.cut = crate::task_sync::progress::ExportCut {
        position: 0,
        anchor: None,
    };
    binding.validate().unwrap();
    binding
}

fn activate(connection: &rusqlite::Connection, binding: &Binding) -> (ObjectId, ObjectId) {
    let id = insert(connection, BINDING_KIND, binding);
    let event = Event {
        schema_version: 1,
        binding: id.clone(),
        sequence: 1,
        predecessor: None,
        actor: binding.actor.clone(),
        recorded_at: at(1),
        transition: Transition::Activated,
    };
    let tail = insert(connection, EVENT_KIND, &event);
    (id, tail)
}

fn seed_count(store: &mut SqliteStore, input: &InspectedEvidence, target: usize) {
    let transaction = store.connection.transaction().unwrap();
    let binding = filler(input);
    let (id, mut tail) = activate(&transaction, &binding);
    let mut sequence = 1;
    let mut count = totals(&transaction).0;
    while count < target {
        let mut intent = input.recipe.intent().unwrap().clone();
        intent.operation = uuid::Uuid::new_v4();
        intent.key = intent.operation.to_string();
        intent.local_cut.binding = id.clone();
        intent.local_cut.origin = binding.store.origin.clone().unwrap();
        intent.local_cut.position.feed = FeedId::Project(binding.project.clone());
        intent.local_cut.position.position = 0;
        intent.local_cut.anchor = None;
        intent.validate(&id, &binding).unwrap();
        sequence += 1;
        let prepared = Event {
            schema_version: 1,
            binding: id.clone(),
            sequence,
            predecessor: Some(tail),
            actor: binding.actor.clone(),
            recorded_at: at(2),
            transition: Transition::EvidencePrepared {
                intent: Box::new(intent),
            },
        };
        tail = insert(&transaction, EVENT_KIND, &prepared);
        count += 1;
        if count == target {
            break;
        }
        sequence += 1;
        let stopped = Event {
            sequence,
            predecessor: Some(tail.clone()),
            transition: Transition::EvidenceStopped {
                prepared: tail,
                outcome: EvidenceStop::Refused {
                    reason: EvidenceRefusal::InvalidPayload,
                },
            },
            ..prepared
        };
        tail = insert(&transaction, EVENT_KIND, &stopped);
        count += 1;
    }
    // Derive all projections once from this genuine, validated event history.
    view::rebuild(&transaction).unwrap();
    transaction.commit().unwrap();
    assert_eq!(totals(&store.connection).0, target);
    Ledger::load(&store.connection).unwrap();
}

fn seed_bytes(store: &mut SqliteStore, input: &InspectedEvidence, target: usize) {
    let transaction = store.connection.transaction().unwrap();
    while totals(&transaction).1 < target {
        let mut binding = filler(input);
        let base = CanonicalObject::freeze(&binding).unwrap().bytes().len();
        let activated = Event {
            schema_version: 1,
            binding: ObjectId::mint(),
            sequence: 1,
            predecessor: None,
            actor: binding.actor.clone(),
            recorded_at: at(1),
            transition: Transition::Activated,
        };
        let event_bytes = CanonicalObject::freeze(&activated).unwrap().bytes().len();
        let remaining = target - totals(&transaction).1;
        assert!(remaining >= base + event_bytes);
        // Path text is canonical data only: it is absolute and never opened.
        // UTF-8 padding ensures the boundary counts bytes, not characters.
        let max = MAX_RECORD_BYTES;
        let padding = (remaining - base - event_bytes).min(max - base - 4096);
        let text = format!("{}{}", "é".repeat(padding / 2), "x".repeat(padding % 2));
        binding.store.database =
            std::path::PathBuf::from(format!("{}{text}", binding.store.database.display()));
        assert_eq!(
            CanonicalObject::freeze(&binding).unwrap().bytes().len(),
            base + padding
        );
        binding.validate().unwrap();
        activate(&transaction, &binding);
    }
    view::rebuild(&transaction).unwrap();
    transaction.commit().unwrap();
    assert_eq!(totals(&store.connection).1, target);
    Ledger::load(&store.connection).unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Applied,
    ExplicitStop,
    PayloadStop,
}

impl Outcome {
    fn event(self, input: &InspectedEvidence) -> Event {
        match self {
            Self::Applied => input.recipe.outcome.clone(),
            Self::ExplicitStop => input.recipe.stopped_event(Self::stop()).unwrap(),
            Self::PayloadStop => input
                .recipe
                .stopped_event(EvidenceStop::Refused {
                    reason: EvidenceRefusal::RecordConflict,
                })
                .unwrap(),
        }
    }

    fn stop() -> EvidenceStop {
        EvidenceStop::Oversize {
            detail: Oversize {
                kind: LimitKind::ComponentTooLarge,
                subject: None,
                dimension: Dimension::Bytes,
                limit: limits::MAX_COMPONENT_BYTES,
                actual: limits::MAX_COMPONENT_BYTES + 1,
            },
        }
    }

    fn apply(
        self,
        store: &mut SqliteStore,
        input: &InspectedEvidence,
    ) -> Result<crate::task_sync::ledger::CompletedExchange, StoreError> {
        if matches!(self, Self::ExplicitStop) {
            store.stop_task_sync_evidence_import(input, &Self::stop())
        } else {
            store.apply_task_sync_evidence_import(input)
        }
    }
}

fn collision(store: &SqliteStore, input: &InspectedEvidence) {
    let original = &input.packs[0].records()[0];
    SqliteStore::insert_object(
        &store.connection,
        "fixture_native_original",
        &CanonicalObject::identified(original.object_id(), &"conflicting native bytes").unwrap(),
    )
    .unwrap();
}

fn assert_result(
    store: &mut SqliteStore,
    input: &InspectedEvidence,
    outcome: Outcome,
    over: usize,
) {
    let before = state(store);
    let result = outcome.apply(store, input);
    if over == 0 {
        let result = result.unwrap();
        assert_eq!(result.event, outcome.event(input));
        assert_eq!(result.prepared, input.recipe.prepared_id);
        assert_eq!(result.outcome, input.recipe.outcome_id);
        // Exhaustive discovery still admits the now-full ledger.
        Ledger::load(&store.connection).unwrap();
        let completed = state(store);
        // Real public completed replay adds no objects and needs no free slot.
        assert_eq!(
            store.apply_task_sync_evidence_import(input).unwrap(),
            result
        );
        assert_eq!(state(store), completed);
        let packs: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM work_task_sync_evidence_packs",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(packs, i64::from(matches!(outcome, Outcome::Applied)));
    } else {
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("capacity exceeded"),
            "{outcome:?}: {error}"
        );
        assert_eq!(
            state(store),
            before,
            "{outcome:?} must roll back all SQL effects"
        );
        assert!(
            store
                .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
                .unwrap()
                .is_none()
        );
        Ledger::load(&store.connection).unwrap();
    }
}

#[test]
fn evidence_import_capacity_counts_both_events_and_replays_at_the_record_limit() {
    for over in [0, 1] {
        let (_home, mut store, binding, _) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let input = retained_input(&mut store, &binding, &id);
        let limit = MAX_RECORDS;
        seed_count(&mut store, &input, limit - 2 + over);
        let bytes = totals(&store.connection).1;
        assert_result(&mut store, &input, Outcome::Applied, over);
        assert_eq!(
            totals(&store.connection).0,
            if over == 0 { limit } else { limit - 1 }
        );
        assert!(bytes < MAX_BYTES);
    }
}

#[test]
fn evidence_import_capacity_uses_actual_success_and_stopped_bytes_atomically() {
    for outcome in [
        Outcome::Applied,
        Outcome::ExplicitStop,
        Outcome::PayloadStop,
    ] {
        for over in [0, 1] {
            let (_home, mut store, binding, _) = initial_fixture();
            let id = store.activate_task_sync_binding(&binding).unwrap();
            let input = retained_input(&mut store, &binding, &id);
            if matches!(outcome, Outcome::PayloadStop) {
                collision(&store, &input);
            }
            let prepared = CanonicalObject::freeze(&input.recipe.prepared)
                .unwrap()
                .bytes()
                .len();
            let terminal = CanonicalObject::freeze(&outcome.event(&input))
                .unwrap()
                .bytes()
                .len();
            let limit = MAX_BYTES;
            let target = limit - prepared - terminal + over;
            seed_bytes(&mut store, &input, target);
            let count = totals(&store.connection).0;
            assert_result(&mut store, &input, outcome, over);
            assert_eq!(
                totals(&store.connection),
                if over == 0 {
                    (count + 2, limit)
                } else {
                    (count, target)
                }
            );
        }
    }
}
