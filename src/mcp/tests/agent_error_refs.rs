use super::*;

#[test]
fn ancestor_and_required_child_errors_use_agent_refs_and_keep_raw_ids() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("error-refs".into());
    let session = SessionId("reader".into());
    let cli = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "reader".into(),
        session.clone(),
        None,
    );
    let server =
        McpServer::new_with_actor_context(database, project, "reader".into(), session, None, None);
    let work = crate::WorkId::new();
    let child = crate::WorkId::new();
    let work_ref = format!("w-{}", work.0.simple().to_string().get(20..).unwrap());
    let child_ref = format!("w-{}", child.0.simple().to_string().get(20..).unwrap());
    let ancestor = crate::domain::WorkBlockingAncestor {
        work_id: child,
        short_ref: child_ref.clone(),
        lifecycle: crate::WorkLifecycle::Completed,
    };
    for error in [
        StoreError::WorkAncestorNotOpen { work, ancestor },
        StoreError::WorkCompletionRecoveryRequired {
            work,
            cause: crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
            context: Box::default(),
        },
    ] {
        let raw = store_error_value(&error);
        assert_eq!(raw["error"]["details"]["work_id"], json!(work));
        let cause = raw["error"]["details"]["cause"].clone();
        let verb_error = VerbError::from(error);
        let message = cli.error_message(&verb_error);
        let guidance = cli.error_guidance(&verb_error);
        let projected = cli.project_error(&verb_error, raw.clone());
        let result = server.verb(Err(verb_error));
        assert_eq!(result.is_error, Some(true));
        let mcp = result.structured_content.unwrap();
        for value in [&projected, &mcp] {
            assert_eq!(value["error"]["code"], raw["error"]["code"]);
            assert_eq!(value["error"]["message"], message);
            assert_eq!(value["error"]["details"]["work_ref"], work_ref);
            assert!(value["error"]["details"].get("work_id").is_none());
            assert_eq!(
                value["error"]["details"]["deciding_observation"],
                raw["error"]["details"]["deciding_observation"]
            );
        }
        assert!(message.contains(&work_ref));
        assert!(message.contains(&child_ref));
        assert!(!message.contains(&work.0.to_string()));
        assert!(!message.contains(&child.0.to_string()));
        assert_eq!(guidance.next[0], format!("engram work show {work_ref}"));
        assert_eq!(mcp["error"]["next"], json!(guidance.next));
        assert_eq!(mcp["error"]["reminders"], json!(guidance.reminders));
        if cause.is_null() {
            assert_eq!(
                projected["error"]["details"]["blocking_ancestor"],
                raw["error"]["details"]["blocking_ancestor"]
            );
            assert_eq!(guidance.next[1], format!("engram work show {child_ref}"));
        } else {
            assert_eq!(
                cause,
                json!({ "kind": "required_child_unsealed", "child": child })
            );
            assert!(
                raw["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&child.0.to_string())
            );
            assert_eq!(
                projected["error"]["details"]["cause"],
                json!({ "kind": "required_child_unsealed", "child": child_ref })
            );
            assert_eq!(
                mcp["error"]["details"]["cause"],
                projected["error"]["details"]["cause"]
            );
            assert!(guidance.reminders[0].contains(&child_ref));
            assert!(!guidance.reminders[0].contains(&child.0.to_string()));
        }
    }
}
