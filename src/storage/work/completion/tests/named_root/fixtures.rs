use super::*;

pub(super) fn basis(
    workspace: &str,
    revision: &str,
    generation: Option<i64>,
) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        git_tree: None,
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: generation,
        source_root_state: generation.map(|_| SourceRootState::Named),
    }
}

pub(super) fn named_work() -> (SqliteStore, WorkItem, WorkClaim, ObjectId) {
    named_work_in(SqliteStore::open_in_memory().expect("store"))
}

/// A bound work item claimed by `runner` in `store`, with a source change in
/// workspace A and then the host naming root B at generation 9.
pub(super) fn named_work_in(
    mut store: SqliteStore,
) -> (SqliteStore, WorkItem, WorkClaim, ObjectId) {
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["run a test".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    let earlier = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-before-name",
        3,
        Some(basis("workspace-A", "A3", None)),
        None,
    );
    let host = open_work_host(&mut store, "runner", at(4));
    let mut host_actor = actor("runner");
    store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            9,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut host_actor,
            "name-B",
            at(4),
        )
        .expect("host names B");
    (store, work, claim, earlier)
}

/// The obligation `pick` selects: the bound criterion's, or the stock one a
/// named change triggered.
pub(super) enum Pick<'a> {
    Criterion,
    TriggeredBy(&'a ObjectId),
}

/// The state of the one obligation `pick` selects, with the evidence that
/// satisfied it when it is satisfied.
pub(super) fn obligation(
    store: &SqliteStore,
    claim: &WorkClaim,
    pick: &Pick<'_>,
) -> (WorkObligationState, Option<ObjectId>) {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    let matching = records
        .iter()
        .filter(|record| {
            let criterion =
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some();
            match pick {
                Pick::Criterion => criterion,
                Pick::TriggeredBy(change) => {
                    !criterion && &record.obligation.triggering_observation == *change
                }
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "exactly one obligation is selected");
    let satisfying = match matching[0]
        .resolution
        .as_ref()
        .map(|event| &event.resolution)
    {
        Some(WorkObligationResolution::Satisfied { evidence, .. }) => Some(evidence.clone()),
        _ => None,
    };
    (matching[0].state, satisfying)
}

pub(super) fn stock_completion_action(
    store: &SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
) -> WorkObligationCompletionAction {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    let stock = records
        .iter()
        .find(|record| {
            record.state == WorkObligationState::Open
                && &record.obligation.triggering_observation == change
                && crate::control::is_stock_source_change_obligation(
                    &record.obligation.rule,
                    &record.obligation.requirement,
                )
        })
        .expect("open stock source-change obligation");
    store
        .work_obligation_completion_actions(&[&stock.obligation])
        .expect("completion guidance")[0]
}

/// A write transaction standing for one host checkpoint.
pub(super) fn begin(store: &mut SqliteStore) -> rusqlite::Transaction<'_> {
    store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("checkpoint transaction")
}

/// The generation of the claim's active named root, if it has one.
pub(super) fn active_generation(store: &SqliteStore, claim: &WorkClaim) -> Option<i64> {
    super::super::super::named_root_context_on(
        &store.connection,
        claim.run_id,
        claim.claim_id,
        i64::MAX,
    )
    .expect("named-root lookup")
    .map(|root| root.binding.generation)
}

pub(super) fn name_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    generation: i64,
    second: i64,
) {
    name_root_in(store, work, claim, "workspace-B", generation, second);
}

pub(super) fn name_root_in(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    workspace: &str,
    generation: i64,
    second: i64,
) {
    let host = open_work_host(store, &claim.holder.0, at(second));
    store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            workspace,
            generation,
            at(second),
            NamedRootBindingKind::Bound,
            None,
            &mut actor(&claim.holder.0),
            &format!("name-B-{generation}"),
            at(second),
        )
        .expect("host names B");
}

pub(super) fn release(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    key: &str,
    second: i64,
) {
    let work = store.get_work_item(work.work_id).expect("item");
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "stepping away from the run".into(),
                waiver_reason: Some("nothing to contribute yet".into()),
                actor: actor(&claim.holder.0),
                idempotency_key: key.into(),
                released_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release the claim");
}

