use super::*;
use crate::domain::{ChildRequirement, CompletionSeal, DecomposeWorkRequest, ReopenWorkRequest};

// Defined before measurement: only these decimal widths may explain history-
// dependent encoding growth. Event: revision + work.revision (2 positive i64,
// <=38 bytes). Delta: sequence (u64, <=20), previous_revision and header.revision
// (positive i64, <=19 each): <=58 bytes. Each written projection header has
// one revision (<=19). No member, text, hash or other field is subtracted.
// These are logical canonical/SQL payload bytes, not SQLite pages, WAL or I/O.
// The limits are not slack: subtract the actual named widths, then require
// exact equality. Even one unexplained payload byte must fail the comparison.
const PROPERTY: &str = "BRAK ZALEŻNOŚCI PAYLOADU OD NIEZMIENIONEJ HISTORII, POZA NAZWANYM I OGRANICZONYM NARZUTEM METADANYCH";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Payload {
    bytes: i64,
    metadata: i64,
    count: i64,
}

impl Payload {
    fn since(self, before: Self) -> Self {
        Self {
            bytes: self.bytes - before.bytes,
            metadata: self.metadata - before.metadata,
            count: self.count - before.count,
        }
    }

    fn without_metadata(self, maximum_per_row: i64) -> i64 {
        assert!(self.count > 0, "{self:?}");
        assert!((0..=maximum_per_row * self.count).contains(&self.metadata));
        assert!(self.bytes > self.metadata, "{self:?}");
        self.bytes - self.metadata
    }
}

fn canonical_payload(store: &SqliteStore, kind: &str) -> Payload {
    store.connection.query_row(
        "SELECT COALESCE(SUM(length(canonical_json)), 0),
         COALESCE(SUM(CASE object_kind WHEN 'work_event' THEN
             length(CAST(json_extract(canonical_json, '$.revision') AS TEXT)) +
             length(CAST(json_extract(canonical_json, '$.work.revision') AS TEXT))
         ELSE
             length(CAST(json_extract(canonical_json, '$.sequence') AS TEXT)) +
             COALESCE(length(CAST(json_extract(canonical_json, '$.previous_revision') AS TEXT)), 0) +
             length(CAST(json_extract(canonical_json, '$.header.revision') AS TEXT))
         END), 0), COUNT(*) FROM objects WHERE object_kind = ?1",
        [kind],
        |row| Ok(Payload { bytes: row.get(0)?, metadata: row.get(1)?, count: row.get(2)? }),
    ).unwrap()
}

fn projection_payload(store: &SqliteStore) -> Payload {
    let (metadata, count) = store
        .connection
        .query_row(
            "SELECT COALESCE(SUM(bytes), 0), COUNT(*) FROM reopen_header_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    Payload {
        bytes: written(store),
        metadata,
        count,
    }
}

fn install_probe(store: &SqliteStore) {
    install_write_probe(store);
    store
        .connection
        .execute_batch(
            "CREATE TEMP TABLE reopen_header_metadata (bytes INTEGER NOT NULL);
         CREATE TEMP TRIGGER reopen_header_insert AFTER INSERT ON main.work_root_executions BEGIN
             INSERT INTO reopen_header_metadata VALUES(length(CAST(NEW.revision AS TEXT))); END;
         CREATE TEMP TRIGGER reopen_header_update AFTER UPDATE ON main.work_root_executions BEGIN
             INSERT INTO reopen_header_metadata VALUES(length(CAST(NEW.revision AS TEXT))); END;",
        )
        .unwrap();
}

fn sample(store: &SqliteStore) -> [Payload; 3] {
    [
        canonical_payload(store, "work_event"),
        canonical_payload(store, KIND),
        projection_payload(store),
    ]
}

