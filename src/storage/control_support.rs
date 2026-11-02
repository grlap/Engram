use super::{
    ActorContext, AssuranceLevel, BUILTIN_CONTROL_GRANT_TTL_SECONDS,
    CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
    CONTROL_POLICY_STATE_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CanonicalObject, Connection,
    ControlPolicy, ControlPolicyProjection, ControlWorkBinding, DateTime, DeserializeOwned,
    EffectClass, HashSet, MAX_CONTROL_POLICY_ATTRIBUTION_BYTES, MAX_CONTROL_POLICY_AUTHORITY_BYTES,
    MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES, MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES,
    MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES, MAX_CONTROL_POLICY_PROVENANCE_LINKS, ObjectId,
    OptionalExtension, ProjectPolicyAuthorityDecision, ProjectPolicyOperation, Redactor, Serialize,
    SessionId, SqliteStore, StoreError, Transaction, Utc, params, work,
};

#[cfg(test)]
use super::CONTROL_POLICY_VERSION_LOAD_COUNT;
#[cfg(test)]
use crate::TaskId;

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Loads the active policy used by one control decision without walking
    /// predecessor objects. Stored canonical object and projection bytes must
    /// match, as must scalar projections; an indexed successor probe refuses
    /// a selected version when a higher epoch exists in history. Open, activation,
    /// and doctor use [`Self::verify_control_policy_history`] to walk the
    /// audit chain and check contiguous epochs; no prior version participates
    /// in a live grant decision.
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
        let (schema_version, epoch, policy_id): (i64, i64, Option<String>) = connection
            .query_row(
                "SELECT schema_version, policy_epoch, policy_id FROM control_policy_state WHERE singleton = 1",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(
                    "control policy singleton is missing from an established store".into(),
                )
            })?;
        if schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION || epoch <= 0 {
            return Err(StoreError::InvalidControlProjection(
                "active control policy has an unknown state schema or invalid bounds".into(),
            ));
        }
        let policy_id = policy_id.ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no selected version".into(),
            )
        })?;
        let active_policy_id = ObjectId::from_stored(policy_id.clone())
            .ok_or(StoreError::InvalidStoredKey(policy_id))?;
        let (policy, authority) = Self::load_control_policy_version(connection, &active_policy_id)?;
        Self::validate_control_policy_shape(&policy)?;
        if authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy authority uses an unsupported schema".into(),
            ));
        }
        if policy.policy_epoch.0 != epoch {
            return Err(StoreError::InvalidControlProjection(
                "active project policy epoch differs from its canonical version".into(),
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
            policy_id: active_policy_id,
            authority_id: policy.authority.clone(),
            epoch: policy.policy_epoch,
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
        record_id: &ObjectId,
        expected_kind: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let (stored_kind, bytes): (String, Vec<u8>) = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
                [record_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "canonical {expected_kind} object {record_id} is missing"
                ))
            })?;
        if stored_kind != expected_kind {
            return Err(StoreError::ObjectKindMismatch {
                hash: record_id.clone(),
                stored: stored_kind,
                requested: expected_kind.into(),
            });
        }
        CanonicalObject::stored(record_id, bytes.clone())?;
        Ok(bytes)
    }

    pub(super) fn validate_control_policy_shape(policy: &ControlPolicy) -> Result<(), StoreError> {
        if policy.schema_version != CONTROL_POLICY_SCHEMA_VERSION
            || policy.control_schema_version != CONTROL_SCHEMA_VERSION
            || policy.policy_epoch.0 <= 0
            || (policy.policy_epoch.0 == 1) != policy.previous_policy.is_none()
            || (!retired_admission_absent(policy)
                && (policy.required_assurance.is_none()
                    || policy.supported_effects.is_none()
                    || policy.grant_ttl_seconds.is_none()))
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
            ProjectPolicyOperation::InitializeProjectPolicy => {
                if previous.is_some()
                    || current.required_assurance.is_some()
                    || current.supported_effects.is_some()
                    || current.grant_ttl_seconds.is_some()
                    || !current.acceptance_evaluation.is_self_asserted()
                {
                    return Err(StoreError::InvalidControlProjection(
                        "invalid initial project policy".into(),
                    ));
                }
            }
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
        if retired_admission_absent(current) {
            return Err(StoreError::InvalidControlProjection(
                "a historical assurance policy lacks its recorded admission metadata".into(),
            ));
        }
        let envelope_changed = previous.is_some_and(|previous| {
            current.supported_effects != previous.supported_effects
                || current.grant_ttl_seconds != previous.grant_ttl_seconds
        });
        let invalid_epoch_one = previous.is_none()
            && (current.supported_effects.as_deref()
                != Some(
                    [
                        EffectClass::Observe,
                        EffectClass::Communicate,
                        EffectClass::Coordinate,
                        EffectClass::MutateLocal,
                    ]
                    .as_slice(),
                )
                || current.grant_ttl_seconds != Some(BUILTIN_CONTROL_GRANT_TTL_SECONDS));
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
        if (!retired_admission_absent(current)
            && (current.required_assurance != previous.required_assurance
                || current.supported_effects != previous.supported_effects
                || current.grant_ttl_seconds != previous.grant_ttl_seconds))
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
        if (!retired_admission_absent(current)
            && (current.required_assurance != previous.required_assurance
                || current.supported_effects != previous.supported_effects
                || current.grant_ttl_seconds != previous.grant_ttl_seconds))
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
        active_policy_id: &ObjectId,
        active_policy: &ControlPolicy,
        active_authority: ProjectPolicyAuthorityDecision,
    ) -> Result<(), StoreError> {
        let mut seen = HashSet::new();
        let mut current_policy_id = active_policy_id.clone();
        let mut current_policy = active_policy.clone();
        let mut current_authority = active_authority;
        loop {
            if !seen.insert(current_policy_id.clone()) {
                return Err(StoreError::InvalidControlProjection(
                    "control policy history contains a cycle".into(),
                ));
            }
            match current_policy.previous_policy.clone() {
                Some(previous_policy_id) => {
                    let (previous, previous_authority) =
                        Self::load_control_policy_version(connection, &previous_policy_id)?;
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
                    current_policy_id = previous_policy_id;
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

    pub(crate) fn control_work_binding_bindable_in_scope(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        binding: &ControlWorkBinding,
        now: DateTime<Utc>,
        scope: &super::RootReadScope<'_>,
    ) -> Result<bool, StoreError> {
        match work::validate_control_work_binding_in_scope(
            &self.connection,
            project_id,
            session_id,
            binding,
            now,
            Some(scope),
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

    #[cfg(test)]
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

    pub(super) fn decode_canonical_projection<T: DeserializeOwned>(
        stored_record_id: &str,
        bytes: Vec<u8>,
    ) -> Result<T, StoreError> {
        let record_id = ObjectId::from_stored(stored_record_id.to_owned())
            .ok_or_else(|| StoreError::InvalidStoredKey(stored_record_id.to_owned()))?;
        CanonicalObject::stored(&record_id, bytes)?.decode()
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

    #[cfg(test)]
    pub(super) fn ensure_active_task_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        if !Self::control_anchor_exists(connection, project_id, task_id)?
            || !connection.query_row("SELECT EXISTS(SELECT 1 FROM fixture_memory_membership WHERE task_id = ?1 AND session_id = ?2)", params![task_id.0.to_string(), session_id.0], |row| row.get::<_,bool>(0))? {
            return Err(StoreError::InvalidTaskProjection(format!(
                "historical memory fixture task {task_id:?} has no participant {:?}",
                session_id.0
            )));
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

    let attribution_bytes = crate::canonical::canonical_bytes(&normalized)?;
    if attribution_bytes.len() > MAX_CONTROL_POLICY_ATTRIBUTION_BYTES {
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

fn retired_admission_absent(policy: &ControlPolicy) -> bool {
    policy.required_assurance.is_none()
        && policy.supported_effects.is_none()
        && policy.grant_ttl_seconds.is_none()
}
