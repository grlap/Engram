use super::super::test_support::{actor, at, checkpoint, claim, completion_request, root_request};
use super::*;
use crate::DevelopmentNoopRedactor;
use crate::domain::*;

mod downstream;
mod host_wire;
mod shared_input;

fn legacy_input(input: &mut RemoteRunInput) -> &mut LegacyRemoteRunInput {
    let RemoteRunInput::Legacy(input) = input else {
        panic!("legacy fixture")
    };
    input
}

pub(super) struct Fixture {
    pub(super) store: SqliteStore,
    pub(super) project: ProjectId,
    pub(super) session: SessionId,
    pub(super) connection: String,
    pub(super) request: RemoteVerificationRequest,
}

pub(super) fn object_name(label: &str) -> String {
    ObjectId::from_canonical_bytes(label.as_bytes()).as_str()[..40].to_owned()
}

pub(super) fn tested_commit() -> String {
    object_name("tested commit")
}

pub(super) fn tested_tree() -> GitTreeId {
    GitTreeId::parse(object_name("tested tree")).unwrap()
}

pub(super) fn committed(purpose: RemoteVerificationPurpose) -> RemoteVerificationUse {
    RemoteVerificationUse::CommittedInput {
        expected_commit: tested_commit(),
        purpose,
    }
}

pub(super) fn test_ref() -> RemoteVerificationUse {
    RemoteVerificationUse::AuthorizedTestRef {
        ref_name: "refs/heads/test/item-0a1b2c3d".into(),
        expected_tree: tested_tree(),
        purpose: RemoteVerificationPurpose::LandingGate,
    }
}

pub(super) fn fixture() -> Fixture {
    fixture_in(SqliteStore::open_in_memory().unwrap(), 0, None)
}

pub(super) fn fixture_in(
    mut store: SqliteStore,
    offset: i64,
    create: Option<&CreateWorkRequest>,
) -> Fixture {
    let at = |second| at(second + offset);
    let default = root_request("project-a", "remote-work", 1 + offset);
    let work = store
        .create_work(create.unwrap_or(&default), &DevelopmentNoopRedactor)
        .unwrap();
    let held = claim(&mut store, &work, "runner", "claim", 2 + offset, 3600);
    let work = store.get_work_item(work.work_id).unwrap();
    let session = SessionId("runner".into());
    let project = work.project_id.clone();
    let connection = store.resume_work_connection(&session, at(3)).unwrap();
    store
        .bind_named_root(
            &project,
            &session,
            &connection,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "name",
            at(4),
        )
        .unwrap();
    let binding = ControlWorkBinding {
        root_execution_id: store.get_work_run(held.run_id).unwrap().root_execution_id,
        work_id: work.work_id,
        run_id: held.run_id,
        work_revision: work.revision,
        claim_id: held.claim_id,
        claim_fence: held.fence,
    };
    let request = RemoteVerificationRequest {
        operation_key: "remote:test-remote:one".into(),
        completion: RemoteRunCompletion {
            schema_version: REMOTE_COMPLETION_SCHEMA_VERSION,
            origin: RemoteRunOrigin {
                host_id: "dl560".into(),
                runner_session_id: "native-root".into(),
            },
            run_id: "test-remote".into(),
            mode: RemoteRunMode::Full,
            result: RemoteRunResult {
                verdict: RemoteRunVerdict::Pass,
                terminal: true,
                exit: Some(0),
                reason: "completed".into(),
            },
            input: LegacyRemoteRunInput {
                commit: Some(tested_commit()),
                tree: Some(tested_tree()),
                before_fingerprint: Some("f".repeat(64)),
                after_fingerprint: Some("f".repeat(64)),
            }
            .into(),
            finished_at: Some(at(9)),
        },
        association: RemoteVerificationAssociation {
            binding,
            root: RegisteredVerificationRoot {
                workspace_id: "workspace-a".into(),
                generation: 1,
                state: SourceRootState::Named,
            },
            intended_use: committed(RemoteVerificationPurpose::Verification),
        },
    };
    Fixture {
        store,
        project,
        session,
        connection,
        request,
    }
}

impl Fixture {
    pub(super) fn record_at(
        &mut self,
        second: i64,
    ) -> Result<RemoteVerificationReceipt, StoreError> {
        self.store.record_remote_verification(
            &self.project,
            &self.session,
            &self.connection,
            crate::storage::HostSessionAuthority::Work,
            &actor("runner"),
            &self.request,
            at(second),
        )
    }

    pub(super) fn record(&mut self) -> Result<RemoteVerificationReceipt, StoreError> {
        self.record_at(10)
    }

    pub(super) fn snapshot(&self) -> String {
        format!(
            "{:?}",
            crate::storage::test_database_shape_snapshot(&self.store.connection).unwrap()
        )
    }

