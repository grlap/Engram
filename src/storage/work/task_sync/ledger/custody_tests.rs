use super::*;
use crate::storage::work::{
    task_sync::tests::{ledger::binding, state},
    test_support::at,
};
use crate::task_sync::{delta::*, inventory::InventoryRoot, retention};

mod resolution;

struct Fixture {
    _home: crate::test_support::TempHome,
    store: SqliteStore,
    binding: Binding,
    id: ObjectId,
    intent: DeltaIntent,
    payload: DeltaCandidatePayload,
}

impl Fixture {
    fn new() -> Self {
        Self::with_retention(true)
    }

    fn with_retention(retain: bool) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let mut binding = binding(home.path());
        binding.store.origin = Some(ObjectId::mint());
        std::fs::create_dir_all(&binding.repository.git_directory).unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        let object = CanonicalObject::mint(&binding).unwrap();
        let id = object.key().clone();
        // Only a binding is stored. Canonical delta admission remains disabled.
        store.connection.execute(
            "INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
            params![id.as_str(),BINDING_KIND,object.bytes()],
        ).unwrap();
        let cut = OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: None,
            position: crate::FeedPosition {
                feed: crate::FeedId::Project(binding.project.clone()),
                position: 0,
            },
            anchor: None,
        };
        let operation = uuid::Uuid::new_v4();
        let output = OutputMembership {
            inventory_root: InventoryRoot::ZERO,
            profile: binding.profile.clone(),
        };
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: binding.project.clone(),
            operation,
            output: output.clone(),
            documents: BTreeMap::new(),
        };
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction: Direction::Outgoing,
            actor: binding.actor.clone(),
            producer: CanonicalObject::freeze(&"producer").unwrap().key().clone(),
            expected_head: None,
            common_commit: None,
            local_cut: cut.clone(),
            basis: DeltaBasis::Outgoing {
                export_before: Box::new(PreparedExportBasis {
                    anchor: PreparedExportAnchor::Bootstrap {
                        checkpoint: ObjectId::mint(),
                    },
                    cut,
                    membership: output.clone(),
                }),
                confirmed_before: None,
            },
            candidate: RetainedDeltaCandidate {
                operation,
                directory: retention::delta::candidate_directory(
                    &binding.repository.git_directory,
                    operation,
                ),
                content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
                output,
                changed: BTreeSet::new(),
            },
        };
        if retain {
            retention::delta::retain_candidate(&id, &binding, &intent, &payload).unwrap();
        }
        Self {
            _home: home,
            store,
            binding,
            id,
            intent,
            payload,
        }
    }

    fn legacy_candidate(&self) -> crate::task_sync::ledger::RetainedCandidate {
        let mut tree = crate::storage::work::task_sync::tests::candidate();
        tree.tasks.clear();
        let prepared = crate::task_sync::progress::PreparedCandidate {
            documents: tree.documents().unwrap(),
            tree,
            provenance: crate::task_sync::progress::PreparedProvenance {
                sequence: 1,
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
                store: self.binding.store.database.clone(),
                destination: self.binding.repository.main_checkout.join("prepared"),
                project: self.binding.project.clone(),
                profile: self.binding.profile.clone(),
                producer: self.intent.producer.clone(),
                cut: self.binding.store.cut.clone(),
                omissions: crate::task_sync::notes::NoteOmissions::default(),
                activity_omissions: crate::task_sync::notes::NoteOmissions::default(),
            },
        };
        retention::retain_prepared(
            &self.binding.repository.git_directory,
            self.intent.operation,
            &prepared,
        )
        .unwrap()
    }

    fn check(&self, request: Option<&ObjectId>) -> Result<(), StoreError> {
        custody::check_on(
            &self.store.connection,
            &self.binding.repository.git_directory,
            &custody::Observed::capture(&self.binding.repository.git_directory)?,
            Some(self.intent.operation),
            request,
            None,
            &mut view::read::ReadBudget::default(),
        )
    }

    fn ledger(&self) -> (Ledger, ObjectId) {
        let prepared = ObjectId::mint();
        let event = Event {
            schema_version: 1,
            binding: self.id.clone(),
            sequence: 2,
            predecessor: None,
            actor: self.binding.actor.clone(),
            recorded_at: at(20),
            transition: Transition::DeltaPrepared {
                intent: Box::new(self.intent.clone()),
                payload: Box::new(self.payload.clone()),
            },
        };
        // Pure custody fixture: it grants no reducer, writer or receipt credit.
        (
            Ledger {
                bindings: BTreeMap::from([(self.id.clone(), self.binding.clone())]),
                events: BTreeMap::from([(prepared.clone(), event)]),
                bytes: 0,
            },
            prepared,
        )
    }
}

#[test]
fn delta_custody_requires_the_exact_typed_request_before_any_mutation() {
    let f = Fixture::new();
    let before = state(&f.store);
    let legacy = origin_request("activate", &f.binding, None, None, None).unwrap();
    assert!(f.check(None).is_err());
    assert!(f.check(Some(&legacy)).is_err());
    let exact = retention::delta::request_fingerprint(&f.id, &f.intent).unwrap();
    f.check(Some(&exact)).unwrap();
    let mut changed = f.intent.clone();
    changed.key.push_str(" changed");
    let different = retention::delta::request_fingerprint(&f.id, &changed).unwrap();
    assert!(f.check(Some(&different)).is_err());
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(f.payload)
    );
}

