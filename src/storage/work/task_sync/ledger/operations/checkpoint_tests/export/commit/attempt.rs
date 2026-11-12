use super::*;
use crate::task_sync::{
    exchange::{
        Cancellation, DeltaOutgoingSelection, PublicationAttempt, RunError, RunLimits,
        delta::attempt::{DeltaAttemptIo, publish_with},
        publish_delta_outgoing,
    },
    ledger::{OutgoingFailure, OutgoingProgress},
};
use std::future::Future;

#[derive(Clone, Copy)]
pub(super) enum PushOutcome {
    Confirm,
    Unknown,
    NonFastForward,
}

pub(super) struct Io {
    pub(super) tip: Option<String>,
    after_push: PushOutcome,
    pub(super) ancestor: bool,
    pub(super) refuse: bool,
    pub(super) pushes: usize,
    pub(super) observations: usize,
    /// Fail this many fetches, as a lost network fetch does, before succeeding.
    pub(super) fetch_failures: usize,
}

fn ready_on_poll<T>(effect: impl FnOnce() -> T) -> impl Future<Output = T> {
    let mut effect = Some(effect);
    std::future::poll_fn(move |_| {
        std::task::Poll::Ready(effect
            .take()
            .expect("fixture future polled after completion")(
        ))
    })
}

impl Io {
    pub(super) fn new(after_push: PushOutcome) -> Self {
        Self {
            tip: None,
            after_push,
            ancestor: true,
            refuse: false,
            pushes: 0,
            observations: 0,
            fetch_failures: 0,
        }
    }
}