    pub(super) fn count(&self, kind: &str) -> i64 {
        self.store
            .connection
            .query_row(
                "SELECT count(*) FROM objects WHERE object_kind = ?1",
                [kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn association(&self, id: &ObjectId) -> RemoteVerificationAssociationRecord {
        super::super::feeds::load_typed_work_object(
            &self.store.connection,
            id,
            REMOTE_VERIFICATION_ASSOCIATION_KIND,
        )
        .unwrap()
    }

    fn run_head(&self) -> i64 {
        self.store
            .work_feed_head(&FeedId::RunExecution(
                self.request.association.binding.run_id,
            ))
            .unwrap()
    }

    fn project_head(&self) -> i64 {
        self.store
            .work_feed_head(&FeedId::Project(self.project.clone()))
            .unwrap()
    }

    fn assert_healthy(&self) {
        let report = self.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }

    fn assert_uncredited(&mut self, reason: RemoteVerificationReason) -> RemoteVerificationReceipt {
        let evidence = self.count("verification_evidence");
        let receipt = self.record().unwrap();
        assert!(!receipt.qualification.credited, "{receipt:?}");
        assert_eq!(receipt.qualification.reason, Some(reason), "{receipt:?}");
        assert_eq!(receipt.verifications.len(), 0);
        assert_eq!(self.count("verification_evidence"), evidence);
        self.assert_healthy();
        receipt
    }

    /// A second item this session holds, with its own named root.
    fn second_destination(&mut self) -> RemoteVerificationAssociation {
        let work = self
            .store
            .create_work(
                &root_request("project-a", "second-remote-work", 1),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let held = claim(&mut self.store, &work, "runner", "second-claim", 2, 3600);
        let work = self.store.get_work_item(work.work_id).unwrap();
        self.store
            .bind_named_root(
                &self.project,
                &self.session,
                &self.connection,
                crate::storage::HostSessionAuthority::Work,
                held.claim_id,
                held.fence,
                "workspace-b",
                1,
                at(4),
                NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "name-second",
                at(4),
            )
            .unwrap();
        RemoteVerificationAssociation {
            binding: ControlWorkBinding {
                root_execution_id: self
                    .store
                    .get_work_run(held.run_id)
                    .unwrap()
                    .root_execution_id,
                work_id: work.work_id,
                run_id: held.run_id,
                work_revision: work.revision,
                claim_id: held.claim_id,
                claim_fence: held.fence,
            },
            root: RegisteredVerificationRoot {
                workspace_id: "workspace-b".into(),
                generation: 1,
                state: SourceRootState::Named,
            },
            intended_use: committed(RemoteVerificationPurpose::Verification),
        }
    }
}

#[test]
fn remote_verification_credits_a_full_pass_in_each_use() {
    for (intended_use, revision, tree) in [
        (
            committed(RemoteVerificationPurpose::Verification),
            format!("git-commit:{}", tested_commit()),
            None,
        ),
        (
            committed(RemoteVerificationPurpose::LandingGate),
            format!("git-commit:{}", tested_commit()),
            None,
        ),
        (
            test_ref(),
            format!("git-tree:{}", tested_tree().as_str()),
            Some(tested_tree()),
        ),
    ] {
        let mut f = fixture();
        f.request.association.intended_use = intended_use;
        let receipt = f.record().unwrap();
        assert_eq!(
            receipt.qualification,
            RemoteVerificationQualification::credited()
        );
        assert_eq!(receipt.verifications.len(), 1);
        assert_eq!(receipt.verifications[0].stage, "full");
        assert!(receipt.origin.is_some());
        let evidence: VerificationEvidence = super::super::feeds::load_typed_work_object(
            &f.store.connection,
            &receipt.verifications[0].record_id,
            "verification_evidence",
        )
        .unwrap();
        assert_eq!(evidence.producer_observation, receipt.operation);
        assert_eq!(
            evidence.source_basis.legacy_revision(),
            Some(revision.as_str())
        );
        assert_eq!(evidence.source_basis.legacy_basis().unwrap().git_tree, tree);
        assert_eq!(evidence.check_kind, VerificationKind::Test);
        assert_eq!(evidence.result, VerificationResult::Passed);
        // The run's own finish is the execution time; the association's is local.
        assert_eq!(evidence.completed_at, at(9));
        assert_eq!(evidence.recorded_at, at(10));
        let producer =
            super::super::load_verification_producer_on(&f.store.connection, &evidence).unwrap();
        assert_eq!(producer.observed_at, Some(at(9)));
        assert_eq!(producer.recorded_at, at(10));
        assert_eq!(producer.run_outcome, Some(ExecutionOutcome::Succeeded));
        assert_eq!(producer.action_fingerprint, evidence.check_fingerprint);
        // Nothing local was synthesized for it.
        for kind in [
            "execution_observation",
            "unadmitted_execution_observation",
            "registered_host_run",
            "turn_grant",
        ] {
            assert_eq!(f.count(kind), 0, "{kind}");
        }
        f.assert_healthy();
    }
}

#[test]
fn remote_verification_ordinary_pass_needs_no_fingerprint_but_a_landing_gate_does() {
    let without = |input: &mut LegacyRemoteRunInput| {
        input.before_fingerprint = None;
        input.after_fingerprint = None;
    };
    let mut f = fixture();
    without(legacy_input(&mut f.request.completion.input));
    assert!(f.record().unwrap().qualification.credited);

    // Differing optional fingerprints invent no ordinary-verification rule.
    let mut f = fixture();
    legacy_input(&mut f.request.completion.input).after_fingerprint = Some("e".repeat(64));
    assert!(f.record().unwrap().qualification.credited);

    for landing in [
        committed(RemoteVerificationPurpose::LandingGate),
        test_ref(),
    ] {
        type Change = fn(&mut LegacyRemoteRunInput);
        let cases: [(Change, &str); 3] = [
            (without, "before and after fingerprints"),
            (
                |input| input.before_fingerprint = None,
                "before fingerprint",
            ),
            (
                |input| input.after_fingerprint = Some("e".repeat(64)),
                "differ",
            ),
        ];
        for (change, detail) in cases {
            let mut f = fixture();
            f.request.association.intended_use = landing.clone();
            change(legacy_input(&mut f.request.completion.input));
            let receipt =
                f.assert_uncredited(RemoteVerificationReason::FingerprintMissingOrChanged);
            assert!(
                receipt
                    .qualification
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains(detail),
                "{receipt:?}"
            );
        }
    }
}

#[test]
fn remote_verification_focused_failed_interrupted_and_debris_stay_visible_and_uncredited() {
    type Change = fn(&mut RemoteRunCompletion);
    let cases: [(Change, RemoteVerificationReason); 6] = [
        (
            |completion| completion.mode = RemoteRunMode::Focused,
            RemoteVerificationReason::NotFullMode,
        ),
        (
            |completion| {
                completion.result.verdict = RemoteRunVerdict::Fail;
                completion.result.exit = Some(1);
                completion.result.reason = "stage-failed".into();
            },
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |completion| {
                completion.result.verdict = RemoteRunVerdict::Interrupted;
                completion.result.exit = None;
                completion.result.reason = "interrupted".into();
            },
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |completion| {
                completion.result.verdict = RemoteRunVerdict::Fail;
                completion.result.exit = Some(1);
                completion.result.reason = "input-changed".into();
            },
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |completion| {
                completion.result.verdict = RemoteRunVerdict::Fail;
                completion.result.exit = Some(1);
                completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
            },
            RemoteVerificationReason::UntrackedDebrisPendingDisposition,
        ),
        // Debris on a focused run is still only a focused run.
        (
            |completion| {
                completion.mode = RemoteRunMode::Focused;
                completion.result.verdict = RemoteRunVerdict::Fail;
                completion.result.exit = Some(1);
                completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
            },
            RemoteVerificationReason::NotFullMode,
        ),
    ];
    for (change, reason) in cases {
        let mut f = fixture();
        change(&mut f.request.completion);
        let before_run = f.run_head();
        let receipt = f.assert_uncredited(reason);
        // Each is a settled terminal, kept with its native reason unchanged.
        let origin = receipt.origin.as_ref().expect("a settled terminal");
        let origin: RemoteTerminalOrigin = super::super::feeds::load_typed_work_object(
            &f.store.connection,
            origin,
            REMOTE_TERMINAL_ORIGIN_KIND,
        )
        .unwrap();
        assert_eq!(origin.completion, f.request.completion);
        assert_eq!(
            f.association(&receipt.operation)
                .request
                .completion
                .result
                .reason,
            f.request.completion.result.reason
        );
        assert_eq!(f.run_head(), before_run + 1, "visible on the item's run");
    }
}

#[test]
fn remote_verification_snapshots_never_fix_a_terminal_and_the_real_one_settles() {
    type Change = fn(&mut RemoteRunResult);
    let snapshots: [Change; 3] = [
        |result| {
            result.verdict = RemoteRunVerdict::Unknown;
            result.terminal = true;
            result.exit = None;
            result.reason = "input-unverified".into();
        },
        |result| {
            result.verdict = RemoteRunVerdict::Running;
            result.terminal = false;
            result.exit = None;
            result.reason = "no-terminal-result".into();
        },
        |result| {
            result.verdict = RemoteRunVerdict::Interrupted;
            result.terminal = false;
            result.exit = None;
            result.reason = "heartbeat-stale".into();
        },
    ];
    for snapshot in snapshots {
        let mut f = fixture();
        let terminal = f.request.clone();
        snapshot(&mut f.request.completion.result);
        f.request.completion.finished_at = None;
        f.request.operation_key = "remote:snapshot".into();
        let provisional = f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
        assert_eq!(provisional.origin, None, "a snapshot reserves no terminal");
        assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 0);
        // The real terminal of the same run settles under its own key, with
        // no conflict and no second run.
        f.request = terminal;
        let settled = f.record().unwrap();
        assert!(settled.qualification.credited, "{settled:?}");
        assert!(settled.origin.is_some());
        assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 1);
        f.assert_healthy();
    }
}

#[test]
fn remote_verification_names_each_input_mismatch() {
    // The same tree from another commit is still another commit.
    let mut f = fixture();
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    let receipt = f.assert_uncredited(RemoteVerificationReason::CommitMismatch);
    assert_eq!(receipt.qualification.detail, None);

    let mut f = fixture();
    legacy_input(&mut f.request.completion.input).commit = None;
    let receipt = f.assert_uncredited(RemoteVerificationReason::CommitMismatch);
    assert!(
        receipt
            .qualification
            .detail
            .unwrap()
            .contains("no tested commit")
    );

    // A test ref judges the exact tree, whatever commit carried it.
    let mut f = fixture();
    f.request.association.intended_use = test_ref();
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    assert!(f.record().unwrap().qualification.credited);

    let mut f = fixture();
    f.request.association.intended_use = test_ref();
    legacy_input(&mut f.request.completion.input).tree =
        Some(GitTreeId::parse(object_name("other tree")).unwrap());
    f.assert_uncredited(RemoteVerificationReason::TreeMismatch);

    let mut f = fixture();
    f.request.association.intended_use = test_ref();
    legacy_input(&mut f.request.completion.input).tree = None;
    let receipt = f.assert_uncredited(RemoteVerificationReason::TreeMismatch);
    assert!(
        receipt
            .qualification
            .detail
            .unwrap()
            .contains("no tested tree")
    );

    // A pass that cannot state its exit or finish time is not a settled pass.
    let mut f = fixture();
    f.request.completion.result.exit = Some(1);
    f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
    let mut f = fixture();
    f.request.completion.result.exit = None;
    f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
    let mut f = fixture();
    f.request.completion.finished_at = None;
    let receipt = f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
    assert!(
        receipt
            .qualification
            .detail
            .unwrap()
            .contains("finish time")
    );
}

#[test]
fn remote_verification_unsupported_schema_reserves_nothing_and_stays_off_the_item() {
    let mut f = fixture();
    f.request.completion.schema_version = 2;
    let run = f.run_head();
    let project = f.project_head();
    let receipt = f.assert_uncredited(RemoteVerificationReason::ProducerUnsupported);
    assert_eq!(receipt.origin, None);
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 0);
    assert_eq!(f.run_head(), run);
    assert_eq!(f.project_head(), project + 1);
}

#[test]
fn remote_verification_two_different_terminals_conflict_and_the_first_stays() {
    let mut f = fixture();
    let first = f.record().unwrap();
    let origin = first.origin.clone().unwrap();
    f.request.operation_key = "remote:test-remote:two".into();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    f.request.completion.result.reason = "stage-failed".into();
    let conflict = f.assert_uncredited(RemoteVerificationReason::ResultConflict);
    assert_eq!(conflict.origin, Some(origin.clone()));
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 1);
    // The first credit is untouched and the settled facts are the first ones.
    let stored: RemoteTerminalOrigin = super::super::feeds::load_typed_work_object(
        &f.store.connection,
        &origin,
        REMOTE_TERMINAL_ORIGIN_KIND,
    )
    .unwrap();
    assert_eq!(stored.completion.result.verdict, RemoteRunVerdict::Pass);
    assert_eq!(f.count("verification_evidence"), 1);