#[test]
fn delta_custody_cold_audit_matches_canonical_intent_and_preserves_unknown_artifacts() {
    let f = Fixture::new();
    let observed = custody::Observed::capture(&f.binding.repository.git_directory).unwrap();
    let (mut ledger, prepared) = f.ledger();
    let root = &f.binding.repository.git_directory;
    custody::check(&ledger, root, &observed, None, |a, b| a == b).unwrap();
    let roots = BTreeSet::from([root.clone()]);
    let refs = audit::artifacts(&f.store.connection, &ledger, &roots, None).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].kind, CustodyArtifactKind::Candidate);
    assert_eq!(refs[0].record, Some(prepared.clone()));
    assert_eq!(
        refs[0].content_fingerprint,
        f.intent.candidate.content_fingerprint
    );
    let Transition::DeltaPrepared { intent, .. } =
        &mut ledger.events.get_mut(&prepared).unwrap().transition
    else {
        panic!("delta fixture")
    };
    intent.key.push_str(" changed");
    assert!(
        custody::check(
            &ledger,
            root,
            &observed,
            Some(f.intent.operation),
            |a, b| a == b
        )
        .is_err()
    );
    assert!(audit::artifacts(&f.store.connection, &ledger, &roots, None).is_err());
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(f.payload)
    );
}

#[test]
fn delta_custody_refuses_a_second_legacy_candidate_for_the_same_operation() {
    let f = Fixture::new();
    let legacy = f.legacy_candidate();
    let before = state(&f.store);
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
    assert!(
        f.check(Some(
            &retention::delta::request_fingerprint(&f.id, &f.intent).unwrap()
        ))
        .is_err()
    );
    assert!(legacy.directory.is_dir());
    assert_eq!(state(&f.store), before);
}

#[test]
fn delta_custody_known_delta_never_admits_legacy_bytes_by_operation_uuid() {
    let f = Fixture::with_retention(false);
    let legacy = f.legacy_candidate();
    let root = &f.binding.repository.git_directory;
    let observed = custody::Observed::capture(root).unwrap();
    let (ledger, _) = f.ledger();
    let before = state(&f.store);
    let error = custody::check(
        &ledger,
        root,
        &observed,
        Some(f.intent.operation),
        |a, b| a == b,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("legacy retained artifact has no matching legacy preparation"),
        "{error}"
    );
    assert!(
        audit::artifacts(
            &f.store.connection,
            &ledger,
            &BTreeSet::from([root.clone()]),
            None,
        )
        .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert!(legacy.directory.is_dir());
}

#[test]
fn delta_custody_audit_accounts_for_exact_commit_and_publication_witness() {
    let f = Fixture::new();
    let (mut ledger, prepared) = f.ledger();
    let fingerprint = CanonicalObject::freeze(&"selected commit")
        .unwrap()
        .key()
        .clone();
    let mut commit = crate::task_sync::ledger::RetainedCommit {
        oid: fingerprint.as_str()[..40].into(),
        tree: fingerprint.as_str()[..40].into(),
        parents: Vec::new(),
        retention_ref: format!(
            "refs/engram/task-sync/exchange/{}/commit",
            f.intent.operation
        ),
        raw_bytes_fingerprint: fingerprint,
    };
    let root = &f.binding.repository.git_directory;
    let selections = root.join("engram-task-sync-commits");
    std::fs::create_dir_all(&selections).unwrap();
    let file = selections.join(format!("{}.json", f.intent.operation));
    std::fs::write(&file, crate::canonical::canonical_bytes(&commit).unwrap()).unwrap();
    let commit_id = ObjectId::mint();
    ledger.events.insert(
        commit_id.clone(),
        Event {
            schema_version: 1,
            binding: f.id.clone(),
            sequence: 3,
            predecessor: Some(prepared.clone()),
            actor: f.binding.actor.clone(),
            recorded_at: at(21),
            transition: Transition::DeltaLocalCommitRecorded {
                prepared: prepared.clone(),
                commit: commit.clone(),
            },
        },
    );
    let publication = Event {
        schema_version: 1,
        binding: f.id.clone(),
        sequence: 4,
        predecessor: Some(commit_id),
        actor: f.binding.actor.clone(),
        recorded_at: at(22),
        transition: Transition::PushStarted { prepared },
    };
    let witness = retention::PublicationWitness {
        operation: f.intent.operation,
        event_id: ObjectId::mint(),
        event: publication,
    };
    retention::retain_publication_witness(root, &witness).unwrap();
    ledger
        .events
        .insert(witness.event_id.clone(), witness.event.clone());
    let observed = custody::Observed::capture(root).unwrap();
    custody::check(&ledger, root, &observed, None, |a, b| a == b).unwrap();
    let roots = BTreeSet::from([root.clone()]);
    let refs = audit::artifacts(&f.store.connection, &ledger, &roots, None).unwrap();
    assert_eq!(
        refs.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
        vec![
            CustodyArtifactKind::Candidate,
            CustodyArtifactKind::Commit,
            CustodyArtifactKind::PublicationWitness,
        ]
    );
    commit.tree = CanonicalObject::freeze(&"another tree")
        .unwrap()
        .key()
        .as_str()[..40]
        .into();
    std::fs::write(&file, crate::canonical::canonical_bytes(&commit).unwrap()).unwrap();
    assert!(audit::artifacts(&f.store.connection, &ledger, &roots, None).is_err());
    assert_eq!(
        std::fs::read(&file).unwrap(),
        crate::canonical::canonical_bytes(&commit).unwrap()
    );
}
