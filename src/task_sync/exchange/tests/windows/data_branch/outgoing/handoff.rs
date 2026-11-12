//! One strict publication split at its actual rejected attempt. The first
//! supervised child publishes into the real non-fast-forward refusal and writes
//! a bounded test-only handoff; the second reopens the same store, the same Git
//! repositories at the same endpoint and the same operation, and recovers.

use super::*;
use crate::task_sync::ledger::{OutgoingFailure, RetainedCommit};
use sha2::{Digest, Sha256};

const HANDOFF: &str = "strict-handoff.json";
const HANDOFF_LIMIT: u64 = 1024 * 1024;
const DIGEST: &str = "ENGRAM_DATA_BRANCH_HANDOFF";

type CodeSnapshot = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

/// What the store cannot name for itself: where it is, the retained
/// selection, the peer that raced it and the refused attempt. The binding and
/// its prepared intent are read back from the reopened store's own ledger.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    database: PathBuf,
    binding_id: ObjectId,
    selection: DeltaCommitSelection,
    tree: task_sync::TaskTree,
    peer: RetainedCommit,
    failure: OutgoingFailure,
    fetched_ref: String,
    tail: ObjectId,
    url: String,
    code: [CodeSnapshot; 2],
}

pub(in super::super) struct Resumed {
    pub repos: Repositories,
    pub store: SqliteStore,
    pub binding_id: ObjectId,
    pub binding: Binding,
    pub intent: DeltaIntent,
    pub selection: DeltaCommitSelection,
    pub tree: task_sync::TaskTree,
    pub peer: RetainedCommit,
    pub attempt: exchange::PublicationAttempt,
    code: [CodeSnapshot; 2],
}

impl Resumed {
    pub fn finish(&self, root: &Path) {
        assert!(self.store.verify_all().unwrap().is_healthy());
        assert_eq!(Repositories::code_snapshot(&self.repos.left), self.code[0]);
        assert_eq!(Repositories::code_snapshot(&self.repos.right), self.code[1]);
        assert!(!root.join("server-errors").exists());
        println!("data-branch-workflow complete");
    }
}

