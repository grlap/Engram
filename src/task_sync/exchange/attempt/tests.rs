use super::*;
use crate::{
    ActorContext, CanonicalObject, ProjectId, SessionId,
    task_sync::{
        TaskTree,
        ledger::{
            Binding, Direction, Intent, RepositoryBasis, RetainedCommit, StoreBasis, TargetBasis,
        },
        notes::NoteOmissions,
        progress::{ExportCut, ExportProfile, PreparedCandidate, PreparedProvenance},
        retention,
    },
};
use std::{collections::VecDeque, fs, future::Future, path::Path};

mod git_workflow;
mod refusal;

// Fixture effects stay lazy, as in the async trait contract, but complete on
// their first poll because these test backends have no asynchronous work.
fn ready_on_poll<T>(effect: impl FnOnce() -> T) -> impl Future<Output = T> {
    let mut effect = Some(effect);
    std::future::poll_fn(move |_| {
        std::task::Poll::Ready(effect
            .take()
            .expect("fixture future polled after completion")(
        ))
    })
}

fn fingerprint(value: &impl serde::Serialize) -> ObjectId {
    CanonicalObject::freeze(value).unwrap().key().clone()
}

fn binding(home: &Path) -> Binding {
    Binding {
        schema_version: 1,
        project: ProjectId("publication-fixture".into()),
        store: StoreBasis {
            database: home.join("work.db"),
            origin: None,
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: home.join("checkout"),
            git_directory: home.join("checkout/.git"),
        },
        branch: "refs/heads/engram-data".into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/engram-data".into(),
            configuration_fingerprint: fingerprint(&"fixture target"),
        },
        profile: ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        supersedes: None,
        superseded_tail: None,
        actor: ActorContext {
            actor_id: "publication-fixture".into(),
            actor_kind: "agent".into(),
            session_id: Some(SessionId("publication-fixture".into())),
            assurance: crate::domain::AssuranceLevel::Asserted,
            run_id: None,
            source_tool: Some("publication_fixture".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "test retained publication dispatch".into(),
        },
        created_at: chrono::Utc::now(),
    }
}

// These exercise real canonical writes, private retention and keyed reads.
// Only the bounded Git observations/transport are substituted, as in the
// storage transaction fixtures; real Git wire classification is tested beside it.
fn input(store: &SqliteStore, id: &ObjectId, binding: &Binding) -> InspectedExchange {
    fs::create_dir_all(&binding.repository.git_directory).unwrap();
    let operation = uuid::Uuid::new_v4();
    let producer = fingerprint(&"fixture producer");
    let tree: TaskTree = store.export_task_tree(&binding.project).unwrap();
    let documents = tree.documents().unwrap();
    let prepared = PreparedCandidate {
        documents,
        tree: tree.clone(),
        provenance: PreparedProvenance {
            sequence: 1,
            operation: uuid::Uuid::new_v4(),
            predecessor: None,
            store: binding.store.database.clone(),
            destination: binding.repository.main_checkout.join("prepared"),
            project: binding.project.clone(),
            profile: binding.profile.clone(),
            producer: producer.clone(),
            cut: binding.store.cut.clone(),
            omissions: NoteOmissions::default(),
            activity_omissions: NoteOmissions::default(),
        },
    };
    let candidate =
        retention::retain_prepared(&binding.repository.git_directory, operation, &prepared)
            .unwrap();
    let commit = RetainedCommit {
        oid: fingerprint(&(operation, "commit")).to_string(),
        tree: fingerprint(&tree).to_string(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: fingerprint(&(operation, "raw commit")),
    };
    let selected = InspectedExchange {
        binding_id: id.clone(),
        binding: binding.clone(),
        tree,
        intent: Intent {
            operation,
            key: operation.to_string(),
            direction: Direction::Outgoing,
            actor: binding.actor.clone(),
            producer,
            expected_head: None,
            common_commit: None,
            candidate,
            commit,
            local_planning_basis: crate::task_sync::progress::documents_fingerprint(
                &prepared.documents,
            )
            .parse()
            .unwrap(),
        },
    };
    save_commit(&selected);
    selected
}

fn save_commit(input: &InspectedExchange) {
    let root = input
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(format!("{}.json", input.intent.operation)),
        crate::canonical::canonical_bytes(&input.intent.commit).unwrap(),
    )
    .unwrap();
}

#[derive(Default)]
struct Io {
    tips: VecDeque<Option<String>>,
    ancestry: VecDeque<bool>,
    results: VecDeque<std::result::Result<(), RunError>>,
    pushes: usize,
    fetched: Vec<String>,
    /// Fail this many fetches, as a lost network fetch does, before succeeding.
    fetch_failures: usize,
    start_elsewhere: bool,
}

