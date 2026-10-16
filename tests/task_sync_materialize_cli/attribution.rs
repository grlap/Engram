use super::*;

fn command(fixture: &Fixture, context: &str, from_environment: bool) -> Command {
    let mut command = fixture.materialize_command();
    if from_environment {
        command.env("ENGRAM_ACTOR_CONTEXT", context);
    } else {
        command.args(["--actor-context", context]);
    }
    command
}

#[test]
fn setup_actor_context_materialize_preserves_raw_text_and_first_attribution() {
    for from_environment in [false, true] {
        for context in [
            String::new(),
            format!("  {}🙂 końcówka  ", "a".repeat(255)),
            "é".repeat(2048),
        ] {
            let fixture = Fixture::new();
            // Setup was prepared independently; only materialization gets this assertion.
            let output = command(&fixture, &context, from_environment)
                .output()
                .unwrap();
            let first = success(&output);
            let expected =
                serde_json::to_value((!context.is_empty()).then_some(context.as_str())).unwrap();
            assert_eq!(first["attribution"]["actor_context"], expected);
            let journal_path = fixture.root.join(format!(
                ".git/engram-task-sync-materialize-{}-{}/journal.json",
                fixture.setup, fixture.prepared
            ));
            let journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
            assert_eq!(journal["attribution"]["actor_context"], expected);
            let before = snapshot(fixture.owned.path());
            let output = command(&fixture, "different valid context", from_environment)
                .output()
                .unwrap();
            let repeated = success(&output);
            assert_eq!(repeated["operation"], first["operation"]);
            assert_eq!(repeated["repeated"], true);
            assert_eq!(repeated["attribution"], first["attribution"]);
            assert_eq!(snapshot(fixture.owned.path()), before);
            assert_eq!(
                fs::read(&fixture.store).unwrap(),
                b"source must not be opened as SQLite"
            );
        }
    }
}

#[test]
fn setup_actor_context_materialize_refuses_invalid_raw_text_before_state() {
    for from_environment in [false, true] {
        for context in [
            "model\tsecret-context".into(),
            "model\nsecret-context".into(),
            "model\u{1b}secret-context".into(),
            format!("{}\tsecret-context", "a".repeat(300)),
            "   ".into(),
            "a".repeat(4097),
        ] {
            let fixture = Fixture::new();
            let before = snapshot(fixture.owned.path());
            let output = command(&fixture, &context, from_environment)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains(
                    "actor context must be nonblank bounded text without control characters"
                ),
                "{error}"
            );
            assert!(!error.contains("secret-context"), "{error}");
            assert_eq!(snapshot(fixture.owned.path()), before);
        }
    }
}
