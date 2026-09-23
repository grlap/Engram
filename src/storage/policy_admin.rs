use super::{
    ActorContext, AssuranceLevel, CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
    CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
    CONTROL_POLICY_STATE_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CanonicalObject, ControlAssurance,
    ControlDiagnostics, ControlPolicy, ControlPolicyOperationFingerprint,
    ControlPolicyUpdateReceipt, DateTime, EffectClass, MAX_CONTROL_POLICY_AUTHORITY_BYTES,
    MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES, ObjectId, ObligationRuleSet,
    ObligationRuleSetUpdateReceipt, ProjectPolicyAuthorityDecision, ProjectPolicyEpoch,
    ProjectPolicyOperation, Redactor, SqliteStore, StoreError, TransactionBehavior, Utc, enum_name,
    normalize_control_policy_actor, normalize_control_policy_idempotency_key, params,
};
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Activates a new immutable project control-policy version.
    ///
    /// Reapplying the active assurance is an idempotent no-op. Callers may
    /// provide the policy id they observed to prevent a concurrent operator
    /// update from being overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when attribution/redaction is invalid, the
    /// expected policy is stale, canonical history is corrupt, or persistence
    /// cannot complete atomically.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the caller key, attribution, CAS guard, clock, and redactor are independent parts of one auditable policy transaction"
    )]
    pub fn set_required_control_assurance<R: Redactor>(
        &mut self,
        required_assurance: ControlAssurance,
        authorized_by: &ActorContext,
        idempotency_key: &str,
        expected_policy: Option<&ObjectId>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<ControlPolicyUpdateReceipt, StoreError> {
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy administration records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let idempotency_key = normalize_control_policy_idempotency_key(idempotency_key)?;
        let intent =
            CanonicalObject::freeze(&ControlPolicyOperationFingerprint::SetRequiredAssurance {
                fingerprint_schema_version: CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
                idempotency_key,
                required_assurance,
                authorized_by: &authorized_by,
                expected_policy,
            })?;
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = Self::replay_control_policy_operation::<ControlPolicyUpdateReceipt>(
            &transaction,
            "set_required_assurance",
            idempotency_key,
            &intent,
        )? {
            transaction.commit()?;
            return Ok(receipt);
        }
        let current = Self::verify_control_policy_history(&transaction)?;
        if let Some(expected) = expected_policy
            && expected != &current.policy_id
        {
            return Err(StoreError::ControlPolicyConflict {
                expected: expected.clone(),
                current: current.policy_id,
            });
        }
        let (active_policy, _) =
            Self::load_control_policy_version(&transaction, &current.policy_id)?;
        if required_assurance == current.required_assurance {
            let receipt = ControlPolicyUpdateReceipt {
                changed: false,
                active_policy: current.policy_id,
                previous_policy: active_policy.previous_policy,
                authority: current.authority_id,
                policy_epoch: current.epoch,
                previous_required_assurance: current.required_assurance,
                required_assurance: current.required_assurance,
                activated_at: current.activated_at,
            };
            Self::persist_control_policy_operation(
                &transaction,
                "set_required_assurance",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            transaction.commit()?;
            return Ok(receipt);
        }

        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
            operation: ProjectPolicyOperation::SetRequiredAssurance,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance,
            obligation_rule_set: current.obligation_rule_set.clone(),
            acceptance_evaluation: active_policy.acceptance_evaluation.clone(),
            authorized_by,
            decided_at: now,
        };
        let authority_object = CanonicalObject::mint(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION,
            control_schema_version: CONTROL_SCHEMA_VERSION,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: current.obligation_rule_set,
            acceptance_evaluation: active_policy.acceptance_evaluation.clone(),
            authority: authority_object.key().clone(),
            activated_at: now,
        };
        Self::validate_control_policy_shape(&policy)?;
        let policy_object = CanonicalObject::mint(&policy)?;
        Self::insert_object(&transaction, "control_policy", &policy_object)?;
        transaction.execute(
            "INSERT INTO control_policy_versions (
                 policy_id, policy_epoch, authority_id, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.key().as_str(),
                policy.policy_epoch.0,
                authority_object.key().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_id = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_id = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.key().as_str(),
                current.epoch.0,
                current.policy_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "active control policy compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::verify_control_policy_history(&transaction)?;
        if activated.policy_id != *policy_object.key() || activated.epoch != policy.policy_epoch {
            return Err(StoreError::InvalidControlProjection(
                "activated control policy failed post-CAS integrity validation".into(),
            ));
        }
        let receipt = ControlPolicyUpdateReceipt {
            changed: true,
            active_policy: policy_object.key().clone(),
            previous_policy: policy.previous_policy,
            authority: authority_object.key().clone(),
            policy_epoch: policy.policy_epoch,
            previous_required_assurance: current.required_assurance,
            required_assurance: policy.required_assurance,
            activated_at: policy.activated_at,
        };
        Self::persist_control_policy_operation(
            &transaction,
            "set_required_assurance",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Selects a canonical obligation rule set through a new immutable project
    /// policy version. This host/operator-only entry point is intentionally not
    /// exposed through the agent MCP or host turn protocol.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the rule set or asserted attribution is
    /// invalid, the expected policy is stale, history is corrupt, or the CAS
    /// activation cannot complete atomically.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the caller key, rule set, attribution, CAS guard, clock, and redactor are independent parts of one auditable policy transaction"
    )]
    pub fn set_obligation_rule_set<R: Redactor>(
        &mut self,
        rule_set: &ObligationRuleSet,
        authorized_by: &ActorContext,
        idempotency_key: &str,
        expected_policy: Option<&ObjectId>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<ObligationRuleSetUpdateReceipt, StoreError> {
        Self::validate_obligation_rule_set(rule_set)?;
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy administration records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let requested = CanonicalObject::freeze(rule_set)?;
        let idempotency_key = normalize_control_policy_idempotency_key(idempotency_key)?;
        let intent =
            CanonicalObject::freeze(&ControlPolicyOperationFingerprint::SetObligationRuleSet {
                fingerprint_schema_version: CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
                idempotency_key,
                obligation_rule_set: requested.key(),
                authorized_by: &authorized_by,
                expected_policy,
            })?;
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) =
            Self::replay_control_policy_operation::<ObligationRuleSetUpdateReceipt>(
                &transaction,
                "set_obligation_rule_set",
                idempotency_key,
                &intent,
            )?
        {
            transaction.commit()?;
            return Ok(receipt);
        }
        let current = Self::verify_control_policy_history(&transaction)?;
        if let Some(expected) = expected_policy
            && expected != &current.policy_id
        {
            return Err(StoreError::ControlPolicyConflict {
                expected: expected.clone(),
                current: current.policy_id,
            });
        }
        let (active_policy, _) =
            Self::load_control_policy_version(&transaction, &current.policy_id)?;
        let current_rule_set = current.obligation_rule_set.clone();
        let current_bytes = Self::load_control_object_bytes(
            &transaction,
            &current_rule_set,
            "obligation_rule_set",
        )?;
        if current_bytes == requested.bytes() {
            let (policy, _) = Self::load_control_policy_version(&transaction, &current.policy_id)?;
            let receipt = ObligationRuleSetUpdateReceipt {
                changed: false,
                active_policy: current.policy_id,
                previous_policy: policy.previous_policy,
                authority: current.authority_id,
                policy_epoch: current.epoch,
                previous_rule_set: Some(current_rule_set.clone()),
                obligation_rule_set: current_rule_set,
                activated_at: current.activated_at,
            };
            Self::persist_control_policy_operation(
                &transaction,
                "set_obligation_rule_set",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            transaction.commit()?;
            return Ok(receipt);
        }

        // Returning to a rule set an earlier policy named reuses that record;
        // equal content is found by comparing the stored bytes.
        let earlier: Option<String> = transaction
            .query_row(
                "SELECT rule_set.object_id
                 FROM control_policy_versions version
                 JOIN objects rule_set
                   ON rule_set.object_id =
                      json_extract(version.policy_json, '$.obligation_rule_set')
                 WHERE rule_set.object_kind = 'obligation_rule_set'
                   AND rule_set.canonical_json = ?1
                 ORDER BY version.policy_epoch LIMIT 1",
                [requested.bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let rule_set_object = match earlier {
            Some(id) => CanonicalObject::identified(
                &ObjectId::from_stored(id.clone()).ok_or(StoreError::InvalidStoredKey(id))?,
                rule_set,
            )?,
            None => CanonicalObject::mint(rule_set)?,
        };
        Self::insert_object(&transaction, "obligation_rule_set", &rule_set_object)?;
        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
            operation: ProjectPolicyOperation::SetObligationRuleSet,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance: current.required_assurance,
            obligation_rule_set: rule_set_object.key().clone(),
            acceptance_evaluation: active_policy.acceptance_evaluation.clone(),
            authorized_by,
            decided_at: now,
        };
        let authority_object = CanonicalObject::mint(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION,
            control_schema_version: CONTROL_SCHEMA_VERSION,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance: current.required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: rule_set_object.key().clone(),
            acceptance_evaluation: active_policy.acceptance_evaluation.clone(),
            authority: authority_object.key().clone(),
            activated_at: now,
        };
        Self::validate_control_policy_shape(&policy)?;
        let policy_object = CanonicalObject::mint(&policy)?;
        Self::insert_object(&transaction, "control_policy", &policy_object)?;
        transaction.execute(
            "INSERT INTO control_policy_versions (
                 policy_id, policy_epoch, authority_id, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.key().as_str(),
                policy.policy_epoch.0,
                authority_object.key().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_id = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_id = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.key().as_str(),
                current.epoch.0,
                current.policy_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "obligation rule-set policy compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::load_active_control_policy(&transaction)?;
        if activated.policy_id != *policy_object.key() || activated.epoch != policy.policy_epoch {
            return Err(StoreError::InvalidControlProjection(
                "activated obligation rule-set policy failed post-CAS integrity validation".into(),
            ));
        }
        Self::verify_control_policy_history(&transaction)?;
        let receipt = ObligationRuleSetUpdateReceipt {
            changed: true,
            active_policy: policy_object.key().clone(),
            previous_policy: policy.previous_policy,
            authority: authority_object.key().clone(),
            policy_epoch: policy.policy_epoch,
            previous_rule_set: Some(current_rule_set),
            obligation_rule_set: rule_set_object.key().clone(),
            activated_at: policy.activated_at,
        };
        Self::persist_control_policy_operation(
            &transaction,
            "set_obligation_rule_set",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Activates one acceptance-evaluation policy under a new epoch, keeping
    /// the required assurance and obligation rule set unchanged. An empty
    /// allowed-mode set restores the self-asserted completion path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the policy shape or asserted attribution
    /// is invalid, the expected policy is stale, history is corrupt, or the
    /// CAS activation cannot complete atomically.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the caller key, policy, attribution, CAS guard, clock, and redactor are independent parts of one auditable policy transaction"
    )]
    pub fn set_acceptance_evaluation_policy<R: Redactor>(
        &mut self,
        acceptance_evaluation: &crate::domain::AcceptanceEvaluationPolicy,
        authorized_by: &ActorContext,
        idempotency_key: &str,
        expected_policy: Option<&ObjectId>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<crate::storage::AcceptanceEvaluationPolicyUpdateReceipt, StoreError> {
        let acceptance_evaluation = acceptance_evaluation.normalized();
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy administration records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let idempotency_key = normalize_control_policy_idempotency_key(idempotency_key)?;
        let intent = CanonicalObject::freeze(
            &ControlPolicyOperationFingerprint::SetAcceptanceEvaluation {
                fingerprint_schema_version: CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
                idempotency_key,
                acceptance_evaluation: &acceptance_evaluation,
                authorized_by: &authorized_by,
                expected_policy,
            },
        )?;
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = Self::replay_control_policy_operation::<
            crate::storage::AcceptanceEvaluationPolicyUpdateReceipt,
        >(
            &transaction,
            "set_acceptance_evaluation",
            idempotency_key,
            &intent,
        )? {
            transaction.commit()?;
            return Ok(receipt);
        }
        let current = Self::verify_control_policy_history(&transaction)?;
        if let Some(expected) = expected_policy
            && expected != &current.policy_id
        {
            return Err(StoreError::ControlPolicyConflict {
                expected: expected.clone(),
                current: current.policy_id,
            });
        }
        let (active_policy, _) =
            Self::load_control_policy_version(&transaction, &current.policy_id)?;
        let previous_acceptance_evaluation = active_policy.acceptance_evaluation.normalized();
        if previous_acceptance_evaluation == acceptance_evaluation {
            let receipt = crate::storage::AcceptanceEvaluationPolicyUpdateReceipt {
                changed: false,
                active_policy: current.policy_id,
                previous_policy: active_policy.previous_policy,
                authority: current.authority_id,
                policy_epoch: current.epoch,
                previous_acceptance_evaluation,
                acceptance_evaluation,
                activated_at: current.activated_at,
            };
            Self::persist_control_policy_operation(
                &transaction,
                "set_acceptance_evaluation",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            transaction.commit()?;
            return Ok(receipt);
        }

        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
            operation: ProjectPolicyOperation::SetAcceptanceEvaluation,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance: current.required_assurance,
            obligation_rule_set: current.obligation_rule_set.clone(),
            acceptance_evaluation: acceptance_evaluation.clone(),
            authorized_by,
            decided_at: now,
        };
        let authority_object = CanonicalObject::mint(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION,
            control_schema_version: CONTROL_SCHEMA_VERSION,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_id.clone()),
            required_assurance: current.required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: current.obligation_rule_set.clone(),
            acceptance_evaluation: acceptance_evaluation.clone(),
            authority: authority_object.key().clone(),
            activated_at: now,
        };
        Self::validate_control_policy_shape(&policy)?;
        let policy_object = CanonicalObject::mint(&policy)?;
        Self::insert_object(&transaction, "control_policy", &policy_object)?;
        transaction.execute(
            "INSERT INTO control_policy_versions (
                 policy_id, policy_epoch, authority_id, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.key().as_str(),
                policy.policy_epoch.0,
                authority_object.key().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_id = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_id = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.key().as_str(),
                current.epoch.0,
                current.policy_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "acceptance evaluation policy compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::load_active_control_policy(&transaction)?;
        if activated.policy_id != *policy_object.key() || activated.epoch != policy.policy_epoch {
            return Err(StoreError::InvalidControlProjection(
                "activated acceptance evaluation policy failed post-CAS integrity validation"
                    .into(),
            ));
        }
        Self::verify_control_policy_history(&transaction)?;
        let receipt = crate::storage::AcceptanceEvaluationPolicyUpdateReceipt {
            changed: true,
            active_policy: policy_object.key().clone(),
            previous_policy: policy.previous_policy,
            authority: authority_object.key().clone(),
            policy_epoch: policy.policy_epoch,
            previous_acceptance_evaluation,
            acceptance_evaluation,
            activated_at: policy.activated_at,
        };
        Self::persist_control_policy_operation(
            &transaction,
            "set_acceptance_evaluation",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Summarizes the built-in control policy and live operational envelope.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when policy or control projections are invalid.
    pub fn control_diagnostics(&self) -> Result<ControlDiagnostics, StoreError> {
        self.control_diagnostics_at(Utc::now())
    }

    /// Summarizes the control envelope at an injected instant.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when policy or control projections are invalid.
    pub fn control_diagnostics_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<ControlDiagnostics, StoreError> {
        let policy = if self.connection.is_autocommit() {
            let snapshot = self.connection.unchecked_transaction()?;
            let policy = Self::verify_control_policy_history(&snapshot)?;
            snapshot.commit()?;
            policy
        } else {
            Self::verify_control_policy_history(&self.connection)?
        };
        if policy.state_schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy uses a non-current state schema".into(),
            ));
        }
        let obligation_rule_set = policy.obligation_rule_set.clone();
        Self::load_obligation_rule_set_on(&self.connection, &obligation_rule_set)?;
        let acceptance_evaluation =
            Self::load_acceptance_evaluation_policy_on(&self.connection)?.normalized();
        let active_sessions = self.connection.query_row(
            "SELECT COUNT(*) FROM control_sessions WHERE phase != 'exited'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let issued_turns = self.connection.query_row(
            "SELECT COUNT(*) FROM control_turn_grants
             WHERE state = 'issued'
               AND expires_at_ms > ?1",
            [now.timestamp_millis()],
            |row| row.get::<_, i64>(0),
        )?;
        let begun_turns = self.connection.query_row(
            "SELECT COUNT(*) FROM control_turn_grants WHERE state = 'begun'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let all_effects = [
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ];
        let unenforced_effects = all_effects
            .into_iter()
            .filter(|effect| !policy.supported_effects.contains(effect))
            .collect();
        Ok(ControlDiagnostics {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            active_policy: policy.policy_id,
            policy_epoch: policy.epoch,
            required_assurance: policy.required_assurance,
            obligation_rule_set,
            acceptance_evaluation,
            supported_effects: policy.supported_effects,
            unenforced_effects,
            active_sessions: Self::control_count(active_sessions, "active session")?,
            issued_turns: Self::control_count(issued_turns, "issued turn")?,
            begun_turns: Self::control_count(begun_turns, "begun turn")?,
            // These are explicit alpha capability disclosures, not probes.
            action_gating_available: false,
            authority_mediation_available: false,
            action_outcome_tracking_available: false,
        })
    }
}
