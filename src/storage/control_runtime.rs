use super::{
    ActorContext, CONTROL_SCHEMA_VERSION, CanonicalObject, Connection, ControlAssurance,
    ControlEpochs, ControlSessionBindFingerprint, ControlSessionBinding, ControlSessionStatus,
    ControlTurnBeginDecision, ControlTurnBeginFingerprint, ControlTurnCheckpointDecision,
    ControlTurnCheckpointFingerprint, ControlTurnDecision, ControlWorkBinding, DateTime,
    DevelopmentNoopRedactor, EffectClass, EnvironmentComponents, EnvironmentEvidence,
    EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservation,
    ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome, HashMap, HashSet,
    IssuedTurnGrant, MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT,
    MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT, MAX_TYPED_EVIDENCE_REF_BYTES,
    MAX_TYPED_EVIDENCE_REFS, MAX_TYPED_EVIDENCE_SUMMARY_BYTES,
    MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT, ObjectId, OptionalExtension, ParticipantMembership,
    Redactor, SCHEMA_VERSION, SessionId, SessionPhase, SqliteStore, StoreError, TaskAdmissionEpoch,
    TaskId, Transaction, TransactionBehavior, TurnBeginDecision, TurnBeginReceipt,
    TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointEvent, TurnCheckpointReceipt,
    TurnCheckpointSnapshot, TurnDecision, TurnEvaluationInput, TurnGrantState,
    TurnGrantSupersession, TurnGrantSupersessionReason, TurnIntent, TurnIntentFingerprint,
    TurnNextIntent, Utc, VerificationEvidence, VerificationEvidenceInput, VerificationKind,
    VerificationResult, effective_mediated_effects, enum_name, params, work,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Resolves the host's project-local rendezvous directly. This scope has
    /// no task lifecycle or participant roster; `control_sessions` owns binding.
    fn bind_control_anchor(
        transaction: &Transaction<'_>,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
    ) -> Result<TaskId, StoreError> {
        transaction.execute(
            "INSERT INTO control_anchors (task_id, project_id, external_ref, title)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_id, external_ref) DO NOTHING",
            params![
                TaskId::new().0.to_string(),
                project_id.0,
                external_ref,
                title
            ],
        )?;
        let id: String = transaction.query_row(
            "SELECT task_id FROM control_anchors WHERE project_id = ?1 AND external_ref = ?2",
            params![project_id.0, external_ref],
            |row| row.get(0),
        )?;
        uuid::Uuid::parse_str(&id)
            .map(TaskId)
            .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))
    }

    /// Resolves the control scope most recently bound by this session.
    /// The retained `TaskId` identifies the scope, not a task lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NoActiveTask`] when the session has no control
    /// binding in this project.
    pub fn bound_task(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<TaskId, StoreError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT task_id FROM control_sessions
                 WHERE session_id = ?1 AND project_id = ?2",
                params![session_id.0, project_id.0],
                |row| row.get(0),
            )
            .optional()?;
        let stored = stored.ok_or_else(|| StoreError::NoActiveTask(session_id.0.clone()))?;
        uuid::Uuid::parse_str(&stored)
            .map(TaskId)
            .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))
    }

    /// Binds a host-private control session to a shared scope and rotates any
    /// prior live, unbegun authority for that runtime session.
    ///
    /// The returned routing token prevents accidental cross-session request
    /// mix-ups. It is asserted host state, not authentication.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the bind is invalid, conflicts with an
    /// earlier request key, or cannot be persisted safely.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn bind_control_session(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
        session_id: &SessionId,
        connection_token: &str,
        actor: &ActorContext,
        assurance: ControlAssurance,
        mediated_effects: &[EffectClass],
        capability_map_revision: i64,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionBinding, StoreError> {
        self.bind_control_session_with_work(
            project_id,
            external_ref,
            title,
            session_id,
            connection_token,
            actor,
            None,
            assurance,
            mediated_effects,
            capability_map_revision,
            idempotency_key,
            now,
        )
    }

    /// Binds a host-private control session to a shared external reference and
    /// an exact live local-work claim basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work/run/root/claim basis is stale,
    /// belongs to another session or project, or the bind cannot be persisted.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the bind validates and rotates one auditable session projection transaction"
    )]
    pub fn bind_control_session_with_work(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
        session_id: &SessionId,
        connection_token: &str,
        actor: &ActorContext,
        work_binding: Option<&ControlWorkBinding>,
        assurance: ControlAssurance,
        mediated_effects: &[EffectClass],
        capability_map_revision: i64,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionBinding, StoreError> {
        crate::storage::admit_session_id(session_id)?;
        if let Some(session) = &actor.session_id {
            crate::storage::admit_session_id(session)?;
        }
        let external_ref = external_ref.trim();
        crate::domain::validate_status_capture_actor(actor).map_err(StoreError::InvalidWork)?;
        let title = title.trim();
        let idempotency_key = idempotency_key.trim();
        let effects_are_unique = mediated_effects
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == mediated_effects.len();
        if external_ref.is_empty()
            || title.is_empty()
            || idempotency_key.is_empty()
            || session_id.0.trim().is_empty()
            || actor.session_id.as_ref() != Some(session_id)
            || actor.run_id.as_deref()
                != work_binding
                    .map(|binding| binding.run_id.0.to_string())
                    .as_deref()
            || capability_map_revision < 0
            || mediated_effects.is_empty()
            || !effects_are_unique
            || matches!(assurance, ControlAssurance::ActionGated)
        {
            return Err(StoreError::InvalidControlSession(
                "bind fields, actor session, mediated effects, or capability revision are invalid"
                    .into(),
            ));
        }
        let bind_intent = CanonicalObject::freeze(&ControlSessionBindFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            project_id,
            external_ref,
            title,
            session_id,
            actor,
            assurance,
            mediated_effects,
            work_binding,
            capability_map_revision,
            idempotency_key,
        })?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let existing = Self::load_control_session_on(&transaction, session_id)?;
        if let Some(existing) = &existing
            && existing.bind_key == idempotency_key
        {
            if existing.bind_intent_hash != bind_intent.key().as_str() {
                return Err(StoreError::ControlSessionBindConflict(
                    idempotency_key.into(),
                ));
            }
            let binding = ControlSessionBinding {
                routing_token: existing.routing_token.clone(),
                effective_mediated_effects: effective_mediated_effects(
                    existing.assurance,
                    &existing.mediated_effects,
                ),
                status: Self::control_session_status_on(&transaction, existing)?,
            };
            transaction.commit()?;
            return Ok(binding);
        }
        if let Some(work_binding) = work_binding {
            work::validate_control_work_binding_on(
                &transaction,
                project_id,
                session_id,
                work_binding,
                now,
            )?;
        }
        if let Some(existing) = &existing
            && matches!(existing.phase, SessionPhase::TurnOpen)
            && Self::session_has_begun_turn(&transaction, session_id)?
        {
            return Err(StoreError::InvalidControlSession(
                "a begun turn must be checkpointed before rebinding".into(),
            ));
        }

        let task_id = Self::bind_control_anchor(&transaction, project_id, external_ref, title)?;
        let policy = Self::load_active_control_policy(&transaction)?;
        let admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM control_anchors WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let previous_revision = existing.as_ref().map_or(0, |session| session.revision);
        let routing_token = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued'",
            [session_id.0.as_str()],
        )?;
        // `confirmed_cursor`, `tentative_cursor` and `blocking_watermark` are
        // retained columns: grants no longer carry a delivery page, so no
        // decision reads them. Only this bind writes them, as zero or null, so
        // a row written earlier keeps its last values until it is rebound.
        // They stay only so the schema is unchanged until the next planned
        // migration drops them.
        transaction.execute(
            "INSERT INTO control_sessions (
                 session_id, project_id, task_id, root_execution_id, work_id,
                 run_id, work_revision, claim_id, claim_fence, routing_token,
                 actor_json, bind_key, bind_intent_hash, bind_intent_json, phase,
                 assurance, mediated_effects_json, confirmed_cursor,
                 tentative_cursor, project_policy_epoch, task_admission_epoch,
                 blocking_watermark, capability_map_revision, revision, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, 'ready', ?15, ?16, 0, NULL, ?17, ?18, 0, ?19,
                 ?20, ?21
             )
             ON CONFLICT(session_id) DO UPDATE SET
                 project_id = excluded.project_id,
                 task_id = excluded.task_id,
                 root_execution_id = excluded.root_execution_id,
                 work_id = excluded.work_id,
                 run_id = excluded.run_id,
                 work_revision = excluded.work_revision,
                 claim_id = excluded.claim_id,
                 claim_fence = excluded.claim_fence,
                 routing_token = excluded.routing_token,
                 actor_json = excluded.actor_json,
                 bind_key = excluded.bind_key,
                 bind_intent_hash = excluded.bind_intent_hash,
                 bind_intent_json = excluded.bind_intent_json,
                 phase = excluded.phase,
                 assurance = excluded.assurance,
                 mediated_effects_json = excluded.mediated_effects_json,
                 confirmed_cursor = excluded.confirmed_cursor,
                 tentative_cursor = excluded.tentative_cursor,
                 project_policy_epoch = excluded.project_policy_epoch,
                 task_admission_epoch = excluded.task_admission_epoch,
                 blocking_watermark = excluded.blocking_watermark,
                 capability_map_revision = excluded.capability_map_revision,
                 revision = excluded.revision,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                session_id.0,
                project_id.0,
                task_id.0.to_string(),
                work_binding.map(|binding| binding.root_execution_id.0.to_string()),
                work_binding.map(|binding| binding.work_id.0.to_string()),
                work_binding.map(|binding| binding.run_id.0.to_string()),
                work_binding.map(|binding| binding.work_revision),
                work_binding.map(|binding| binding.claim_id.0.to_string()),
                work_binding.map(|binding| binding.claim_fence),
                routing_token,
                serde_json::to_vec(actor)?,
                idempotency_key,
                bind_intent.key().as_str(),
                bind_intent.bytes(),
                enum_name(assurance)?,
                serde_json::to_string(mediated_effects)?,
                policy.epoch.0,
                admission_epoch,
                capability_map_revision,
                previous_revision + 1,
                now.timestamp_millis(),
            ],
        )?;
        let stored = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        let binding = ControlSessionBinding {
            routing_token: stored.routing_token.clone(),
            effective_mediated_effects: effective_mediated_effects(
                stored.assurance,
                &stored.mediated_effects,
            ),
            status: Self::control_session_status_on(&transaction, &stored)?,
        };
        transaction.commit()?;
        Ok(binding)
    }

    /// Returns current host-control state after validating the routing token.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an unknown session, wrong project, or token
    /// mismatch.
    pub fn control_status(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionStatus, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let mut stored = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&stored, project_id, routing_token)?;
        if Self::expire_unbegun_turn(&transaction, &stored, now)? {
            stored = Self::load_control_session_on(&transaction, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        }
        let status = Self::control_session_status_on(&transaction, &stored)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Invalidates authority that was issued but never begun when a new
    /// host-control connection takes ownership of the runtime session.
    /// Begun turns remain checkpoint-required so an uncertain prompt outcome
    /// cannot be silently replayed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the restart transition cannot be persisted.
    pub fn resume_control_connection(
        &mut self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<String, StoreError> {
        crate::storage::admit_session_id(session_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let connection_token = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "INSERT INTO control_connections (session_id, connection_token, opened_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 connection_token = excluded.connection_token,
                 opened_at_ms = excluded.opened_at_ms",
            params![session_id.0, connection_token, now.timestamp_millis()],
        )?;
        let Some(session) = Self::load_control_session_on(&transaction, session_id)? else {
            transaction.commit()?;
            return Ok(connection_token);
        };
        let invalidated = transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued'",
            [session_id.0.as_str()],
        )?;
        if invalidated > 0
            && matches!(session.phase, SessionPhase::TurnOpen)
            && !Self::session_has_begun_turn(&transaction, session_id)?
        {
            transaction.execute(
                "UPDATE control_sessions SET
                     phase = 'ready',
                     revision = revision + 1, updated_at_ms = ?2
                 WHERE session_id = ?1",
                params![session_id.0, now.timestamp_millis()],
            )?;
        }
        transaction.commit()?;
        Ok(connection_token)
    }

    /// Evaluates and persists one host-enforced turn request from durable
    /// policy, membership, lifecycle, and work-binding state. The grant
    /// carries no delivery page.
    ///
    /// The built-in alpha policy grants `observe`, `communicate`, and
    /// turn-gated `mutate_local`. Local mutation requires policy permission
    /// and declared host mediation for every requested effect.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, idempotency conflicts,
    /// corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_lines,
        reason = "evaluation snapshots context and persists the decision and grant atomically"
    )]
    pub fn evaluate_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        intent: &TurnIntent,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnDecision, StoreError> {
        let mut intent = intent.clone();
        intent.resource_intents = intent
            .resource_intents
            .iter()
            .map(|resource| {
                self.path_policy_for(resource)
                    .map(|policy| resource.normalized_for_project_with_policy(project_id, policy))
            })
            .collect::<Result<Option<Vec<_>>, StoreError>>()?
            .ok_or_else(|| {
                StoreError::InvalidControlSession(
                    "turn resource intent is invalid or belongs to another project".into(),
                )
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let mut session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        let intent_object = CanonicalObject::freeze(&TurnIntentFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            task_id: Some(session.task_id),
            intent: &intent,
        })?;
        if let Some((stored_intent_hash, decision_hash, decision_json)) = transaction
            .query_row(
                "SELECT intent_hash, decision_hash, decision_json
                 FROM control_turn_results
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![session_id.0, intent.idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if stored_intent_hash != intent_object.key().as_str() {
                return Err(StoreError::ControlTurnIdempotencyConflict(
                    intent.idempotency_key.clone(),
                ));
            }
            let decision = Self::decode_canonical_projection(&decision_hash, decision_json)?;
            transaction.commit()?;
            return Ok(decision);
        }

        let superseded_grant = Self::supersede_issued_turn(&transaction, &session, now)?;
        if superseded_grant.is_some() {
            session = Self::load_control_session_on(&transaction, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        }
        let policy = Self::load_active_control_policy(&transaction)?;
        let anchor_exists = Self::control_anchor_exists(&transaction, project_id, session.task_id)?;
        let membership = Self::session_is_current_participant(
            &transaction,
            project_id,
            session.task_id,
            session_id,
        )?;
        let task_admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM control_anchors WHERE task_id = ?1",
            [session.task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let work_binding_current = Self::control_work_binding_is_current(
            &transaction,
            project_id,
            session_id,
            session.work_binding.as_ref(),
            now,
        )?;
        let input = TurnEvaluationInput {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: Some(session.task_id),
            work_binding: session.work_binding.clone(),
            work_binding_current,
            participant_membership: if membership {
                ParticipantMembership::Member
            } else {
                ParticipantMembership::NotMember
            },
            anchor_exists,
            phase: session.phase,
            host_assurance: session.assurance,
            required_assurance: policy.required_assurance,
            policy_effects: policy.supported_effects,
            mediated_effects: session.mediated_effects.clone(),
            current_epochs: ControlEpochs {
                project_policy: policy.epoch,
                task_admission: TaskAdmissionEpoch(task_admission_epoch),
            },
            session_epochs: session.epochs,
            capability_map_revision: session.capability_map_revision,
            intent: intent.clone(),
            evaluated_at: now,
            grant_ttl_seconds: policy.grant_ttl_seconds,
        };
        let observed = crate::control::observe_turn(&input);
        let decision = match observed.decision {
            TurnDecision::Grant { basis } => {
                let grant = IssuedTurnGrant {
                    control_schema_version: CONTROL_SCHEMA_VERSION,
                    grant_id: uuid::Uuid::now_v7().to_string(),
                    request_key: intent.idempotency_key.clone(),
                    basis: *basis,
                    delivery: None,
                    issued_at: now,
                };
                let grant_json = crate::canonical::canonical_bytes(&grant)?;
                transaction.execute(
                    "INSERT INTO control_turn_grants (
                         grant_id, session_id, task_id, request_key,
                         grant_json, state, issued_at_ms, expires_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'issued', ?6, ?7)",
                    params![
                        grant.grant_id,
                        session_id.0,
                        session.task_id.0.to_string(),
                        intent.idempotency_key,
                        grant_json,
                        now.timestamp_millis(),
                        grant.basis.expires_at.timestamp_millis(),
                    ],
                )?;
                transaction.execute(
                    "UPDATE control_sessions SET
                         phase = 'turn_open',
                         revision = revision + 1, updated_at_ms = ?2
                     WHERE session_id = ?1",
                    params![session_id.0, now.timestamp_millis()],
                )?;
                ControlTurnDecision::Grant {
                    grant: Box::new(grant),
                }
            }
            TurnDecision::Refuse { directive } => {
                if directive.code == crate::domain::ControlRefusalCode::PolicyEpochChanged {
                    transaction.execute(
                        "UPDATE control_sessions SET
                             project_policy_epoch = ?2,
                             revision = revision + 1, updated_at_ms = ?3
                         WHERE session_id = ?1",
                        params![session_id.0, policy.epoch.0, now.timestamp_millis()],
                    )?;
                }
                ControlTurnDecision::Refuse { directive }
            }
        };
        let decision_object = CanonicalObject::freeze(&decision)?;
        transaction.execute(
            "INSERT INTO control_turn_results (
                 session_id, task_id, idempotency_key, intent_hash, intent_json,
                 decision_hash, decision_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id.0,
                session.task_id.0.to_string(),
                intent.idempotency_key,
                intent_object.key().as_str(),
                intent_object.bytes(),
                decision_object.key().as_str(),
                decision_object.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        if let Some(superseded) = superseded_grant {
            let transition = TurnGrantSupersession {
                control_schema_version: CONTROL_SCHEMA_VERSION,
                session_id: session_id.clone(),
                task_id: session.task_id,
                superseded_grant_id: superseded.grant_id,
                superseded_request_key: superseded.request_key,
                replacement_request_key: intent.idempotency_key.clone(),
                replacement_decision: decision_object.key().clone(),
                reason: TurnGrantSupersessionReason::FreshEvaluation,
                superseded_at: now,
            };
            let transition_json = crate::canonical::canonical_bytes(&transition)?;
            transaction.execute(
                "INSERT INTO control_turn_grant_supersessions (
                     superseded_grant_id, session_id, task_id,
                     replacement_request_key, replacement_decision_hash,
                     supersession_json, superseded_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    transition.superseded_grant_id,
                    transition.session_id.0,
                    transition.task_id.0.to_string(),
                    transition.replacement_request_key,
                    transition.replacement_decision.as_str(),
                    transition_json,
                    transition.superseded_at.timestamp_millis(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(decision)
    }

    /// Atomically rechecks and begins one issued turn grant.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, unknown grants,
    /// idempotency conflicts, corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "begin rechecks and consumes the complete persisted grant basis atomically"
    )]
    pub fn begin_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        delivery_tokens: &[String],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnBeginDecision, StoreError> {
        let intent_object = CanonicalObject::freeze(&ControlTurnBeginFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            grant_id,
            delivery_tokens,
            idempotency_key,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        // An opener that could not resolve the project root's filesystem
        // identity must not begin, or replay the beginning of, a turn whose
        // basis names paths, even one issued earlier by a resolved opener.
        if self.host_path_policy.is_none()
            && Self::load_turn_grant(&transaction, session_id, grant_id)?.is_some_and(|grant| {
                grant
                    .grant
                    .basis
                    .resource_intents
                    .iter()
                    .any(|subject| matches!(subject, crate::domain::ResourceSubject::Path { .. }))
            })
        {
            return Err(StoreError::HostPathIdentityUnresolved);
        }
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "turn_begin",
            idempotency_key,
            intent_object.key(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let grant = Self::load_turn_grant(&transaction, session_id, grant_id)?
            .ok_or_else(|| StoreError::ControlTurnGrantNotFound(grant_id.into()))?;
        let policy = Self::load_active_control_policy(&transaction)?;
        let task_admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM control_anchors WHERE task_id = ?1",
            [session.task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let anchor_exists = Self::control_anchor_exists(&transaction, project_id, session.task_id)?;
        let membership = Self::session_is_current_participant(
            &transaction,
            project_id,
            session.task_id,
            session_id,
        )?;
        let work_binding_current = Self::control_work_binding_is_current(
            &transaction,
            project_id,
            session_id,
            session.work_binding.as_ref(),
            now,
        )?;
        let snapshot = TurnBeginSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            work_binding_current,
            phase: session.phase,
            participant_membership: if membership {
                ParticipantMembership::Member
            } else {
                ParticipantMembership::NotMember
            },
            anchor_exists,
            grant_state: grant.state,
            current_epochs: ControlEpochs {
                project_policy: policy.epoch,
                task_admission: TaskAdmissionEpoch(task_admission_epoch),
            },
            capability_map_revision: session.capability_map_revision,
            delivery_tokens: delivery_tokens.to_vec(),
            observed_at: now,
        };
        let decision = match crate::control::evaluate_turn_begin(&grant.grant, &snapshot) {
            TurnBeginDecision::Begin => {
                let changed = transaction.execute(
                    "UPDATE control_turn_grants SET state = 'begun', begun_at_ms = ?2
                     WHERE grant_id = ?1 AND state = 'issued'",
                    params![grant_id, now.timestamp_millis()],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "turn grant {grant_id:?} was not issued during begin"
                    )));
                }
                let revision = transaction.query_row(
                    "UPDATE control_sessions SET
                         revision = revision + 1, updated_at_ms = ?2
                     WHERE session_id = ?1
                     RETURNING revision",
                    params![session_id.0, now.timestamp_millis()],
                    |row| row.get::<_, i64>(0),
                )?;
                ControlTurnBeginDecision::Begin {
                    receipt: TurnBeginReceipt {
                        grant_id: grant_id.into(),
                        session_id: session_id.clone(),
                        task_id: session.task_id,
                        phase: SessionPhase::TurnOpen,
                        tentative_cursor: None,
                        session_revision: revision,
                        begun_at: now,
                    },
                }
            }
            TurnBeginDecision::Refuse { code } => {
                if matches!(
                    code,
                    crate::domain::ControlRefusalCode::GrantExpired
                        | crate::domain::ControlRefusalCode::PolicyEpochChanged
                        | crate::domain::ControlRefusalCode::TaskAdmissionEpochChanged
                        | crate::domain::ControlRefusalCode::StaleFence
                        | crate::domain::ControlRefusalCode::TaskUnbound
                        | crate::domain::ControlRefusalCode::TaskAccessDenied
                ) && matches!(grant.state, TurnGrantState::Issued)
                {
                    transaction.execute(
                        "UPDATE control_turn_grants SET state = 'expired'
                         WHERE grant_id = ?1 AND state = 'issued'",
                        [grant_id],
                    )?;
                    transaction.execute(
                        "UPDATE control_sessions SET
                             phase = 'ready',
                             revision = revision + 1, updated_at_ms = ?2
                         WHERE session_id = ?1",
                        params![session_id.0, now.timestamp_millis()],
                    )?;
                    if matches!(code, crate::domain::ControlRefusalCode::PolicyEpochChanged) {
                        transaction.execute(
                            "UPDATE control_sessions SET project_policy_epoch = ?2
                             WHERE session_id = ?1",
                            params![session_id.0, policy.epoch.0],
                        )?;
                    }
                }
                ControlTurnBeginDecision::Refuse { code }
            }
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "turn_begin",
            idempotency_key,
            &intent_object,
            &decision,
            now,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Checkpoints a begun turn, returns the session to `ready` (or `exited`),
    /// and appends one immutable event to the task's audit index.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, unknown grants,
    /// idempotency conflicts, corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits its canonical transition atomically"
    )]
    #[cfg(test)]
    pub fn checkpoint_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        self.checkpoint_control_turn_with_evidence(
            project_id,
            session_id,
            connection_token,
            routing_token,
            grant_id,
            next_intent,
            &[],
            &[],
            &[],
            idempotency_key,
            now,
        )
    }

    /// Checkpoints a begun turn and atomically records asserted host execution
    /// observations against its frozen local-work binding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when any observation is malformed, outside the
    /// grant effect envelope, or cannot be routed to the grant's exact run.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits its canonical transition atomically"
    )]
    #[cfg(test)]
    pub fn checkpoint_control_turn_with_observations(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        observations: &[ExecutionObservationInput],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        self.checkpoint_control_turn_with_evidence(
            project_id,
            session_id,
            connection_token,
            routing_token,
            grant_id,
            next_intent,
            observations,
            &[],
            &[],
            idempotency_key,
            now,
        )
    }

    /// Checkpoints a begun turn and atomically records host-captured execution,
    /// verification, and environment evidence against its frozen work basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when evidence is malformed, its producer cannot
    /// be resolved, or any fact falls outside the grant's exact run binding.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits all host evidence atomically"
    )]
    pub fn checkpoint_control_turn_with_evidence(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        observations: &[ExecutionObservationInput],
        verification_evidence: &[VerificationEvidenceInput],
        environment_evidence: &[EnvironmentEvidenceInput],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        validate_execution_observation_inputs(observations)?;
        validate_typed_evidence_inputs(
            verification_evidence,
            environment_evidence,
            now,
            &DevelopmentNoopRedactor,
        )?;
        let intent_object = CanonicalObject::freeze(&ControlTurnCheckpointFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            grant_id,
            next_intent,
            observations,
            verification_evidence,
            environment_evidence,
            idempotency_key,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !observations.is_empty()
            || !verification_evidence.is_empty()
            || !environment_evidence.is_empty()
        {
            work::require_work_schema_version(&transaction, self.work_schema_version)?;
        }
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        crate::domain::validate_status_capture_actor(&session.actor)
            .map_err(StoreError::InvalidWork)?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "turn_checkpoint",
            idempotency_key,
            intent_object.key(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let grant = Self::load_turn_grant(&transaction, session_id, grant_id)?
            .ok_or_else(|| StoreError::ControlTurnGrantNotFound(grant_id.into()))?;
        let snapshot = TurnCheckpointSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            phase: session.phase,
            grant_state: grant.state,
        };
        let decision = match crate::control::evaluate_turn_checkpoint(&grant.grant, &snapshot) {
            TurnCheckpointDecision::Checkpoint => {
                let records_work_evidence = !observations.is_empty()
                    || !verification_evidence.is_empty()
                    || !environment_evidence.is_empty();
                let binding = if records_work_evidence {
                    let binding = session.work_binding.clone().ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "host evidence requires an exact local-work binding".into(),
                        )
                    })?;
                    if grant.grant.basis.work_binding.as_ref() != Some(&binding) {
                        return Err(StoreError::WorkClaimMismatch {
                            work: binding.work_id,
                        });
                    }
                    Some(binding)
                } else {
                    None
                };
                let (obligation_rule_set, _) = Self::obligation_rule_set_for_policy_epoch_on(
                    &transaction,
                    grant.grant.basis.project_policy_epoch,
                )?;
                let mut observation_records = HashMap::new();
                let mut execution_observations = Vec::with_capacity(observations.len());
                for input in observations {
                    if !grant.grant.basis.requested_effects.contains(&input.effect) {
                        return Err(StoreError::ControlObservationScopeMismatch {
                            observation_id: input.observation_id.clone(),
                        });
                    }
                    let observation_binding = binding.clone().ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "execution observation lost its work binding".into(),
                        )
                    })?;
                    // The revision decides a source change: a reported change
                    // that leaves the revision where this run last saw it is
                    // not one, so it opens no tests obligation. Evaluation
                    // freshness still compares the revision it carries.
                    let source_changed = input.source_changed
                        && !work::source_revision_repeats_on(
                            &transaction,
                            observation_binding.run_id,
                            input.source_basis.as_ref(),
                        )?;
                    let observation = ExecutionObservation {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: observation_binding,
                        session_id: session_id.clone(),
                        grant_id: grant_id.into(),
                        observation_id: input.observation_id.trim().into(),
                        action_fingerprint: input.action_fingerprint.clone(),
                        effect: input.effect,
                        outcome: input.outcome,
                        source_changed,
                        obligation_rule_set: obligation_rule_set.clone(),
                        source_basis: input.source_basis.clone(),
                        observed_at: input.observed_at,
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    let hash =
                        work::append_control_execution_observation_on(&transaction, &observation)?;
                    observation_records.insert(
                        observation.observation_id.clone(),
                        (hash.clone(), observation),
                    );
                    execution_observations.push(hash);
                }
                let binding = binding.as_ref();
                let mut environment_hashes = Vec::with_capacity(environment_evidence.len());
                let mut environment_records = Vec::with_capacity(environment_evidence.len());
                for input in environment_evidence {
                    let binding = binding.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "environment evidence lost its work binding".into(),
                        )
                    })?;
                    if input.components.as_ref().is_some_and(|components| {
                        components.capability_map_revision != session.capability_map_revision
                    }) {
                        return Err(StoreError::EnvironmentBasisMismatch(
                            input.environment_fingerprint.to_string(),
                        ));
                    }
                    let evidence = EnvironmentEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: binding.clone(),
                        session_id: session_id.clone(),
                        source_basis: input.source_basis.clone(),
                        environment_fingerprint: input.environment_fingerprint.clone(),
                        components: input.components.clone(),
                        observed_at: input.observed_at,
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    let hash =
                        work::append_control_environment_evidence_on(&transaction, &evidence)?;
                    environment_hashes.push(hash.clone());
                    environment_records.push((hash, evidence));
                }
                let mut verification_hashes = Vec::with_capacity(verification_evidence.len());
                for input in verification_evidence {
                    let (producer_hash, producer) = match &input.producer_observation {
                        ExecutionObservationReference::ObjectId { object_id } => (
                            object_id.clone(),
                            work::load_control_execution_observation_on(&transaction, object_id)?
                                .ok_or_else(|| {
                                StoreError::VerificationProducerObservationNotFound(
                                    object_id.to_string(),
                                )
                            })?,
                        ),
                        ExecutionObservationReference::ObservationId { observation_id } => {
                            observation_records
                                .get(observation_id)
                                .cloned()
                                .ok_or_else(|| {
                                    StoreError::VerificationProducerObservationNotFound(
                                        observation_id.clone(),
                                    )
                                })?
                        }
                    };
                    let binding = binding.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification evidence lost its work binding".into(),
                        )
                    })?;
                    validate_verification_producer(
                        &producer, project_id, session_id, binding, now,
                    )?;
                    let completed_at = producer.observed_at.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification producer has no observed_at timestamp".into(),
                        )
                    })?;
                    let source_basis = producer.source_basis.clone().ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification producer has no source content basis".into(),
                        )
                    })?;
                    let environment = resolve_verification_environment_on(
                        &transaction,
                        input.environment.as_ref(),
                        &environment_records,
                        project_id,
                        binding,
                        &source_basis,
                    )?;
                    let evidence = VerificationEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: binding.clone(),
                        session_id: session_id.clone(),
                        producer_observation: producer_hash,
                        source_basis,
                        environment,
                        check_kind: input.check_kind,
                        check_fingerprint: producer.action_fingerprint.clone(),
                        result: verification_result(producer.outcome),
                        completed_at,
                        summary: normalize_verification_summary(input),
                        refs: normalize_typed_evidence_refs(&input.refs),
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    verification_hashes.push(work::append_control_verification_evidence_on(
                        &transaction,
                        &evidence,
                    )?);
                }
                let event = TurnCheckpointEvent {
                    schema_version: SCHEMA_VERSION,
                    task_id: session.task_id,
                    session_id: session_id.clone(),
                    grant_id: grant_id.into(),
                    delivered_cursor: grant.grant.basis.delivery_cursor,
                    next_intent,
                    execution_observations: execution_observations.clone(),
                    verification_evidence: verification_hashes.clone(),
                    environment_evidence: environment_hashes.clone(),
                    actor: session.actor.clone(),
                    created_at: now,
                };
                let event_object = CanonicalObject::mint(&event)?;
                Self::insert_object(&transaction, "turn_checkpoint_event", &event_object)?;
                // The task's change index is a write-only audit trail: no
                // grant delivers it any more, and no decision reads it.
                let cursor = Self::insert_task_change(
                    &transaction,
                    session.task_id,
                    "turn_checkpoint_event",
                    &event_object,
                )?;
                let phase = if matches!(next_intent, TurnNextIntent::Exit) {
                    SessionPhase::Exited
                } else {
                    SessionPhase::Ready
                };
                let changed = transaction.execute(
                    "UPDATE control_turn_grants SET
                         state = 'completed', completed_at_ms = ?2
                      WHERE grant_id = ?1 AND state = 'begun'",
                    params![grant_id, now.timestamp_millis()],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "turn grant {grant_id:?} was not begun during checkpoint"
                    )));
                }
                let revision = transaction.query_row(
                    "UPDATE control_sessions SET
                         phase = ?2,
                         revision = revision + 1, updated_at_ms = ?3
                     WHERE session_id = ?1
                     RETURNING revision",
                    params![session_id.0, enum_name(phase)?, now.timestamp_millis()],
                    |row| row.get::<_, i64>(0),
                )?;
                ControlTurnCheckpointDecision::Checkpointed {
                    receipt: TurnCheckpointReceipt {
                        grant_id: grant_id.into(),
                        checkpoint: event_object.key().clone(),
                        execution_observations,
                        verification_evidence: verification_hashes,
                        environment_evidence: environment_hashes,
                        cursor,
                        confirmed_cursor: cursor,
                        phase,
                        session_revision: revision,
                        checkpointed_at: now,
                    },
                }
            }
            TurnCheckpointDecision::Refuse { code } => ControlTurnCheckpointDecision::Refuse {
                code,
                directive: Some(crate::control::control_directive(
                    &format!("turn_checkpoint:{idempotency_key}"),
                    code,
                    None,
                    None,
                    None,
                    None,
                )),
            },
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "turn_checkpoint",
            idempotency_key,
            &intent_object,
            &decision,
            now,
        )?;
        transaction.commit()?;
        Ok(decision)
    }
}

