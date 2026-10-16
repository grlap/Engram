use std::collections::HashSet;

use super::TaskSyncRefusal;

const DOCUMENT: &str = include_str!("../../docs/features/task-planning-text.md");

#[test]
fn every_task_sync_refusal_code_is_unique_and_documented() {
    let mut codes = HashSet::new();
    for refusal in TaskSyncRefusal::ALL {
        let code = refusal.code();
        assert!(codes.insert(code), "{code} is repeated");
        assert!(code.starts_with("task_sync_"), "{code}");
        assert!(
            DOCUMENT.contains(&format!("| `{code}` |")),
            "{code} has no row in the refusal code table"
        );
    }
    let rows = DOCUMENT
        .lines()
        .filter(|line| line.starts_with("| `task_sync_"))
        .count();
    assert_eq!(rows, TaskSyncRefusal::ALL.len(), "every row names a code");
}

#[test]
fn classification_keeps_text_and_passes_other_errors_through() {
    for refusal in TaskSyncRefusal::ALL {
        let plain = super::invalid("candidate refused");
        let typed = refusal.invalid("candidate refused");
        assert_eq!(typed.to_string(), plain.to_string());
        let value = crate::store_error_value(&typed);
        assert_eq!(value["error"]["code"], refusal.code());
        // The reason stays; the work-item remedy is left out.
        let mut expected = crate::store_error_value(&plain)["error"]["details"].clone();
        expected.as_object_mut().unwrap().remove("remedy");
        assert_eq!(value["error"]["details"], expected);
        assert_eq!(value["error"]["details"]["reason"], "candidate refused");
        let typed_again = TaskSyncRefusal::InvalidGraph.classify(typed);
        assert_eq!(
            crate::store_error_value(&typed_again)["error"]["code"],
            refusal.code(),
            "a classified refusal keeps its first family"
        );
        let other = refusal.classify(crate::storage::StoreError::EmptyNote);
        assert!(matches!(other, crate::storage::StoreError::EmptyNote));
    }
}

#[test]
fn an_oversized_shared_note_is_classified_and_keeps_its_wire_code() {
    let oversized = || crate::storage::StoreError::WorkNoteTooLarge {
        bytes: 70_000,
        limit: 65_536,
    };
    let classified = TaskSyncRefusal::InvalidGraph.classify(oversized());
    assert_eq!(classified.to_string(), oversized().to_string());
    assert_eq!(
        crate::store_error_value(&classified)["error"]["code"],
        "task_sync_invalid_graph"
    );
    assert_eq!(
        crate::host::store_error_code(&classified),
        crate::host::store_error_code(&oversized())
    );
}