fn reopen(store: &mut SqliteStore, item: &crate::WorkItem, time: i64) -> crate::WorkRun {
    let current = store.get_work_item(item.work_id).unwrap();
    store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: item.work_id,
                expected_work_revision: current.revision,
                reason: "repeat the same work".into(),
                actor: actor("holder"),
                idempotency_key: format!("reopen-{time}"),
                reopened_at: at(time),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

fn finish_child(store: &mut SqliteStore, item: &crate::WorkItem, time: i64) -> CompletionSeal {
    let item = store.get_work_item(item.work_id).unwrap();
    let held = claim(store, &item, "child", &format!("claim-{time}"), time, 3600);
    let item = store.get_work_item(item.work_id).unwrap();
    let proof = evidence(
        store,
        &item,
        &held,
        "child",
        &format!("proof-{time}"),
        time + 1,
    );
    checkpoint(
        store,
        &item,
        &held,
        "child",
        &format!("cp-{time}"),
        time + 2,
        std::slice::from_ref(&proof),
    );
    complete(
        store,
        &item,
        &held,
        "child",
        &proof,
        &format!("done-{time}"),
        time + 3,
    )
    .unwrap()
}

fn change_shape(before: &RootExecution, after: &RootExecution) -> (usize, usize, usize, usize) {
    let before = members(before).unwrap();
    let after = members(after).unwrap();
    let removed: Vec<_> = before
        .iter()
        .filter(|(hash, _)| !after.contains_key(*hash))
        .map(|(_, member)| member)
        .collect();
    let added: Vec<_> = after
        .iter()
        .filter(|(hash, _)| !before.contains_key(*hash))
        .map(|(_, member)| member)
        .collect();
    assert!(
        removed
            .iter()
            .all(|member| matches!(member, RootExecutionMember::ChildSeal(_)))
    );
    assert!(
        added
            .iter()
            .all(|member| matches!(member, RootExecutionMember::Run(_)))
    );
    (
        removed.len(),
        removed
            .iter()
            .map(|m| CanonicalObject::freeze(m).unwrap().bytes().len())
            .sum(),
        added.len(),
        added
            .iter()
            .map(|m| CanonicalObject::freeze(m).unwrap().bytes().len())
            .sum(),
    )
}

struct Ready {
    store: SqliteStore,
    root: crate::WorkItem,
    child: crate::WorkItem,
    held: crate::WorkClaim,
    proof: ObjectHash,
    child_seal: CompletionSeal,
    id: RootExecutionId,
}

fn ready(prior: u32) -> Ready {
    let (mut store, root, id) = fixture();
    let plan = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child(
                    "required",
                    ChildRequirement::Required,
                    "Required child",
                )],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "plan".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let root = plan.parent;
    let child = plan.children[0].clone();
    let held = claim(&mut store, &root, "holder", "root-claim", 2, 3600);
    let root = store.get_work_item(root.work_id).unwrap();
    let proof = evidence(&mut store, &root, &held, "holder", "root-proof", 3);
    for index in 0..prior {
        checkpoint(
            &mut store,
            &root,
            &held,
            "holder",
            &format!("root-cp-{index}"),
            4 + i64::from(index),
            std::slice::from_ref(&proof),
        );
    }
    let child_seal = finish_child(&mut store, &child, 100);
    Ready {
        store,
        root,
        child,
        held,
        proof,
        child_seal,
        id,
    }
}

