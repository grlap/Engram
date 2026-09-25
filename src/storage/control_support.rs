use super::{
    ActorContext, AssuranceLevel, BUILTIN_CONTROL_GRANT_TTL_SECONDS,
    CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
    CONTROL_POLICY_STATE_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CanonicalObject, Connection,
    ControlAssurance, ControlEpochs, ControlPolicy, ControlPolicyProjection, ControlSessionStatus,
    ControlWorkBinding, DateTime, DeserializeOwned, EffectClass, HashSet,
    MAX_CONTROL_POLICY_ATTRIBUTION_BYTES, MAX_CONTROL_POLICY_AUTHORITY_BYTES,
    MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES, MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES,
    MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES, MAX_CONTROL_POLICY_PROVENANCE_LINKS, ObjectId,
    OptionalExtension, PendingTurnGrantSupersession, ProjectPolicyAuthorityDecision,
    ProjectPolicyEpoch, ProjectPolicyOperation, RawControlSession, Redactor, Serialize, SessionId,
    SessionPhase, SqliteStore, StoreError, StoredControlSession, StoredTurnGrant,
    TaskAdmissionEpoch, TaskId, Transaction, Utc, params, parse_enum, work,
};

#[cfg(test)]
use super::CONTROL_POLICY_VERSION_LOAD_COUNT;

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Loads the active policy used by one control decision without walking
    /// predecessor objects. The selected version is hash- and byte-verified,
    /// its scalar projection must match, and one aggregate must prove that it
    /// is the unique maximal contiguous head. Open, activation, and doctor use
    /// [`Self::verify_control_policy_history`] to additionally walk the audit
    /// chain; no prior version participates in a live grant decision.
    pub(super) fn load_active_control_policy(
        connection: &Connection,
    ) -> Result<ControlPolicyProjection, StoreError> {
        let (projection, policy, _) = Self::load_control_policy_head(connection)?;
        if projection.state_schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy uses a non-current state schema".into(),
            ));
        }
        Self::validate_control_policy_shape(&policy)?;
        Self::load_obligation_rule_set_on(connection, &policy.obligation_rule_set)?;
        Ok(projection)
    }

    /// The active acceptance-evaluation policy in canonical form; defaults to
    /// self-asserted when the active policy carries none.
    pub(in crate::storage) fn load_acceptance_evaluation_policy_on(
        connection: &Connection,
    ) -> Result<crate::domain::AcceptanceEvaluationPolicy, StoreError> {
        let (_, policy, _) = Self::load_control_policy_head(connection)?;
        Ok(policy.acceptance_evaluation.normalized())
    }

    /// The active acceptance-evaluation policy of this store.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the control policy cannot be read.
    pub fn acceptance_evaluation_policy(
        &self,
    ) -> Result<crate::domain::AcceptanceEvaluationPolicy, StoreError> {
        Self::load_acceptance_evaluation_policy_on(&self.connection)
    }

    pub(super) fn verify_control_policy_history(
        connection: &Connection,
    ) -> Result<ControlPolicyProjection, StoreError> {
        work::on_one_snapshot(connection, |connection| {
            let (projection, active_policy, active_authority) =
                Self::load_control_policy_head(connection)?;
            Self::verify_control_policy_chain(
                connection,
                &projection.policy_id,
                &active_policy,
                active_authority,
            )?;
            Ok(projection)
        })
    }

    /// The state row, the version row and the successor check are compared,
    /// so outside a transaction they are read from one commit.
    pub(super) fn load_control_policy_head(
        connection: &Connection,
    ) -> Result<
        (
            ControlPolicyProjection,
            ControlPolicy,
            ProjectPolicyAuthorityDecision,
        ),
        StoreError,
    > {
        work::on_one_snapshot(connection, |connection| {
            Self::load_control_policy_head_on_snapshot(connection)
        })
    }

    fn load_control_policy_head_on_snapshot(
        connection: &Connection,
    ) -> Result<
        (
            ControlPolicyProjection,
            ControlPolicy,
            ProjectPolicyAuthorityDecision,
        ),
        StoreError,
    > {
        let (schema_version, epoch, required_assurance, supported_effects, grant_ttl, policy_id): (
            i64,
            i64,
            String,
            String,
            i64,
            Option<String>,
        ) = connection
            .query_row(
                "SELECT schema_version, policy_epoch, required_assurance,
                        supported_effects_json, grant_ttl_seconds, policy_id
                 FROM control_policy_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(
                    "control policy singleton is missing from an established store".into(),
                )
            })?;
        if schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION || epoch <= 0 || grant_ttl <= 0 {
            return Err(StoreError::InvalidControlProjection(
                "active control policy has an unknown state schema or invalid bounds".into(),
            ));
        }
        let policy_id = policy_id.ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no selected version".into(),
            )
        })?;
        let active_hash = ObjectId::from_stored(policy_id.clone())
            .ok_or(StoreError::InvalidStoredKey(policy_id))?;
        let (policy, authority) = Self::load_control_policy_version(connection, &active_hash)?;
        Self::validate_control_policy_shape(&policy)?;
        if authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy authority uses an unsupported schema".into(),
            ));
        }
        let projected_effects: Vec<EffectClass> = serde_json::from_str(&supported_effects)?;
        let projected_assurance: ControlAssurance = parse_enum(&required_assurance)?;
        if policy.policy_epoch.0 != epoch
            || policy.required_assurance != projected_assurance
            || policy.supported_effects != projected_effects
            || policy.grant_ttl_seconds != grant_ttl
        {
            return Err(StoreError::InvalidControlProjection(
                "active control policy scalars do not match its canonical version".into(),
            ));
        }
        let successor_exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM control_policy_versions WHERE policy_epoch > ?1
             )",
            [epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if successor_exists {
            return Err(StoreError::InvalidControlProjection(
                "active control policy is not the maximal history head".into(),
            ));
        }
        let projection = ControlPolicyProjection {
            state_schema_version: schema_version,
            policy_id: active_hash,
            authority_id: policy.authority.clone(),
            epoch: policy.policy_epoch,
            required_assurance: policy.required_assurance,
            supported_effects: policy.supported_effects.clone(),
            grant_ttl_seconds: policy.grant_ttl_seconds,
            obligation_rule_set: policy.obligation_rule_set.clone(),
            activated_at: policy.activated_at,
        };
        Ok((projection, policy, authority))
    }

    pub(super) fn load_control_policy_version(
        connection: &Connection,
        policy_id: &ObjectId,
    ) -> Result<(ControlPolicy, ProjectPolicyAuthorityDecision), StoreError> {
        #[cfg(test)]
        CONTROL_POLICY_VERSION_LOAD_COUNT.set(CONTROL_POLICY_VERSION_LOAD_COUNT.get() + 1);
        let (projected_epoch, authority_id, projected_json): (i64, String, Vec<u8>) = connection
            .query_row(
                "SELECT policy_epoch, authority_id, policy_json
                 FROM control_policy_versions WHERE policy_id = ?1",
                [policy_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "control policy version {policy_id} is missing"
                ))
            })?;
        let policy_bytes =
            Self::load_control_object_bytes(connection, policy_id, "control_policy")?;
        if policy_bytes != projected_json {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_id} projection bytes do not match the canonical object"
            )));
        }
        let policy: ControlPolicy = CanonicalObject::stored(policy_id, policy_bytes)?.decode()?;
        let stored_authority = ObjectId::from_stored(authority_id.clone())
            .ok_or(StoreError::InvalidStoredKey(authority_id))?;
        if policy.policy_epoch.0 != projected_epoch || policy.authority != stored_authority {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_id} is not bound to its version row"
            )));
        }
        Self::validate_control_policy_shape(&policy)?;
        let authority_bytes = Self::load_control_object_bytes(
            connection,
            &policy.authority,
            "project_policy_authority_decision",
        )?;
        let authority: ProjectPolicyAuthorityDecision =
            CanonicalObject::stored(&policy.authority, authority_bytes.clone())?.decode()?;
        if authority_bytes.len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_id} authority exceeds its canonical byte limit"
            )));
        }
        if authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION
            || authority.policy_epoch != policy.policy_epoch
            || authority.previous_policy != policy.previous_policy
            || authority.required_assurance != policy.required_assurance
            || authority.obligation_rule_set != policy.obligation_rule_set
            || authority.acceptance_evaluation != policy.acceptance_evaluation
            || authority.decided_at != policy.activated_at
            || authority.authorized_by.assurance != AssuranceLevel::Asserted
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_id} authority is invalid"
            )));
        }
        validate_control_policy_actor_shape(&authority.authorized_by)?;
        Ok((policy, authority))
    }

    pub(super) fn load_control_object_bytes(
        connection: &Connection,
        hash: &ObjectId,
        expected_kind: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let (stored_kind, bytes): (String, Vec<u8>) = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
                [hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "canonical {expected_kind} object {hash} is missing"
                ))
            })?;
        if stored_kind != expected_kind {
            return Err(StoreError::ObjectKindMismatch {
                hash: hash.clone(),
                stored: stored_kind,
                requested: expected_kind.into(),
            });
        }
        CanonicalObject::stored(hash, bytes.clone())?;
        Ok(bytes)
    }

    pub(super) fn validate_control_policy_shape(policy: &ControlPolicy) -> Result<(), StoreError> {
        let unique_effects: HashSet<_> = policy.supported_effects.iter().collect();
        if policy.schema_version != CONTROL_POLICY_SCHEMA_VERSION
            || policy.control_schema_version != CONTROL_SCHEMA_VERSION
            || policy.policy_epoch.0 <= 0
            || policy.grant_ttl_seconds != BUILTIN_CONTROL_GRANT_TTL_SECONDS
            || policy.supported_effects != Self::builtin_control_effects()
            || unique_effects.len() != policy.supported_effects.len()
            || (policy.policy_epoch.0 == 1) != policy.previous_policy.is_none()
        {
            return Err(StoreError::InvalidControlProjection(
                "canonical control policy has an invalid structural shape".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_control_policy_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
        authority: &ProjectPolicyAuthorityDecision,
    ) -> Result<(), StoreError> {
        match authority.operation {
            ProjectPolicyOperation::SetRequiredAssurance => {
                Self::validate_assurance_policy_transition(previous, current)?;
            }
            ProjectPolicyOperation::SetObligationRuleSet => {
                Self::validate_obligation_rule_set_transition(previous, current)?;
            }
            ProjectPolicyOperation::SetAcceptanceEvaluation => {
                Self::validate_acceptance_evaluation_transition(previous, current)?;
            }
        }
        Ok(())
    }

    fn validate_assurance_policy_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let envelope_changed = previous.is_some_and(|previous| {
            current.supported_effects != previous.supported_effects
                || current.grant_ttl_seconds != previous.grant_ttl_seconds
        });
        let invalid_epoch_one = previous.is_none()
            && (current.supported_effects != Self::builtin_control_effects()
                || current.grant_ttl_seconds != BUILTIN_CONTROL_GRANT_TTL_SECONDS);
        let rule_set_changed = previous
            .is_some_and(|previous| current.obligation_rule_set != previous.obligation_rule_set);
        let acceptance_changed = previous.is_some_and(|previous| {
            current.acceptance_evaluation != previous.acceptance_evaluation
        }) || (previous.is_none()
            && !current.acceptance_evaluation.is_self_asserted());
        if envelope_changed || invalid_epoch_one || rule_set_changed || acceptance_changed {
            return Err(StoreError::InvalidControlProjection(
                "a SetRequiredAssurance policy transition changed a preserved policy field".into(),
            ));
        }
        Ok(())
    }

    fn validate_acceptance_evaluation_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let Some(previous) = previous else {
            return Err(StoreError::InvalidControlProjection(
                "an acceptance-evaluation selection cannot create policy epoch one".into(),
            ));
        };
        if current.required_assurance != previous.required_assurance
            || current.supported_effects != previous.supported_effects
            || current.grant_ttl_seconds != previous.grant_ttl_seconds
            || current.obligation_rule_set != previous.obligation_rule_set
            || current.acceptance_evaluation == previous.acceptance_evaluation
            || current.acceptance_evaluation != current.acceptance_evaluation.normalized()
        {
            return Err(StoreError::InvalidControlProjection(
                "an acceptance-evaluation selection must change only the acceptance-evaluation policy".into(),
            ));
        }
        Ok(())
    }

    fn validate_obligation_rule_set_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let Some(previous) = previous else {
            return Err(StoreError::InvalidControlProjection(
                "a rule-set selection cannot create policy epoch one".into(),
            ));
        };
        if current.required_assurance != previous.required_assurance
            || current.supported_effects != previous.supported_effects
            || current.grant_ttl_seconds != previous.grant_ttl_seconds
            || current.obligation_rule_set == previous.obligation_rule_set
            || current.acceptance_evaluation != previous.acceptance_evaluation
        {
            return Err(StoreError::InvalidControlProjection(
                "a rule-set selection must change only the selected obligation rule set".into(),
            ));
        }
        Ok(())
    }

    fn verify_control_policy_chain(
        connection: &Connection,
        active_hash: &ObjectId,
        active_policy: &ControlPolicy,
        active_authority: ProjectPolicyAuthorityDecision,
    ) -> Result<(), StoreError> {
        let mut seen = HashSet::new();
        let mut current_hash = active_hash.clone();
        let mut current_policy = active_policy.clone();
        let mut current_authority = active_authority;
        loop {
            if !seen.insert(current_hash.clone()) {
                return Err(StoreError::InvalidControlProjection(
                    "control policy history contains a cycle".into(),
                ));
            }
            match current_policy.previous_policy.clone() {
                Some(previous_hash) => {
                    let (previous, previous_authority) =
                        Self::load_control_policy_version(connection, &previous_hash)?;
                    if previous.policy_epoch.0.checked_add(1) != Some(current_policy.policy_epoch.0)
                    {
                        return Err(StoreError::InvalidControlProjection(
                            "control policy history has a non-contiguous epoch".into(),
                        ));
                    }
                    Self::validate_control_policy_transition(
                        Some(&previous),
                        &current_policy,
                        &current_authority,
                    )?;
                    current_hash = previous_hash;
                    current_policy = previous;
                    current_authority = previous_authority;
                }
                None if current_policy.policy_epoch.0 == 1 => {
                    Self::validate_control_policy_transition(
                        None,
                        &current_policy,
                        &current_authority,
                    )?;
                    break;
                }
                None => {
                    return Err(StoreError::InvalidControlProjection(
                        "control policy history ends before epoch one".into(),
                    ));
                }
            }
        }
        let expected_versions = usize::try_from(active_policy.policy_epoch.0).map_err(|_| {
            StoreError::InvalidControlProjection("control policy history count overflowed".into())
        })?;
        if seen.len() != expected_versions {
            return Err(StoreError::InvalidControlProjection(
                "control policy history contains unreachable version rows".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn control_count(value: i64, label: &str) -> Result<usize, StoreError> {
        usize::try_from(value)
            .map_err(|_| StoreError::InvalidControlProjection(format!("{label} count overflowed")))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the loader verifies every redundant scalar and canonical work-binding field together"
    )]
    pub(super) fn load_control_session_on(
        connection: &Connection,
        session_id: &SessionId,
    ) -> Result<Option<StoredControlSession>, StoreError> {
        let raw = connection
            .query_row(
                "SELECT project_id, task_id, root_execution_id, work_id, run_id,
                        work_revision, claim_id, claim_fence, routing_token, actor_json,
                        bind_key, bind_intent_hash, bind_intent_json, phase, assurance,
                        mediated_effects_json, confirmed_cursor, tentative_cursor,
                        project_policy_epoch, task_admission_epoch, blocking_watermark,
                        capability_map_revision, revision,
                        (SELECT grant_id FROM control_turn_grants g
                         WHERE g.session_id = control_sessions.session_id
                           AND g.state IN ('issued', 'begun')
                         ORDER BY g.issued_at_ms DESC LIMIT 1)
                 FROM control_sessions WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| {
                    Ok(RawControlSession {
                        project_id: row.get(0)?,
                        task_id: row.get(1)?,
                        root_execution_id: row.get(2)?,
                        work_id: row.get(3)?,
                        run_id: row.get(4)?,
                        work_revision: row.get(5)?,
                        claim_id: row.get(6)?,
                        claim_fence: row.get(7)?,
                        routing_token: row.get(8)?,
                        actor_json: row.get(9)?,
                        bind_key: row.get(10)?,
                        bind_intent_hash: row.get(11)?,
                        bind_intent_json: row.get(12)?,
                        phase: row.get(13)?,
                        assurance: row.get(14)?,
                        mediated_effects_json: row.get(15)?,
                        confirmed_cursor: row.get(16)?,
                        tentative_cursor: row.get(17)?,
                        project_policy_epoch: row.get(18)?,
                        task_admission_epoch: row.get(19)?,
                        blocking_watermark: row.get(20)?,
                        capability_map_revision: row.get(21)?,
                        revision: row.get(22)?,
                        open_grant_id: row.get(23)?,
                    })
                },
            )
            .optional()?;
        raw.map(|raw| {
            let task_id = uuid::Uuid::parse_str(&raw.task_id)
                .map(TaskId)
                .map_err(|error| StoreError::InvalidControlProjection(error.to_string()))?;
            let actor: ActorContext = serde_json::from_slice(&raw.actor_json)?;
            let mediated_effects: Vec<EffectClass> =
                serde_json::from_str(&raw.mediated_effects_json)?;
            let bind_hash = ObjectId::from_stored(raw.bind_intent_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredKey(raw.bind_intent_hash.clone()))?;
            let bind_value: serde_json::Value =
                CanonicalObject::stored(&bind_hash, raw.bind_intent_json.clone())?.decode()?;
            let work_binding = match (
                raw.root_execution_id,
                raw.work_id,
                raw.run_id,
                raw.work_revision,
                raw.claim_id,
                raw.claim_fence,
            ) {
                (None, None, None, None, None, None) => None,
                (
                    Some(root_execution_id),
                    Some(work_id),
                    Some(run_id),
                    Some(work_revision),
                    Some(claim_id),
                    Some(claim_fence),
                ) => Some(ControlWorkBinding {
                    root_execution_id: crate::domain::RootExecutionId(
                        uuid::Uuid::parse_str(&root_execution_id).map_err(|error| {
                            StoreError::InvalidControlProjection(error.to_string())
                        })?,
                    ),
                    work_id: crate::domain::WorkId(uuid::Uuid::parse_str(&work_id).map_err(
                        |error| StoreError::InvalidControlProjection(error.to_string()),
                    )?),
                    run_id: crate::domain::WorkRunId(uuid::Uuid::parse_str(&run_id).map_err(
                        |error| StoreError::InvalidControlProjection(error.to_string()),
                    )?),
                    work_revision,
                    claim_id: crate::domain::WorkClaimId(
                        uuid::Uuid::parse_str(&claim_id).map_err(|error| {
                            StoreError::InvalidControlProjection(error.to_string())
                        })?,
                    ),
                    claim_fence,
                }),
                _ => {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "control session {:?} has a partial work binding",
                        session_id.0
                    )));
                }
            };
            let canonical_work_binding = bind_value
                .get("work_binding")
                .cloned()
                .map(serde_json::from_value::<ControlWorkBinding>)
                .transpose()?;
            if raw.confirmed_cursor < 0
                || raw.tentative_cursor.is_some_and(|cursor| cursor < 0)
                || raw.project_policy_epoch < 0
                || raw.task_admission_epoch < 0
                || raw.blocking_watermark < 0
                || raw.capability_map_revision < 0
                || raw.revision <= 0
                || work_binding
                    .as_ref()
                    .is_some_and(|binding| binding.work_revision <= 0 || binding.claim_fence <= 0)
                || mediated_effects.is_empty()
                || actor.session_id.as_ref() != Some(session_id)
                || actor.run_id.as_deref()
                    != work_binding
                        .as_ref()
                        .map(|binding| binding.run_id.0.to_string())
                        .as_deref()
                || canonical_work_binding != work_binding
                || bind_value
                    .get("project_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.project_id.as_str())
                || bind_value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(session_id.0.as_str())
                || bind_value
                    .get("idempotency_key")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.bind_key.as_str())
            {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control session {:?} contains invalid bounds or actor binding",
                    session_id.0
                )));
            }
            Ok(StoredControlSession {
                project_id: crate::domain::ProjectId(raw.project_id),
                task_id,
                work_binding,
                session_id: session_id.clone(),
                routing_token: raw.routing_token,
                actor,
                bind_key: raw.bind_key,
                bind_intent_hash: raw.bind_intent_hash,
                phase: parse_enum(&raw.phase)?,
                assurance: parse_enum(&raw.assurance)?,
                mediated_effects,
                epochs: ControlEpochs {
                    project_policy: ProjectPolicyEpoch(raw.project_policy_epoch),
                    task_admission: TaskAdmissionEpoch(raw.task_admission_epoch),
                },
                capability_map_revision: raw.capability_map_revision,
                revision: raw.revision,
                open_grant_id: raw.open_grant_id,
            })
        })
        .transpose()
    }

    pub(super) fn control_session_status_on(
        connection: &Connection,
        session: &StoredControlSession,
    ) -> Result<ControlSessionStatus, StoreError> {
        let open_grant = session
            .open_grant_id
            .as_deref()
            .map(|grant_id| Self::load_turn_grant(connection, &session.session_id, grant_id))
            .transpose()?
            .flatten();
        let open_grant_state = open_grant.as_ref().map(|stored| stored.state);
        Ok(ControlSessionStatus {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            project_id: session.project_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            session_id: session.session_id.clone(),
            phase: session.phase,
            assurance: session.assurance,
            mediated_effects: session.mediated_effects.clone(),
            epochs: session.epochs,
            capability_map_revision: session.capability_map_revision,
            revision: session.revision,
            open_grant_id: session.open_grant_id.clone(),
            open_grant_state,
        })
    }

    pub(super) fn verify_control_connection(
        connection: &Connection,
        session_id: &SessionId,
        connection_token: &str,
    ) -> Result<(), StoreError> {
        let current: Option<String> = connection
            .query_row(
                "SELECT connection_token FROM control_connections WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if connection_token.trim().is_empty() || current.as_deref() != Some(connection_token) {
            return Err(StoreError::ControlConnectionSuperseded(
                session_id.0.clone(),
            ));
        }
        Ok(())
    }

    pub(super) fn verify_control_session(
        session: &StoredControlSession,
        project_id: &crate::domain::ProjectId,
        routing_token: &str,
    ) -> Result<(), StoreError> {
        if &session.project_id != project_id {
            return Err(StoreError::ControlSessionNotBound(
                session.session_id.0.clone(),
            ));
        }
        if session.routing_token != routing_token || routing_token.trim().is_empty() {
            return Err(StoreError::ControlSessionTokenMismatch(
                session.session_id.0.clone(),
            ));
        }
        Ok(())
    }

    pub(super) fn control_work_binding_is_current(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        binding: Option<&ControlWorkBinding>,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let Some(binding) = binding else {
            return Ok(true);
        };
        match work::validate_control_work_binding_on(
            connection, project_id, session_id, binding, now,
        ) {
            Ok(()) => Ok(true),
            Err(
                StoreError::ControlWorkBindingStale { .. }
                | StoreError::WorkClaimMismatch { .. }
                | StoreError::WorkClaimLapsed { .. }
                | StoreError::WorkRevisionConflict { .. }
                | StoreError::WorkNotFound(_)
                | StoreError::InvalidWork(_),
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether session bind would accept this work binding for the session
    /// now. It runs the same validation bind runs, on this connection, so a
    /// caller inside a read snapshot sees the answer bind would give at that
    /// cut.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the stored state cannot be read or is
    /// invalid. A binding bind would refuse is `Ok(false)`, not an error.
    pub(crate) fn control_work_binding_bindable(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        binding: &ControlWorkBinding,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        Self::control_work_binding_is_current(
            &self.connection,
            project_id,
            session_id,
            Some(binding),
            now,
        )
    }

    pub(super) fn session_has_begun_turn(
        connection: &Connection,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM control_turn_grants
                 WHERE session_id = ?1 AND state = 'begun'
             )",
            [session_id.0.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists == 1)
    }

    pub(super) fn session_is_current_participant(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        connection
            .query_row(
                "SELECT EXISTS(
                 SELECT 1 FROM control_sessions s
                 JOIN control_anchors a ON a.task_id = s.task_id
                 WHERE s.task_id = ?1 AND s.project_id = ?2
                   AND a.project_id = ?2 AND s.session_id = ?3
             )",
                params![task_id.0.to_string(), project_id.0, session_id.0],
                |row| row.get(0),
            )
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn control_anchor_exists(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
    ) -> Result<bool, StoreError> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM control_anchors
             WHERE task_id = ?1 AND project_id = ?2)",
                params![task_id.0.to_string(), project_id.0],
                |row| row.get(0),
            )
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn expire_unbegun_turn(
        transaction: &Transaction<'_>,
        session: &StoredControlSession,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let expired = transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued' AND expires_at_ms <= ?2",
            params![session.session_id.0, now.timestamp_millis()],
        )?;
        if expired > 0 && matches!(session.phase, SessionPhase::TurnOpen) {
            transaction.execute(
                "UPDATE control_sessions SET
                     phase = 'ready',
                     revision = revision + 1, updated_at_ms = ?2
                 WHERE session_id = ?1",
                params![session.session_id.0, now.timestamp_millis()],
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Supersedes any issued-but-unbegun grant before evaluating a fresh
    /// request. Begun grants are deliberately untouched: their prompt outcome
    /// remains checkpoint-required and the evaluator will refuse a second
    /// turn as already open.
    pub(super) fn supersede_issued_turn(
        transaction: &Transaction<'_>,
        session: &StoredControlSession,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingTurnGrantSupersession>, StoreError> {
        let issued = {
            let mut statement = transaction.prepare(
                "SELECT grant_id, request_key FROM control_turn_grants
                 WHERE session_id = ?1 AND state = 'issued'
                 ORDER BY issued_at_ms, grant_id",
            )?;
            statement
                .query_map([session.session_id.0.as_str()], |row| {
                    Ok(PendingTurnGrantSupersession {
                        grant_id: row.get(0)?,
                        request_key: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let Some(superseded) = issued.into_iter().next() else {
            return Ok(None);
        };
        let issued_count = transaction.query_row(
            "SELECT COUNT(*) FROM control_turn_grants
             WHERE session_id = ?1 AND state = 'issued'",
            [session.session_id.0.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        if issued_count != 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "control session {:?} has {issued_count} issued turn grants",
                session.session_id
            )));
        }
        let changed = transaction.execute(
            "UPDATE control_turn_grants SET state = 'superseded'
             WHERE grant_id = ?1 AND session_id = ?2 AND state = 'issued'",
            params![superseded.grant_id, session.session_id.0],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "issued turn grant {:?} changed during supersession",
                superseded.grant_id
            )));
        }
        transaction.execute(
            "UPDATE control_sessions SET
                 phase = 'ready',
                 revision = revision + 1, updated_at_ms = ?2
             WHERE session_id = ?1",
            params![session.session_id.0, now.timestamp_millis()],
        )?;
        Ok(Some(superseded))
    }

    pub(super) fn load_turn_grant(
        connection: &Connection,
        session_id: &SessionId,
        grant_id: &str,
    ) -> Result<Option<StoredTurnGrant>, StoreError> {
        let row = connection
            .query_row(
                "SELECT grant_json, state
                 FROM control_turn_grants
                 WHERE grant_id = ?1 AND session_id = ?2",
                params![grant_id, session_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(bytes, state)| {
            let grant = Self::decode_json_projection(&bytes)?;
            Ok(StoredTurnGrant {
                grant,
                state: parse_enum(&state)?,
            })
        })
        .transpose()
    }

    pub(super) fn decode_canonical_projection<T: DeserializeOwned>(
        stored_hash: &str,
        bytes: Vec<u8>,
    ) -> Result<T, StoreError> {
        let hash = ObjectId::from_stored(stored_hash.to_owned())
            .ok_or_else(|| StoreError::InvalidStoredKey(stored_hash.to_owned()))?;
        CanonicalObject::stored(&hash, bytes)?.decode()
    }

    pub(super) fn decode_json_projection<T: DeserializeOwned>(
        bytes: &[u8],
    ) -> Result<T, StoreError> {
        CanonicalObject::decode_bytes(bytes)
    }

    pub(super) fn replay_control_operation<T: DeserializeOwned>(
        connection: &Connection,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        intent_hash: &ObjectId,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT intent_hash, result_json
                 FROM control_operation_results
                 WHERE session_id = ?1 AND operation = ?2 AND idempotency_key = ?3",
                params![session_id.0, operation, idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((stored_intent, result_json)) = stored else {
            return Ok(None);
        };
        if stored_intent != intent_hash.as_str() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: idempotency_key.into(),
            });
        }
        Self::decode_json_projection(&result_json).map(Some)
    }

    pub(super) fn replay_control_policy_operation<T: DeserializeOwned>(
        connection: &Connection,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT sequence, intent_hash, intent_json, result_json
                 FROM control_policy_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                params![operation, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((sequence, stored_intent_hash, stored_intent_json, result_json)) = stored else {
            return Ok(None);
        };
        let stored_intent = ObjectId::from_stored(stored_intent_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredKey(stored_intent_hash))?;
        CanonicalObject::stored(&stored_intent, stored_intent_json.clone())?;
        if stored_intent != *intent.key() || stored_intent_json != intent.bytes() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: idempotency_key.into(),
            });
        }
        if result_json.len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation result {sequence} exceeds its canonical byte limit"
            )));
        }
        Self::decode_json_projection(&result_json).map(Some)
    }

    pub(super) fn persist_control_policy_operation<T: Serialize>(
        transaction: &Transaction<'_>,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
        result: &T,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }
        let result_json = crate::canonical::canonical_bytes(result)?;
        if result_json.len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation result exceeds the {MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES}-byte canonical limit"
            )));
        }
        transaction.execute(
            "INSERT INTO control_policy_operation_results (
                 operation, idempotency_key, intent_hash, intent_json,
                 result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                operation,
                idempotency_key,
                intent.key().as_str(),
                intent.bytes(),
                result_json,
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "operation idempotency rows bind every independent key component"
    )]
    pub(super) fn persist_control_operation<T: Serialize>(
        transaction: &Transaction<'_>,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
        result: &T,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if idempotency_key.trim().is_empty() {
            return Err(StoreError::InvalidControlSession(
                "control operation idempotency key is empty".into(),
            ));
        }
        let result_json = crate::canonical::canonical_bytes(result)?;
        transaction.execute(
            "INSERT INTO control_operation_results (
                 session_id, operation, idempotency_key, intent_hash, intent_json,
                 result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.0,
                operation,
                idempotency_key,
                intent.key().as_str(),
                intent.bytes(),
                result_json,
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub(super) fn ensure_active_task_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        if !Self::session_is_current_participant(connection, project_id, task_id, session_id)? {
            return Err(StoreError::TaskAccessDenied {
                task: task_id,
                session: session_id.0.clone(),
            });
        }
        Ok(())
    }
}

