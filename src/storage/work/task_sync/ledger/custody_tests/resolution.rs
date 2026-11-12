use super::*;
use crate::task_sync::ledger::{OutgoingFailure, OutgoingReconciliation, RetainedCommit};

struct Resolution {
    f: Fixture,
    ledger: Ledger,
    prepared: ObjectId,
    recipe: ObjectId,
    resolved: DeltaIntent,
    commit: RetainedCommit,
    witnesses: [retention::PublicationWitness; 2],
}

fn comparison(label: &str) -> ObjectId {
    CanonicalObject::freeze(&label).unwrap().key().clone()
}

fn commit(operation: uuid::Uuid, label: &str) -> RetainedCommit {
    RetainedCommit {
        oid: comparison(label).as_str()[..40].into(),
        tree: comparison(&format!("tree {label}")).as_str()[..40].into(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: comparison(&format!("raw {label}")),
    }
}

fn retain_commit(f: &Fixture, operation: uuid::Uuid, selected: &RetainedCommit) {
    // These are metadata/custody fixtures, not an actual Git admission proof.
    let directory = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{operation}.json")),
        crate::canonical::canonical_bytes(selected).unwrap(),
    )
    .unwrap();
}

fn append(f: &Fixture, ledger: &mut Ledger, transition: Transition) -> (ObjectId, Event) {
    let previous = ledger.events.iter().max_by_key(|(_, event)| event.sequence);
    let event = Event {
        schema_version: 1,
        binding: f.id.clone(),
        sequence: previous.map_or(1, |(_, event)| event.sequence + 1),
        predecessor: previous.map(|(id, _)| id.clone()),
        actor: f.binding.actor.clone(),
        recorded_at: at(20),
        transition,
    };
    let object = CanonicalObject::mint(&event).unwrap();
    f.store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
        params![object.key().as_str(), EVENT_KIND, object.bytes()]).unwrap();
    ledger.events.insert(object.key().clone(), event.clone());
    view::record(&f.store.connection, ledger, &event).unwrap();
    (object.key().clone(), event)
}

impl Resolution {
    fn new() -> Self {
        let f = Fixture::new();
        let root = &f.binding.repository.git_directory;
        let mut ledger = Ledger {
            bindings: BTreeMap::from([(f.id.clone(), f.binding.clone())]),
            ..Ledger::default()
        };
        append(&f, &mut ledger, Transition::Activated);
        let (prepared, _) = append(
            &f,
            &mut ledger,
            Transition::DeltaPrepared {
                intent: Box::new(f.intent.clone()),
                payload: Box::new(f.payload.clone()),
            },
        );
        let local = commit(f.intent.operation, "local data commit");
        retain_commit(&f, f.intent.operation, &local);
        append(
            &f,
            &mut ledger,
            Transition::DeltaLocalCommitRecorded {
                prepared: prepared.clone(),
                commit: local.clone(),
            },
        );
        let (event_id, event) = append(
            &f,
            &mut ledger,
            Transition::PushStarted {
                prepared: prepared.clone(),
            },
        );
        let first = retention::PublicationWitness {
            operation: f.intent.operation,
            event_id,
            event,
        };
        retention::retain_publication_witness(root, &first).unwrap();
        append(
            &f,
            &mut ledger,
            Transition::PushRejected {
                prepared: prepared.clone(),
                push_started: first.event_id.clone(),
            },
        );
        let mut resolved = f.intent.clone();
        resolved.operation = uuid::Uuid::new_v4();
        resolved.key = resolved.operation.to_string();
        resolved.expected_head = Some(local.oid.clone());
        resolved.candidate.operation = resolved.operation;
        resolved.candidate.directory =
            retention::delta::candidate_directory(root, resolved.operation);
        let DeltaBasis::Outgoing { export_before, .. } = &mut resolved.basis else {
            unreachable!()
        };
        export_before.anchor = PreparedExportAnchor::Prepared {
            event: prepared.clone(),
        };
        export_before.membership = f.intent.candidate.output.clone();
        export_before.cut = f.intent.local_cut.clone();
        let mut payload = f.payload.clone();
        payload.operation = resolved.operation;
        resolved.candidate.content_fingerprint =
            CanonicalObject::freeze(&payload).unwrap().key().clone();
        retention::delta::retain_candidate(&f.id, &f.binding, &resolved, &payload).unwrap();
        let tip = comparison("fetched remote").as_str()[..40].to_owned();
        let mut commit = commit(resolved.operation, "resolved data commit");
        commit.parents = vec![local.oid, tip.clone()];
        retain_commit(&f, resolved.operation, &commit);
        let (resolution, _) = append(
            &f,
            &mut ledger,
            Transition::OutgoingReconciled {
                prepared: prepared.clone(),
                reconciliation: Box::new(OutgoingReconciliation::DeltaResolved {
                    failure: OutgoingFailure::NonFastForward {
                        push_started: first.event_id.clone(),
                        fetched_tip: tip,
                    },
                    resolved: Box::new(resolved.clone()),
                    payload: Box::new(payload),
                    commit: Box::new(commit.clone()),
                }),
            },
        );
        let (event_id, event) = append(
            &f,
            &mut ledger,
            Transition::OutgoingReconciled {
                prepared: prepared.clone(),
                reconciliation: Box::new(OutgoingReconciliation::PushStarted {
                    reconciliation: resolution.clone(),
                }),
            },
        );
        let retry = retention::PublicationWitness {
            operation: resolved.operation,
            event_id,
            event,
        };
        retention::retain_publication_witness(root, &retry).unwrap();
        Self {
            f,
            ledger,
            prepared,
            recipe: resolution,
            resolved,
            commit,
            witnesses: [first, retry],
        }
    }

