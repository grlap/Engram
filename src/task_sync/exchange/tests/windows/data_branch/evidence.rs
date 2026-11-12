//! Incoming evidence through the operator's public exchange step: real native
//! capture on one store, a commit on the dedicated data branch over confined
//! HTTPS, and exact retention and application on a second store.

use super::*;
use crate::storage::{SqliteStore, TaskSyncFolderApply, exchange_fixture};
use crate::task_sync::{
    self,
    data_worktree::PackFile,
    driver::{EvidenceRequest, StepReceipt, StepRequest},
    evidence::{EvidencePack, RecordEnvelope, RecordKind},
    exchange,
    ledger::{
        Binding, Direction, EvidenceIntent, EvidencePackSelection, EvidenceRefusal, EvidenceStop,
        RetainedCommit, StoreBasis, TargetBasis, Transition,
    },
    progress::{ExportCut, ExportProfile},
};
use crate::{CanonicalObject, ObjectId, ProjectId, SessionId};
use fixture::Repositories;
use sha2::{Digest, Sha256};

fn project() -> ProjectId {
    ProjectId("https-data-fixture".into())
}

struct Receiver {
    database: PathBuf,
    binding: Binding,
    id: ObjectId,
}

/// The empty planning tree whose disclosure matches the binding profile.
fn planning_tree(profile: &ExportProfile) -> task_sync::TaskTree {
    let mut tree = super::workflow::empty_tree();
    tree.project.notes_included = profile.include_notes;
    tree
}

/// The receiver's own enrolled, activated binding, with its store outside both
/// code clones so the clone can be hidden for offline replay.
async fn receiver(root: &Path, repos: &Repositories) -> Receiver {
    let project = project();
    let data = Repositories::data(&repos.right);
    let target = super::workflow::target(&repos.right).await;
    // An explicit disclosure selection: run activity needs an origin machine
    // and evidence prose needs notes. The sender's capture uses this profile.
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("https-data-fixture-sender".into()),
    };
    let database = crate::project_database_path(&root.join("receiver-home"), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let empty = store.export_task_sync_folder(&project, &profile).unwrap();
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project,
                actor: &super::outgoing::actor(),
                tree: &planning_tree(&profile),
                export_basis: &empty.export_basis,
                profile: &profile,
                complete: true,
                now: chrono::Utc::now(),
            },
            "explicit-observed-folder-bootstrap",
        )
        .unwrap();
    let origin = store
        .enroll_task_sync_origin(
            &project,
            &super::outgoing::actor(),
            &task_sync::origin::EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: database.clone(),
            origin: Some(origin.origin_id),
            restore_occurrence: None,
            cut: store
                .capture_task_changes(
                    &project,
                    &ExportCut {
                        position: 0,
                        anchor: None,
                    },
                )
                .unwrap()
                .cut,
        },
        repository: data.repository_basis(uuid::Uuid::new_v4()),
        branch: fixture::BRANCH.into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: fixture::BRANCH.into(),
            configuration_fingerprint: target.fingerprint().unwrap(),
        },
        profile,
        supersedes: None,
        superseded_tail: None,
        actor: super::outgoing::actor(),
        created_at: chrono::Utc::now(),
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    // The operator's explicit host-local opt-in, as `task-sync enable` records it.
    task_sync::settings::set(
        &database,
        &project,
        task_sync::settings::Action::Enable,
        Some("fixture: explicit task sync opt-in for the evidence exchange test"),
        "data-fixture",
        "data-fixture",
        chrono::Utc::now(),
    )
    .unwrap();
    Receiver {
        database,
        binding,
        id,
    }
}

fn service(receiver: &Receiver) -> crate::LocalWorkService {
    crate::LocalWorkService::new(
        receiver.database.clone(),
        project(),
        "data-fixture".into(),
        SessionId("data-fixture".into()),
        None,
    )
}

fn selection(file: &PackFile) -> EvidencePackSelection {
    EvidencePackSelection {
        publishing_origin: file.publishing_origin.clone(),
        admission: ObjectId::mint(),
        content_fingerprint: file.content_fingerprint.clone(),
        encoded_bytes: file.encoded_bytes,
        records: file.records,
    }
}