    // Even a different native failure pair conflicts: immutable either way.
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    f.request.completion.result.reason = "stage-failed".into();
    f.record().unwrap();
    f.request.operation_key = "remote:test-remote:two".into();
    f.request.completion.result.verdict = RemoteRunVerdict::Pass;
    f.request.completion.result.exit = Some(0);
    f.request.completion.result.reason = "completed".into();
    f.assert_uncredited(RemoteVerificationReason::ResultConflict);

    // One host's run is not another host's run of the same name.
    let mut f = fixture();
    f.record().unwrap();
    f.request.operation_key = "remote:test-remote:other-host".into();
    f.request.completion.origin.host_id = "mac".into();
    f.request.completion.result.exit = Some(1);
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.reason = "stage-failed".into();
    let other = f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 2);
    assert!(other.origin.is_some());
}

#[test]
fn remote_verification_reasons_follow_the_agreed_precedence() {
    // A stale destination comes before every native or input fact.
    let mut f = fixture();
    f.request.association.root.generation = 2;
    f.request.completion.mode = RemoteRunMode::Focused;
    legacy_input(&mut f.request.completion.input).commit = None;
    f.assert_uncredited(RemoteVerificationReason::AssociationStale);

    // A conflict comes before not_full_mode and the input.
    let mut f = fixture();
    f.record().unwrap();
    f.request.operation_key = "remote:test-remote:two".into();
    f.request.completion.mode = RemoteRunMode::Focused;
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    f.assert_uncredited(RemoteVerificationReason::ResultConflict);

    // not_full_mode comes before result_not_pass.
    let mut f = fixture();
    f.request.completion.mode = RemoteRunMode::Focused;
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    f.assert_uncredited(RemoteVerificationReason::NotFullMode);

    // Native facts come before the input, and the input before fingerprints.
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    f.assert_uncredited(RemoteVerificationReason::ResultNotPass);
    let mut f = fixture();
    f.request.association.intended_use = committed(RemoteVerificationPurpose::LandingGate);
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    legacy_input(&mut f.request.completion.input).after_fingerprint = None;
    f.assert_uncredited(RemoteVerificationReason::CommitMismatch);
}

