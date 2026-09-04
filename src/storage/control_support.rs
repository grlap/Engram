use super::{
    ActorContext, AssuranceLevel, BUILTIN_CONTROL_GRANT_TTL_SECONDS,
    CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
    CONTROL_POLICY_STATE_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CanonicalObject, ChangeCursor,
    Connection, ControlAssurance, ControlEpochs, ControlPolicy, ControlPolicyProjection,
    ControlSessionStatus, ControlWorkBinding, DateTime, DeserializeOwned, EffectClass, HashSet,
    MAX_CONTROL_POLICY_ATTRIBUTION_BYTES, MAX_CONTROL_POLICY_AUTHORITY_BYTES,
    MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES, MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES,
    MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES, MAX_CONTROL_POLICY_PROVENANCE_LINKS, ObjectHash,
    OptionalExtension, PendingTurnGrantSupersession, ProjectPolicyAuthorityDecision,
    ProjectPolicyEpoch, ProjectPolicyOperation, RawControlSession, Redactor, Serialize, SessionId,
    SessionPhase, SqliteStore, StoreError, StoredControlSession, StoredTurnGrant,
    StoredWorkLeaseRow, TaskAdmissionEpoch, TaskId, TaskState, Transaction, TurnGrantState, Utc,
    WorkLease, WorkLeaseDecision, enum_name, params, parse_enum,
    safely_redeliverable_partial_recovery, work,
};

#[cfg(test)]
use super::CONTROL_POLICY_VERSION_LOAD_COUNT;

#[cfg(test)]
mod tests;

