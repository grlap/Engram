use super::*;
use crate::domain::*;
use crate::storage::work::test_support::{actor, at, claim, root_request};
use crate::storage::{HostSessionAuthority, SqliteStore};
use crate::task_sync::evidence::verification::ApplicabilityRequest;
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope};
use crate::task_sync::origin::Enrollment;
use crate::{DevelopmentNoopRedactor, ProjectId, SessionId};
use applicability::APPLICABILITY_KIND;

#[path = "tests/kept_root.rs"]
mod kept_root;

#[path = "../acceptance/tests.rs"]
mod acceptance;

struct Fixture {
    store: SqliteStore,
    binding: ControlWorkBinding,
    request: RemoteVerificationRequest,
    token: String,
    workspace: String,
}

fn project() -> ProjectId {
    ProjectId("union-project".into())
}
fn session() -> SessionId {
    SessionId("runner".into())
}
fn git(root: &std::path::Path, args: &[&str]) {
    let mut command = crate::task_sync::git::command(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.templateDir=",
        ],
    );
    let output = command
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"))
        .env_remove("GIT_TEMPLATE_DIR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn repository() -> crate::test_support::TempHome {
    let directory = crate::test_support::temp_home().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("file"), "initial\n").unwrap();
    git(directory.path(), &["add", "file"]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Create fixture",
        ],
    );
    directory
}
fn source(root: &std::path::Path) -> crate::source::SourceId {
    crate::source::inspect(root, crate::source::MAX_BUDGET)
        .source_id
        .unwrap()
}