    fn cold(&self) -> Result<(), StoreError> {
        let root = &self.f.binding.repository.git_directory;
        custody::check(
            &self.ledger,
            root,
            &custody::Observed::capture(root)?,
            None,
            |a, b| a == b,
        )
    }

    fn selected(&self) -> Result<(), StoreError> {
        let root = &self.f.binding.repository.git_directory;
        custody::check_on(
            &self.f.store.connection,
            root,
            &custody::Observed::capture(root)?,
            None,
            None,
            None,
            &mut view::read::ReadBudget::default(),
        )
    }

    fn artifacts(&self) -> Result<Vec<CustodyArtifactRef>, StoreError> {
        audit::artifacts(
            &self.f.store.connection,
            &self.ledger,
            &BTreeSet::from([self.f.binding.repository.git_directory.clone()]),
            None,
        )
    }

    fn witness_path(&self, operation: uuid::Uuid) -> std::path::PathBuf {
        self.f
            .binding
            .repository
            .git_directory
            .join("engram-task-sync-publication-attempts")
            .join(format!("{operation}.json"))
    }
}

#[test]
fn resolved_delta_custody_accounts_for_both_candidates_commits_and_publication_witnesses() {
    let r = Resolution::new();
    let before = state(&r.f.store);
    r.cold().unwrap();
    r.selected().unwrap();
    let artifacts = r.artifacts().unwrap();
    assert_eq!(artifacts.len(), 6);
    for (operation, record) in [
        (r.f.intent.operation, &r.prepared),
        (r.resolved.operation, &r.recipe),
    ] {
        let candidate = artifacts
            .iter()
            .find(|entry| {
                entry.operation == operation && entry.kind == CustodyArtifactKind::Candidate
            })
            .unwrap();
        assert_eq!(candidate.record.as_ref(), Some(record));
        let commit = artifacts
            .iter()
            .find(|entry| entry.operation == operation && entry.kind == CustodyArtifactKind::Commit)
            .unwrap();
        assert_eq!(commit.record.as_ref(), Some(record));
    }
    for witness in &r.witnesses {
        assert_eq!(
            retention::read_publication_witness(
                &r.f.binding.repository.git_directory,
                witness.operation
            )
            .unwrap()
            .as_ref(),
            Some(witness)
        );
    }
    assert_eq!(state(&r.f.store), before);
}

#[test]
fn resolved_delta_custody_requires_original_and_retry_witness_without_repair() {
    for index in [0, 1] {
        let r = Resolution::new();
        let path = r.witness_path(r.witnesses[index].operation);
        std::fs::remove_file(&path).unwrap();
        let before = state(&r.f.store);
        assert!(r.cold().is_err());
        assert!(r.selected().is_err());
        assert!(!path.exists(), "a read must not recreate a missing witness");
        assert_eq!(state(&r.f.store), before);
        assert!(
            retention::delta::read_candidate(&r.f.id, &r.f.binding, &r.resolved)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn resolved_delta_custody_preserves_retained_retry_after_its_canonical_recipe_is_lost() {
    let mut r = Resolution::new();
    let path = r.witness_path(r.resolved.operation);
    let bytes = std::fs::read(&path).unwrap();
    r.f.store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_id=?1",
            [r.recipe.as_str()],
        )
        .unwrap();
    r.ledger.events.remove(&r.recipe);
    let before = state(&r.f.store);
    assert!(r.cold().is_err());
    assert!(r.selected().is_err());
    assert!(r.artifacts().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        retention::commit_selection(&r.f.binding.repository.git_directory, r.resolved.operation)
            .unwrap(),
        Some(r.commit)
    );
    assert_eq!(state(&r.f.store), before);
}

#[test]
fn resolved_delta_audit_refuses_changed_commit_metadata_and_keeps_the_original_bytes() {
    let r = Resolution::new();
    let root = &r.f.binding.repository.git_directory;
    let original = retention::commit_selection(root, r.f.intent.operation).unwrap();
    let mut changed = r.commit.clone();
    changed.tree = comparison("changed resolved tree").as_str()[..40].into();
    retain_commit(&r.f, r.resolved.operation, &changed);
    let before = state(&r.f.store);
    assert!(r.artifacts().is_err());
    assert_eq!(
        retention::commit_selection(root, r.f.intent.operation).unwrap(),
        original
    );
    assert_eq!(
        retention::commit_selection(root, r.resolved.operation).unwrap(),
        Some(changed)
    );
    assert_eq!(state(&r.f.store), before);
}
