use serde_json::json;

use crate::domain::RootExecutionDelta;
use crate::{CanonicalObject, RootExecution};

use super::RootHistoryEncoder;

fn observed() -> RootExecution {
    serde_json::from_value(json!({
        "schema_version":1,"root_execution_id":uuid::Uuid::new_v4(),"project_id":"p",
        "root_id":uuid::Uuid::new_v4(),"generation":1,"state":"active","revision":7,
        "run_ids":[uuid::Uuid::new_v4()],"required_child_seals":[],"required_child_waivers":[],
        "expected_contributors":["owner"],"contributions":[],"waivers":[],
        "created_at":"2026-09-08T00:00:00Z","updated_at":"2026-09-08T00:02:00Z"
    }))
    .expect("observed aggregate")
}

#[test]
fn migration_root_encoder_sources_every_header_field_and_reuses_equal_cuts() {
    let root = observed();
    let mut encoder = RootHistoryEncoder::default();
    let result = encoder.push(root.clone()).expect("encode");
    assert_eq!(result.objects.len(), 2);
    let origin: RootExecutionDelta = result.objects[0].decode().expect("origin");
    assert_eq!(origin.sequence, 0);
    assert!(origin.predecessor.is_none());
    assert!(origin.added.is_empty());
    let delta: RootExecutionDelta = result.objects[1].decode().expect("observed head");
    assert_eq!(
        delta.state_checksum,
        *CanonicalObject::freeze(&root).expect("state").hash()
    );
    assert_eq!(delta.predecessor.as_ref(), Some(result.objects[0].hash()));
    assert_eq!(result.reference.head, *result.objects[1].hash());
    let source = serde_json::to_value(&root).expect("source fields");
    let fields = serde_json::to_value(&delta.header).expect("header");
    assert_eq!(fields.as_object().expect("object").len(), 9);
    for (field, value) in fields.as_object().expect("header fields") {
        assert_eq!(
            Some(value),
            source.get(field),
            "header field {field} must be sourced, not synthesized"
        );
    }
    let repeat = encoder.push(root.clone()).expect("same observed cut");
    assert!(repeat.objects.is_empty());
    assert_eq!(repeat.reference, result.reference);
    let mut changed = root.clone();
    changed.revision += 1;
    changed.updated_at += chrono::Duration::seconds(1);
    changed.waivers.push(crate::domain::CompletionWaiver {
        participant: crate::SessionId("peer".into()),
        waived_by: "operator".into(),
        reason: "captured source reason".into(),
    });
    let next = encoder.push(changed).expect("next observed cut");
    assert_eq!(next.objects.len(), 1);
    let delta: RootExecutionDelta = next.objects[0].decode().expect("delta");
    assert_eq!(delta.sequence, 2);
    assert_eq!(delta.previous_revision, Some(root.revision));
    assert_eq!(delta.predecessor, Some(result.reference.head));
    assert!(delta.removed.is_empty());
    assert_eq!(delta.added.len(), 1);
    assert!(encoder.push(root).is_err(), "backward history must refuse");
}

#[test]
fn migration_root_encoder_refuses_duplicate_member_keys_without_deduplicating_data() {
    let mut root = observed();
    root.run_ids.push(root.run_ids[0]);
    assert!(RootHistoryEncoder::default().push(root).is_err());
    let mut root = observed();
    root.waivers = ["first", "different"]
        .into_iter()
        .map(|reason| crate::domain::CompletionWaiver {
            participant: crate::SessionId("peer".into()),
            waived_by: "operator".into(),
            reason: reason.into(),
        })
        .collect();
    assert!(RootHistoryEncoder::default().push(root).is_err());
}
