//! A stored completion result is decoded again on replay by the shape it
//! claims: a present `seal` makes it a receipt, otherwise a `code` makes it a
//! refusal. A result that does not decode is refused with that shape's
//! reason, naming an unknown or missing member and never a stored value.

use super::super::replayed_completion_result;
use super::*;

/// One refusal and one receipt, as each is stored, from the same item.
fn stored_results() -> (serde_json::Value, serde_json::Value) {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let service = LocalWorkService::new(
        directory.path().join("engram.sqlite3"),
        ProjectId("replay-decode".into()),
        "agent".into(),
        SessionId("replay-decode-session".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(root_input("Decoded replay", "decode-root"), at(0))
        .expect("root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "decode-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let complete = |acceptance: Option<Vec<WorkAcceptanceInput>>, note: Option<&str>, key: &str| {
        WorkCompleteInput {
            source_fingerprint: None,
            landing: None,
            links: Vec::new(),
            link_basis: None,
            capture: Some(WorkCompletionCaptureInput {
                summary: "delivered".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance,
            note: note.map(str::to_owned),
            idempotency_key: key.into(),
        }
    };
    let refused = service
        .work_complete(complete(Some(Vec::new()), None, "decode-refused"), at(2))
        .expect("missing acceptance refuses");
    assert!(matches!(refused, WorkCompleteResult::Refused(_)));
    let completed = service
        .work_complete(complete(None, Some("reviewed by hand"), "decode-ok"), at(3))
        .expect("completes");
    assert!(matches!(completed, WorkCompleteResult::Completed(_)));
    (
        serde_json::to_value(&refused).expect("refusal JSON"),
        serde_json::to_value(&completed).expect("receipt JSON"),
    )
}

fn reason(value: serde_json::Value) -> String {
    let error = replayed_completion_result(value).expect_err("the stored result is refused");
    assert!(matches!(error, StoreError::Json(_)), "{error:?}");
    let reason = error.to_string();
    assert!(
        !reason.contains("untagged"),
        "never the union's mismatch: {reason}"
    );
    reason
}

#[test]
fn a_stored_completion_result_decodes_as_the_shape_it_claims() {
    let (refusal, receipt) = stored_results();

    // Valid results decode to their own variant and serialize unchanged.
    let decoded = replayed_completion_result(receipt.clone()).expect("receipt");
    assert!(matches!(decoded, WorkCompleteResult::Completed(_)));
    assert_eq!(serde_json::to_value(&decoded).expect("JSON"), receipt);
    let decoded = replayed_completion_result(refusal.clone()).expect("refusal");
    assert!(matches!(decoded, WorkCompleteResult::Refused(_)));
    assert_eq!(serde_json::to_value(&decoded).expect("JSON"), refusal);

    // A seal takes precedence over a code.
    let mut both = receipt.clone();
    both["code"] = refusal["code"].clone();
    assert!(matches!(
        replayed_completion_result(both).expect("a sealed result is a receipt"),
        WorkCompleteResult::Completed(_)
    ));

    // A missing member is named, for each shape.
    let mut without = receipt.clone();
    without
        .as_object_mut()
        .expect("receipt object")
        .remove("work_id");
    assert!(
        reason(without).contains("missing field `work_id`"),
        "receipt member"
    );
    let mut without = refusal.clone();
    without
        .as_object_mut()
        .expect("refusal object")
        .remove("remedy");
    assert!(
        reason(without).contains("missing field `remedy`"),
        "refusal member"
    );
}

#[test]
fn a_malformed_or_unclaimed_completion_result_is_refused_by_its_shape() {
    let (refusal, receipt) = stored_results();

    // A null or malformed seal still claims a receipt; the refusal members
    // beside it are never decoded instead.
    for seal in [serde_json::Value::Null, serde_json::json!(7)] {
        let mut malformed = refusal.clone();
        malformed["seal"] = seal;
        let reason = reason(malformed);
        assert!(
            reason.contains("a field of the wrong type or value"),
            "{reason}"
        );
    }

    // Neither a seal nor a code claims no shape at all.
    let mut unclaimed = receipt.clone();
    unclaimed
        .as_object_mut()
        .expect("receipt object")
        .remove("seal");
    assert!(reason(unclaimed).contains("neither a `seal` nor a refusal `code`"));
    assert!(reason(serde_json::json!("not an object")).contains("neither"));

    // A stored value never reaches the reason.
    let hostile = "hostile-stored-value";
    let mut wrong = receipt.clone();
    wrong["completed_at"] = serde_json::json!(hostile);
    let reason_text = reason(wrong);
    assert!(!reason_text.contains(hostile), "{reason_text}");
    assert!(reason_text.contains("a field of the wrong type or value"));
    // An unknown variant, whose decoder message would quote it.
    let mut wrong = refusal;
    wrong["recovery"]["cause"]["kind"] = serde_json::json!(hostile);
    let raw = serde_json::from_value::<WorkCompleteRefusal>(wrong.clone())
        .expect_err("an unknown cause")
        .to_string();
    assert!(raw.contains(hostile), "the raw message quotes it: {raw}");
    let reason_text = reason(wrong);
    assert!(!reason_text.contains(hostile), "{reason_text}");
}