impl AttemptIo for Io {
    fn revalidate(&mut self, input: &InspectedExchange) -> impl Future<Output = Result<()>> {
        ready_on_poll(move || {
            if std::mem::take(&mut self.start_elsewhere) {
                let mut competing = SqliteStore::open(&input.binding.store.database)?;
                let (_, fresh) = competing.record_task_sync_exchange_progress_once(
                    input,
                    OutgoingProgress::PushStarted,
                    chrono::Utc::now(),
                )?;
                assert!(fresh);
            }
            input.recheck_files()
        })
    }
    fn tip(&mut self) -> impl Future<Output = Result<Option<String>>> {
        ready_on_poll(move || Ok(self.tips.pop_front().unwrap_or(None)))
    }
    fn fetch(&mut self, tip: &str) -> impl Future<Output = Result<String>> {
        ready_on_poll(move || {
            if self.fetch_failures > 0 {
                self.fetch_failures -= 1;
                anyhow::bail!("fixture fetch failed");
            }
            self.fetched.push(tip.into());
            Ok(format!("refs/engram-exchange/fixture/{tip}"))
        })
    }
    fn ancestor(&mut self, _: &str, _: &str) -> impl Future<Output = Result<bool>> {
        ready_on_poll(move || Ok(self.ancestry.pop_front().unwrap_or(true)))
    }
    fn push(&mut self, _: &str) -> impl Future<Output = std::result::Result<(), RunError>> {
        ready_on_poll(move || {
            self.pushes += 1;
            self.results.pop_front().unwrap_or(Ok(()))
        })
    }
}

fn prepare(store: &mut SqliteStore, selected: &InspectedExchange) -> ObjectId {
    let id = store
        .prepare_task_sync_exchange(selected, chrono::Utc::now())
        .unwrap();
    store
        .record_task_sync_exchange_progress(
            selected,
            OutgoingProgress::LocalCommitRecorded,
            chrono::Utc::now(),
        )
        .unwrap();
    id
}

#[tokio::test]
async fn uncertain_push_replays_only_observation_and_can_confirm_later_without_resending() {
    let home = crate::test_support::temp_home().unwrap();
    let binding = binding(home.path());
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding);
    prepare(&mut store, &selected);
    let request = CompletedExchangeRequest {
        binding: id,
        intent: selected.intent.clone(),
    };
    let mut io = Io {
        results: VecDeque::from([Err(RunError::Deadline)]),
        ..Io::default()
    };
    let first = publish_with(&mut store, &selected, &request, &mut io)
        .await
        .unwrap();
    assert!(matches!(first, PublicationAttempt::Unknown { .. }));
    assert_eq!(io.pushes, 1);
    drop(store);
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let witness = retention::read_publication_witness(
        &binding.repository.git_directory,
        selected.intent.operation,
    )
    .unwrap();
    assert_eq!(
        publish_with(&mut store, &selected, &request, &mut io)
            .await
            .unwrap(),
        first
    );
    assert_eq!(io.pushes, 1);
    io.tips.push_back(Some(selected.intent.commit.oid.clone()));
    let confirmed = publish_with(&mut store, &selected, &request, &mut io)
        .await
        .unwrap();
    assert!(matches!(confirmed, PublicationAttempt::Confirmed { .. }));
    assert_eq!(io.pushes, 1);
    assert_eq!(
        retention::read_publication_witness(
            &binding.repository.git_directory,
            selected.intent.operation
        )
        .unwrap(),
        witness
    );
    assert_eq!(
        publish_with(&mut store, &selected, &request, &mut io)
            .await
            .unwrap(),
        confirmed
    );
    assert_eq!(io.pushes, 1);
}

