//! Distinct capacity accounting must not cache validation or relax either bound.

use super::*;

fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE objects (
                object_id TEXT PRIMARY KEY,
                object_kind TEXT NOT NULL,
                canonical_json BLOB NOT NULL
            ) STRICT;",
        )
        .unwrap();
    connection
}

fn insert(connection: &Connection, bytes: &[u8]) -> ObjectId {
    let id = ObjectId::mint();
    connection
        .execute(
            "INSERT INTO objects VALUES (?1,?2,?3)",
            params![id.as_str(), EVENT_KIND, bytes],
        )
        .unwrap();
    id
}

fn replace(connection: &Connection, id: &ObjectId, kind: &str, bytes: &[u8]) {
    connection
        .execute(
            "UPDATE objects SET object_kind=?2,canonical_json=?3 WHERE object_id=?1",
            params![id.as_str(), kind, bytes],
        )
        .unwrap();
}

fn read(connection: &Connection, id: &ObjectId, budget: &mut ReadBudget) -> serde_json::Value {
    object(connection, id, EVENT_KIND, budget).unwrap()
}

fn refused(connection: &Connection, id: &ObjectId, budget: &mut ReadBudget, reason: &str) {
    assert!(matches!(
        object::<serde_json::Value>(connection, id, EVENT_KIND, budget),
        Err(StoreError::InvalidWork(actual)) if actual == reason
    ));
}

#[test]
fn repeated_reads_still_query_and_decode_stored_objects() {
    let connection = database();
    let id = insert(&connection, b"1");
    let mut budget = ReadBudget::default();
    assert_eq!(read(&connection, &id, &mut budget), 1);
    // Equal-length valid bytes are still decoded; metadata is not a payload cache.
    replace(&connection, &id, EVENT_KIND, b"2");
    assert_eq!(read(&connection, &id, &mut budget), 2);
    replace(&connection, &id, EVENT_KIND, b"[");
    assert!(object::<serde_json::Value>(&connection, &id, EVENT_KIND, &mut budget).is_err());
    replace(&connection, &id, EVENT_KIND, b"3");
    assert!(object::<String>(&connection, &id, EVENT_KIND, &mut budget).is_err());
    replace(&connection, &id, EVENT_KIND, b"10");
    refused(
        &connection,
        &id,
        &mut budget,
        "exchange canonical record metadata differs between reads",
    );
    replace(&connection, &id, BINDING_KIND, b"1");
    refused(
        &connection,
        &id,
        &mut budget,
        "exchange canonical record kind differs",
    );
    assert!(matches!(
        object::<serde_json::Value>(&connection, &id, BINDING_KIND, &mut budget),
        Err(StoreError::InvalidWork(reason))
            if reason == "exchange canonical record metadata differs between reads"
    ));
    replace(
        &connection,
        &id,
        EVENT_KIND,
        &vec![b' '; MAX_RECORD_BYTES + 1],
    );
    refused(
        &connection,
        &id,
        &mut budget,
        "exchange canonical record exceeds supported byte bound",
    );
    connection
        .execute("DELETE FROM objects WHERE object_id=?1", [id.as_str()])
        .unwrap();
    refused(
        &connection,
        &id,
        &mut budget,
        "exchange canonical record is missing; preserve the store and recover its canonical history",
    );
}

#[test]
fn distinct_byte_limit_allows_repeats_but_refuses_an_additional_object() {
    let connection = database();
    let mut budget = ReadBudget::default();
    let bytes = crate::canonical::canonical_bytes(&"x".repeat(MAX_RECORD_BYTES - 2)).unwrap();
    assert_eq!(bytes.len(), MAX_RECORD_BYTES);
    let mut last = None;
    for _ in 0..MAX_BYTES / MAX_RECORD_BYTES {
        let id = insert(&connection, &bytes);
        read(&connection, &id, &mut budget);
        read(&connection, &id, &mut budget);
        last = Some(id);
    }
    assert_eq!(budget.bytes, MAX_BYTES);
    let extra = insert(&connection, b"0");
    refused(
        &connection,
        &extra,
        &mut budget,
        "exchange lookup exceeds byte bound",
    );
    read(&connection, &last.unwrap(), &mut budget);
    assert_eq!(budget.bytes, MAX_BYTES);
}

#[test]
fn distinct_record_limit_allows_repeats_but_refuses_an_additional_object() {
    let connection = database();
    connection.execute_batch("BEGIN").unwrap();
    let mut budget = ReadBudget::default();
    let mut last = None;
    for _ in 0..MAX_RECORDS {
        let id = insert(&connection, b"0");
        read(&connection, &id, &mut budget);
        last = Some(id);
    }
    read(&connection, last.as_ref().unwrap(), &mut budget);
    let extra = insert(&connection, b"0");
    refused(
        &connection,
        &extra,
        &mut budget,
        "exchange lookup exceeds record bound",
    );
    read(&connection, &last.unwrap(), &mut budget);
    assert_eq!(budget.objects.len(), MAX_RECORDS);
    assert_eq!(budget.bytes, MAX_RECORDS);
    connection.execute_batch("ROLLBACK").unwrap();
}
