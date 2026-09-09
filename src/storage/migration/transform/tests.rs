use serde_json::{Value, json};

use crate::{CanonicalObject, ObjectHash};

use super::convert_observation;

fn address(label: &str) -> ObjectHash {
    CanonicalObject::freeze(&json!({"label":label}))
        .expect("runtime address")
        .hash()
        .clone()
}

fn observation(hash: &ObjectHash) -> Value {
    json!({
        "schema_version":1,"project_id":"p","root_id":uuid::Uuid::new_v4(),
        "work_id":uuid::Uuid::new_v4(),"work_revision":1,
        "basis":{"kind":"native_event","event":hash},"sequence":1,
        "summary":hash.as_str(),"refs":[hash.as_str()],
        "actor":{"actor_id":"author","actor_kind":"test_agent","assurance":"asserted",
            "run_id":null,"session_id":"s","source_tool":null,"source_skill":null,
            "provenance_chain":[],"reason":hash.as_str()},
        "created_at":"2026-09-08T00:00:00Z"
    })
}

#[test]
fn migration_transform_changes_declared_basis_only_not_hash_looking_prose() {
    let old = address("old event");
    let new = address("new event");
    let source = observation(&old);
    let original = CanonicalObject::freeze(&source).expect("original");
    let mut seen = Vec::new();
    let transformed = convert_observation(&source, &mut |hash| {
        seen.push(hash.clone());
        assert_eq!(hash, &old);
        Ok(new.clone())
    })
    .expect("explicit typed conversion");
    assert_eq!(seen, vec![old.clone()]);
    let mut expected = source.clone();
    expected["basis"]["event"] = json!(new);
    assert_eq!(transformed.decode::<Value>().expect("target"), expected);
    assert_eq!(original.decode::<Value>().expect("retained source"), source);
    assert_ne!(original.hash(), transformed.hash());
    assert!(convert_observation(&source, &mut |_| Err(super::refused("missing mapping"))).is_err());
}

#[test]
fn migration_transform_refuses_unknown_or_defaulted_source_fields() {
    let old = address("old event");
    let mut source = observation(&old);
    source["unrecognized"] = json!(true);
    assert!(convert_observation(&source, &mut |hash| Ok(hash.clone())).is_err());
    let mut source = observation(&old);
    source.as_object_mut().expect("object").remove("refs");
    assert!(convert_observation(&source, &mut |hash| Ok(hash.clone())).is_err());
}

#[test]
fn migration_run_projection_does_not_drop_unknown_fields_when_rebinding_hashes() {
    let mut run = json!({"schema_version":1,"run_id":uuid::Uuid::new_v4(),
        "root_execution_id":uuid::Uuid::new_v4(),"work_id":uuid::Uuid::new_v4(),
        "generation":1,"executor":null,"state":"open","revision":1,
        "last_checkpoint":null,"completion_seal":null,
        "created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:00:00Z"});
    super::strict::<crate::WorkRun>(&run).expect("exact known run shape");
    run["unrecognized"] = json!({"must":"not disappear"});
    assert!(super::strict::<crate::WorkRun>(&run).is_err());
}