impl Fixture {
    fn new(root: &std::path::Path) -> Self {
        Self::with_bindings(root, true)
    }
    fn with_bindings(root: &std::path::Path, bound: bool) -> Self {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut request = root_request(&project().0, "union work", 1);
        request.acceptance_bindings = vec![AcceptanceBinding {
            criterion: 1,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
        }];
        if !bound {
            request.acceptance_bindings.clear();
        }
        let work = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        Self::with_work(root, store, &work)
    }
    fn with_work(root: &std::path::Path, mut store: SqliteStore, work: &WorkItem) -> Self {
        let held = claim(&mut store, work, "runner", "claim", 2, 3600);
        let work = store.get_work_item(work.work_id).unwrap();
        let token = store.resume_work_connection(&session(), at(3)).unwrap();
        let workspace = root.to_string_lossy().into_owned();
        store
            .bind_named_root(
                &project(),
                &session(),
                &token,
                HostSessionAuthority::Work,
                held.claim_id,
                held.fence,
                &workspace,
                1,
                at(4),
                NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "named",
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
        let source = source(root);
        let request = RemoteVerificationRequest {
            operation_key: "remote-one".into(),
            completion: RemoteRunCompletion {
                schema_version: REMOTE_COMPLETION_SCHEMA_VERSION,
                origin: RemoteRunOrigin {
                    host_id: "origin-host".into(),
                    runner_session_id: "native-runner".into(),
                },
                run_id: "test-union".into(),
                mode: RemoteRunMode::Focused,
                result: RemoteRunResult {
                    verdict: RemoteRunVerdict::Pass,
                    terminal: true,
                    exit: Some(0),
                    reason: "completed".into(),
                },
                input: RemoteRunInput::SourceId(Box::new(RemoteSourceIdInput::new(
                    Some(source.clone()),
                    Some(source.clone()),
                ))),
                finished_at: Some(at(9)),
            },
            association: RemoteVerificationAssociation {
                binding: binding.clone(),
                root: RegisteredVerificationRoot {
                    workspace_id: workspace.clone(),
                    generation: 1,
                    state: SourceRootState::Named,
                },
                intended_use: RemoteVerificationUse::CommittedInput {
                    expected_commit: source.commit,
                    purpose: RemoteVerificationPurpose::Verification,
                },
            },
        };
        Self {
            store,
            binding,
            request,
            token,
            workspace,
        }
    }
    fn record(&mut self, second: i64) -> RemoteVerificationReceipt {
        self.store
            .record_remote_verification(
                &project(),
                &session(),
                &self.token,
                HostSessionAuthority::Work,
                &actor("runner"),
                &self.request,
                at(second),
            )
            .unwrap()
    }
    fn observe(&mut self, root: &std::path::Path, second: i64) {
        self.report_source(root, second, false);
    }
    fn report_source(&mut self, root: &std::path::Path, second: i64, changed: bool) {
        let current = self
            .store
            .read_named_root(
                &project(),
                &session(),
                &self.token,
                HostSessionAuthority::Work,
                self.binding.run_id,
                self.binding.claim_id,
            )
            .unwrap();
        let generation = match &current.named_root {
            NamedRootState::Bound { generation, .. } => *generation,
            state => panic!("fixture capture requires a bound root: {state:?}"),
        };
        let policy = SqliteStore::load_active_control_policy(&self.store.connection).unwrap();
        let mut observer = actor("runner");
        observer.run_id = Some(self.binding.run_id.0.to_string());
        self.store
            .record_unadmitted_execution_observation(
                &project(),
                &session(),
                &self.token,
                HostSessionAuthority::Work,
                &observer,
                ExecutionObserveInput {
                    idempotency_key: format!("sighting-{second}"),
                    binding: self.binding.clone(),
                    root_basis: ObservationRootBasis {
                        capture_run_cut: current.read_cut.position,
                        latest_event: current
                            .latest_event
                            .as_ref()
                            .map(|event| event.event.clone()),
                        state: current.named_root,
                    },
                    observed_interval: ObservedInterval {
                        from: at(second),
                        through: at(second),
                    },
                    occurrence: if changed {
                        ObservedOccurrence::InterTurnChange {
                            source_change: ObservedSourceChange::AssumedMissingBaseline {
                                workspace_id: self.workspace.clone(),
                                sighting: MeasuredSighting {
                                    source_basis: VerificationSourceBasis {
                                        workspace_id: self.workspace.clone(),
                                        identity: VerificationSourceIdentity::SourceId {
                                            source_id: source(root),
                                        },
                                        source_root_generation: Some(generation),
                                        source_root_state: Some(SourceRootState::Named),
                                    },
                                    observed_at: at(second),
                                },
                            },
                        }
                    } else {
                        ObservedOccurrence::SourceSighting {
                            source_basis: VerificationSourceBasis {
                                workspace_id: self.workspace.clone(),
                                identity: VerificationSourceIdentity::SourceId {
                                    source_id: source(root),
                                },
                                source_root_generation: Some(generation),
                                source_root_state: Some(SourceRootState::Named),
                            },
                            observed_at: at(second),
                        }
                    },
                    causality: ObservationCausality::Unknown {},
                    policy_basis: ObservationPolicyBasis::AccountIfEligible {
                        project_policy_epoch: policy.epoch,
                        policy: policy.policy_id,
                        obligation_rule_set: policy.obligation_rule_set,
                    },
                },
                at(second),
            )
            .unwrap();
    }
    fn local_actor(&self) -> crate::ActorContext {
        let mut result = actor("runner");
        result.run_id = Some(self.binding.run_id.0.to_string());
        result
    }
}

fn enrollment() -> (ObjectId, RecordEnvelope) {
    let id = ObjectId::mint();
    let enrollment = Enrollment {
        project_id: project(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: actor("exporter"),
        enrolled_at: at(20),
    };
    let record = RecordEnvelope::from_canonical(
        id.clone(),
        id.clone(),
        RecordKind::ProducingOrigin.as_str(),
        serde_json_canonicalizer::to_vec(&enrollment).unwrap(),
    )
    .unwrap();
    (id, record)
}
fn envelope(store: &SqliteStore, origin: &ObjectId, id: &ObjectId) -> RecordEnvelope {
    let (kind, bytes) = object_on(&store.connection, id).unwrap().unwrap();
    RecordEnvelope::from_canonical(origin.clone(), id.clone(), &kind, bytes).unwrap()
}
fn pack(origin: &ObjectId, records: Vec<RecordEnvelope>) -> EvidencePack {
    EvidencePack::prepare(ObjectId::mint(), project(), origin.clone(), records).unwrap()
}
fn with_witnesses(
    fixture: &Fixture,
    origin: &ObjectId,
    records: Vec<RecordEnvelope>,
) -> EvidencePack {
    pack(
        origin,
        with_feed_witnesses(
            fixture,
            origin,
            &crate::FeedId::RunExecution(fixture.binding.run_id),
            records,
        ),
    )
}

fn with_feed_witnesses(
    fixture: &Fixture,
    origin: &ObjectId,
    feed: &crate::FeedId,
    mut records: Vec<RecordEnvelope>,
) -> Vec<RecordEnvelope> {
    use crate::task_sync::evidence::witnesses::{
        FeedMember, OriginFeed, OriginFeedCut, OriginFeedSegment,
    };
    let (feed_kind, feed_id) = crate::storage::work::query::feed_parts(feed);
    let mut statement=fixture.store.connection.prepare("SELECT position,object_kind,object_id FROM work_feed_entries WHERE feed_kind=?1 AND feed_id=?2 ORDER BY position").unwrap();
    let members = statement
        .query_map([feed_kind, feed_id.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(|row| {
            let (position, object_kind, id) = row.unwrap();
            FeedMember {
                position,
                object_kind,
                object_id: ObjectId::from_stored(id).unwrap(),
            }
        })
        .collect::<Vec<_>>();
    for member in &members {
        if !records
            .iter()
            .any(|record| record.object_id() == &member.object_id)
        {
            records.push(envelope(&fixture.store, origin, &member.object_id));
        }
    }
    let source = OriginFeed {
        origin: origin.clone(),
        project_id: project(),
        feed: feed.clone(),
    };
    let through = members.last().unwrap().position;
    let endpoint = members.last().cloned();
    let segment_id = ObjectId::mint();
    let segment = OriginFeedSegment {
        source: source.clone(),
        from_exclusive: 0,
        through,
        predecessor: None,
        anchor: None,
        entries: members,
    };
    let cut = OriginFeedCut {
        source,
        through,
        endpoint,
        segments: vec![segment_id.clone()],
    };
    for (id, kind, bytes) in [
        (
            segment_id,
            RecordKind::OriginFeedSegment,
            serde_json_canonicalizer::to_vec(&segment).unwrap(),
        ),
        (
            ObjectId::mint(),
            RecordKind::OriginFeedCut,
            serde_json_canonicalizer::to_vec(&cut).unwrap(),
        ),
    ] {
        records.push(
            RecordEnvelope::from_canonical(origin.clone(), id, kind.as_str(), bytes).unwrap(),
        );
    }
    records
}
fn retain(store: &mut SqliteStore, pack: &EvidencePack) {
    store
        .retain_evidence_pack(&project(), pack, &ObjectId::mint())
        .unwrap();
}
fn healthy(store: &SqliteStore) {
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
fn state(store: &SqliteStore, id: &ObjectId) -> (bool, bool) {
    store.connection.query_row("SELECT eligible,invalidated FROM work_task_sync_evidence_applicability WHERE decision_id=?1",[id.as_str()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap()
}
fn intent(target: &Fixture, original: ObjectId, origin: ObjectId) -> ApplicabilityRequest {
    let cut=target.store.connection.query_row("SELECT witness.object_id FROM work_task_sync_evidence_witnesses witness WHERE witness.origin_id=?1 AND witness.object_kind='evidence_origin_feed_cut' LIMIT 1",[origin.as_str()],|row|row.get::<_,String>(0)).optional().unwrap().map_or_else(ObjectId::mint, |id|ObjectId::from_stored(id).unwrap());
    ApplicabilityRequest {
        original,
        origin,
        origin_cut: cut,
        binding: target.binding.clone(),
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
        criterion: None,
        decision: ObjectId::mint(),
    }
}

#[test]
fn union_is_order_independent_and_refused_association_keeps_its_own_terminal() {
    let root = repository();
    let mut a = Fixture::new(root.path());
    let pass = a.record(10);
    a.request.operation_key = "remote-conflict".into();
    a.request.completion.result.verdict = RemoteRunVerdict::Fail;
    let failed = a.record(11);
    assert_eq!(failed.origin, pass.origin);
    assert_eq!(
        failed.qualification.reason,
        Some(RemoteVerificationReason::ResultConflict)
    );
    let (origin, enrolled) = enrollment();
    let p = envelope(&a.store, &origin, pass.origin.as_ref().unwrap());
    let association = envelope(&a.store, &origin, &failed.operation);
    let base = pack(&origin, vec![enrolled, p]);
    let conflict = pack(&origin, vec![association.clone()]);
    let key = ExecutionKey::new(project(), &a.request.completion);
    for reverse in [false, true] {
        let mut target = SqliteStore::open_in_memory().unwrap();
        if reverse {
            // The complete association component still carries its linked P.
            let merged = pack(
                &origin,
                vec![
                    association.clone(),
                    base.records()[0].clone(),
                    base.records()[1].clone(),
                ],
            );
            retain(&mut target, &merged);
            retain(&mut target, &base);
        } else {
            retain(&mut target, &base);
            retain(&mut target, &conflict);
        }
        let union = union_on(&target.connection, &key).unwrap();
        assert!(union.conflicted);
        assert_eq!(union.contributors.len(), 2);
        let bytes = object_on(&target.connection, &failed.operation)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(bytes, association.canonical_bytes());
        assert_eq!(
            target
                .connection
                .query_row("SELECT count(*) FROM work_run_evidence", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        healthy(&target);
        super::rebuild_on(&target.connection).unwrap();
        healthy(&target);
    }
    healthy(&a.store);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "both delivery orders, late conflict and rollback are one retained execution scenario"
)]
fn equivalent_cross_store_results_are_one_execution_and_late_conflict_invalidates_only_current_use()
{
    for reverse in [false, true] {
        let root = repository();
        let mut a = Fixture::new(root.path());
        let pass = a.record(10);
        let mut b = Fixture::new(root.path());
        let same = b.record(10);
        let (origin_a, enrollment_a) = enrollment();
        let (origin_b, enrollment_b) = enrollment();
        let pack_a = with_witnesses(
            &a,
            &origin_a,
            vec![
                enrollment_a,
                envelope(&a.store, &origin_a, pass.origin.as_ref().unwrap()),
                envelope(&a.store, &origin_a, &pass.operation),
                envelope(&a.store, &origin_a, &pass.verifications[0].record_id),
            ],
        );
        let pack_b = pack(
            &origin_b,
            vec![
                enrollment_b,
                envelope(&b.store, &origin_b, same.origin.as_ref().unwrap()),
                envelope(&b.store, &origin_b, &same.operation),
            ],
        );
        let key = ExecutionKey::new(project(), &a.request.completion);
        let mut target = Fixture::new(root.path());
        target.observe(root.path(), 5);
        if reverse {
            retain(&mut target.store, &pack_a);
            retain(&mut target.store, &pack_b);
        } else {
            retain(&mut target.store, &pack_b);
            retain(&mut target.store, &pack_a);
        }
        let union = union_on(&target.store.connection, &key).unwrap();
        assert!(!union.conflicted);
        assert_eq!(union.contributors.len(), 4);
        let request = intent(
            &target,
            pass.verifications[0].record_id.clone(),
            origin_a.clone(),
        );
        let local_actor = target.local_actor();
        let decision = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &request, at(30))
            .unwrap();
        assert_eq!(state(&target.store, &decision), (true, false));
        let original_decision = object_on(&target.store.connection, &decision)
            .unwrap()
            .unwrap()
            .1;
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        assert_eq!(
            target
                .store
                .associate_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &request,
                    at(31)
                )
                .unwrap(),
            decision
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
        );
        a.request.operation_key = "conflict".into();
        a.request.completion.result.verdict = RemoteRunVerdict::Fail;
        let conflict = a.record(12);
        let pack_conflict = pack(
            &origin_a,
            vec![envelope(&a.store, &origin_a, &conflict.operation)],
        );
        let mut dangling: RemoteVerificationAssociationRecord = serde_json::from_slice(
            &object_on(&a.store.connection, &conflict.operation)
                .unwrap()
                .unwrap()
                .1,
        )
        .unwrap();
        dangling.origin = Some(ObjectId::mint());
        let dangling = RecordEnvelope::from_canonical(
            origin_a.clone(),
            ObjectId::mint(),
            RecordKind::RemoteVerificationAssociation.as_str(),
            serde_json_canonicalizer::to_vec(&dangling).unwrap(),
        )
        .unwrap();
        let invalid_pack = pack(
            &origin_a,
            vec![pack_conflict.records()[0].clone(), dangling],
        );
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        let mut tx = target.store.connection.transaction().unwrap();
        assert!(
            super::super::retain_pack_on(&mut tx, &project(), &invalid_pack, &ObjectId::mint())
                .is_err()
        );
        tx.commit().unwrap();
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
        );
        assert_eq!(state(&target.store, &decision), (true, false));
        retain(&mut target.store, &pack_conflict);
        assert_eq!(state(&target.store, &decision), (true, true));
        assert_eq!(
            object_on(&target.store.connection, &decision)
                .unwrap()
                .unwrap()
                .1,
            original_decision
        );
        let new_request = intent(&target, request.original.clone(), origin_a.clone());
        let refused = target
            .store
            .associate_imported_verification(
                &project(),
                &session(),
                &local_actor,
                &new_request,
                at(32),
            )
            .unwrap();
        assert_eq!(state(&target.store, &refused), (false, true));
        healthy(&target.store);
        super::rebuild_on(&target.store.connection).unwrap();
        healthy(&target.store);
        assert_eq!(state(&target.store, &decision), (true, true));
    }
}

#[test]
fn full_source_equality_accepts_clean_and_dirty_but_not_changed_components_or_carrier() {
    for dirty in [false, true] {
        let root = repository();
        if dirty {
            std::fs::write(root.path().join("file"), "dirty\n").unwrap();
        }
        let mut a = Fixture::new(root.path());
        let pass = a.record(10);
        let (origin, enrolled) = enrollment();
        let input = with_witnesses(
            &a,
            &origin,
            vec![
                enrolled,
                envelope(&a.store, &origin, pass.origin.as_ref().unwrap()),
                envelope(&a.store, &origin, &pass.operation),
                envelope(&a.store, &origin, &pass.verifications[0].record_id),
            ],
        );
        let mut target = Fixture::new(root.path());
        target.observe(root.path(), 5);
        retain(&mut target.store, &input);
        let local_actor = target.local_actor();
        let mut request = intent(
            &target,
            pass.verifications[0].record_id.clone(),
            origin.clone(),
        );
        let original: VerificationEvidence = serde_json::from_slice(
            &object_on(&a.store.connection, &request.original)
                .unwrap()
                .unwrap()
                .1,
        )
        .unwrap();
        request.requirement.check_fingerprint = Some(original.check_fingerprint);
        let id = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &request, at(30))
            .unwrap();
        assert_eq!(state(&target.store, &id), (true, false));
        let summary = target.store.imported_verification_summary(&id).unwrap();
        assert_eq!(summary.source, source(root.path()).summary());
        assert!(serde_json::to_vec(&summary).unwrap().len() < 1024);
        std::fs::write(root.path().join("file"), "changed\n").unwrap();
        target.observe(root.path(), 31);
        request.decision = ObjectId::mint();
        let id = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &request, at(32))
            .unwrap();
        assert!(!state(&target.store, &id).0);
        git(root.path(), &["add", "file"]);
        git(
            root.path(),
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "Carry task data",
            ],
        );
        target.observe(root.path(), 33);
        request.decision = ObjectId::mint();
        let id = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &request, at(34))
            .unwrap();
        assert!(!state(&target.store, &id).0);
        healthy(&target.store);
    }
}

#[test]
fn invalid_provenance_and_outer_transaction_failure_leave_no_partial_eligibility() {
    let root = repository();
    let mut target = Fixture::new(root.path());
    target.observe(root.path(), 5);
    let (origin, enrolled) = enrollment();
    retain(&mut target.store, &pack(&origin, vec![enrolled]));
    let request = intent(&target, ObjectId::mint(), origin);
    let measured = applicability::measure_destination(&target.store.connection, &request).unwrap();
    let local_actor = target.local_actor();
    let before = crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
    let mut tx = target.store.connection.transaction().unwrap();
    assert!(
        applicability::record_applicability_on(
            &mut tx,
            &project(),
            &session(),
            &local_actor,
            &request,
            &measured,
            at(30)
        )
        .is_err()
    );
    tx.commit().unwrap();
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
    );
    assert_eq!(
        target
            .store
            .connection
            .query_row(
                "SELECT count(*) FROM objects WHERE object_kind=?1",
                [APPLICABILITY_KIND],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn unsupported_and_provisional_deliveries_and_reported_passes_never_become_verification() {
    let root = repository();
    for unsupported in [false, true] {
        let mut a = Fixture::new(root.path());
        if unsupported {
            a.request.completion.schema_version += 1;
        } else {
            a.request.completion.result.verdict = RemoteRunVerdict::Unknown;
        }
        let receipt = a.record(10);
        assert_eq!(receipt.verifications, []);
        assert!(receipt.origin.is_none());
        let (origin, enrolled) = enrollment();
        let reported=RecordEnvelope::from_canonical(origin.clone(),ObjectId::mint(),RecordKind::WorkEvidence.as_str(),serde_json_canonicalizer::to_vec(&serde_json::json!({
            "schema_version":SCHEMA_VERSION,"work_id":a.binding.work_id,"run_id":a.binding.run_id,"claim_id":a.binding.claim_id,"claim_fence":a.binding.claim_fence,
            "summary":"reported PASS","refs":[],"actor":a.local_actor(),"created_at":at(10)
        })).unwrap()).unwrap();
        let original = reported.object_id().clone();
        let input = pack(
            &origin,
            vec![
                enrolled,
                reported,
                envelope(&a.store, &origin, &receipt.operation),
            ],
        );
        let mut target = Fixture::new(root.path());
        target.observe(root.path(), 5);
        retain(&mut target.store, &input);
        let key = ExecutionKey::new(project(), &a.request.completion);
        assert_eq!(
            union_on(&target.store.connection, &key)
                .unwrap()
                .contributors,
            []
        );
        let request = intent(&target, original, origin);
        let local_actor = target.local_actor();
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        assert!(
            target
                .store
                .associate_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &request,
                    at(30)
                )
                .is_err()
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
        );
        healthy(&target.store);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real producer fixture compares aggregate and bound/unbound requirement decisions"
)]
fn imported_registered_check_uses_common_aggregate_and_bound_requirement_rules() {
    for failed_aggregate in [false, true] {
        let root = repository();
        let mut a = Fixture::new(root.path());
        let identity = source(root.path());
        let request = RegisteredVerificationRequest {
            operation_key: "termal-run:test-imported-registered".into(),
            registration: RegisteredVerificationAssociation {
                host_run_id: "test-imported-registered".into(),
                request_identity: "registered-request".into(),
                registered_at: at(5),
                initiating_session_id: session(),
                owning_session_id: None,
                binding: a.binding.clone(),
                root: a.request.association.root.clone(),
            },
            input: RegisteredVerificationInput::SourceId {
                source_id: identity.clone(),
            },
            run: RegisteredVerificationRun {
                mode: "focused".into(),
                command: "cargo test --lib".into(),
                started_at: at(5),
                finished_at: at(9),
                outcome: if failed_aggregate {
                    RegisteredVerificationOutcome::Failed
                } else {
                    RegisteredVerificationOutcome::Passed
                },
            },
            checks: vec![RegisteredVerificationCheck {
                stage: "passed-stage".into(),
                check_kind: VerificationKind::Test,
                command: "cargo test --lib".into(),
                check_fingerprint: ObjectId::from_canonical_bytes(b"cargo test --lib"),
                started_at: Some(at(6)),
                completed_at: Some(at(9)),
                outcome: RegisteredVerificationOutcome::Passed,
                summary: None,
                refs: vec![],
                counts: None,
            }],
            post_run: RegisteredVerificationPostRun::SourceId {
                source_id: identity,
            },
        };
        let receipt = a
            .store
            .record_registered_verification(
                &project(),
                &session(),
                &a.token,
                HostSessionAuthority::Work,
                &actor("runner"),
                &request,
                at(10),
            )
            .unwrap();
        let (origin, enrolled) = enrollment();
        let input = with_witnesses(
            &a,
            &origin,
            vec![
                enrolled,
                envelope(&a.store, &origin, &receipt.producer),
                envelope(&a.store, &origin, &receipt.verifications[0].record_id),
            ],
        );
        let mut target = Fixture::new(root.path());
        target.observe(root.path(), 5);
        retain(&mut target.store, &input);
        let mut intent = intent(&target, receipt.verifications[0].record_id.clone(), origin);
        intent.criterion = Some(1);
        let local_actor = target.local_actor();
        let decision = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &intent, at(30))
            .unwrap();
        assert_eq!(state(&target.store, &decision).0, !failed_aggregate);
        intent.decision = ObjectId::mint();
        intent.requirement.check_fingerprint =
            Some(ObjectId::from_canonical_bytes(b"different check"));
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        assert!(
            target
                .store
                .associate_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &intent,
                    at(31)
                )
                .is_err()
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
        );
        // Unbound explicit selection still compares the pinned requirement.
        intent.criterion = None;
        let decision = target
            .store
            .associate_imported_verification(&project(), &session(), &local_actor, &intent, at(32))
            .unwrap();
        assert!(!state(&target.store, &decision).0);
        healthy(&target.store);
    }
}