#[tokio::test]
async fn both_known_failures_allow_one_resolved_recipe_and_stop_after_a_second_failure() {
    for first_push in [false, true] {
        for terminal in ["confirmed", "stopped", "unknown"] {
            for second_push in [false, true] {
                let second_failure = terminal == "stopped";
                let home = crate::test_support::temp_home().unwrap();
                let binding = binding(home.path());
                let mut store = SqliteStore::open(&binding.store.database).unwrap();
                let id = store.activate_task_sync_binding(&binding).unwrap();
                let original = input(&store, &id, &binding);
                let prepared = prepare(&mut store, &original);
                let request = CompletedExchangeRequest {
                    binding: id.clone(),
                    intent: original.intent.clone(),
                };
                let tip = fingerprint(&"competing data tip").to_string();
                let mut io = if first_push {
                    Io {
                        tips: VecDeque::from([None, Some(tip.clone())]),
                        results: VecDeque::from([Err(RunError::NonFastForward)]),
                        ..Io::default()
                    }
                } else {
                    Io {
                        tips: VecDeque::from([Some(tip.clone())]),
                        ancestry: VecDeque::from([false]),
                        ..Io::default()
                    }
                };
                let PublicationAttempt::ReconciliationRequired { failure, .. } =
                    publish_with(&mut store, &original, &request, &mut io)
                        .await
                        .unwrap()
                else {
                    panic!("known failure must request reconciliation");
                };
                assert_eq!(
                    matches!(failure, OutgoingFailure::NonFastForward { .. }),
                    first_push
                );
                assert_eq!(io.pushes, usize::from(first_push));
                assert_eq!(io.fetched.as_slice(), std::slice::from_ref(&tip));
                let mut resolved = input(&store, &id, &binding);
                resolved.intent.expected_head = Some(original.intent.commit.oid.clone());
                resolved.intent.commit.parents =
                    vec![original.intent.commit.oid.clone(), tip.clone()];
                save_commit(&resolved);
                store
                    .record_task_sync_exchange_progress(
                        &resolved,
                        OutgoingProgress::Reconciled {
                            prepared,
                            reconciliation: Box::new(OutgoingReconciliation::Resolved {
                                failure,
                                resolved: Box::new(resolved.intent.clone()),
                            }),
                        },
                        chrono::Utc::now(),
                    )
                    .unwrap();
                drop(store);
                let mut store = SqliteStore::open(&binding.store.database).unwrap();
                let next_tip = fingerprint(&"next competing data tip").to_string();
                let before = io.pushes;
                if second_failure && !second_push {
                    io.tips.push_back(Some(next_tip.clone()));
                    io.ancestry.push_back(false);
                } else {
                    io.tips.push_back(Some(tip.clone()));
                    io.ancestry.push_back(true);
                    if second_failure {
                        io.results.push_back(Err(RunError::NonFastForward));
                        io.tips.push_back(Some(next_tip));
                    } else if terminal == "unknown" {
                        io.results.push_back(Err(RunError::Process));
                        io.tips.push_back(None);
                    } else {
                        io.tips.push_back(Some(resolved.intent.commit.oid.clone()));
                    }
                }
                let result = publish_with(&mut store, &resolved, &request, &mut io)
                    .await
                    .unwrap();
                assert_eq!(
                    matches!(result, PublicationAttempt::Stopped { .. }),
                    second_failure
                );
                assert_eq!(
                    matches!(result, PublicationAttempt::Confirmed { .. }),
                    terminal == "confirmed"
                );
                assert_eq!(
                    matches!(result, PublicationAttempt::Unknown { .. }),
                    terminal == "unknown"
                );
                assert_eq!(
                    io.pushes - before,
                    usize::from(!second_failure || second_push)
                );
                let pushes = io.pushes;
                assert_eq!(
                    publish_with(&mut store, &resolved, &request, &mut io)
                        .await
                        .unwrap(),
                    result
                );
                assert_eq!(io.pushes, pushes);
                assert!(store.verify_all().unwrap().is_healthy());
            }
        }
    }
}

#[tokio::test]
async fn competing_start_between_read_and_append_does_not_dispatch_the_same_push_twice() {
    let home = crate::test_support::temp_home().unwrap();
    let binding = binding(home.path());
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding);
    prepare(&mut store, &selected);
    let request = CompletedExchangeRequest {
        binding: id,
        intent: selected.intent.clone(),
    };
    let mut io = Io {
        start_elsewhere: true,
        ..Io::default()
    };
    assert!(matches!(
        publish_with(&mut store, &selected, &request, &mut io)
            .await
            .unwrap(),
        PublicationAttempt::Unknown { .. }
    ));
    assert_eq!(io.pushes, 0);
}

#[tokio::test]
async fn committed_push_start_after_a_lost_response_never_dispatches_again() {
    let home = crate::test_support::temp_home().unwrap();
    let binding = binding(home.path());
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding);
    prepare(&mut store, &selected);
    let progress = OutgoingProgress::PushStarted;
    let (start, fresh) = store
        .record_task_sync_exchange_progress_once(&selected, progress.clone(), chrono::Utc::now())
        .unwrap();
    assert!(fresh);
    drop(store);
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    assert_eq!(
        store
            .record_task_sync_exchange_progress_once(&selected, progress, chrono::Utc::now())
            .unwrap(),
        (start, false)
    );
    let request = CompletedExchangeRequest {
        binding: id,
        intent: selected.intent.clone(),
    };
    let mut io = Io::default();
    assert!(matches!(
        publish_with(&mut store, &selected, &request, &mut io)
            .await
            .unwrap(),
        PublicationAttempt::Unknown { .. }
    ));
    assert_eq!(io.pushes, 0);
}

#[test]
fn selected_tip_requires_one_complete_exact_ref_and_never_exposes_other_output() {
    let tip = fingerprint(&"selected remote data commit").to_string();
    let reference = "refs/heads/engram-data";
    assert_eq!(selected_tip(b"", reference).unwrap(), None);
    assert_eq!(
        selected_tip(format!("{tip}\t{reference}\n").as_bytes(), reference).unwrap(),
        Some(tip.clone())
    );
    for text in [
        format!("{tip}\t{reference}"),
        format!("{tip}\trefs/heads/other\n"),
        format!("{tip}\t{reference}\n{tip}\t{reference}\n"),
        format!("secret\t{reference}\n"),
    ] {
        let error = selected_tip(text.as_bytes(), reference)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
    }
}