fn validate_execution_observation_inputs(
    observations: &[ExecutionObservationInput],
) -> Result<(), StoreError> {
    if observations.len() > MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlProjection(format!(
            "turn checkpoint accepts at most {MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT} execution observations"
        )));
    }
    let mut ids = HashSet::new();
    for observation in observations {
        let id = observation.observation_id.trim();
        if id.is_empty() || id.len() > 256 || id != observation.observation_id || !ids.insert(id) {
            return Err(StoreError::InvalidControlProjection(
                "execution observation ids must be unique, trimmed, nonempty, and at most 256 bytes"
                    .into(),
            ));
        }
        if observation.source_changed
            && !matches!(
                observation.effect,
                EffectClass::MutateLocal | EffectClass::MutateShared
            )
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "execution observation {id:?} reports a source mutation for a non-mutation effect"
            )));
        }
        match (&observation.source_basis, observation.observed_at) {
            (None, None) => {}
            (Some(source_basis), Some(_)) => {
                validate_execution_source_basis(source_basis, id)?;
            }
            _ => {
                return Err(StoreError::InvalidControlProjection(format!(
                    "execution observation {id:?} must supply source_basis and observed_at together"
                )));
            }
        }
    }
    Ok(())
}

fn validate_typed_evidence_inputs<R: Redactor>(
    verification: &[VerificationEvidenceInput],
    environment: &[EnvironmentEvidenceInput],
    now: DateTime<Utc>,
    redactor: &R,
) -> Result<(), StoreError> {
    if verification.len() > MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlSession(format!(
            "turn checkpoint accepts at most {MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT} verification evidence records"
        )));
    }
    if environment.len() > MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlSession(format!(
            "turn checkpoint accepts at most {MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT} environment evidence records"
        )));
    }
    for input in verification {
        match &input.producer_observation {
            ExecutionObservationReference::ObjectId { .. } => {}
            ExecutionObservationReference::ObservationId { observation_id } => {
                let trimmed = observation_id.trim();
                if trimmed.is_empty() || trimmed != observation_id || observation_id.len() > 256 {
                    return Err(StoreError::InvalidControlSession(
                        "verification observation ids must be trimmed, nonempty, and at most 256 bytes"
                            .into(),
                    ));
                }
            }
        }
        if input.summary.as_ref().is_some_and(|summary| {
            let trimmed = summary.trim();
            trimmed.is_empty()
                || trimmed != summary
                || summary.len() > MAX_TYPED_EVIDENCE_SUMMARY_BYTES
        }) {
            return Err(StoreError::InvalidControlSession(format!(
                "verification summaries must be trimmed, nonempty, and at most {MAX_TYPED_EVIDENCE_SUMMARY_BYTES} bytes"
            )));
        }
        validate_typed_evidence_refs(&input.refs)?;
    }
    for (index, input) in environment.iter().enumerate() {
        validate_execution_source_basis(&input.source_basis, &format!("environment-{index}"))?;
        if let Some(components) = &input.components {
            validate_environment_components(components, &input.source_basis, redactor)?;
            if environment_components_fingerprint(components)? != input.environment_fingerprint {
                return Err(StoreError::EnvironmentFingerprintMismatch);
            }
        }
        if input.observed_at > now {
            return Err(StoreError::InvalidControlSession(format!(
                "environment evidence {index} is timestamped after its checkpoint"
            )));
        }
    }
    Ok(())
}