pub(super) fn normalize_control_text(value: &str, label: &str) -> Result<String, StoreError> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > 4_096 {
        return Err(StoreError::InvalidControlProjection(format!(
            "{label} must contain from 1 through 4096 bytes"
        )));
    }
    Ok(normalized.to_owned())
}

pub(super) fn normalize_control_policy_idempotency_key(value: &str) -> Result<&str, StoreError> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy idempotency key must contain from 1 through {MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES} bytes"
        )));
    }
    Ok(normalized)
}

fn normalize_optional_control_text(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, StoreError> {
    value
        .map(|value| normalize_control_text(value, label))
        .transpose()
}

fn normalized_control_policy_actor(actor: &ActorContext) -> Result<ActorContext, StoreError> {
    if actor.provenance_chain.len() > MAX_CONTROL_POLICY_PROVENANCE_LINKS {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy administrator provenance must contain at most {MAX_CONTROL_POLICY_PROVENANCE_LINKS} links"
        )));
    }
    let mut normalized = actor.clone();
    normalized.actor_id =
        normalize_control_text(&normalized.actor_id, "control policy administrator actor")?;
    normalized.actor_kind =
        normalize_control_text(&normalized.actor_kind, "control policy administrator kind")?;
    normalized.reason = normalize_control_text(
        &normalized.reason,
        "control policy administrator attribution",
    )?;
    normalized.run_id = normalize_optional_control_text(
        normalized.run_id.as_deref(),
        "control policy administrator run",
    )?;
    normalized.session_id = normalized
        .session_id
        .as_ref()
        .map(|session| {
            normalize_control_text(&session.0, "control policy administrator session")
                .map(SessionId)
        })
        .transpose()?;
    normalized.source_tool = normalize_optional_control_text(
        normalized.source_tool.as_deref(),
        "control policy administrator source tool",
    )?;
    normalized.source_skill = normalize_optional_control_text(
        normalized.source_skill.as_deref(),
        "control policy administrator source skill",
    )?;
    for (index, link) in normalized.provenance_chain.iter_mut().enumerate() {
        link.source = normalize_control_text(
            &link.source,
            &format!("control policy administrator provenance source {index}"),
        )?;
        link.reference = normalize_optional_control_text(
            link.reference.as_deref(),
            &format!("control policy administrator provenance reference {index}"),
        )?;
    }

    let canonical_bytes = crate::canonical::canonical_bytes(&normalized)?;
    if canonical_bytes.len() > MAX_CONTROL_POLICY_ATTRIBUTION_BYTES {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy administrator attribution exceeds the {MAX_CONTROL_POLICY_ATTRIBUTION_BYTES}-byte canonical limit"
        )));
    }
    Ok(normalized)
}

