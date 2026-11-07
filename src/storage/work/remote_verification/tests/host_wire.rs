//! The operation as the host channel carries it.

use super::*;

fn serve(f: Fixture, frames: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut server = crate::host::HostControlServer::from_test_store(
        f.store,
        f.project,
        f.session,
        f.connection,
    );
    let input = frames
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut output = Vec::new();
    server
        .serve(std::io::Cursor::new(input), &mut output)
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The fixture at the host transport's real clock.
fn current() -> Fixture {
    let offset = (chrono::Utc::now() - at(10)).num_seconds();
    fixture_in(SqliteStore::open_in_memory().unwrap(), offset, None)
}

fn frame(f: &Fixture) -> serde_json::Value {
    let mut wire = serde_json::to_value(&f.request).unwrap();
    wire["operation"] = serde_json::json!("remote_verification_record");
    wire
}

#[test]
fn remote_verification_wire_carries_the_frozen_shape() {
    let f = current();
    let request = frame(&f);
    crate::host::id_contract::assert_host_ids(&request);
    let parsed: crate::host::HostControlRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(parsed.operation(), "remote_verification_record");
    // Exactly the agreed members, with the destination's use under `use`.
    assert_eq!(
        request["association"]["use"]["kind"],
        serde_json::json!("committed_input")
    );
    assert_eq!(request["completion"]["result"]["verdict"], "PASS");
    assert_eq!(request["completion"]["mode"], "full");

    let mut uncredited = request.clone();
    uncredited["operation_key"] = serde_json::json!("remote:uncredited");
    uncredited["completion"]["run_id"] = serde_json::json!("test-uncredited");
    uncredited["completion"]["mode"] = serde_json::json!("focused");
    let replies = serve(f, &[request.clone(), request, uncredited]);
    for reply in &replies {
        crate::host::id_contract::assert_host_ids(reply);
    }
    assert_eq!(replies[0]["status"], "ok", "{}", replies[0]);
    let result = &replies[0]["result"];
    assert_eq!(
        result["qualification"],
        serde_json::json!({"credited": true})
    );
    assert_eq!(result["verifications"][0]["stage"], "full");
    assert_eq!(replies[1], replies[0], "an exact retry repeats the receipt");
    let uncredited = &replies[2]["result"];
    assert_eq!(uncredited["verifications"], serde_json::json!([]));
    assert_eq!(
        uncredited["qualification"],
        serde_json::json!({"credited": false, "reason": "not_full_mode"})
    );
}

#[test]
fn remote_verification_snapshot_receipt_has_a_null_origin() {
    let f = current();
    let mut request = frame(&f);
    request["completion"]["result"]["verdict"] = serde_json::json!("RUNNING");
    request["completion"]["result"]["terminal"] = serde_json::json!(false);
    request["completion"]["result"]["exit"] = serde_json::Value::Null;
    request["completion"]["result"]["reason"] = serde_json::json!("no-terminal-result");
    request["completion"]
        .as_object_mut()
        .unwrap()
        .remove("finished_at");
    let replies = serve(f, &[request]);
    assert_eq!(replies[0]["status"], "ok", "{}", replies[0]);
    assert_eq!(replies[0]["result"]["origin"], serde_json::Value::Null);
    assert_eq!(
        replies[0]["result"]["qualification"]["reason"],
        "result_not_pass"
    );
}

#[test]
fn remote_verification_wire_accepts_two_shared_endpoints_and_refuses_partial_input() {
    let f = current();
    let mut request = frame(&f);
    let source = shared_input_source();
    request["completion"]["input"] =
        serde_json::json!({"kind":"source_id","source_id_before":source,"source_id_after":source});
    let parsed: crate::host::HostControlRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(parsed.operation(), "remote_verification_record");
    let mut unavailable = request.clone();
    unavailable["operation_key"] = serde_json::json!("remote:null-source");
    unavailable["completion"]["run_id"] = serde_json::json!("test-null-source");
    unavailable["completion"]["input"]["source_id_before"] = serde_json::Value::Null;
    unavailable["completion"]["input"]["source_id_after"] = serde_json::Value::Null;
    let mut partial = request.clone();
    partial["completion"]["input"]
        .as_object_mut()
        .unwrap()
        .remove("source_id_after");
    let mut mixed = request.clone();
    mixed["completion"]["input"]["commit"] = serde_json::json!(tested_commit());
    let replies = serve(f, &[request, unavailable, partial, mixed]);
    assert_eq!(
        replies[0]["result"]["qualification"],
        serde_json::json!({"credited":true})
    );
    assert_eq!(
        replies[1]["result"]["qualification"]["reason"],
        "source_id_missing_or_changed"
    );
    for reply in &replies[2..] {
        assert_eq!(reply["error"]["code"], "invalid_request", "{reply}");
    }
}

fn shared_input_source() -> crate::source::SourceId {
    crate::source::SourceId {
        commit: tested_commit(),
        tree: tested_tree().as_str().into(),
        submodules: vec![],
        changes: vec![],
    }
}

#[test]
fn remote_verification_malformed_frames_answer_invalid_request() {
    let f = current();
    let request = frame(&f);
    let mut frames = Vec::new();
    let mut malformed = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut frame = request.clone();
        change(&mut frame);
        frames.push(frame);
    };
    malformed(&|frame| frame["extra"] = serde_json::json!(true));
    malformed(&|frame| frame["completion"]["provenance"] = serde_json::json!({"toolchain": "x"}));
    malformed(&|frame| frame["completion"]["input"]["counts"] = serde_json::json!(1));
    malformed(&|frame| frame["completion"]["finished_at"] = serde_json::Value::Null);
    malformed(&|frame| frame["completion"]["input"]["tree"] = serde_json::Value::Null);
    malformed(&|frame| frame["completion"]["input"]["tree"] = serde_json::json!("not-a-tree"));
    malformed(&|frame| frame["completion"]["mode"] = serde_json::json!("whole-bin"));
    malformed(&|frame| frame["completion"]["result"]["verdict"] = serde_json::json!("pass"));
    malformed(&|frame| {
        frame["completion"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("exit");
    });
    malformed(&|frame| frame["association"]["use"]["purpose"] = serde_json::json!("publish"));
    malformed(&|frame| frame["association"]["use"]["kind"] = serde_json::json!("staged_tree"));
    // Well-formed JSON, malformed value: the store's own refusal, same code.
    malformed(&|frame| frame["completion"]["input"]["commit"] = serde_json::json!("abc"));
    let count = frames.len();
    let replies = serve(f, &frames);
    assert_eq!(replies.len(), count);
    for reply in &replies {
        assert_eq!(reply["status"], "error", "{reply}");
        assert_eq!(reply["error"]["code"], "invalid_request", "{reply}");
    }
    assert_eq!(
        replies[count - 1]["error"]["details"]["field"],
        "input.commit"
    );
}
