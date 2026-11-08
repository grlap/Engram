use super::*;

#[test]
fn historical_git_tree_observation_decoding_refuses_malformed_values_without_echoing_them() {
    // Historical typed decoding remains supported; this does not admit a new
    // observation through the store's current new-write validator.
    let mut observe = execution_observe_frame();
    let basis = &mut observe["occurrence"]["source_change"]["sighting"]["source_basis"];
    basis.as_object_mut().unwrap().remove("source_id");
    basis["source_revision"] = "historical-revision".into();
    for (frame, pointer) in [(observe, "/occurrence/source_change/sighting/source_basis")] {
        assert!(parse_host_control_request(&serde_json::to_vec(&frame).unwrap()).is_ok());
        for length in [40, 64] {
            let mut valid = frame.clone();
            valid.pointer_mut(pointer).unwrap()["git_tree"] = Value::String("a".repeat(length));
            assert!(parse_host_control_request(&serde_json::to_vec(&valid).unwrap()).is_ok());
        }
        for invalid in [
            Value::Null,
            Value::String("A".repeat(40)),
            Value::String("private-invalid-tree".into()),
        ] {
            let mut bad = frame.clone();
            bad.pointer_mut(pointer).unwrap()["git_tree"] = invalid;
            let response = parse_frame(Ok(serde_json::to_vec(&bad).unwrap())).unwrap_err();
            let HostControlResponse::Error { error } = response else {
                panic!("invalid tree admitted");
            };
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("Data"), "{}", error.message);
            assert!(!error.message.contains("private-invalid-tree"));
        }
    }
}