pub(super) fn normalize_control_policy_actor<R: Redactor>(
    actor: &ActorContext,
    redactor: &R,
) -> Result<ActorContext, StoreError> {
    crate::storage::admit_live_actor_session(actor)?;
    let normalized = normalized_control_policy_actor(actor)?;
    for prose in [
        Some(normalized.actor_id.as_str()),
        Some(normalized.actor_kind.as_str()),
        Some(normalized.reason.as_str()),
        normalized.run_id.as_deref(),
        normalized
            .session_id
            .as_ref()
            .map(|session| session.0.as_str()),
        normalized.source_tool.as_deref(),
        normalized.source_skill.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        redactor
            .inspect(prose)
            .map_err(StoreError::RedactionRefused)?;
    }
    for link in &normalized.provenance_chain {
        redactor
            .inspect(&link.source)
            .map_err(StoreError::RedactionRefused)?;
        if let Some(reference) = link.reference.as_deref() {
            redactor
                .inspect(reference)
                .map_err(StoreError::RedactionRefused)?;
        }
    }
    Ok(normalized)
}

fn validate_control_policy_actor_shape(actor: &ActorContext) -> Result<(), StoreError> {
    if normalized_control_policy_actor(actor)? != *actor {
        return Err(StoreError::InvalidControlProjection(
            "control policy administrator attribution is not normalized".into(),
        ));
    }
    Ok(())
}