#[test]
fn remote_verification_moved_scope_writes_nothing_on_the_item() {
    type Change = fn(&mut Fixture);
    let moves: [Change; 5] = [
        |f| f.request.association.root.generation = 2,
        |f| f.request.association.root.workspace_id = "workspace-other".into(),
        |f| f.request.association.binding.work_revision += 1,
        |f| f.request.association.binding.claim_fence += 1,
        |f| f.request.association.binding.work_id = WorkId::new(),
    ];
    for change in moves {
        let mut f = fixture();
        change(&mut f);
        let run = f.run_head();
        let project = f.project_head();
        let receipt = f.assert_uncredited(RemoteVerificationReason::AssociationStale);
        assert!(receipt.qualification.detail.is_some());
        assert_eq!(f.run_head(), run, "nothing on the item's run");
        // The terminal origin and the visible delivery, both project-scoped.
        assert_eq!(f.project_head(), project + 2);
        assert!(
            receipt.origin.is_some(),
            "the run's terminal is still a fact"
        );
    }

    // A claim retaken after expiry moves the fence of the old binding.
    let mut f = fixture();
    let binding = f.request.association.binding.clone();
    let item = f.store.get_work_item(binding.work_id).unwrap();
    let retaken = claim(&mut f.store, &item, "runner", "retake", 4000, 3600);
    assert!(retaken.fence > binding.claim_fence);
    let run = f.run_head();
    f.assert_uncredited(RemoteVerificationReason::AssociationStale);
    assert_eq!(f.run_head(), run);
}