/// The digest of the handoff a successful first phase wrote, for the parent
/// to require of the second.
pub(in super::super) fn digest(root: &Path) -> String {
    let path = root.join(HANDOFF);
    assert!(
        fs::metadata(&path).unwrap().len() <= HANDOFF_LIMIT,
        "the strict handoff stays bounded"
    );
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

pub(in super::super) fn require(command: &mut std::process::Command, digest: &str) {
    command.env(DIGEST, digest);
}

/// Prepare, select and publish one strict delta while a peer advances the
/// remote, ending at the actual refusal with its exact retained state.
pub(in super::super) async fn publish_into_refusal(root: &Path) {
    let repos = Repositories::new(root);
    let code = [
        Repositories::code_snapshot(&repos.left),
        Repositories::code_snapshot(&repos.right),
    ];
    let prepare::Prepared {
        mut store,
        data,
        id,
        binding,
        intent,
        selection,
        tree,
        ..
    } = Box::pin(prepare::prepare(&repos)).await;
    data.advance_retained(&selection.commit, None).unwrap();
    let outgoing = exchange::DeltaOutgoingSelection {
        binding_id: &id,
        binding: &binding,
        intent: &intent,
        commit: &selection,
    };
    let peer = recovery::arm(&repos);
    let attempt = {
        let _phase = super::super::timing::phase("outgoing-publish");
        exchange::publish_delta_outgoing(
            &mut store,
            &outgoing,
            publication_limits(),
            &Cancellation::default(),
        )
        .await
        .unwrap()
    };
    let exchange::PublicationAttempt::ReconciliationRequired {
        failure,
        fetched_ref,
    } = attempt
    else {
        panic!("actual rejected strict delta push must request reconciliation: {attempt:?}")
    };
    assert!(store.verify_all().unwrap().is_healthy());
    let tail = store
        .task_sync_exchange_events(&id)
        .unwrap()
        .last()
        .unwrap()
        .0
        .clone();
    let handoff = Handoff {
        database: binding.store.database.clone(),
        binding_id: id.clone(),
        selection: selection.clone(),
        tree,
        peer,
        failure,
        fetched_ref,
        tail,
        url: repos.url.clone(),
        code,
    };
    let bytes = serde_json::to_vec(&handoff).unwrap();
    assert!(bytes.len() as u64 <= HANDOFF_LIMIT);
    fs::write(root.join(HANDOFF), bytes).unwrap();
    println!("data-branch-workflow complete");
}

/// Reopen exactly what the first phase left: the handoff it wrote, the same
/// endpoint, and the same store whose ledger ends at the refused attempt.
pub(in super::super) fn resume(root: &Path) -> Resumed {
    let _phase = super::super::timing::phase("outgoing-resume");
    let expected = std::env::var(DIGEST).expect("the first phase succeeded and wrote a handoff");
    assert_eq!(
        digest(root),
        expected,
        "the handoff is the one the first phase wrote"
    );
    let handoff: Handoff = serde_json::from_slice(&fs::read(root.join(HANDOFF)).unwrap()).unwrap();
    let repos = Repositories::reopen(root);
    assert_eq!(
        repos.url, handoff.url,
        "the binding's endpoint is unchanged"
    );
    let store = SqliteStore::open(&handoff.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let binding = store
        .task_sync_binding(&handoff.binding_id)
        .unwrap()
        .expect("the same binding");
    assert_eq!(binding.store.database, handoff.database);
    let events = store
        .task_sync_exchange_events(&handoff.binding_id)
        .unwrap();
    assert_eq!(
        events.last().unwrap().0,
        handoff.tail,
        "nothing was recorded after the refused attempt"
    );
    let Some(Transition::DeltaPrepared { intent, .. }) = events
        .iter()
        .find(|(id, _)| id == &handoff.selection.prepared)
        .map(|(_, event)| event.transition.clone())
    else {
        panic!("the selection names its own recorded preparation")
    };
    let data = Repositories::data(&repos.left);
    assert_eq!(
        fixture::line(&data.checkout, &["rev-parse", "HEAD"]),
        handoff.selection.commit.oid
    );
    Resumed {
        repos,
        store,
        binding_id: handoff.binding_id,
        binding,
        intent: *intent,
        selection: handoff.selection,
        tree: handoff.tree,
        peer: handoff.peer,
        attempt: exchange::PublicationAttempt::ReconciliationRequired {
            failure: handoff.failure,
            fetched_ref: handoff.fetched_ref,
        },
        code: handoff.code,
    }
}

/// The refused publication's one resolved retry confirms, and a later
/// cancelled retry returns its historical receipt before any Git or payload
/// read, with the retained candidates and the data checkout unchanged.
pub(in super::super) async fn recover(root: &Path) {
    let mut resumed = resume(root);
    let Resumed {
        repos,
        store,
        binding_id,
        binding,
        intent,
        selection,
        tree,
        peer,
        attempt,
        ..
    } = &mut resumed;
    let outgoing = exchange::DeltaOutgoingSelection {
        binding_id: &*binding_id,
        binding: &*binding,
        intent: &*intent,
        commit: &*selection,
    };
    let recovered = Box::pin(recovery::finish(
        store,
        repos,
        &outgoing,
        peer,
        attempt.clone(),
    ))
    .await;
    let original = &recovered.outcome;
    assert!(store.verify_all().unwrap().is_healthy());
    let data = Repositories::data(&repos.left);
    create(store, &binding.project, "later-native-work");
    let preserved = data.git_directory.join("fixture-hidden-candidate");
    fs::rename(&intent.candidate.directory, &preserved).unwrap();
    let preserved_resolution = data.git_directory.join("fixture-hidden-resolution");
    fs::rename(&recovered.candidate, &preserved_resolution).unwrap();
    let requests = fs::read(repos.left.parent().unwrap().join("requests")).unwrap();
    let launches = exchange::test_support::LAUNCHES.with(std::cell::Cell::get);
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert_eq!(
        &Box::pin(exchange::publish_delta_retry(
            store,
            &recovered.input,
            RunLimits::default(),
            &cancelled
        ))
        .await
        .unwrap(),
        original
    );
    assert_eq!(
        exchange::test_support::LAUNCHES.with(std::cell::Cell::get),
        launches,
        "historical receipt must return before Git or payload reads"
    );
    assert_eq!(
        fs::read(repos.left.parent().unwrap().join("requests")).unwrap(),
        requests
    );
    fs::rename(preserved, &intent.candidate.directory).unwrap();
    fs::rename(preserved_resolution, &recovered.candidate).unwrap();
    let snapshot =
        task_sync::git::read_task_snapshot(&data.checkout, &binding.project, &recovered.commit.oid)
            .unwrap();
    assert_eq!(snapshot.tree, *tree);
    assert_eq!(
        git(&data.checkout, &["rev-parse", "HEAD"]),
        format!("{}\n", recovered.commit.oid).as_bytes()
    );
    resumed.finish(root);
}