fn intent(
    receiver: &Receiver,
    operation: uuid::Uuid,
    source: RetainedCommit,
    packs: Vec<(ObjectId, EvidencePackSelection)>,
) -> EvidenceIntent {
    let store = SqliteStore::open(&receiver.database).unwrap();
    EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: super::outgoing::actor(),
        producer: CanonicalObject::freeze(&"native evidence encoder")
            .unwrap()
            .key()
            .clone(),
        expected_head: None,
        source,
        local_cut: super::outgoing::cut(&store, &receiver.id, &receiver.binding),
        packs: packs.into_iter().collect(),
    }
}

async fn apply(receiver: &Receiver, intent: &EvidenceIntent) -> anyhow::Result<StepReceipt> {
    let _phase = super::timing::phase("evidence-apply");
    service(receiver)
        .task_sync_exchange_step(
            &StepRequest::ApplyEvidence {
                request: Box::new(EvidenceRequest {
                    binding: receiver.id.clone(),
                    intent: intent.clone(),
                }),
            },
            RunLimits::default(),
            &Cancellation::default(),
        )
        .await
}

fn outcome(receipt: &StepReceipt) -> (ObjectId, ObjectId, String) {
    let StepReceipt::Outcome {
        outcome: Some(outcome),
    } = receipt
    else {
        panic!("evidence step must return its terminal outcome")
    };
    (
        outcome.prepared.clone(),
        outcome.outcome.clone(),
        outcome.transition.clone(),
    )
}

fn terminal(receiver: &Receiver, outcome: &ObjectId) -> Transition {
    let store = SqliteStore::open(&receiver.database).unwrap();
    store
        .task_sync_exchange_events(&receiver.id)
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == outcome)
        .unwrap()
        .1
        .transition
}

fn admitted(receiver: &Receiver, intent: &EvidenceIntent) -> Vec<bool> {
    let store = SqliteStore::open(&receiver.database).unwrap();
    intent
        .packs
        .values()
        .map(|selected| exchange_fixture::object(&store, &selected.admission).is_some())
        .collect()
}

/// A pack whose producing origin is enrolled nowhere the receiver knows.
fn unenrolled_pack(project: &ProjectId) -> EvidencePack {
    let origin = ObjectId::mint();
    let body = CanonicalObject::mint(&crate::domain::WorkEvidence {
        schema_version: crate::schema::SCHEMA_VERSION,
        work_id: crate::WorkId::new(),
        run_id: crate::WorkRunId::new(),
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
        summary: "evidence from an origin the receiver never enrolled".into(),
        refs: Vec::new(),
        gate: None,
        actor: super::outgoing::actor(),
        created_at: chrono::Utc::now(),
    })
    .unwrap();
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        body.key().clone(),
        RecordKind::WorkEvidence.as_str(),
        body.bytes().to_vec(),
    )
    .unwrap();
    EvidencePack::prepare(ObjectId::mint(), project.clone(), origin, vec![record]).unwrap()
}

/// A second enrollment body under the sender's existing origin identity.
fn conflicting_pack(project: &ProjectId, origin: &ObjectId) -> EvidencePack {
    let enrollment = task_sync::origin::Enrollment {
        project_id: project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: super::outgoing::actor(),
        enrolled_at: chrono::Utc::now(),
    };
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        origin.clone(),
        RecordKind::ProducingOrigin.as_str(),
        serde_json_canonicalizer::to_vec(&enrollment).unwrap(),
    )
    .unwrap();
    EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin.clone(),
        vec![record],
    )
    .unwrap()
}

/// Selections whose frozen size, fingerprint or record count disagrees with a
/// staged pack, and one for a staged pack of a format no reader supports. Each
/// can only disagree again with the immutable carrier, so each request ends
/// with a known stop instead of refusing on every retry while it holds custody.
fn mismatched_selections(
    declared: &PackFile,
    malformed: &EvidencePack,
    malformed_bytes: &[u8],
) -> Vec<(ObjectId, EvidencePackSelection, EvidenceRefusal)> {
    let wrong = |change: fn(&mut EvidencePackSelection)| {
        let mut selected = selection(declared);
        change(&mut selected);
        (
            declared.pack.clone(),
            selected,
            EvidenceRefusal::InvalidPayload,
        )
    };
    vec![
        wrong(|selected| selected.encoded_bytes += 1),
        wrong(|selected| selected.encoded_bytes -= 1),
        wrong(|selected| {
            selected.content_fingerprint = format!("{:x}", Sha256::digest(b"another encoding"))
                .parse()
                .unwrap();
        }),
        wrong(|selected| selected.records += 1),
        (
            malformed.pack_id().clone(),
            EvidencePackSelection {
                publishing_origin: malformed.publishing_origin().clone(),
                admission: ObjectId::mint(),
                content_fingerprint: format!("{:x}", Sha256::digest(malformed_bytes))
                    .parse()
                    .unwrap(),
                encoded_bytes: malformed_bytes.len(),
                records: 1,
            },
            EvidenceRefusal::InvalidPayload,
        ),
    ]
}