fn environment_components_fingerprint(
    components: &EnvironmentComponents,
) -> Result<ObjectId, StoreError> {
    Ok(CanonicalObject::freeze(components)?.key().clone())
}

fn validate_environment_components<R: Redactor>(
    components: &EnvironmentComponents,
    source_basis: &crate::domain::ExecutionSourceBasis,
    redactor: &R,
) -> Result<(), StoreError> {
    let valid_text = |value: &str| {
        let trimmed = value.trim();
        !trimmed.is_empty() && trimmed == value && value.len() <= 256
    };
    if !valid_text(&components.toolchain)
        || !valid_text(&components.workspace_id)
        || components
            .sandbox
            .as_deref()
            .is_some_and(|value| !valid_text(value))
        || components.workspace_id != source_basis.workspace_id
        || components.capability_map_revision <= 0
    {
        return Err(StoreError::InvalidControlSession(
            "environment components must be trimmed, nonempty, at most 256 bytes, and match their source workspace"
                .into(),
        ));
    }
    redactor
        .inspect(&components.toolchain)
        .map_err(StoreError::RedactionRefused)?;
    redactor
        .inspect(&components.workspace_id)
        .map_err(StoreError::RedactionRefused)?;
    if let Some(sandbox) = &components.sandbox {
        redactor
            .inspect(sandbox)
            .map_err(StoreError::RedactionRefused)?;
    }
    Ok(())
}

