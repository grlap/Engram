use super::*;
use std::collections::BTreeMap;

fn command(fixture: &Fixture, context: &str, from_environment: bool) -> Command {
    let mut command = fixture.command(&fixture.root, &fixture.root.join(".engram-project"));
    command.env_remove("ENGRAM_ACTOR_CONTEXT");
    if from_environment {
        command.env("ENGRAM_ACTOR_CONTEXT", context);
    } else {
        command.args(["--actor-context", context]);
    }
    command
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.insert(path.clone(), None);
            result.extend(snapshot(&path));
        } else {
            let bytes = fs::read(&path).unwrap();
            result.insert(path, Some(bytes));
        }
    }
    result
}

#[test]
fn setup_actor_context_prepare_preserves_raw_text_and_first_attribution() {
    for from_environment in [false, true] {
        for context in [
            String::new(),
            format!("  {}🙂 końcówka  ", "a".repeat(255)),
            "é".repeat(2048),
        ] {
            let fixture = Fixture::new(false);
            let output = command(&fixture, &context, from_environment)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let first: Value = serde_json::from_slice(&output.stdout).unwrap();
            let expected = (!context.is_empty()).then_some(context.as_str());
            assert_eq!(
                fixture.intent()["attribution"]["actor_context"],
                serde_json::to_value(expected).unwrap()
            );
            let before = snapshot(fixture.owned.path());
            let output = command(&fixture, "different valid context", from_environment)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let repeated: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(repeated["operation"], first["operation"]);
            assert_eq!(repeated["repeated"], true);
            assert_eq!(snapshot(fixture.owned.path()), before);
            task_sync_opt_in::assert_no_database(&fixture.home);
        }
    }
}

#[test]
fn setup_actor_context_prepare_refuses_invalid_raw_text_before_state() {
    for from_environment in [false, true] {
        for context in [
            "model\tsecret-context".into(),
            "model\nsecret-context".into(),
            "model\u{1b}secret-context".into(),
            format!("{}\tsecret-context", "a".repeat(300)),
            "   ".into(),
            "a".repeat(4097),
        ] {
            let fixture = Fixture::new(false);
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
            assert!(!fixture.intent_path().exists());
            assert!(!fixture.admin.join("engram-task-sync-setup.lock").exists());
            task_sync_opt_in::assert_no_database(&fixture.home);
        }
    }
}