/// Appends `resolution` for the open obligation `change` opened, as if the
/// store had recorded it.
pub(super) fn forge_resolution(
    store: &mut SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
    resolution: WorkObligationResolution,
) -> String {
    let transaction = begin(store);
    let record = super::super::super::load_work_obligation_records_on(
        &transaction,
        claim.run_id,
        Some(WorkObligationState::Open),
    )
    .expect("open obligations")
    .into_iter()
    .find(|record| &record.obligation.triggering_observation == change)
    .expect("the change's open obligation");
    let event = WorkObligationResolutionEvent {
        registered_input_decision: None,
        schema_version: SCHEMA_VERSION,
        project_id: record.obligation.project_id.clone(),
        obligation_id: record.obligation.obligation_id,
        definition: record.definition_id.clone(),
        run_id: claim.run_id,
        resolution,
        actor: actor("runner"),
        created_at: at(20),
    };
    super::super::super::append_obligation_resolution_on(&transaction, &record, &event)
        .expect("append the forged resolution");
    transaction.commit().expect("commit the forgery");
    format!("work_obligation:{}", record.obligation.obligation_id.0)
}

/// The host ends the claim's root `workspace` at `generation`, named at
/// `named_second`, as a root that is no longer valid.
pub(super) fn end_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    workspace: &str,
    generation: i64,
    named_second: i64,
    second: i64,
) {
    let host = open_work_host(store, &claim.holder.0, at(second));
    store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            workspace,
            generation,
            at(named_second),
            NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::RootInvalid),
            &mut actor(&claim.holder.0),
            &format!("end-{generation}"),
            at(second),
        )
        .expect("host ends the root");
}

/// How the claim loses its root between a foreign change and a check.
#[derive(Clone, Copy, Debug)]
pub(super) enum Unbind {
    Release,
    End,
}

/// An operator rule: every source change owes a passing test with the pinned
/// check of `suite`.
pub(super) fn pinned_rule(suite: &str) -> crate::domain::ObligationRuleDefinition {
    crate::domain::ObligationRuleDefinition {
        rule: crate::domain::BuiltinObligationRuleRef {
            rule_id: "operator-pinned-suite".into(),
            rule_version: 1,
        },
        trigger: crate::domain::BuiltinObligationTrigger::SourceChanged,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(check_fingerprint(suite)),
        },
    }
}

/// Selects `rules` as `store`'s active rule set.
pub(super) fn select_rules(
    store: &mut SqliteStore,
    rules: Vec<crate::domain::ObligationRuleDefinition>,
) {
    store
        .set_obligation_rule_set(
            &crate::domain::ObligationRuleSet {
                schema_version: crate::domain::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules,
            },
            &actor("obligation-rule-admin"),
            "select-rules",
            None,
            at(1),
            &DevelopmentNoopRedactor,
        )
        .expect("select the rule set");
}

/// A store whose active rule set is the operator rule pinning `suite`
/// alone, without the stock rule.
pub(super) fn pinned_store(suite: &str) -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().expect("store");
    select_rules(&mut store, vec![pinned_rule(suite)]);
    store
}

/// The one obligation the operator rule opened for `change`.
pub(super) fn operator_obligation(
    store: &SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
) -> crate::storage::WorkObligationRecord {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .into_iter()
        .filter(|record| &record.obligation.triggering_observation == change)
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1, "one obligation per change");
    assert!(!crate::control::is_stock_source_change_obligation(
        &records[0].obligation.rule,
        &records[0].obligation.requirement
    ));
    records.into_iter().next().expect("the operator obligation")
}

/// Whether `done` refused on the open obligation `record`.
pub(super) fn refused_on(
    refused: &Result<CompletionSeal, StoreError>,
    record: &crate::storage::WorkObligationRecord,
) -> bool {
    matches!(
        refused,
        Err(StoreError::OpenWorkObligations { obligations, .. })
            if obligations
                .iter()
                .any(|open| open.obligation_id == record.obligation.obligation_id)
    )
}
