use super::*;
use chrono::{Duration, TimeZone};

pub(super) fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, 3, 0, 0)
        .single()
        .expect("fixed timestamp")
        + Duration::seconds(second)
}

pub(super) fn process_default_session_at(pid: u32, created_at: DateTime<Utc>) -> SessionId {
    let seconds = u64::try_from(created_at.timestamp()).expect("positive test timestamp");
    let timestamp = uuid::Timestamp::from_unix(
        uuid::NoContext,
        seconds,
        created_at.timestamp_subsec_nanos(),
    );
    SessionId(format!(
        "{PROCESS_DEFAULT_WORK_SESSION_PREFIX}{pid}-{}",
        uuid::Uuid::new_v7(timestamp)
    ))
}

pub(super) fn obligation_record(
    identity: i64,
    state: WorkObligationState,
    trigger_position: i64,
    resolution_position: Option<i64>,
    rule_padding: usize,
) -> crate::storage::WorkObligationRecord {
    let run_id = WorkRunId(uuid::Uuid::from_u128(10));
    crate::storage::WorkObligationRecord {
        definition_id: ObjectId::from_canonical_bytes(format!("definition-{identity}").as_bytes()),
        obligation: WorkObligation {
            schema_version: SCHEMA_VERSION,
            obligation_id: crate::WorkObligationId(uuid::Uuid::from_u128(
                u128::try_from(identity).expect("positive test identity"),
            )),
            project_id: ProjectId("obligation-page-project".into()),
            root_execution_id: crate::RootExecutionId(uuid::Uuid::from_u128(20)),
            root_id: WorkId(uuid::Uuid::from_u128(30)),
            work_id: WorkId(uuid::Uuid::from_u128(31)),
            run_id,
            work_revision: 1,
            rule_set: ObjectId::from_canonical_bytes(b"obligation-rule-set"),
            rule: crate::BuiltinObligationRuleRef {
                rule_id: format!("rule-{identity}-{}", "x".repeat(rule_padding)),
                rule_version: 1,
            },
            triggering_observation: ObjectId::from_canonical_bytes(
                format!("observation-{identity}").as_bytes(),
            ),
            trigger_position: crate::FeedPosition {
                feed: FeedId::RunExecution(run_id),
                position: trigger_position,
            },
            requirement: crate::VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
            opened_at: at(trigger_position),
        },
        state,
        resolution_id: (state != WorkObligationState::Open)
            .then(|| ObjectId::from_canonical_bytes(format!("resolution-{identity}").as_bytes())),
        resolution: None,
        resolution_position: resolution_position.map(|position| crate::FeedPosition {
            feed: FeedId::RunExecution(run_id),
            position,
        }),
    }
}

pub(super) fn root_input(title: &str, key: &str) -> WorkProposeInput {
    WorkProposeInput::Root {
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        external_ref: None,
        notes: Vec::new(),
        title: title.into(),
        outcome: format!("{title} outcome"),
        acceptance: vec![format!("{title} accepted")],
        work_kind: None,
        priority: None,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        idempotency_key: key.into(),
    }
}

pub(super) fn proposed_root(result: WorkProposeResult) -> WorkItemSummary {
    match result {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) | WorkProposeResult::Plan(_) => panic!("expected root"),
    }
}

pub(super) fn completion_input(summary: &str, key: &str) -> WorkCompleteInput {
    WorkCompleteInput {
        source_fingerprint: None,
        landing: None,
        links: Vec::new(),
        link_basis: None,
        capture: Some(WorkCompletionCaptureInput {
            summary: summary.into(),
            refs: Vec::new(),
        }),
        evidence: Vec::new(),
        acceptance: None,
        note: None,
        idempotency_key: key.into(),
    }
}