fn measure_new_generation(
    store: &mut SqliteStore,
    root: &crate::WorkItem,
    held: &crate::WorkClaim,
    proof: &ObjectHash,
    id: RootExecutionId,
    prior: u32,
) -> [Payload; 3] {
    let root_seal = complete(store, root, held, "holder", proof, "root-done", 210).unwrap();
    let root_seal_hash = CanonicalObject::freeze(&root_seal).unwrap().hash().clone();
    let sealed = store.completion_root_execution(&root_seal_hash).unwrap();
    let old_generation = projected(&store.connection, id).unwrap();
    let before = sample(store);
    let new_run = reopen(store, root, 220);
    let after = sample(store);
    let bytes = std::array::from_fn::<_, 3, _>(|i| after[i].since(before[i]));
    eprintln!(
        "root_reopen sample=unverified phase=generation prior={prior} event={:?} delta={:?} projection={:?}",
        bytes[0], bytes[1], bytes[2]
    );
    let (new_generation, _) = projected(&store.connection, new_run.root_execution_id).unwrap();
    assert_eq!(projected(&store.connection, id).unwrap(), old_generation);
    assert_ne!(new_run.root_execution_id, id);
    assert_eq!(new_generation.generation, old_generation.0.generation + 1);
    let new_members = members(&new_generation).unwrap();
    assert_eq!(new_members.len(), 1);
    assert_eq!(new_generation.run_ids, vec![new_run.run_id]);
    assert_eq!(
        store.completion_root_execution(&root_seal_hash).unwrap(),
        sealed
    );
    bytes
}

#[test]
fn root_delta_reopen_payload_depends_on_change_not_unchanged_history() {
    eprintln!("root_reopen verdict=PENDING; samples are unverified until final verdict=PASS");
    let mut results = Vec::new();
    // Keep the equal-width pair and cross a decimal-width boundary separately.
    for prior in [10, 20, 90] {
        let Ready {
            mut store,
            root,
            child,
            held,
            proof,
            child_seal,
            id,
        } = ready(prior);
        let seal_hash = CanonicalObject::freeze(&child_seal).unwrap().hash().clone();
        let historical = store.completion_root_execution(&seal_hash).unwrap();
        let (before_root, before_ref) = projected(&store.connection, id).unwrap();
        let depth = load_head(&store.connection, &before_ref).unwrap().sequence;
        let root_size = CanonicalObject::freeze(&before_root).unwrap().bytes().len();
        install_probe(&store);
        let before = sample(&store);
        let reopened = reopen(&mut store, &child, 200);
        let after = sample(&store);
        let child_bytes = std::array::from_fn::<_, 3, _>(|i| after[i].since(before[i]));
        eprintln!(
            "root_reopen sample=unverified phase=child prior={prior} depth={depth} root_bytes={root_size} event={:?} delta={:?} projection={:?}",
            child_bytes[0], child_bytes[1], child_bytes[2]
        );
        let (after_root, _) = projected(&store.connection, id).unwrap();
        assert_eq!(reopened.root_execution_id, id);
        assert_eq!(after_root.generation, before_root.generation);
        let changed = change_shape(&before_root, &after_root);
        assert_eq!((changed.0, changed.2), (1, 1));
        assert_eq!(
            store.completion_root_execution(&seal_hash).unwrap(),
            historical
        );

        finish_child(&mut store, &child, 201);
        let root_bytes = measure_new_generation(&mut store, &root, &held, &proof, id, prior);
        assert!(store.verify_all().unwrap().is_healthy());
        for bytes in [child_bytes, root_bytes] {
            assert_eq!(bytes[0].count, 1);
            assert_eq!(bytes[1].count, 2);
            assert_eq!(bytes[2].count, 2);
        }
        let normalized = [child_bytes, root_bytes].map(|bytes| {
            [
                bytes[0].without_metadata(38),
                bytes[1].without_metadata(58),
                bytes[2].without_metadata(19),
            ]
        });
        results.push((depth, root_size, changed, normalized, child_bytes));
    }
    for pair in results.windows(2) {
        assert!(pair[1].0 > pair[0].0);
        assert!(pair[1].1 > pair[0].1);
        assert_eq!(
            pair[0].2, pair[1].2,
            "changed member counts AND byte lengths"
        );
        assert_eq!(pair[0].3, pair[1].3, "{PROPERTY}");
    }
    assert!(
        results[2].4[1].metadata > results[0].4[1].metadata,
        "the third sample must exercise different delta metadata widths"
    );
    eprintln!(
        "root_reopen verdict=PASS normalized={:?}; {PROPERTY}",
        results[0].3
    );
}
