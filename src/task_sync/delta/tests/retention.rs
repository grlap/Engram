use super::*;
use crate::task_sync::retention::{self, delta};

fn retained_fixture() -> (crate::test_support::TempHome, Fixture) {
    let home = crate::test_support::temp_home().unwrap();
    let mut f = fixture();
    f.binding.repository.git_directory = home.path().join(".git");
    f.binding.repository.main_checkout = home.path().to_owned();
    f.binding.store.database = home.path().join("work.db");
    f.intent.candidate.directory =
        delta::candidate_directory(&f.binding.repository.git_directory, f.intent.operation);
    std::fs::create_dir_all(&f.binding.repository.git_directory).unwrap();
    (home, f)
}

#[test]
fn exact_delta_retention_replays_without_reading_disconnected_history_or_granting_admission() {
    let (_home, f) = retained_fixture();
    assert_eq!(
        delta::read_candidate(&f.binding_id, &f.binding, &f.intent).unwrap(),
        None
    );
    let result = delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).unwrap();
    assert_eq!(result, f.intent.candidate);
    let intent_file = result.directory.join("intent.pending");
    let payload_file = result.directory.join("payload.json");
    let request = std::fs::read(&intent_file).unwrap();
    let bytes = std::fs::read(&payload_file).unwrap();
    let observed = retention::inventory(&f.binding.repository.git_directory).unwrap();
    assert_eq!(observed.delta_candidates.len(), 1);
    assert_eq!(
        observed.delta_candidates[&f.intent.operation].binding,
        f.binding_id
    );
    assert_eq!(
        observed.delta_candidates[&f.intent.operation].intent,
        f.intent
    );
    assert!(observed.candidates.is_empty());
    // The hot selection has no obligation to inspect arbitrary other entries;
    // the cold audit must still refuse incomplete unrelated artifacts.
    for _ in 0..100 {
        std::fs::create_dir(
            result
                .directory
                .parent()
                .unwrap()
                .join(uuid::Uuid::new_v4().to_string()),
        )
        .unwrap();
    }
    assert_eq!(
        delta::read_candidate(&f.binding_id, &f.binding, &f.intent).unwrap(),
        Some(f.payload.clone())
    );
    assert_eq!(
        delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).unwrap(),
        result
    );
    assert_eq!(std::fs::read(intent_file).unwrap(), request);
    assert_eq!(std::fs::read(payload_file).unwrap(), bytes);
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
    let mut changed = f.intent.clone();
    changed.key.push_str(" changed");
    assert!(delta::read_candidate(&f.binding_id, &f.binding, &changed).is_err());
}

#[test]
fn partial_delta_retention_is_preserved_and_invalid_payload_creates_no_namespace() {
    let (_home, mut f) = retained_fixture();
    f.payload.format = "invalid".into();
    assert!(delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).is_err());
    assert!(
        std::fs::read_dir(&f.binding.repository.git_directory)
            .unwrap()
            .next()
            .is_none()
    );
    f.payload.format = DELTA_PAYLOAD_FORMAT.into();
    delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).unwrap();
    let file = f.intent.candidate.directory.join("payload.json");
    std::fs::remove_file(&file).unwrap();
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
    let request = std::fs::read(f.intent.candidate.directory.join("intent.pending")).unwrap();
    assert!(delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).is_err());
    assert!(!file.exists());
    std::fs::write(&file, b"partial retained payload").unwrap();
    assert!(delta::read_candidate(&f.binding_id, &f.binding, &f.intent).is_err());
    assert!(delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).is_err());
    assert_eq!(std::fs::read(file).unwrap(), b"partial retained payload");
    assert_eq!(
        std::fs::read(f.intent.candidate.directory.join("intent.pending")).unwrap(),
        request
    );
}

#[test]
fn cold_delta_inventory_refuses_noncanonical_request_and_operation_alias_without_repair() {
    let (_home, f) = retained_fixture();
    delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).unwrap();
    let file = f.intent.candidate.directory.join("intent.pending");
    let original = std::fs::read(&file).unwrap();
    let mut noncanonical = original.clone();
    noncanonical.push(b'\n');
    std::fs::write(&file, &noncanonical).unwrap();
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), noncanonical);
    std::fs::write(&file, &original).unwrap();
    let renamed = f
        .intent
        .candidate
        .directory
        .parent()
        .unwrap()
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::rename(&f.intent.candidate.directory, &renamed).unwrap();
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
    assert_eq!(
        std::fs::read(renamed.join("intent.pending")).unwrap(),
        original
    );
}

#[test]
fn oversized_selected_delta_refuses_before_creating_repository_custody() {
    let (_home, mut f) = retained_fixture();
    let template = f.payload.documents.values().find_map(Clone::clone).unwrap();
    f.payload.documents.clear();
    for _ in 0..40_000 {
        let mut task = template.clone();
        task.id = WorkId::new();
        f.payload.documents.insert(task.id, Some(task));
    }
    let keys = f.payload.documents.keys().copied().collect::<BTreeSet<_>>();
    let DeltaBasis::Incoming { source_changed, .. } = &mut f.intent.basis else {
        panic!("incoming fixture expected")
    };
    *source_changed = keys.clone();
    f.intent.candidate.changed = keys;
    refresh(&mut f);
    f.payload
        .validate(&f.binding_id, &f.binding, &f.intent)
        .unwrap();
    assert!(crate::canonical::canonical_bytes(&f.payload).unwrap().len() > 16 * 1024 * 1024);
    let error =
        delta::retain_candidate(&f.binding_id, &f.binding, &f.intent, &f.payload).unwrap_err();
    assert!(error.to_string().contains("byte bound"), "{error}");
    assert!(
        std::fs::read_dir(&f.binding.repository.git_directory)
            .unwrap()
            .next()
            .is_none()
    );
}
