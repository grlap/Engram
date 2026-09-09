use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::{CanonicalObject, ObjectHash};

use super::super::inspect_pre_seal_history;

fn insert(connection: &Connection, kind: &str, value: &Value) -> ObjectHash {
    let object = CanonicalObject::freeze(value).expect("canonical fixture");
    connection
        .execute(
            "INSERT INTO objects VALUES (?1, ?2, ?3)",
            params![object.hash().as_str(), kind, object.bytes()],
        )
        .expect("object");
    object.hash().clone()
}

#[test]
fn migration_predecessor_uses_exact_previous_root_event_not_accounting_or_run_match() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let path = directory.path().join("source.db");
    let connection = Connection::open(&path).expect("source");
    connection.execute_batch("CREATE TABLE objects(object_hash TEXT PRIMARY KEY, object_kind TEXT, canonical_json BLOB);
        CREATE TABLE work_feed_entries(feed_kind TEXT, feed_id TEXT, position INTEGER, object_kind TEXT, object_hash TEXT);
        CREATE TABLE work_root_executions(root_execution_id TEXT PRIMARY KEY, execution_json BLOB);").expect("locator fixture, not a full import fixture");
    let root = uuid::Uuid::new_v4().to_string();
    let execution = uuid::Uuid::new_v4().to_string();
    let child = uuid::Uuid::new_v4().to_string();
    let run = uuid::Uuid::new_v4().to_string();
    let peer_run = uuid::Uuid::new_v4().to_string();
    let pre = json!({"schema_version":1,"root_execution_id":execution,"project_id":"p",
        "root_id":root,"generation":1,"state":"active","revision":7,
        "run_ids":[run,peer_run],"required_child_seals":[],"required_child_waivers":[],
        "expected_contributors":["owner"],"contributions":[],"waivers":[],
        "created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:02:00Z"});
    let seal = json!({"root_execution_id":execution,"root_id":root,"work_id":child,"run_id":run,
        "expected_contributors":["owner"],"contributions":[],"waivers":[],"required_child_seals":[],
        "completed_at":"2026-09-08T00:03:00Z"});
    let seal_hash = insert(&connection, "completion_seal", &seal);
    let event = |state: Value, event_run: &str, transition: Value| {
        json!({
        "project_id":"p","root_id":root,"work_id":child,"run_id":event_run,
        "root_execution":state,"work":{"child_requirement":"required"},"transition":transition})
    };
    let mut earlier = pre.clone();
    earlier["revision"] = json!(6);
    earlier["updated_at"] = json!("2026-09-08T00:01:00Z");
    let earlier_hash = insert(
        &connection,
        "work_event",
        &event(earlier, &run, json!({"kind":"checkpointed"})),
    );
    let pre_hash = insert(
        &connection,
        "work_event",
        &event(pre.clone(), &peer_run, json!({"kind":"checkpointed"})),
    );
    let mut post = pre;
    post["revision"] = json!(8);
    post["updated_at"] = seal["completed_at"].clone();
    post["required_child_seals"] = json!([seal_hash]);
    let post_hash = insert(
        &connection,
        "work_event",
        &event(
            post.clone(),
            &run,
            json!({"kind":"completed","seal":seal_hash}),
        ),
    );
    for (index, hash) in [earlier_hash, pre_hash.clone(), post_hash.clone()]
        .iter()
        .enumerate()
    {
        connection
            .execute(
                "INSERT INTO work_feed_entries VALUES ('project','p',?1,'work_event',?2)",
                params![i64::try_from(index + 1).expect("position"), hash.as_str()],
            )
            .expect("feed");
    }
    connection
        .execute(
            "INSERT INTO work_root_executions VALUES (?1,?2)",
            params![execution, serde_json::to_vec(&post).expect("projection")],
        )
        .expect("root");
    let before = crate::storage::test_database_shape_snapshot(&connection).expect("before");
    let plan = inspect_pre_seal_history(&path).expect("observed predecessor");
    assert_eq!(plan.events, 3);
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.bindings[0].pre_event, pre_hash);
    assert_eq!(plan.bindings[0].completed_event, post_hash);
    assert_eq!(plan.bindings[0].seal, seal_hash);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).expect("after"),
        before
    );
    connection
        .execute_batch("SAVEPOINT accounting_fault")
        .expect("savepoint");
    let mut unmatched = seal.clone();
    unmatched["waivers"] = json!([{"participant":"missing","waived_by":"operator","reason":"not in any observed state"}]);
    let unmatched_hash = insert(&connection, "completion_seal", &unmatched);
    let mut unmatched_post = post.clone();
    unmatched_post["required_child_seals"] = json!([unmatched_hash]);
    let changed_event = insert(
        &connection,
        "work_event",
        &event(
            unmatched_post,
            &run,
            json!({"kind":"completed","seal":unmatched_hash}),
        ),
    );
    connection
        .execute(
            "UPDATE work_feed_entries SET object_hash=?1 WHERE position=3",
            [changed_event.as_str()],
        )
        .expect("bind fault");
    // The probe uses its own read-only connection, so commit the fixture fault
    // before probing; an uncommitted fault would not be in its read cut.
    connection
        .execute_batch("RELEASE accounting_fault")
        .expect("publish fixture fault");
    let damaged = crate::storage::test_database_shape_snapshot(&connection).expect("damaged cut");
    let error = inspect_pre_seal_history(&path).expect_err("unmatched accounting");
    assert!(
        error
            .to_string()
            .contains("pre-seal accounting differs: waivers"),
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).expect("after refusal"),
        damaged
    );
    connection
        .execute(
            "UPDATE work_feed_entries SET object_hash=?1 WHERE position=3",
            [post_hash.as_str()],
        )
        .expect("restore feed");
    connection
        .execute(
            "DELETE FROM objects WHERE object_hash IN (?1,?2)",
            params![unmatched_hash.as_str(), changed_event.as_str()],
        )
        .expect("remove injected objects");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).expect("restored fixture"),
        before
    );
    connection
        .execute("DELETE FROM work_feed_entries WHERE position=2", [])
        .expect("remove exact predecessor");
    assert!(
        inspect_pre_seal_history(&path).is_err(),
        "equal accounting in an earlier revision must not be substituted"
    );
    connection
        .execute(
            "INSERT INTO work_feed_entries VALUES ('project','p',2,'work_event',?1)",
            [pre_hash.as_str()],
        )
        .expect("restore event");
    connection
        .execute(
            "INSERT INTO work_feed_entries VALUES ('project','p',4,'work_event',?1)",
            [post_hash.as_str()],
        )
        .expect("duplicate completion");
    assert!(
        inspect_pre_seal_history(&path).is_err(),
        "ambiguous seal binding must refuse"
    );
}