#[test]
fn remote_verification_exact_retry_replays_after_scope_moves_and_changed_intent_conflicts() {
    let mut f = fixture();
    let receipt = f.record().unwrap();
    let snapshot = f.snapshot();
    assert_eq!(f.record_at(11).unwrap(), receipt);
    assert_eq!(f.snapshot(), snapshot);

    let binding = f.request.association.binding.clone();
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            "workspace-moved",
            2,
            at(11),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "rename",
            at(11),
        )
        .unwrap();
    let snapshot = f.snapshot();
    assert_eq!(
        f.record_at(12).unwrap(),
        receipt,
        "replay precedes authority"
    );
    assert_eq!(f.snapshot(), snapshot);

    f.request.completion.result.reason = "changed".into();
    assert!(matches!(
        f.record_at(12),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    assert_eq!(f.snapshot(), snapshot);
    f.assert_healthy();
}

#[test]
fn remote_verification_repeat_under_a_new_key_earns_no_second_credit() {
    let mut f = fixture();
    let receipt = f.record().unwrap();
    let associations = f.count(REMOTE_VERIFICATION_ASSOCIATION_KIND);
    let evidence = f.count("verification_evidence");
    f.request.operation_key = "remote:test-remote:again".into();
    assert_eq!(f.record().unwrap(), receipt);
    assert_eq!(f.count(REMOTE_VERIFICATION_ASSOCIATION_KIND), associations);
    assert_eq!(f.count("verification_evidence"), evidence);
    f.assert_healthy();
    // The same result for another stated use is another association.
    f.request.operation_key = "remote:test-remote:landing".into();
    f.request.association.intended_use = committed(RemoteVerificationPurpose::LandingGate);
    let landing = f.record().unwrap();
    assert_ne!(landing.operation, receipt.operation);
    assert_eq!(landing.origin, receipt.origin);
    assert!(landing.qualification.credited);
    f.assert_healthy();
}

#[test]
fn remote_verification_one_origin_serves_distinct_items() {
    let mut f = fixture();
    let first = f.record().unwrap();
    let second = f.second_destination();
    f.request.operation_key = "remote:test-remote:second-item".into();
    f.request.association = second.clone();
    let other = f.record().unwrap();
    assert_eq!(other.origin, first.origin);
    assert_ne!(other.operation, first.operation);
    assert!(other.qualification.credited);
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 1);
    assert_eq!(f.count(REMOTE_VERIFICATION_ASSOCIATION_KIND), 2);
    let evidence: VerificationEvidence = super::super::feeds::load_typed_work_object(
        &f.store.connection,
        &other.verifications[0].record_id,
        "verification_evidence",
    )
    .unwrap();
    assert_eq!(evidence.binding, second.binding);
    assert_eq!(evidence.source_basis.workspace_id, "workspace-b");
    assert_eq!(evidence.bound_from, None, "a delivery, not a bound copy");
    f.assert_healthy();
}

#[test]
fn remote_verification_a_later_holder_associates_the_existing_origin_under_its_own_key() {
    let mut f = fixture();
    let first = f.record().unwrap();
    let later = SessionId("later".into());
    let work = f
        .store
        .create_work(
            &root_request("project-a", "later-remote-work", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut f.store, &work, "later", "later-claim", 2, 3600);
    let work = f.store.get_work_item(work.work_id).unwrap();
    let connection = f.store.resume_work_connection(&later, at(3)).unwrap();
    f.store
        .bind_named_root(
            &f.project,
            &later,
            &connection,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "workspace-later",
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("later"),
            "name-later",
            at(4),
        )
        .unwrap();
    f.request.operation_key = "remote:test-remote:later".into();
    f.request.association.binding = ControlWorkBinding {
        root_execution_id: f.store.get_work_run(held.run_id).unwrap().root_execution_id,
        work_id: work.work_id,
        run_id: held.run_id,
        work_revision: work.revision,
        claim_id: held.claim_id,
        claim_fence: held.fence,
    };
    f.request.association.root.workspace_id = "workspace-later".into();
    let receipt = f
        .store
        .record_remote_verification(
            &f.project,
            &later,
            &connection,
            crate::storage::HostSessionAuthority::Work,
            &actor("later"),
            &f.request,
            at(10),
        )
        .unwrap();
    assert!(receipt.qualification.credited, "{receipt:?}");
    assert_eq!(receipt.origin, first.origin);
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 1);
    f.assert_healthy();

    // Another session's binding is stale for this session's connection.
    f.request.operation_key = "remote:test-remote:not-mine".into();
    f.assert_uncredited(RemoteVerificationReason::AssociationStale);
}

#[test]
fn remote_verification_malformed_values_are_refused_without_effects() {
    type Change = fn(&mut RemoteVerificationRequest);
    let cases: [(Change, &str); 11] = [
        (|r| r.operation_key = " padded".into(), "operation_key"),
        (|r| r.completion.origin.host_id = String::new(), "origin"),
        (
            |r| r.completion.origin.runner_session_id = "bad\u{202e}text".into(),
            "origin",
        ),
        (|r| r.completion.run_id = "x".repeat(129), "run_id"),
        (
            |r| r.completion.result.reason = String::new(),
            "result.reason",
        ),
        (|r| r.completion.result.exit = Some(i64::MAX), "result.exit"),
        (
            |r| legacy_input(&mut r.completion.input).commit = Some("A".repeat(40)),
            "input.commit",
        ),
        (
            |r| legacy_input(&mut r.completion.input).before_fingerprint = Some(" ".into()),
            "input.fingerprint",
        ),
        (|r| r.association.root.generation = 0, "association.root"),
        (
            |r| {
                r.association.intended_use = RemoteVerificationUse::CommittedInput {
                    expected_commit: "b".repeat(64),
                    purpose: RemoteVerificationPurpose::Verification,
                };
            },
            "association.use.expected_commit",
        ),
        (
            |r| {
                r.association.intended_use = RemoteVerificationUse::AuthorizedTestRef {
                    ref_name: "refs/heads/test/x".into(),
                    expected_tree: tested_tree(),
                    purpose: RemoteVerificationPurpose::Verification,
                };
            },
            "association.use.purpose",
        ),
    ];
    for (change, expected) in cases {
        let mut f = fixture();
        change(&mut f.request);
        let before = f.snapshot();
        match f.record() {
            Err(StoreError::RemoteVerificationInvalid { field, .. }) => {
                assert_eq!(field, expected);
            }
            other => panic!("{expected}: {other:?}"),
        }
        assert_eq!(f.snapshot(), before, "{expected}");
    }
}