/// Rename the receiver's whole code clone, including its Git administration and
/// data worktree, so neither Git nor pack files are reachable.
struct Hidden<'a> {
    from: &'a Path,
    to: PathBuf,
}

impl<'a> Hidden<'a> {
    fn new(from: &'a Path) -> Self {
        let to = from.with_file_name("right-hidden");
        fs::rename(from, &to).unwrap();
        Self { from, to }
    }
}

impl Drop for Hidden<'_> {
    fn drop(&mut self) {
        fs::rename(&self.to, self.from).unwrap();
    }
}

/// The binding's exchange events and the canonical object count: an exact
/// replay may write neither.
fn durable(receiver: &Receiver) -> (Vec<ObjectId>, i64) {
    let store = SqliteStore::open(&receiver.database).unwrap();
    let events = store
        .task_sync_exchange_events(&receiver.id)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let objects = rusqlite::Connection::open(&receiver.database)
        .unwrap()
        .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    (events, objects)
}

async fn replay_offline(receiver: &Receiver, right: &Path, intent: &EvidenceIntent) -> StepReceipt {
    let before = durable(receiver);
    let launches = exchange::test_support::LAUNCHES.with(std::cell::Cell::get);
    let replayed = {
        let _hidden = Hidden::new(right);
        apply(receiver, intent).await.unwrap()
    };
    assert_eq!(
        exchange::test_support::LAUNCHES.with(std::cell::Cell::get),
        launches,
        "a completed evidence exchange must replay before Git or pack reads"
    );
    assert_eq!(durable(receiver), before);
    replayed
}

type CodeSnapshot = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

/// One real sender capture with every case's pack staged beside it, committed
/// and pushed once: the published carrier each evidence child selects from.
struct Published {
    repos: Repositories,
    receiver: Receiver,
    code: [CodeSnapshot; 2],
    native_origin: ObjectId,
    originals: Vec<(ObjectId, String, Vec<u8>)>,
    native_files: Vec<PackFile>,
    extra_files: Vec<PackFile>,
    unenrolled: EvidencePack,
    conflicting: EvidencePack,
    invalid: EvidencePack,
    invalid_bytes: Vec<u8>,
    malformed: EvidencePack,
    malformed_bytes: Vec<u8>,
    carrier: RetainedCommit,
    right_target: crate::task_sync::exchange::ExchangeTarget,
}

impl Published {
    fn native_selections(&self) -> Vec<(ObjectId, EvidencePackSelection)> {
        self.native_files
            .iter()
            .map(|file| (file.pack.clone(), selection(file)))
            .collect()
    }

    /// A request in its own fetched operation over the published carrier.
    async fn request(&self, packs: Vec<(ObjectId, EvidencePackSelection)>) -> EvidenceIntent {
        let operation = uuid::Uuid::new_v4();
        let source = super::workflow::fetch_operation(
            &self.repos.right,
            &self.right_target,
            &self.carrier,
            operation,
        )
        .await;
        let mut request = intent(&self.receiver, operation, source, packs);
        request.expected_head = Some(self.carrier.oid.clone());
        request
    }

    /// Populate only the receiver's orphan data index and worktree from a
    /// fetched carrier, then take the public retained fast-forward, as an
    /// incoming data checkout does.
    fn advance_receiver(&self, fetched: &RetainedCommit) {
        let data = Repositories::data(&self.repos.right);
        fixture::git(
            &data.checkout,
            &["read-tree", "--reset", "-u", &fetched.oid],
        );
        data.advance_retained(fetched, None).unwrap();
    }