pub(super) fn commit_completion_core_without_finishing(
    service: &LocalWorkService,
    input: &WorkCompleteInput,
    now: DateTime<Utc>,
) -> CompletionSeal {
    let mut store = service.store().expect("completion store");
    let basis = service
        .protocol_basis(&store, true, false, None, now)
        .expect("completion basis");
    let intent = service.protocol_intent(input);
    let raw_key = service
        .effective_idempotency_key(
            &input.idempotency_key,
            "work_complete",
            &basis,
            &intent,
            now,
        )
        .expect("completion key");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_complete",
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })
        .expect("pending completion attempt");
    let work = basis.focused_work.clone().expect("focused completion work");
    let claim = service
        .live_protocol_claim(&basis, &work, now)
        .expect("completion claim");
    let actor = service.actor("work_complete", "complete ambient local work");
    let evidence_basis =
        LocalWorkService::completion_evidence_basis(&store, &claim, &input.evidence)
            .expect("completion evidence basis");
    let acceptance = LocalWorkService::prevalidate_completion_acceptance(
        &work,
        input.acceptance.as_deref(),
        input.note.as_deref(),
        &evidence_basis,
        actor.assurance,
        &actor.actor_id,
    )
    .expect("completion acceptance");
    let prepared = service
        .prepare_completion_evidence(
            &mut store,
            CompletionEvidencePlan {
                work: &work,
                claim: &claim,
                capture: input.capture.as_ref(),
                evidence: evidence_basis,
                base_key: &raw_key,
                now,
            },
        )
        .expect("completion substeps");
    let scoped_key = service
        .core_operation_key("work_complete", &prepared.attempt_key, "complete_work")
        .expect("completion core key");
    let evidence = prepared.evidence;
    match store
        .complete_work_for_protocol(
            &CompleteWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                holder: service.session_id.clone(),
                expected_work_revision: work.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                evidence,
                acceptance,
                drain: CompletionDrainAttestation {
                    reconciled_action_outcomes: Vec::new(),
                    released_resource_leases: Vec::new(),
                },
                source_fingerprint: None,
                landing: None,
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("completion core commits")
    {
        CompleteWorkStorageResult::Completed(seal) => *seal,
        CompleteWorkStorageResult::Recovery(_) => {
            panic!("completion fixture must cross every barrier")
        }
    }
}

pub(super) fn completion_run_feed_head(service: &LocalWorkService, work_id: WorkId) -> i64 {
    let store = service.store().expect("completion-basis store");
    let run_id = store
        .latest_work_run(work_id)
        .expect("latest work run")
        .expect("completion-basis run")
        .run_id;
    store
        .work_feed_head(&FeedId::RunExecution(run_id))
        .expect("completion run-feed head")
}

#[derive(Clone, Debug)]
pub(super) struct ScaleSample {
    pub(super) elapsed_us: u128,
    pub(super) canonical_decodes: usize,
    pub(super) work_event_decodes: usize,
    pub(super) item_decodes: usize,
    pub(super) sql: crate::storage::work_cost::sqlite::Metrics,
}

pub(super) fn measure_scale_operation<T>(
    samples: &mut Vec<ScaleSample>,
    operation: impl FnOnce() -> T,
) -> T {
    crate::canonical::reset_canonical_decode_count();
    crate::storage::reset_work_event_decode_count();
    crate::storage::reset_work_item_projection_decode_count();
    let sql = crate::storage::work_cost::sqlite::start();
    let started = std::time::Instant::now();
    let result = operation();
    let elapsed_us = started.elapsed().as_micros();
    let sql = sql.finish();
    assert!(
        sql.total.statements > 0,
        "scale operation executed no observed SQLite statements"
    );
    samples.push(ScaleSample {
        elapsed_us,
        canonical_decodes: crate::canonical::canonical_decode_count(),
        work_event_decodes: crate::storage::work_event_decode_count(),
        item_decodes: crate::storage::work_item_projection_decode_count(),
        sql,
    });
    result
}

pub(super) fn scale_p95<T: Copy + Ord>(values: impl Iterator<Item = T>) -> T {
    let mut values = values.collect::<Vec<_>>();
    assert!(!values.is_empty(), "scale percentile needs samples");
    values.sort_unstable();
    values[(values.len() * 95).div_ceil(100) - 1]
}

pub(super) fn report_scale_samples(operation: &str, samples: &[ScaleSample]) {
    eprintln!(
        "claim mutation scale {operation}: samples={} p95_us={} canonical_decodes_p95={} canonical_decodes_max={} work_event_decodes_p95={} work_event_decodes_max={} item_decodes_p95={} item_decodes_max={}",
        samples.len(),
        scale_p95(samples.iter().map(|sample| sample.elapsed_us)),
        scale_p95(samples.iter().map(|sample| sample.canonical_decodes)),
        samples
            .iter()
            .map(|sample| sample.canonical_decodes)
            .max()
            .expect("scale samples"),
        scale_p95(samples.iter().map(|sample| sample.work_event_decodes)),
        samples
            .iter()
            .map(|sample| sample.work_event_decodes)
            .max()
            .expect("scale samples"),
        scale_p95(samples.iter().map(|sample| sample.item_decodes)),
        samples
            .iter()
            .map(|sample| sample.item_decodes)
            .max()
            .expect("scale samples"),
    );
    eprintln!(
        "claim mutation SQL scale {operation}: {}",
        scale_sql_report(samples)
    );
}