#[test]
fn remote_verification_finish_time_ahead_of_this_clock_stays_provisional() {
    let mut f = fixture();
    let provisional = f.record_at(8).unwrap();
    assert!(!provisional.qualification.credited);
    assert_eq!(
        provisional.qualification.reason,
        Some(RemoteVerificationReason::ResultNotPass)
    );
    assert_eq!(provisional.origin, None, "it reserves no terminal");
    assert_eq!(provisional.verifications.len(), 0);
    let detail = provisional.qualification.detail.clone().unwrap();
    assert!(detail.contains(&at(9).to_rfc3339()), "{detail}");
    assert!(detail.contains(&at(8).to_rfc3339()), "{detail}");
    assert_eq!(f.count(REMOTE_TERMINAL_ORIGIN_KIND), 0);
    f.assert_healthy();
    // It stays visible on the item, and its exact retry stays its own receipt.
    let run = f.run_head();
    assert_eq!(f.record_at(12).unwrap(), provisional);
    assert_eq!(f.run_head(), run);
    // A later delivery settles under a new key, with no conflict.
    f.request.operation_key = "remote:test-remote:settled".into();
    let settled = f.record_at(12).unwrap();
    assert!(settled.qualification.credited, "{settled:?}");
    assert!(settled.origin.is_some());
    f.assert_healthy();

    // A focused run ahead of the clock is still only a focused run.
    let mut f = fixture();
    f.request.completion.mode = RemoteRunMode::Focused;
    let focused = f.record_at(8).unwrap();
    assert_eq!(
        focused.qualification.reason,
        Some(RemoteVerificationReason::NotFullMode)
    );
    assert_eq!(focused.origin, None);
    f.assert_healthy();
}

#[test]
fn remote_verification_receipts_and_origin_rows_stand_on_their_records() {
    let mut f = fixture();
    f.record().unwrap();
    f.assert_healthy();
    let deleted = f
        .store
        .connection
        .execute(
            "DELETE FROM work_operation_results WHERE operation = ?1",
            [ORIGIN_OPERATION],
        )
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(
        !f.store.verify_all().unwrap().is_healthy(),
        "an origin without its row could admit a second terminal"
    );

    let mut f = fixture();
    let receipt = f.record().unwrap();
    let other = ObjectId::mint();
    f.store
        .connection
        .execute(
            "UPDATE work_operation_results SET result_json = ?1 WHERE operation = ?2",
            rusqlite::params![serde_json::to_vec(&other).unwrap(), ORIGIN_OPERATION],
        )
        .unwrap();
    assert!(!f.store.verify_all().unwrap().is_healthy());
    assert!(receipt.origin.is_some());
}

/// Rewrites one field of a stored delivery, as damage or forgery would.
fn tamper(f: &Fixture, id: &ObjectId, edits: &[(&str, serde_json::Value)]) {
    for (path, value) in edits {
        let bytes: Vec<u8> = f
            .store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let mut record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let (parent, leaf) = path.rsplit_once('/').unwrap();
        let holder = record.pointer_mut(parent).unwrap().as_object_mut().unwrap();
        if value.is_null() {
            holder.remove(leaf);
        } else {
            holder.insert(leaf.to_owned(), value.clone());
        }
        f.store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
                rusqlite::params![
                    id.as_str(),
                    crate::canonical::canonical_bytes(&record).unwrap()
                ],
            )
            .unwrap();
    }
}

fn assert_doctor_names(f: &Fixture, problem: &str) {
    let report = f.store.verify_all().unwrap();
    assert!(!report.is_healthy());
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|finding| finding.ends_with(problem)),
        "{problem} not in {:?}",
        report.invalid_work_records
    );
}

#[test]
fn remote_verification_doctor_names_a_forged_decision() {
    // Credit claimed over a focused completion.
    let mut f = fixture();
    f.request.completion.mode = RemoteRunMode::Focused;
    let receipt = f.record().unwrap();
    tamper(
        &f,
        &receipt.operation,
        &[
            ("/qualification/credited", serde_json::json!(true)),
            ("/qualification/reason", serde_json::Value::Null),
        ],
    );
    assert_doctor_names(&f, "remote_verification:decision_differs");

    // A stored reason the facts do not give.
    let mut f = fixture();
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    let receipt = f.record().unwrap();
    tamper(
        &f,
        &receipt.operation,
        &[("/qualification/reason", serde_json::json!("tree_mismatch"))],
    );
    assert_doctor_names(&f, "remote_verification:decision_differs");

    // A conflict with nothing different from the settled terminal.
    let mut f = fixture();
    f.request.completion.mode = RemoteRunMode::Focused;
    let receipt = f.record().unwrap();
    tamper(
        &f,
        &receipt.operation,
        &[(
            "/qualification/reason",
            serde_json::json!("result_conflict"),
        )],
    );
    assert_doctor_names(&f, "remote_verification:conflict_without_difference");

    // A delivery called stale that stands on its item's feeds.
    let mut f = fixture();
    f.request.completion.mode = RemoteRunMode::Focused;
    let receipt = f.record().unwrap();
    tamper(
        &f,
        &receipt.operation,
        &[(
            "/qualification/reason",
            serde_json::json!("association_stale"),
        )],
    );
    assert_doctor_names(&f, "wrong_membership");

    // An origin that its completion could not have settled.
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Running;
    f.request.completion.result.terminal = false;
    let receipt = f.record().unwrap();
    tamper(
        &f,
        &receipt.operation,
        &[("/origin", serde_json::json!(ObjectId::mint()))],
    );
    assert_doctor_names(&f, "remote_verification:origin_differs");
}

