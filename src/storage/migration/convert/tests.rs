use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::{CanonicalObject, CompletionSeal, ObjectHash};

use super::convert_aggregate_store_objects;

pub(in crate::storage::migration) fn target() -> Connection {
    let db = Connection::open_in_memory().expect("empty canonical phase target");
    db.execute_batch("CREATE TABLE objects(object_hash TEXT PRIMARY KEY,object_kind TEXT,canonical_json BLOB,created_at TEXT);").expect("object schema");
    db.execute_batch(super::super::MIGRATION_SCHEMA)
        .expect("migration provenance schema");
    db
}

fn read<T: serde::de::DeserializeOwned>(db: &Connection, hash: &str) -> T {
    let bytes = db
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash=?1",
            [hash],
            |row| row.get(0),
        )
        .expect("converted object");
    CanonicalObject::verify(&hash.parse().expect("address"), bytes)
        .expect("canonical bytes")
        .decode()
        .expect("typed value")
}

fn put(connection: &Connection, kind: &str, value: &Value) -> ObjectHash {
    let object = CanonicalObject::freeze(value).expect("canonical fixture");
    connection.execute("INSERT INTO objects(object_hash,object_kind,canonical_json,created_at) VALUES (?1,?2,?3,'original timestamp')",
        params![object.hash().as_str(),kind,object.bytes()]).expect("source object");
    object.hash().clone()
}

#[test]
fn migration_conversion_preserves_historical_restore_field_omissions() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let path = directory.path().join("source.db");
    let (source, seal, _, _) = source_profile(&path, true);
    let mut target = target();
    let report = convert_aggregate_store_objects(&path, &mut target).expect("known omission shape");
    assert_eq!(report.changed_objects, 4);
    let mut q = source.prepare("SELECT object_hash,object_kind,canonical_json FROM objects WHERE object_kind IN ('work_event','completion_seal')").expect("source");
    let mut rows = q.query([]).expect("rows");
    while let Some(row) = rows.next().expect("row") {
        let hash: String = row.get(0).expect("hash");
        let kind: String = row.get(1).expect("kind");
        let original: Vec<u8> = row.get(2).expect("bytes");
        let mapped: String = target
            .query_row(
                "SELECT target_hash FROM migration_object_map WHERE source_hash=?1",
                [&hash],
                |row| row.get(0),
            )
            .expect("total mapping");
        let converted: Value = read(&target, &mapped);
        let source_value: Value = serde_json::from_slice(&original).expect("value");
        let retained: Vec<u8> = target
            .query_row(
                "SELECT canonical_json FROM migration_original_objects WHERE object_hash=?1",
                [&hash],
                |row| row.get(0),
            )
            .expect("retained");
        assert_eq!(retained, original);
        if kind == "work_event" {
            assert!(converted["work"].get("restored").is_none());
            if source_value["root_execution"].is_null() {
                assert_eq!(hash, mapped);
            }
        } else {
            assert_eq!(hash, seal.as_str());
            assert!(converted.get("restored").is_none());
            assert!(converted.get("restored_child_completions").is_none());
            let current: CompletionSeal =
                serde_json::from_value(converted).expect("current reader");
            assert!(!current.restored);
            assert!(current.restored_child_completions.is_empty());
        }
    }
}

// A typed canonical-phase fixture, not a claim that it passes full store doctor.
pub(in crate::storage::migration) fn source(
    path: &std::path::Path,
) -> (Connection, ObjectHash, ObjectHash, ObjectHash) {
    source_profile(path, false)
}