pub(super) fn scale_sql_report(samples: &[ScaleSample]) -> serde_json::Value {
    let summary = |get: fn(&ScaleSample) -> u64| {
        serde_json::json!({
            "count": samples.len(),
            "total": samples.iter().map(get).sum::<u64>(),
            "p95": scale_p95(samples.iter().map(get)),
            "max": samples.iter().map(get).max().expect("scale samples"),
        })
    };
    let mut categories =
        std::collections::BTreeMap::<&str, crate::storage::work_cost::sqlite::Counters>::new();
    for sample in samples {
        for (kind, counters) in &sample.sql.by_kind {
            let total = categories.entry(kind).or_default();
            total.statements += counters.statements;
            total.fullscan_steps += counters.fullscan_steps;
            total.sorts += counters.sorts;
            total.vm_steps += counters.vm_steps;
        }
    }
    serde_json::json!({
        "statements": summary(|sample| sample.sql.total.statements),
        "fullscan_steps": summary(|sample| sample.sql.total.fullscan_steps),
        "sorts": summary(|sample| sample.sql.total.sorts),
        "vm_steps": summary(|sample| sample.sql.total.vm_steps),
        "statement_categories": categories,
    })
}

#[test]
fn scale_sql_report_keeps_operation_totals_percentiles_and_categories() {
    let connection = crate::storage::open_sqlite_file(
        std::path::Path::new(":memory:"),
        rusqlite::OpenFlags::default(),
    )
    .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE scale_control(value INTEGER);
         WITH RECURSIVE fixture(value) AS (
             SELECT 0 UNION ALL SELECT value + 1 FROM fixture WHERE value < 127
         ) INSERT INTO scale_control SELECT value FROM fixture;",
        )
        .unwrap();
    let mut samples = Vec::new();
    for _ in 0..20 {
        measure_scale_operation(&mut samples, || {
            let mut statement = connection
                .prepare_cached("SELECT value FROM scale_control ORDER BY value DESC")
                .unwrap();
            let values = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(values.len(), 128);
        });
    }
    let report = scale_sql_report(&samples);
    assert_eq!(
        report["fullscan_steps"],
        serde_json::json!({
            "count": 20, "total": 2_540, "p95": 127, "max": 127,
        })
    );
    assert_eq!(
        report["sorts"],
        serde_json::json!({
            "count": 20, "total": 20, "p95": 1, "max": 1,
        })
    );
    assert_eq!(report["statements"]["total"], 20);
    assert_eq!(
        report["statement_categories"]["select"]["fullscan_steps"],
        2_540
    );
    assert!(samples.iter().all(|sample| sample.canonical_decodes == 0
        && sample.work_event_decodes == 0
        && sample.item_decodes == 0));
}

pub(super) fn assert_lapsed_completion_refuses_without_mutation(
    service: &LocalWorkService,
    work: &WorkItemSummary,
    input: &WorkCompleteInput,
    now: DateTime<Utc>,
) {
    let store = service.store().expect("store before refusal");
    let claim = store
        .current_work_claim(work.work_id)
        .expect("claim projection")
        .expect("lapsed claim");
    let events = store
        .work_event_tail(work.work_id, 64)
        .expect("events before refusal")
        .len();
    let evidence = store
        .work_run_evidence(claim.run_id)
        .expect("evidence before refusal");
    drop(store);

    assert!(matches!(
        service.work_complete(input.clone(), now),
        Err(StoreError::WorkClaimLapsed { work: refused, .. }) if refused == work.work_id
    ));
    let store = service.store().expect("store after refusal");
    assert_eq!(
        store
            .current_work_claim(work.work_id)
            .expect("claim after refusal"),
        Some(claim.clone())
    );
    assert_eq!(
        store
            .work_event_tail(work.work_id, 64)
            .expect("events after refusal")
            .len(),
        events
    );
    assert_eq!(
        store
            .work_run_evidence(claim.run_id)
            .expect("evidence after refusal"),
        evidence
    );
}