#[test]
fn remote_verification_doctor_names_a_missing_origin_without_writing() {
    let mut f = fixture();
    let receipt = f.record().unwrap();
    assert!(receipt.qualification.credited);
    assert!(receipt.origin.is_some());
    f.assert_healthy();
    let missing = ObjectId::mint();
    let existing: i64 = f
        .store
        .connection
        .query_row(
            "SELECT count(*) FROM objects WHERE object_id = ?1",
            [missing.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(existing, 0);
    tamper(
        &f,
        &receipt.operation,
        &[("/origin", serde_json::json!(missing))],
    );
    let before = f.snapshot();
    assert_doctor_names(&f, "remote_verification:origin_differs");
    assert_eq!(f.snapshot(), before);
}

#[test]
fn remote_verification_doctor_names_an_origin_of_the_wrong_kind_without_writing() {
    let mut f = fixture();
    let receipt = f.record().unwrap();
    assert!(receipt.qualification.credited);
    assert!(receipt.origin.is_some());
    assert_ne!(receipt.origin.as_ref(), Some(&receipt.operation));
    f.assert_healthy();
    tamper(
        &f,
        &receipt.operation,
        &[("/origin", serde_json::json!(receipt.operation))],
    );
    let before = f.snapshot();
    assert_doctor_names(&f, "remote_verification:origin_differs");
    assert_eq!(f.snapshot(), before);
}

#[test]
fn remote_verification_doctor_finds_credit_with_no_minted_verification() {
    // A real mismatch rewritten to qualify: the decision now agrees with the
    // facts, so only the missing verification shows it never earned credit.
    let mut f = fixture();
    f.request.association.intended_use = RemoteVerificationUse::CommittedInput {
        expected_commit: object_name("other commit"),
        purpose: RemoteVerificationPurpose::Verification,
    };
    let receipt = f.record().unwrap();
    assert_eq!(receipt.verifications.len(), 0);
    tamper(
        &f,
        &receipt.operation,
        &[
            (
                "/request/association/use/expected_commit",
                serde_json::json!(tested_commit()),
            ),
            ("/qualification/credited", serde_json::json!(true)),
            ("/qualification/reason", serde_json::Value::Null),
        ],
    );
    assert_doctor_names(&f, "remote_verification:verification_count_differs");
}

#[test]
fn remote_verification_delivery_next_and_peek_for_author_and_peer() {
    use crate::work_service::{
        LocalWorkService, WorkChangeProjection, WorkNextQuery, WorkNextSection,
    };
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let store =
        SqliteStore::open_with_host_path_policy(&database, crate::HostPathPolicy::host_default())
            .unwrap();
    let mut f = fixture_in(store, 0, None);
    f.record().unwrap();
    // A stale delivery to an item that does not exist is delivered too.
    f.request.operation_key = "remote:test-remote:stale".into();
    f.request.association.binding.work_id = WorkId::new();
    f.record().unwrap();
    let project = f.project.clone();
    drop(f.store);
    for session in ["runner", "peer"] {
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            session.into(),
            SessionId(session.into()),
            None,
        );
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Changes],
            ..WorkNextQuery::default()
        };
        let peek = service
            .work_next_peek_for_agent(100, 100, true, query.clone(), at(11), |_| true)
            .unwrap();
        let next = service.work_next(100, query, at(12)).unwrap();
        if session != "peer" {
            continue;
        }
        for changes in [peek.changes.unwrap(), next.changes.unwrap()] {
            let summaries: Vec<_> = changes
                .iter()
                .filter_map(|change| match &change.delivery {
                    WorkChangeProjection::Visible(summary) => Some(summary),
                    WorkChangeProjection::Omitted(_) => None,
                })
                .collect();
            let of_kind = |kind: &str, text: &str| {
                summaries
                    .iter()
                    .any(|summary| summary.change_kind == kind && summary.summary.contains(text))
            };
            assert!(of_kind(REMOTE_TERMINAL_ORIGIN_KIND, "PASS"));
            assert!(of_kind(REMOTE_VERIFICATION_ASSOCIATION_KIND, "credited"));
            assert!(of_kind(
                REMOTE_VERIFICATION_ASSOCIATION_KIND,
                "association_stale"
            ));
        }
    }
}