impl DeltaAttemptIo for Io {
    fn revalidate(
        &mut self,
        input: &InspectedDelta,
        _commit: &DeltaCommitSelection,
    ) -> impl Future<Output = anyhow::Result<()>> {
        ready_on_poll(move || {
            anyhow::ensure!(!self.refuse, "fixture selection drift");
            input.recheck_candidate()
        })
    }
    fn tip(&mut self) -> impl Future<Output = anyhow::Result<Option<String>>> {
        ready_on_poll(move || {
            self.observations += 1;
            Ok(self.tip.clone())
        })
    }
    fn fetch(&mut self, tip: &str) -> impl Future<Output = anyhow::Result<String>> {
        let tip = tip.to_owned();
        ready_on_poll(move || {
            if self.fetch_failures > 0 {
                self.fetch_failures -= 1;
                anyhow::bail!("fixture fetch failed");
            }
            Ok(format!("refs/engram-exchange/fixture/{tip}"))
        })
    }
    fn ancestor(
        &mut self,
        _tip: &str,
        _commit: &str,
    ) -> impl Future<Output = anyhow::Result<bool>> {
        ready_on_poll(move || Ok(self.ancestor))
    }
    fn push(&mut self, commit: &str) -> impl Future<Output = std::result::Result<(), RunError>> {
        let commit = commit.to_owned();
        ready_on_poll(move || {
            self.pushes += 1;
            match self.after_push {
                PushOutcome::Confirm => {
                    self.tip = Some(commit);
                    Ok(())
                }
                PushOutcome::Unknown => Ok(()),
                PushOutcome::NonFastForward => {
                    self.tip = Some(
                        CanonicalObject::freeze(&"competing push")
                            .unwrap()
                            .key()
                            .to_string(),
                    );
                    Err(RunError::NonFastForward)
                }
            }
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_dispatch_confirms_once_and_replays_the_original_outcome_offline() {
    let mut f = ExportFixture::new(2);
    let (input, commit) = progress::committed(&mut f);
    let mut io = Io::new(PushOutcome::Confirm);
    let outcome = publish_with(&mut f.store, &input, &commit, &mut io)
        .await
        .unwrap();
    assert!(matches!(&outcome, PublicationAttempt::Confirmed { .. }));
    assert_eq!(io.pushes, 1);
    assert_eq!(io.observations, 2);
    let before = state(&f.store);
    // Canonical completion lookup must not need retained files or Git.
    std::fs::remove_file(input.intent.candidate.directory.join("payload.json")).unwrap();
    assert_eq!(
        publish_delta_outgoing(
            &mut f.store,
            &DeltaOutgoingSelection {
                binding_id: &f.id,
                binding: &f.binding,
                intent: &f.intent,
                commit: &commit,
            },
            RunLimits::default(),
            &Cancellation::default(),
        )
        .await
        .unwrap(),
        outcome
    );
    assert_eq!(state(&f.store), before);
}

#[tokio::test(flavor = "current_thread")]
async fn delta_dispatch_unknown_and_existing_start_only_observe_after_reopen() {
    for started_only in [false, true] {
        let mut f = ExportFixture::new(2);
        let (input, commit) = progress::committed(&mut f);
        if started_only {
            f.store
                .record_task_sync_delta_progress_once(
                    &input,
                    &commit,
                    OutgoingProgress::PushStarted,
                    at(42),
                )
                .unwrap();
        } else {
            let mut io = Io::new(PushOutcome::Unknown);
            let outcome = publish_with(&mut f.store, &input, &commit, &mut io)
                .await
                .unwrap();
            assert!(matches!(outcome, PublicationAttempt::Unknown { .. }));
            assert_eq!(io.pushes, 1);
        }
        drop(f.store);
        let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
        let mut io = Io::new(PushOutcome::Confirm);
        let outcome = publish_with(&mut store, &input, &commit, &mut io)
            .await
            .unwrap();
        assert!(matches!(outcome, PublicationAttempt::Unknown { .. }));
        assert_eq!(io.pushes, 0);
        io.tip = Some(
            CanonicalObject::freeze(&"different observed head")
                .unwrap()
                .key()
                .to_string(),
        );
        assert!(matches!(
            publish_with(&mut store, &input, &commit, &mut io)
                .await
                .unwrap(),
            PublicationAttempt::Unknown { .. }
        ));
        assert_eq!(io.pushes, 0);
        io.tip = Some(commit.commit.oid.clone());
        let outcome = publish_with(&mut store, &input, &commit, &mut io)
            .await
            .unwrap();
        assert!(matches!(outcome, PublicationAttempt::Confirmed { .. }));
        assert_eq!(io.pushes, 0);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_dispatch_known_ancestry_and_non_fast_forward_request_agent_reconciliation() {
    for ancestry in [true, false] {
        let mut f = ExportFixture::new(2);
        let (input, commit) = progress::committed(&mut f);
        let mut io = Io::new(PushOutcome::NonFastForward);
        if ancestry {
            io.tip = Some(
                CanonicalObject::freeze(&"prior competing head")
                    .unwrap()
                    .key()
                    .to_string(),
            );
            io.ancestor = false;
        }
        let before = state(&f.store);
        let outcome = publish_with(&mut f.store, &input, &commit, &mut io)
            .await
            .unwrap();
        let PublicationAttempt::ReconciliationRequired {
            failure,
            fetched_ref,
        } = outcome
        else {
            panic!("known failure must return agent reconciliation")
        };
        assert!(fetched_ref.starts_with("refs/engram-exchange/fixture/"));
        if ancestry {
            assert!(matches!(failure, OutgoingFailure::AncestryChanged { .. }));
            assert_eq!(io.pushes, 0);
            assert_eq!(state(&f.store), before);
        } else {
            assert!(matches!(failure, OutgoingFailure::NonFastForward { .. }));
            assert_eq!(io.pushes, 1);
            // The recorded refusal stays known on an ordinary repeat: the same
            // failure again, from a fetch only, and never a second push.
            let PublicationAttempt::ReconciliationRequired {
                failure: repeated, ..
            } = publish_with(&mut f.store, &input, &commit, &mut io)
                .await
                .unwrap()
            else {
                panic!("a recorded refusal must request reconciliation again")
            };
            assert_eq!(repeated, failure);
            assert_eq!(io.pushes, 1);
        }
        assert!(f.store.verify_all().unwrap().is_healthy());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_dispatch_refuses_drift_and_changed_commit_before_start_or_transport() {
    let mut f = ExportFixture::new(2);
    let (input, commit) = progress::committed(&mut f);
    let before = state(&f.store);
    let mut io = Io::new(PushOutcome::Confirm);
    io.refuse = true;
    assert!(
        publish_with(&mut f.store, &input, &commit, &mut io)
            .await
            .is_err()
    );
    let mut changed = commit.clone();
    changed.commit.tree = CanonicalObject::freeze(&"changed tree")
        .unwrap()
        .key()
        .to_string();
    io.refuse = false;
    assert!(
        publish_with(&mut f.store, &input, &changed, &mut io)
            .await
            .is_err()
    );
    assert_eq!(io.pushes, 0);
    assert_eq!(io.observations, 0);
    assert_eq!(state(&f.store), before);
}

#[tokio::test(flavor = "current_thread")]
async fn delta_dispatch_refusal_with_a_lost_fetch_resumes_by_fetch_only() {
    use crate::task_sync::ledger::Transition;
    let mut f = ExportFixture::new(2);
    let (input, commit) = progress::committed(&mut f);
    let mut io = Io::new(PushOutcome::NonFastForward);
    io.fetch_failures = 1;
    assert!(
        publish_with(&mut f.store, &input, &commit, &mut io)
            .await
            .is_err()
    );
    assert_eq!(io.pushes, 1);
    let events = f
        .store
        .task_sync_delta_outgoing_events(&f.id, &f.intent)
        .unwrap();
    let Some((_, last)) = events.last() else {
        panic!("delta progress is missing")
    };
    let Transition::PushRejected { push_started, .. } = &last.transition else {
        panic!("the refusal must be recorded before the fetch")
    };
    let push_started = push_started.clone();
    assert!(
        !events
            .iter()
            .any(|(_, event)| matches!(event.transition, Transition::PushObserved { .. }))
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
    for _ in 0..2 {
        let PublicationAttempt::ReconciliationRequired { failure, .. } =
            publish_with(&mut f.store, &input, &commit, &mut io)
                .await
                .unwrap()
        else {
            panic!("a recorded refusal must request reconciliation again")
        };
        assert!(matches!(
            failure,
            OutgoingFailure::NonFastForward { push_started: ref start, .. } if start == &push_started
        ));
        assert_eq!(io.pushes, 1);
    }
    assert!(f.store.verify_all().unwrap().is_healthy());
}