impl SqliteStore {
    pub(super) fn latest_task_cursor(
        transaction: &Transaction<'_>,
        task_id: TaskId,
    ) -> Result<ChangeCursor, StoreError> {
        let cursor = transaction.query_row(
            "SELECT COALESCE(MAX(task_cursor), 0) FROM task_changes WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }

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

    pub(super) fn verify_control_policy_history(
        connection: &Connection,
    ) -> Result<ControlPolicyProjection, StoreError> {
        let (projection, active_policy, active_authority) =
            Self::load_control_policy_head(connection)?;
        Self::verify_control_policy_chain(
            connection,
            &projection.policy_hash,
            &active_policy,
            active_authority,
        )?;
        Ok(projection)
    }

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
        let (
            schema_version,
            epoch,
            required_assurance,
            supported_effects,
            grant_ttl,
            policy_hash,
        ): (i64, i64, String, String, i64, Option<String>) = connection
            .query_row(
                "SELECT schema_version, policy_epoch, required_assurance,
                        supported_effects_json, grant_ttl_seconds, policy_hash
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
        let policy_hash = policy_hash.ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no selected version".into(),
            )
        })?;
        let active_hash = ObjectHash::from_stored(policy_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(policy_hash))?;
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
            policy_hash: active_hash,
            authority_hash: policy.authority.clone(),
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
        policy_hash: &ObjectHash,
    ) -> Result<(ControlPolicy, ProjectPolicyAuthorityDecision), StoreError> {
        #[cfg(test)]
        CONTROL_POLICY_VERSION_LOAD_COUNT.set(CONTROL_POLICY_VERSION_LOAD_COUNT.get() + 1);
        let (projected_epoch, authority_hash, projected_json): (i64, String, Vec<u8>) = connection
            .query_row(
                "SELECT policy_epoch, authority_hash, policy_json
                 FROM control_policy_versions WHERE policy_hash = ?1",
                [policy_hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "control policy version {policy_hash} is missing"
                ))
            })?;
        let policy_bytes =
            Self::load_control_object_bytes(connection, policy_hash, "control_policy")?;
        if policy_bytes != projected_json {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} projection bytes do not match the canonical object"
            )));
        }
        let policy: ControlPolicy = CanonicalObject::verify(policy_hash, policy_bytes)?.decode()?;
        let stored_authority = ObjectHash::from_stored(authority_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(authority_hash))?;
        if policy.policy_epoch.0 != projected_epoch || policy.authority != stored_authority {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} is not bound to its version row"
            )));
        }
        Self::validate_control_policy_shape(&policy)?;
        let authority_bytes = Self::load_control_object_bytes(
            connection,
            &policy.authority,
            "project_policy_authority_decision",
        )?;
        let authority: ProjectPolicyAuthorityDecision =
            CanonicalObject::verify(&policy.authority, authority_bytes.clone())?.decode()?;
        if authority_bytes.len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} authority exceeds its canonical byte limit"
            )));
        }
        if authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION
            || authority.policy_epoch != policy.policy_epoch
            || authority.previous_policy != policy.previous_policy
            || authority.required_assurance != policy.required_assurance
            || authority.obligation_rule_set != policy.obligation_rule_set
            || authority.decided_at != policy.activated_at
            || authority.authorized_by.assurance != AssuranceLevel::Asserted
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} authority is invalid"
            )));
        }
        validate_control_policy_actor_shape(&authority.authorized_by)?;
        if normalize_control_text(&authority.reason, "control policy authority reason")?
            != authority.reason
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} authority reason is not normalized"
            )));
        }
        Ok((policy, authority))
    }

    pub(super) fn load_control_object_bytes(
        connection: &Connection,
        hash: &ObjectHash,
        expected_kind: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let (stored_kind, bytes): (String, Vec<u8>) = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
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
        CanonicalObject::verify(hash, bytes.clone())?;
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
        if envelope_changed || invalid_epoch_one || rule_set_changed {
            return Err(StoreError::InvalidControlProjection(
                "a SetRequiredAssurance policy transition changed a preserved policy field".into(),
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
        {
            return Err(StoreError::InvalidControlProjection(
                "a rule-set selection must change only the selected obligation rule set".into(),
            ));
        }
        Ok(())
    }

    fn verify_control_policy_chain(
        connection: &Connection,
        active_hash: &ObjectHash,
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
            let bind_hash = ObjectHash::from_stored(raw.bind_intent_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(raw.bind_intent_hash.clone()))?;
            let bind_value: serde_json::Value =
                CanonicalObject::verify(&bind_hash, raw.bind_intent_json.clone())?.decode()?;
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
                confirmed_cursor: ChangeCursor(raw.confirmed_cursor),
                tentative_cursor: raw.tentative_cursor.map(ChangeCursor),
                epochs: ControlEpochs {
                    project_policy: ProjectPolicyEpoch(raw.project_policy_epoch),
                    task_admission: TaskAdmissionEpoch(raw.task_admission_epoch),
                },
                blocking_watermark: ChangeCursor(raw.blocking_watermark),
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
        let recoverable_grant = open_grant
            .filter(|stored| {
                matches!(stored.state, TurnGrantState::Begun)
                    && safely_redeliverable_partial_recovery(&stored.grant)
            })
            .map(|stored| Box::new(stored.grant));
        Ok(ControlSessionStatus {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            project_id: session.project_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            session_id: session.session_id.clone(),
            phase: session.phase,
            assurance: session.assurance,
            mediated_effects: session.mediated_effects.clone(),
            confirmed_cursor: session.confirmed_cursor,
            tentative_cursor: session.tentative_cursor,
            epochs: session.epochs,
            blocking_watermark: session.blocking_watermark,
            capability_map_revision: session.capability_map_revision,
            revision: session.revision,
            open_grant_id: session.open_grant_id.clone(),
            open_grant_state,
            recoverable_grant,
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

    pub(super) fn begun_turn_pinning_lease(
        connection: &Connection,
        session_id: &SessionId,
        lease_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let grant_ids = {
            let mut statement = connection.prepare(
                "SELECT grant_id FROM control_turn_grants
                 WHERE session_id = ?1 AND state = 'begun'
                 ORDER BY issued_at_ms, grant_id",
            )?;
            statement
                .query_map([session_id.0.as_str()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if grant_ids.len() > 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "control session {:?} has more than one begun turn",
                session_id.0
            )));
        }
        let Some(grant_id) = grant_ids.into_iter().next() else {
            return Ok(None);
        };
        let grant = Self::load_turn_grant(connection, session_id, &grant_id)?.ok_or_else(|| {
            StoreError::InvalidControlProjection(format!(
                "begun turn {grant_id:?} disappeared while checking lease {lease_id:?}"
            ))
        })?;
        if !matches!(grant.state, TurnGrantState::Begun) {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn {grant_id:?} is not begun while checking lease {lease_id:?}"
            )));
        }
        Ok(grant
            .grant
            .basis
            .leases
            .iter()
            .any(|lease| lease.lease_id == lease_id)
            .then_some(grant_id))
    }

    pub(super) fn session_is_current_participant(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let current = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM tasks t JOIN task_participants p
                   ON p.task_id = t.task_id
                 JOIN session_bindings b
                   ON b.task_id = t.task_id AND b.session_id = p.session_id
                 WHERE t.task_id = ?1 AND t.project_id = ?2
                   AND p.session_id = ?3
             )",
            params![task_id.0.to_string(), project_id.0, session_id.0],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(current == 1)
    }

    pub(super) fn task_state_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
    ) -> Result<TaskState, StoreError> {
        let state = connection.query_row(
            "SELECT state FROM tasks WHERE task_id = ?1 AND project_id = ?2",
            params![task_id.0.to_string(), project_id.0],
            |row| row.get::<_, String>(0),
        )?;
        parse_enum(&state)
    }

    pub(super) fn resource_subjects_overlap(
        left: &crate::domain::ResourceSubject,
        right: &crate::domain::ResourceSubject,
    ) -> bool {
        left.covers(right) || right.covers(left)
    }

    fn work_lease_rows(
        connection: &Connection,
        task_id: TaskId,
    ) -> Result<Vec<StoredWorkLeaseRow>, StoreError> {
        let mut statement = connection.prepare(
            "SELECT lease_id, task_id, holder_session_id, lease_hash,
                    lease_json, state, expires_at_ms
             FROM control_work_leases WHERE task_id = ?1 ORDER BY lease_id",
        )?;
        let rows = statement.query_map([task_id.0.to_string()], |row| {
            Ok(StoredWorkLeaseRow {
                lease_id: row.get(0)?,
                task_id: row.get(1)?,
                holder_session_id: row.get(2)?,
                lease_hash: row.get(3)?,
                lease_json: row.get(4)?,
                state: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn project_work_lease_rows(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
    ) -> Result<Vec<StoredWorkLeaseRow>, StoreError> {
        let mut statement = connection.prepare(
            "SELECT lease.lease_id, lease.task_id, lease.holder_session_id,
                    lease.lease_hash, lease.lease_json, lease.state, lease.expires_at_ms
             FROM control_work_leases lease
             JOIN tasks task ON task.task_id = lease.task_id
             WHERE task.project_id = ?1
             ORDER BY lease.lease_id",
        )?;
        let rows = statement.query_map([&project_id.0], |row| {
            Ok(StoredWorkLeaseRow {
                lease_id: row.get(0)?,
                task_id: row.get(1)?,
                holder_session_id: row.get(2)?,
                lease_hash: row.get(3)?,
                lease_json: row.get(4)?,
                state: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn work_lease_row(
        connection: &Connection,
        lease_id: &str,
    ) -> Result<Option<StoredWorkLeaseRow>, StoreError> {
        connection
            .query_row(
                "SELECT lease_id, task_id, holder_session_id, lease_hash,
                        lease_json, state, expires_at_ms
                 FROM control_work_leases WHERE lease_id = ?1",
                [lease_id],
                |row| {
                    Ok(StoredWorkLeaseRow {
                        lease_id: row.get(0)?,
                        task_id: row.get(1)?,
                        holder_session_id: row.get(2)?,
                        lease_hash: row.get(3)?,
                        lease_json: row.get(4)?,
                        state: row.get(5)?,
                        expires_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn decode_work_lease_row(row: &StoredWorkLeaseRow) -> Result<WorkLease, StoreError> {
        let lease: WorkLease =
            Self::decode_canonical_projection(&row.lease_hash, row.lease_json.clone())?;
        if lease.control_schema_version != CONTROL_SCHEMA_VERSION
            || lease.lease_id != row.lease_id
            || lease.task_id.0.to_string() != row.task_id
            || lease.holder.0 != row.holder_session_id
            || lease.expires_at.timestamp_millis() != row.expires_at_ms
            || lease.fence <= 0
            || lease.revision <= 0
            || !lease.subject.has_valid_shape()
            || !matches!(row.state.as_str(), "active" | "released" | "expired")
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "work lease {:?} is not bound to its row",
                row.lease_id
            )));
        }
        Ok(lease)
    }

    pub(super) fn active_work_lease_bases(
        connection: &Connection,
        task_id: TaskId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<crate::domain::LeaseBasis>, StoreError> {
        Self::work_lease_rows(connection, task_id)?
            .into_iter()
            .filter(|row| row.state == "active")
            .map(|row| Self::decode_work_lease_row(&row))
            .filter_map(|lease| match lease {
                Ok(lease) if lease.holder == *session_id && lease.expires_at > now => {
                    Some(Ok(lease.basis()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
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
                     phase = 'sync_required', tentative_cursor = NULL,
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
        let head = Self::latest_task_cursor(transaction, session.task_id)?;
        let phase = if session.confirmed_cursor < head {
            SessionPhase::SyncRequired
        } else {
            SessionPhase::Ready
        };
        transaction.execute(
            "UPDATE control_sessions SET
                 phase = ?2, tentative_cursor = NULL,
                 revision = revision + 1, updated_at_ms = ?3
             WHERE session_id = ?1",
            params![
                session.session_id.0,
                enum_name(phase)?,
                now.timestamp_millis()
            ],
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
                "SELECT grant_hash, grant_json, state
                 FROM control_turn_grants
                 WHERE grant_id = ?1 AND session_id = ?2",
                params![grant_id, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(hash, bytes, state)| {
            let grant = Self::decode_canonical_projection(&hash, bytes)?;
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
        let hash = ObjectHash::from_stored(stored_hash.to_owned())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.to_owned()))?;
        CanonicalObject::verify(&hash, bytes)?.decode()
    }

    pub(super) fn replay_control_operation<T: DeserializeOwned>(
        connection: &Connection,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        intent_hash: &ObjectHash,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT intent_hash, result_hash, result_json
                 FROM control_operation_results
                 WHERE session_id = ?1 AND operation = ?2 AND idempotency_key = ?3",
                params![session_id.0, operation, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_intent, result_hash, result_json)) = stored else {
            return Ok(None);
        };
        if stored_intent != intent_hash.as_str() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: idempotency_key.into(),
            });
        }
        Self::decode_canonical_projection(&result_hash, result_json).map(Some)
    }

    pub(super) fn replay_control_policy_operation<T: DeserializeOwned>(
        connection: &Connection,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT sequence, intent_hash, intent_json, result_hash, result_json
                 FROM control_policy_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                params![operation, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((sequence, stored_intent_hash, stored_intent_json, result_hash, result_json)) =
            stored
        else {
            return Ok(None);
        };
        let stored_intent = ObjectHash::from_stored(stored_intent_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_intent_hash))?;
        CanonicalObject::verify(&stored_intent, stored_intent_json.clone())?;
        if stored_intent != *intent.hash() || stored_intent_json != intent.bytes() {
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
        Self::decode_canonical_projection(&result_hash, result_json).map(Some)
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
        let result = CanonicalObject::freeze(result)?;
        if result.bytes().len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation result exceeds the {MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES}-byte canonical limit"
            )));
        }
        transaction.execute(
            "INSERT INTO control_policy_operation_results (
                 operation, idempotency_key, intent_hash, intent_json,
                 result_hash, result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                operation,
                idempotency_key,
                intent.hash().as_str(),
                intent.bytes(),
                result.hash().as_str(),
                result.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub(super) fn refuse_work_lease(
        transaction: &Transaction<'_>,
        session_id: &SessionId,
        idempotency_key: &str,
        intent: &CanonicalObject,
        directive: crate::domain::ControlDirective,
        now: DateTime<Utc>,
    ) -> Result<WorkLeaseDecision, StoreError> {
        let decision = WorkLeaseDecision::Refuse { directive };
        Self::persist_control_operation(
            transaction,
            session_id,
            "lease_acquire",
            idempotency_key,
            intent,
            &decision,
            now,
        )?;
        Ok(decision)
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
        let result = CanonicalObject::freeze(result)?;
        transaction.execute(
            "INSERT INTO control_operation_results (
                 session_id, operation, idempotency_key, intent_hash, intent_json,
                 result_hash, result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id.0,
                operation,
                idempotency_key,
                intent.hash().as_str(),
                intent.bytes(),
                result.hash().as_str(),
                result.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    fn ensure_task_participant_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        let participant: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM tasks t JOIN task_participants p
                   ON p.task_id = t.task_id
                 WHERE t.task_id = ?1 AND t.project_id = ?2
                   AND p.session_id = ?3",
                params![task_id.0.to_string(), project_id.0, session_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if participant.is_none() {
            return Err(StoreError::TaskAccessDenied {
                task: task_id,
                session: session_id.0.clone(),
            });
        }
        Ok(())
    }

    pub(super) fn ensure_active_task_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        Self::ensure_task_participant_on(connection, project_id, task_id, session_id)?;
        let bound_task = connection
            .query_row(
                "SELECT task_id FROM session_bindings WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if bound_task.as_deref() != Some(task_id.0.to_string().as_str()) {
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

    let canonical_candidate = CanonicalObject::freeze(&normalized)?;
    if canonical_candidate.bytes().len() > MAX_CONTROL_POLICY_ATTRIBUTION_BYTES {
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