#[test]
fn remote_verification_show_keeps_remote_and_local_times_apart_and_refuses_citation() {
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    f.request.completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
    let receipt = f.record().unwrap();
    let binding = f.request.association.binding.clone();
    let index = f
        .store
        .work_record_index(
            &f.project,
            binding.work_id,
            crate::storage::WorkRecordKind::NotesWithGates,
        )
        .unwrap();
    let row = index
        .iter()
        .find(|row| row.address.hash == receipt.operation)
        .expect("the delivery is in the item's records");
    assert_eq!(
        row.record_family,
        crate::storage::WorkRecordFamily::Observations
    );
    let crate::storage::WorkRecordContent::Note(note) = f
        .store
        .work_record_content(&f.project, binding.work_id, row)
        .unwrap()
    else {
        panic!("a delivery reads as a note");
    };
    assert_eq!(note.recorded_at, at(10));
    for fact in [
        "remote full run test-remote on dl560 runner native-root",
        "FAIL",
        "reason=new-untracked-only",
        &format!("commit={}", tested_commit()),
        &format!("finished_at={}", at(9).to_rfc3339()),
        &format!("associated_at={}", at(10).to_rfc3339()),
        "committed_input verification",
        "not credited: untracked_debris_pending_disposition",
    ] {
        assert!(note.summary.contains(fact), "{fact} in {}", note.summary);
    }
    // A visible delivery is never run evidence an agent may cite.
    assert!(
        f.store
            .resolve_criterion_evidence_classified(
                &f.project,
                binding.work_id,
                binding.run_id,
                receipt.operation.as_str(),
                &index,
            )
            .unwrap()
            .is_err()
    );
}

#[test]
fn remote_verification_import_and_projection_repair_preserve_receipt_and_ids() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let export = directory.path().join("store.jsonl");
    let store =
        SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
            .unwrap();
    let mut f = fixture_in(store, 0, None);
    let receipt = f.record().unwrap();
    let bytes = |store: &SqliteStore, id: &ObjectId| -> Vec<u8> {
        store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    };
    let association = bytes(&f.store, &receipt.operation);
    let origin = bytes(&f.store, receipt.origin.as_ref().unwrap());
    drop(f.store);
    crate::storage::migration::export_json(&source, &export).unwrap();
    crate::storage::migration::import_json(&export, &target).unwrap();
    assert!(
        SqliteStore::repair_rebuildable_projections(&target)
            .unwrap()
            .is_healthy()
    );
    f.store =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    f.connection = f.store.resume_work_connection(&f.session, at(11)).unwrap();
    assert_eq!(f.record_at(12).unwrap(), receipt);
    assert_eq!(bytes(&f.store, &receipt.operation), association);
    assert_eq!(bytes(&f.store, receipt.origin.as_ref().unwrap()), origin);
    // The origin's uniqueness survives too.
    f.request.operation_key = "remote:test-remote:after-import".into();
    f.request.completion.result.reason = "different".into();
    assert_eq!(
        f.record_at(12).unwrap().qualification.reason,
        Some(RemoteVerificationReason::ResultConflict)
    );
    f.assert_healthy();
}

#[test]
fn remote_verification_stays_out_of_shared_task_text() {
    let mut f = fixture();
    f.record().unwrap();
    let result = f
        .store
        .export_task_tree_with_notes(&f.project, "host-A")
        .unwrap();
    // The delivery and the verification it credited, both kept local.
    assert_eq!(result.omissions.counts["host verification"], 2);
    let shared = serde_json::to_string(&result.tree).unwrap();
    assert!(!shared.contains("test-remote"), "{shared}");
    assert!(!shared.contains("dl560"), "{shared}");
}

#[test]
fn remote_verification_decides_existing_stores_without_changing_their_schema() {
    // The two kinds and the two operation names need no durable shape: the
    // schema is the same before and after a store first holds them.
    let schema = |store: &SqliteStore| -> Vec<(String, Option<String>)> {
        let mut statement = store
            .connection
            .prepare("SELECT name, sql FROM sqlite_schema ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let mut f = fixture();
    let before = schema(&f.store);
    f.record().unwrap();
    assert_eq!(schema(&f.store), before);
}

#[test]
fn remote_verification_an_agent_gate_never_turns_a_native_failure_into_credit() {
    let mut create = root_request("project-a", "gated-remote-work", 1);
    create.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut f = fixture_in(SqliteStore::open_in_memory().unwrap(), 0, Some(&create));
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.exit = Some(1);
    f.request.completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
    f.assert_uncredited(RemoteVerificationReason::UntrackedDebrisPendingDisposition);
    let binding = f.request.association.binding.clone();
    let work = f.store.get_work_item(binding.work_id).unwrap();
    let held = f.store.claim_fixture(work.work_id);
    let gate = f
        .store
        .record_gate_evidence(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id: binding.run_id,
                expected_work_revision: work.revision,
                holder: f.session.clone(),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                name: "full".into(),
                failed: Vec::new(),
                evidence_ref: None,
                actor: actor("runner"),
                recorded_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        f.count("verification_evidence"),
        0,
        "a gate mints no credit"
    );
    // Nor does a check the host observed the host passing.
    let observed = downstream::observe(
        &mut f,
        "observed-check",
        11,
        ObservedOccurrence::UnadmittedTurn {
            outcome: None,
            host_turn_ref: "turn-observed".into(),
            source_change: None,
            observed_checks: vec![ObservedCheck {
                host_check_id: "native-full".into(),
                check_kind: VerificationKind::Test,
                observed_result: VerificationResult::Passed,
                started_at: Some(at(9)),
                finished_at: Some(at(10)),
                observed_at: at(10),
                source_basis: None,
                host_evidence_ref: None,
            }],
        },
    );
    assert_eq!(observed.observed_checks.len(), 1);
    assert_eq!(
        observed.observed_checks[0].credit,
        ObservedCheckCredit::Uncredited
    );
    assert_eq!(
        f.count("verification_evidence"),
        0,
        "nor does an observed check"
    );
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "gate-checkpoint",
        12,
        std::slice::from_ref(&gate),
    );
    let request = completion_request(&work, &held, "runner", &gate, "gate-completion", 13);
    assert!(
        f.store
            .complete_work(&request, &DevelopmentNoopRedactor)
            .is_err(),
        "a bound test criterion needs verification, not a gate"
    );
}