pub(super) fn resolve_verification_environment_on(
    connection: &Connection,
    reference: Option<&EnvironmentEvidenceReference>,
    same_checkpoint: &[(ObjectId, EnvironmentEvidence)],
    project_id: &crate::domain::ProjectId,
    binding: &ControlWorkBinding,
    source_basis: &crate::domain::ExecutionSourceBasis,
) -> Result<Option<ObjectId>, StoreError> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let (hash, evidence) = match reference {
        EnvironmentEvidenceReference::Index { index } => same_checkpoint
            .get(*index)
            .cloned()
            .ok_or_else(|| StoreError::EnvironmentEvidenceNotFound(index.to_string()))?,
        EnvironmentEvidenceReference::ObjectId { object_id } => {
            let evidence = work::load_control_environment_evidence_on(connection, object_id)?
                .ok_or_else(|| StoreError::EnvironmentEvidenceNotFound(object_id.to_string()))?;
            (object_id.clone(), evidence)
        }
    };
    let same_run = &evidence.project_id == project_id
        && evidence.binding.root_execution_id == binding.root_execution_id
        && evidence.binding.work_id == binding.work_id
        && evidence.binding.run_id == binding.run_id;
    if !same_run || evidence.source_basis.source_revision != source_basis.source_revision {
        return Err(StoreError::EnvironmentBasisMismatch(hash.to_string()));
    }
    Ok(Some(hash))
}