    /// The request ends with this known refusal, admits nothing and replays
    /// the same outcome offline.
    async fn assert_stopped(&self, request: &EvidenceIntent, expected: EvidenceRefusal) {
        let refused = apply(&self.receiver, request).await.unwrap();
        let (prepared, outcome_id, transition) = outcome(&refused);
        assert_eq!(transition, "evidence_stopped", "{expected:?}");
        assert_eq!(
            terminal(&self.receiver, &outcome_id),
            Transition::EvidenceStopped {
                prepared,
                outcome: EvidenceStop::Refused { reason: expected },
            },
        );
        assert_eq!(
            admitted(&self.receiver, request),
            vec![false; request.packs.len()]
        );
        assert_eq!(
            outcome(&replay_offline(&self.receiver, &self.repos.right, request).await),
            outcome(&refused)
        );
    }

    /// The request applies, its originals keep their identities and bytes,
    /// and it grants no native execution credit.
    async fn assert_applied(
        &self,
        request: &EvidenceIntent,
        native_before: [i64; 3],
    ) -> (ObjectId, ObjectId, String) {
        let applied = apply(&self.receiver, request).await.unwrap();
        let (prepared, outcome_id, transition) = outcome(&applied);
        assert_eq!(transition, "evidence_retained");
        let Transition::EvidenceImportApplied { receipt, .. } =
            terminal(&self.receiver, &outcome_id)
        else {
            panic!("retained evidence must record its import receipt")
        };
        assert_eq!(
            receipt.admissions,
            request
                .packs
                .iter()
                .map(|(pack, selected)| (pack.clone(), selected.admission.clone()))
                .collect()
        );
        let store = SqliteStore::open(&self.receiver.database).unwrap();
        for (id, kind, bytes) in &self.originals {
            assert_eq!(
                exchange_fixture::object(&store, id),
                Some((kind.clone(), bytes.clone())),
                "original identity and bytes are retained unchanged"
            );
            assert!(exchange_fixture::imported_only(&store, id));
        }
        // No native feed entry, run or claim: no execution or verification credit.
        assert_eq!(exchange_fixture::native_counts(&store), native_before);
        assert!(store.verify_all().unwrap().is_healthy());
        (prepared, outcome_id, transition)
    }

    fn finish(&self, root: &Path, native_before: [i64; 3]) {
        let store = SqliteStore::open(&self.receiver.database).unwrap();
        // No refused body ever replaced the sender's original enrollment.
        assert!(
            self.originals
                .iter()
                .filter(|(id, ..)| id == &self.native_origin)
                .all(|(id, kind, bytes)| exchange_fixture::object(&store, id)
                    == Some((kind.clone(), bytes.clone())))
        );
        assert_eq!(exchange_fixture::native_counts(&store), native_before);
        assert!(store.verify_all().unwrap().is_healthy());
        assert_eq!(Repositories::code_snapshot(&self.repos.left), self.code[0]);
        assert_eq!(Repositories::code_snapshot(&self.repos.right), self.code[1]);
        assert!(!root.join("server-errors").exists());
        println!("data-branch-workflow complete");
    }
}

