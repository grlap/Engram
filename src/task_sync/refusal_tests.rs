use std::collections::HashSet;

use super::TaskSyncRefusal;

const DOCUMENT: &str = include_str!("../../docs/features/task-planning-text.md");

#[test]
fn every_task_sync_refusal_code_is_unique_and_documented() {
    check_documented_codes(TaskSyncRefusal::ALL, DOCUMENT).unwrap();
}

fn check_documented_codes(inventory: &[TaskSyncRefusal], document: &str) -> Result<(), String> {
    let expected: HashSet<_> = TaskSyncRefusal::ALL.iter().map(|r| r.code()).collect();
    let mut codes = HashSet::new();
    for refusal in inventory {
        let code = refusal.code();
        if !codes.insert(code) || !code.starts_with("task_sync_") {
            return Err(format!("invalid or repeated code: {code}"));
        }
        if !document.contains(&format!("| `{code}` |")) {
            return Err(format!("{code} has no row in the refusal code table"));
        }
    }
    if codes != expected {
        return Err("the documentation inventory omits a refusal family".into());
    }
    let rows = document
        .lines()
        .filter(|line| line.starts_with("| `task_sync_"))
        .count();
    if rows != expected.len() {
        return Err("every table row must name exactly one refusal code".into());
    }
    Ok(())
}

#[test]
fn documented_codes_guard_rejects_each_omitted_variant_even_with_its_row_removed() {
    for omitted in TaskSyncRefusal::ALL {
        let inventory: Vec<_> = TaskSyncRefusal::ALL
            .iter()
            .copied()
            .filter(|refusal| refusal != omitted)
            .collect();
        assert!(check_documented_codes(&inventory, DOCUMENT).is_err());
        let document = DOCUMENT
            .lines()
            .filter(|line| !line.starts_with(&format!("| `{}` |", omitted.code())))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(check_documented_codes(TaskSyncRefusal::ALL, &document).is_err());
        assert!(check_documented_codes(&inventory, &document).is_err());
    }
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