fn validate_execution_source_basis(
    source_basis: &crate::domain::ExecutionSourceBasis,
    label: &str,
) -> Result<(), StoreError> {
    let workspace_id = source_basis.workspace_id.trim();
    let source_revision = source_basis.source_revision.trim();
    if workspace_id.is_empty()
        || source_revision.is_empty()
        || workspace_id != source_basis.workspace_id
        || source_revision != source_basis.source_revision
        || workspace_id.len() > 512
        || source_revision.len() > 512
    {
        return Err(StoreError::InvalidControlSession(format!(
            "evidence {label:?} source basis fields must be trimmed, nonempty, and at most 512 bytes"
        )));
    }
    Ok(())
}

fn validate_typed_evidence_refs(refs: &[String]) -> Result<(), StoreError> {
    if refs.len() > MAX_TYPED_EVIDENCE_REFS {
        return Err(StoreError::InvalidControlSession(format!(
            "typed evidence accepts at most {MAX_TYPED_EVIDENCE_REFS} references"
        )));
    }
    if refs.iter().any(|reference| {
        let trimmed = reference.trim();
        trimmed.is_empty() || trimmed != reference || reference.len() > MAX_TYPED_EVIDENCE_REF_BYTES
    }) {
        return Err(StoreError::InvalidControlSession(format!(
            "typed evidence references must be trimmed, nonempty, and at most {MAX_TYPED_EVIDENCE_REF_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_verification_producer(
    producer: &ExecutionObservation,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let actor_run_id = binding.run_id.0.to_string();
    let consistent = &producer.project_id == project_id
        && &producer.session_id == session_id
        && &producer.binding == binding
        && producer.actor.session_id.as_ref() == Some(session_id)
        && producer.actor.run_id.as_deref() == Some(actor_run_id.as_str());
    if !consistent {
        return Err(StoreError::InvalidControlSession(
            "verification producer does not match the checkpoint work/session binding".into(),
        ));
    }
    let source_basis = producer.source_basis.as_ref().ok_or_else(|| {
        StoreError::InvalidControlSession(
            "verification producer must carry a source content basis".into(),
        )
    })?;
    validate_execution_source_basis(source_basis, &producer.observation_id)?;
    let observed_at = producer.observed_at.ok_or_else(|| {
        StoreError::InvalidControlSession(
            "verification producer must carry observed_at with its source basis".into(),
        )
    })?;
    if observed_at > producer.recorded_at || producer.recorded_at > now {
        return Err(StoreError::InvalidControlSession(
            "verification producer timestamps are not monotone".into(),
        ));
    }
    Ok(())
}

const fn verification_result(outcome: ExecutionOutcome) -> VerificationResult {
    match outcome {
        ExecutionOutcome::Succeeded => VerificationResult::Passed,
        ExecutionOutcome::Failed => VerificationResult::Failed,
        ExecutionOutcome::Unknown => VerificationResult::Indeterminate,
    }
}

fn normalize_verification_summary(input: &VerificationEvidenceInput) -> String {
    input.summary.clone().unwrap_or_else(|| {
        format!(
            "host-recorded {} verification",
            verification_kind_name(input.check_kind)
        )
    })
}

const fn verification_kind_name(kind: VerificationKind) -> &'static str {
    match kind {
        VerificationKind::Test => "test",
        VerificationKind::Build => "build",
        VerificationKind::Lint => "lint",
        VerificationKind::Review => "review",
        VerificationKind::Acceptance => "acceptance",
    }
}

fn normalize_typed_evidence_refs(refs: &[String]) -> Vec<String> {
    let mut refs = refs.to_vec();
    refs.sort();
    refs.dedup();
    refs
}