async fn publish(root: &Path) -> Published {
    let repos = Repositories::new(root);
    let project = project();
    let code = [
        Repositories::code_snapshot(&repos.left),
        Repositories::code_snapshot(&repos.right),
    ];
    let receiver = receiver(root, &repos).await;

    // Sender: a real claimed native history captured into several small packs.
    let sender_database = crate::project_database_path(&root.join("sender-home"), &project);
    fs::create_dir_all(sender_database.parent().unwrap()).unwrap();
    let mut sender = SqliteStore::open(&sender_database).unwrap();
    // The capture carries the receiver binding's explicit disclosure profile.
    let profile = receiver.binding.profile.clone();
    let native = exchange_fixture::native_sender(&mut sender, &project, &profile);
    let captured = exchange_fixture::capture(&mut sender, &project, &native.request);
    let page = sender
        .export_native_evidence_capture(&project, &captured.cursor, 32, &profile)
        .unwrap();
    assert!(page.next.is_none());
    assert!(page.packs.len() > 1, "several small packs");
    let originals: Vec<(ObjectId, String, Vec<u8>)> = page
        .packs
        .iter()
        .flat_map(EvidencePack::records)
        .map(|record| {
            (
                record.object_id().clone(),
                record.kind().as_str().to_owned(),
                record.canonical_bytes().to_vec(),
            )
        })
        .collect();
    assert!(originals.iter().any(|(id, ..)| id == &native.original));

    // Every pack for every case goes in one data-branch commit, pushed once.
    task_sync::directory::write_folder(
        &repos.left.join(".engram-data/.engram"),
        &planning_tree(&profile),
        &profile,
    )
    .unwrap();
    let left = Repositories::data(&repos.left);
    let native_files = left.stage_evidence_packs(&project, &page.packs).unwrap();
    let unenrolled = unenrolled_pack(&project);
    let conflicting = conflicting_pack(&project, &native.request.origin);
    let extra_files = [&unenrolled, &conflicting]
        .into_iter()
        .flat_map(|pack| {
            left.stage_evidence_packs(&project, std::slice::from_ref(pack))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let invalid = unenrolled_pack(&project);
    let invalid_bytes = b"{\"not\": \"an evidence pack\"\n".to_vec();
    let invalid_path = left.checkout.join(invalid.relative_path());
    fs::create_dir_all(invalid_path.parent().unwrap()).unwrap();
    fs::write(&invalid_path, &invalid_bytes).unwrap();
    // Well-formed JSON in a pack's shape, but of a format no reader supports.
    let malformed = unenrolled_pack(&project);
    let mut wire: serde_json::Value =
        serde_json::from_slice(&malformed.to_bytes().unwrap()).unwrap();
    wire["format"] = "unsupported-evidence-pack".into();
    let malformed_bytes = serde_json::to_vec(&wire).unwrap();
    let malformed_path = left.checkout.join(malformed.relative_path());
    fs::create_dir_all(malformed_path.parent().unwrap()).unwrap();
    fs::write(&malformed_path, &malformed_bytes).unwrap();
    let carrier = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        Vec::new(),
        "Publish captured native evidence",
    );
    left.advance_retained(&carrier, None).unwrap();
    let left_target = super::workflow::target(&repos.left).await;
    exchange::run(
        &left_target,
        &ExchangeOperation::Push {
            commit: carrier.oid.clone(),
        },
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let right_target = super::workflow::target(&repos.right).await;
    Published {
        repos,
        receiver,
        code,
        native_origin: native.request.origin,
        originals,
        native_files,
        extra_files,
        unenrolled,
        conflicting,
        invalid,
        invalid_bytes,
        malformed,
        malformed_bytes,
        carrier,
        right_target,
    }
}

/// Native packs through the operator step: a changed selection, an interrupted
/// retained request, the applied request and its replays, then the known
/// refusals. Each case fetches its operation just before its request: custody
/// refuses any exchange while another fetched operation is unknown to the ledger.
pub(super) async fn run(root: &Path) {
    let published = publish(root).await;
    let Published {
        repos,
        receiver,
        extra_files,
        unenrolled,
        conflicting,
        invalid,
        invalid_bytes,
        carrier,
        right_target,
        ..
    } = &published;
    let native_selections = || published.native_selections();
    // A fresh request whose selected head differs from the branch ends with a
    // known stop under the identities its construction retains, so neither
    // its fetched carrier nor its recipe can strand custody.
    let stale_intent = published.request(native_selections()).await;
    let stale = apply(receiver, &stale_intent).await.unwrap();
    let stale_recipe =
        task_sync::retention::evidence::read_recipe(&receiver.id, &receiver.binding, &stale_intent)
            .unwrap()
            .expect("a stopped request keeps its retained construction");
    assert_eq!(
        outcome(&stale),
        (
            stale_recipe.prepared_id.clone(),
            stale_recipe.outcome_id.clone(),
            "evidence_stopped".to_owned()
        )
    );
    assert_eq!(
        terminal(receiver, &stale_recipe.outcome_id),
        Transition::EvidenceStopped {
            prepared: stale_recipe.prepared_id.clone(),
            outcome: EvidenceStop::Refused {
                reason: EvidenceRefusal::SelectionChanged
            },
        }
    );
    assert_eq!(
        admitted(receiver, &stale_intent),
        vec![false; stale_intent.packs.len()]
    );
    // A request interrupted after the public step retained its construction,
    // whose selected head then moves through the supported data-branch advance.
    // The exact retry settles it with a known stop under the retained
    // identities instead of refusing forever, with no rewind or private-file
    // removal, and the corrected request at the current head then completes.
    let drift_operation = uuid::Uuid::new_v4();
    let drift_source =
        super::workflow::fetch_operation(&repos.right, right_target, carrier, drift_operation)
            .await;
    let drift_intent = intent(
        receiver,
        drift_operation,
        drift_source.clone(),
        native_selections(),
    );
    let retained = SqliteStore::open(&receiver.database)
        .unwrap()
        .retain_task_sync_evidence_recipe(
            &receiver.id,
            &receiver.binding,
            &drift_intent,
            chrono::Utc::now(),
        )
        .unwrap();
    published.advance_receiver(&drift_source);
    let settled = apply(receiver, &drift_intent).await.unwrap();
    assert_eq!(
        outcome(&settled),
        (
            retained.prepared_id.clone(),
            retained.outcome_id.clone(),
            "evidence_stopped".to_owned()
        )
    );
    assert_eq!(
        terminal(receiver, &retained.outcome_id),
        Transition::EvidenceStopped {
            prepared: retained.prepared_id.clone(),
            outcome: EvidenceStop::Refused {
                reason: EvidenceRefusal::SelectionChanged
            },
        }
    );
    assert_eq!(
        admitted(receiver, &drift_intent),
        vec![false; drift_intent.packs.len()]
    );
    assert_eq!(
        outcome(&replay_offline(receiver, &repos.right, &drift_intent).await),
        outcome(&settled)
    );
    // The corrected request selects the current head and completes.
    let applied_intent = published.request(native_selections()).await;
    let native_before =
        exchange_fixture::native_counts(&SqliteStore::open(&receiver.database).unwrap());
    let applied = published
        .assert_applied(&applied_intent, native_before)
        .await;
    let replayed = replay_offline(receiver, &repos.right, &applied_intent).await;
    assert_eq!(outcome(&replayed), applied);
    let mut changed = applied_intent.clone();
    changed.packs.pop_last();
    assert!(apply(receiver, &changed).await.is_err());

    // Refusals, each in its own operation over the same carrier commit.
    let unenrolled_file = extra_files
        .iter()
        .find(|file| &file.pack == unenrolled.pack_id())
        .unwrap();
    let conflicting_file = extra_files
        .iter()
        .find(|file| &file.pack == conflicting.pack_id())
        .unwrap();
    let invalid_selection = EvidencePackSelection {
        publishing_origin: invalid.publishing_origin().clone(),
        admission: ObjectId::mint(),
        content_fingerprint: format!("{:x}", Sha256::digest(invalid_bytes))
            .parse()
            .unwrap(),
        encoded_bytes: invalid_bytes.len(),
        records: 1,
    };
    for (pack, selected, expected) in [
        (
            invalid.pack_id().clone(),
            invalid_selection,
            EvidenceRefusal::InvalidPayload,
        ),
        (
            unenrolled.pack_id().clone(),
            selection(unenrolled_file),
            EvidenceRefusal::Dependency,
        ),
        (
            conflicting.pack_id().clone(),
            selection(conflicting_file),
            EvidenceRefusal::RecordConflict,
        ),
    ] {
        let refused = published.request(vec![(pack, selected)]).await;
        published.assert_stopped(&refused, expected).await;
    }
    published.finish(root, native_before);
}

/// A request whose frozen selection disagrees with the published immutable
/// carrier can only disagree again, so each ends with a known stop under its
/// retained identities and replays offline, instead of refusing on every
/// retry while its construction holds custody. A later valid request over the
/// same carrier then applies.
pub(super) async fn run_mismatches(root: &Path) {
    let published = publish(root).await;
    let native_before =
        exchange_fixture::native_counts(&SqliteStore::open(&published.receiver.database).unwrap());
    for (index, (pack, selected, expected)) in mismatched_selections(
        &published.native_files[0],
        &published.malformed,
        &published.malformed_bytes,
    )
    .into_iter()
    .enumerate()
    {
        let refused = published.request(vec![(pack, selected)]).await;
        if index == 0 {
            // The receiver's data branch holds the carrier each request
            // selects, so only the frozen selection can disagree.
            published.advance_receiver(&refused.source);
        }
        published.assert_stopped(&refused, expected).await;
    }
    let later = published.request(published.native_selections()).await;
    published.assert_applied(&later, native_before).await;
    published.finish(root, native_before);
}