fn source_profile(
    path: &std::path::Path,
    omitted_restore_fields: bool,
) -> (Connection, ObjectHash, ObjectHash, ObjectHash) {
    let db = Connection::open(path).expect("source");
    db.execute_batch("CREATE TABLE objects(object_hash TEXT PRIMARY KEY,object_kind TEXT,canonical_json BLOB,created_at TEXT);
        CREATE TABLE work_feed_entries(feed_kind TEXT,feed_id TEXT,position INTEGER,object_kind TEXT,object_hash TEXT);
        CREATE TABLE work_root_executions(root_execution_id TEXT PRIMARY KEY,execution_json BLOB);").expect("phase fixture");
    let marker = put(&db, "fixture_marker", &json!({"immutable":"body"}));
    let work = uuid::Uuid::new_v4();
    let execution = uuid::Uuid::new_v4();
    let run = uuid::Uuid::new_v4();
    let actor = json!({"actor_id":"author","actor_kind":"test_agent","assurance":"asserted","run_id":null,
        "session_id":"s","source_tool":null,"source_skill":null,"provenance_chain":[],"reason":marker});
    let mut item = json!({"schema_version":1,"project_id":"p","work_id":work,"short_ref":"w-fixture","root_id":work,
        "parent_id":null,"child_requirement":"required","title":"title","outcome":"outcome","acceptance":["criterion"],
        "kind":"task","priority":1,"labels":[],"assigned_to":null,"deferred_until":null,"origin":"local",
        "source_snapshot_id":null,"lifecycle":"open","revision":1,"active_run_id":run,"restored":false,
        "superseded_by":null,"created_by":actor,"created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:00:00Z"});
    if omitted_restore_fields {
        item.as_object_mut().expect("item").remove("restored");
    }
    let waivers = json!([{"participant":"other participant","waived_by":"operator","reason":"retained accounting, not discarded prose"}]);
    let root = json!({"schema_version":1,"root_execution_id":execution,"project_id":"p","root_id":work,"generation":1,
        "state":"active","revision":1,"run_ids":[run],"required_child_seals":[],"required_child_waivers":[],
        "expected_contributors":["s"],"contributions":[],"waivers":waivers,"created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:00:00Z"});
    let event = |root: Value, transition: Value| {
        let completed = transition["kind"] == "completed";
        let mut work_item = item.clone();
        if completed {
            work_item["revision"] = json!(2);
            work_item["lifecycle"] = json!("completed");
            work_item["updated_at"] = json!("2026-09-08T00:01:00Z");
        }
        let run_projection = if completed {
            json!({"schema_version":1,"run_id":run,"root_execution_id":execution,
            "work_id":work,"generation":1,"executor":"s","state":"completed","revision":2,
            "last_checkpoint":null,"completion_seal":transition["seal"],
            "created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:01:00Z"})
        } else {
            Value::Null
        };
        json!({"schema_version":1,"project_id":"p","root_id":work,"work_id":work,
        "run_id":run,"revision":if completed {2} else {1},"work":work_item,"run":run_projection,"root_execution":root,"claim":null,"handoff_offer":null,
        "blocker":null,"relation_fingerprint":marker,"transition":transition,"actor":actor,"created_at":if completed {"2026-09-08T00:01:00Z"} else {"2026-09-08T00:00:00Z"}})
    };
    let initial = put(
        &db,
        "work_event",
        &event(Value::Null, json!({"kind":"created","prerequisites":[]})),
    );
    let previous = put(
        &db,
        "work_event",
        &event(
            root.clone(),
            json!({"kind":"checkpointed","checkpoint":marker}),
        ),
    );
    let mut seal = json!({"schema_version":1,"work_id":work,"root_id":work,"root_execution_id":execution,"run_id":run,
        "run_generation":1,"accepted_work_revision":1,"accepted_work_revision_hash":marker,"claim_id":uuid::Uuid::new_v4(),
        "claim_fence":1,"completion_cut":{"feed":{"kind":"run_execution","id":run},"position":1},"checkpoint":null,
        "evidence":[],"acceptance":[],"obligation_schema_version":1,"environment_schema_version":1,
        "required_child_seals":[],"required_child_waivers":[],"restored_child_completions":[],"restored":false,
        "unfinished_optional_children":[],"drain":{"reconciled_action_outcomes":[],"released_resource_leases":[]},
        "actor":actor,"completed_at":"2026-09-08T00:01:00Z","expected_contributors":["s"],"contributions":[],"waivers":waivers});
    if omitted_restore_fields {
        let fields = seal.as_object_mut().expect("seal");
        fields.remove("restored");
        fields.remove("restored_child_completions");
    }
    let seal = put(&db, "completion_seal", &seal);
    let mut post = root;
    post["revision"] = json!(2);
    post["state"] = json!("completed");
    post["updated_at"] = json!("2026-09-08T00:01:00Z");
    let completed = put(
        &db,
        "work_event",
        &event(post.clone(), json!({"kind":"completed","seal":seal})),
    );
    let observation = put(
        &db,
        "work_observation",
        &json!({"schema_version":1,"project_id":"p","root_id":work,"work_id":work,
        "work_revision":1,"basis":{"kind":"native_event","event":previous},"sequence":1,"summary":previous,"refs":[previous],
        "actor":actor,"created_at":"2026-09-08T00:00:30Z"}),
    );
    put(
        &db,
        "work_protocol_result",
        &json!({"seal":seal,"receipt":{"result":observation}}),
    );
    for (position, hash) in [initial, previous, completed].iter().enumerate() {
        db.execute(
            "INSERT INTO work_feed_entries VALUES ('project','p',?1,'work_event',?2)",
            params![
                i64::try_from(position + 1).expect("position"),
                hash.as_str()
            ],
        )
        .expect("feed");
    }
    db.execute(
        "INSERT INTO work_root_executions VALUES (?1,?2)",
        params![
            execution.to_string(),
            serde_json::to_vec(&post).expect("root")
        ],
    )
    .expect("projection");
    (db, seal, observation, marker)
}

#[test]
fn migration_conversion_retains_every_original_and_identity_mapping_without_rewriting_replay() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let path = directory.path().join("source.db");
    let (source, seal, observation, _) = source(&path);
    let mut target = target();
    let counts =
        convert_aggregate_store_objects(&path, &mut target).expect("convert canonical phase");
    assert_eq!(counts.original_objects, 7);
    assert_eq!(counts.changed_objects, 4);
    assert_eq!(counts.generated_root_deltas, 3);
    let rows: Vec<(String,String,Vec<u8>,String,i64)> = source.prepare("SELECT object_hash,object_kind,canonical_json,created_at,rowid FROM objects ORDER BY rowid").expect("query")
        .query_map([],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).expect("rows").collect::<Result<_,_>>().expect("all originals");
    for (hash, kind, bytes, created, rowid) in rows {
        let retained: (String,Vec<u8>,String,i64) = target.query_row("SELECT object_kind,canonical_json,created_at,source_rowid FROM migration_original_objects WHERE object_hash=?1",[&hash],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).expect("original");
        assert_eq!(retained, (kind.clone(), bytes, created, rowid));
        let mapped: String = target
            .query_row(
                "SELECT target_hash FROM migration_object_map WHERE source_hash=?1",
                [&hash],
                |row| row.get(0),
            )
            .expect("total map");
        if kind == "work_protocol_result" {
            assert_eq!(mapped, hash, "exact replay bytes stay immutable");
        }
    }
    let mapped: String = target
        .query_row(
            "SELECT target_hash FROM migration_object_map WHERE source_hash=?1",
            [seal.as_str()],
            |row| row.get(0),
        )
        .expect("seal mapping");
    let converted: CompletionSeal = read(&target, &mapped);
    assert!(!converted.root_execution.head.as_str().is_empty());
    let head: crate::domain::RootExecutionDelta =
        read(&target, converted.root_execution.head.as_str());
    let original: Value = read(&source, seal.as_str());
    // All references carried by this seal point at the stable fixture marker.
    // Compare every remaining field, not merely deserialization or the hash of
    // another value produced by the converter being tested.
    let mut expected = original.clone();
    let fields = expected.as_object_mut().expect("original seal");
    for name in ["expected_contributors", "contributions", "waivers"] {
        fields.remove(name);
    }
    fields.insert(
        "root_execution".into(),
        serde_json::to_value(&converted.root_execution).expect("addressed predecessor"),
    );
    let actual: Value = read(&target, &mapped);
    assert_eq!(actual, expected, "only the declared representation changes");
    let carried: Vec<_> = head
        .added
        .iter()
        .filter_map(|member| match member {
            crate::domain::RootExecutionMember::Waiver(waiver) => {
                Some(serde_json::to_value(waiver).expect("waiver"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(Value::Array(carried), original["waivers"]);
    let contributors: Vec<_> = head
        .added
        .iter()
        .filter_map(|member| match member {
            crate::domain::RootExecutionMember::Contributor(session) => {
                Some(serde_json::to_value(session).expect("session"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        Value::Array(contributors),
        original["expected_contributors"]
    );
    assert_eq!(head.header.state, crate::RootExecutionState::Active);
    let mapped: String = target
        .query_row(
            "SELECT target_hash FROM migration_object_map WHERE source_hash=?1",
            [observation.as_str()],
            |row| row.get(0),
        )
        .expect("observation mapping");
    let converted: Value = read(&target, &mapped);
    assert_ne!(converted["basis"]["event"], converted["summary"]);
    assert_eq!(converted["summary"], converted["refs"][0]);
}

#[test]
fn migration_conversion_missing_reference_rolls_back_originals_and_outputs() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let path = directory.path().join("source.db");
    let (source, _, _, marker) = source(&path);
    source
        .execute(
            "DELETE FROM objects WHERE object_hash=?1",
            [marker.as_str()],
        )
        .expect("missing dependency");
    let mut target = target();
    let before = crate::storage::test_database_shape_snapshot(&target).expect("before");
    let error =
        convert_aggregate_store_objects(&path, &mut target).expect_err("missing canonical mapping");
    assert!(
        error
            .to_string()
            .contains("missing or non-causal canonical mapping"),
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&target).expect("after"),
        before
    );
}

#[test]
fn migration_converted_seal_resolves_through_the_production_root_reader() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let source_path = directory.path().join("source.db");
    let (_, seal, _, _) = source(&source_path);
    let mut converted = target();
    convert_aggregate_store_objects(&source_path, &mut converted).expect("convert");
    let mapped: String = converted
        .query_row(
            "SELECT target_hash FROM migration_object_map WHERE source_hash=?1",
            [seal.as_str()],
            |row| row.get(0),
        )
        .expect("seal map");
    let phase_path = directory.path().join("phase.db");
    converted
        .execute("VACUUM INTO ?1", [phase_path.to_str().expect("path")])
        .expect("phase copy");
    let store = crate::SqliteStore::open_in_memory().expect("production reader fixture");
    store
        .connection
        .execute(
            "ATTACH DATABASE ?1 AS converted",
            [phase_path.to_str().expect("path")],
        )
        .expect("attach fixture");
    store
        .connection
        .execute_batch(
            "INSERT INTO objects SELECT * FROM converted.objects; DETACH DATABASE converted;",
        )
        .expect("canonical objects");
    let event: String = store.connection.query_row("SELECT object_hash FROM objects WHERE object_kind='work_event' AND json_extract(canonical_json,'$.transition.kind')='completed'", [], |row|row.get(0)).expect("completion event");
    let current: CompletionSeal = read(&store.connection, &mapped);
    let completed: crate::domain::WorkEvent = read(&store.connection, &event);
    store.connection.execute("INSERT INTO work_items(work_id,project_id,short_ref,root_id,child_requirement,lifecycle,priority,revision,created_at_ms,updated_at_ms,item_json) VALUES (?1,?2,?3,?1,'required','completed',1,2,?4,?5,?6)", params![current.work_id.0.to_string(),completed.project_id.0,completed.work.short_ref,completed.work.created_at.timestamp_millis(),completed.work.updated_at.timestamp_millis(),serde_json::to_vec(&completed.work).expect("item")]).expect("native item binding");
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads(feed_kind,feed_id,position) VALUES ('root_work',?1,1)",
            [current.root_id.0.to_string()],
        )
        .expect("feed head");
    store.connection.execute("INSERT INTO work_feed_entries(feed_kind,feed_id,position,object_hash,object_kind,work_id) VALUES ('root_work',?1,1,?2,'work_event',?3)", params![current.root_id.0.to_string(),event,current.work_id.0.to_string()]).expect("native feed binding");
    let result = store
        .completion_root_execution(&mapped.parse().expect("hash"))
        .expect("actual native kind and predecessor checks");
    assert_eq!(result.revision, 1);
    assert_eq!(result.state, crate::RootExecutionState::Active);
    assert_eq!(result.waivers.len(), 1);
    assert_eq!(
        result.waivers[0].reason,
        "retained accounting, not discarded prose"
    );
}
