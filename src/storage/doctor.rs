use super::{
    CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION, CONTROL_POLICY_STATE_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, CanonicalObject, Connection, ControlPolicyRecoveryFinding,
    ControlPolicyRecoveryReport, ControlPolicyUpdateReceipt, ControlTurnDecision, IntegrityReport,
    IssuedTurnGrant, MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES,
    MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES, MemoryAssertionEvent, MemoryProjectionMode,
    MemoryVersion, ObjectHash, ObligationRuleSetUpdateReceipt, ObservedTurnDecision,
    OptionalExtension, SCHEMA_VERSION, Scope, SessionId, SqliteStore, StoreError,
    StoredControlGrantRow, StoredControlObservation, StoredControlOperation,
    StoredControlPolicyOperation, StoredControlTurnResult, StoredTurnGrantSupersession,
    StoredWorkLeaseRow, Transaction, TurnDecision, TurnEvaluationInput, TurnGrantState,
    TurnGrantSupersession, TurnGrantSupersessionReason, TurnObservationIntentFingerprint,
    enum_name, params, parse_enum, validate_keyed_project_memory_shape,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Verifies canonical bytes and hashes for every stored object and control
    /// observation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot scan the object table.
    #[allow(
        clippy::too_many_lines,
        reason = "the integrity scanner enumerates every canonical and operational control tier"
    )]
    pub fn verify_all(&self) -> Result<IntegrityReport, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut report = IntegrityReport::default();
        for row in rows {
            let (stored_hash, bytes) = row?;
            report.checked_objects += 1;
            let valid = ObjectHash::from_stored(stored_hash.clone())
                .is_some_and(|expected| CanonicalObject::verify(&expected, bytes).is_ok());
            if !valid {
                report.invalid_objects.push(stored_hash);
            }
        }
        drop(statement);
        Self::verify_memory_head_projections_on(
            &self.connection,
            &mut report.checked_objects,
            &mut report.invalid_objects,
        )?;
        Self::verify_object_fts_on(
            &self.connection,
            &mut report.checked_objects,
            &mut report.invalid_objects,
        )?;
        Self::verify_project_memory_state_on(
            &self.connection,
            &mut report.checked_objects,
            &mut report.invalid_objects,
        )?;

        if self.connection.is_autocommit() {
            let policy_snapshot = self.connection.unchecked_transaction()?;
            Self::verify_control_policy_records_on(&policy_snapshot, &mut report)?;
            policy_snapshot.commit()?;
        } else {
            // Corruption fixtures and embedders may already own a transaction
            // or savepoint. Reuse that snapshot instead of nesting BEGIN.
            Self::verify_control_policy_records_on(&self.connection, &mut report)?;
        }

        let mut control_statement = self.connection.prepare(
            "SELECT sequence, session_id, task_id, idempotency_key, intent_hash,
                    observed_at_ms, input_hash, input_json, decision_hash, decision_json
             FROM control_observations ORDER BY sequence",
        )?;
        let control_rows = control_statement.query_map([], |row| {
            Ok(StoredControlObservation {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                observed_at_ms: row.get(5)?,
                input_hash: row.get(6)?,
                input_json: row.get(7)?,
                decision_hash: row.get(8)?,
                decision_json: row.get(9)?,
            })
        })?;
        for row in control_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::decode_control_observation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_observation:{}", stored.sequence));
            }
        }

        let mut session_statement = self
            .connection
            .prepare("SELECT session_id FROM control_sessions ORDER BY session_id")?;
        let session_rows = session_statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in session_rows {
            let session_id = row?;
            report.checked_control_records += 1;
            if Self::load_control_session_on(&self.connection, &SessionId(session_id.clone()))
                .is_err()
            {
                report
                    .invalid_control_records
                    .push(format!("control_session:{session_id}"));
            }
        }

        let mut turn_statement = self.connection.prepare(
            "SELECT sequence, session_id, task_id, idempotency_key,
                    intent_hash, intent_json, decision_hash, decision_json
             FROM control_turn_results ORDER BY sequence",
        )?;
        let turn_rows = turn_statement.query_map([], |row| {
            Ok(StoredControlTurnResult {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                intent_json: row.get(5)?,
                decision_hash: row.get(6)?,
                decision_json: row.get(7)?,
            })
        })?;
        for row in turn_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_turn_result(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_turn_result:{}", stored.sequence));
            }
        }

        let grant_rows = {
            let mut grant_statement = self.connection.prepare(
                "SELECT grant_id, session_id, task_id, request_key, grant_hash,
                        grant_json, state, issued_at_ms, expires_at_ms
                 FROM control_turn_grants ORDER BY issued_at_ms, grant_id",
            )?;
            grant_statement
                .query_map([], |row| {
                    Ok(StoredControlGrantRow {
                        grant_id: row.get(0)?,
                        session_id: row.get(1)?,
                        task_id: row.get(2)?,
                        request_key: row.get(3)?,
                        grant_hash: row.get(4)?,
                        grant_json: row.get(5)?,
                        state: row.get(6)?,
                        issued_at_ms: row.get(7)?,
                        expires_at_ms: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for stored in grant_rows {
            report.checked_control_records += 1;
            let supersession_count = if stored.state == "superseded" {
                self.connection.query_row(
                    "SELECT COUNT(*) FROM control_turn_grant_supersessions
                     WHERE superseded_grant_id = ?1",
                    [&stored.grant_id],
                    |row| row.get::<_, i64>(0),
                )?
            } else {
                0
            };
            if Self::verify_control_grant_row(&stored).is_err()
                || (stored.state == "superseded" && supersession_count != 1)
            {
                report
                    .invalid_control_records
                    .push(format!("control_turn_grant:{}", stored.grant_id));
            }
        }

        let supersession_rows = {
            let mut statement = self.connection.prepare(
                "SELECT superseded_grant_id, session_id, task_id,
                        replacement_request_key, replacement_decision_hash,
                        supersession_hash, supersession_json, superseded_at_ms
                 FROM control_turn_grant_supersessions
                 ORDER BY superseded_at_ms, superseded_grant_id",
            )?;
            statement
                .query_map([], |row| {
                    Ok(StoredTurnGrantSupersession {
                        superseded_grant_id: row.get(0)?,
                        session_id: row.get(1)?,
                        task_id: row.get(2)?,
                        replacement_request_key: row.get(3)?,
                        replacement_decision_hash: row.get(4)?,
                        supersession_hash: row.get(5)?,
                        supersession_json: row.get(6)?,
                        superseded_at_ms: row.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for stored in supersession_rows {
            report.checked_control_records += 1;
            if Self::verify_turn_grant_supersession(&self.connection, &stored).is_err() {
                report.invalid_control_records.push(format!(
                    "control_turn_grant_supersession:{}",
                    stored.superseded_grant_id
                ));
            }
        }

        let mut lease_statement = self.connection.prepare(
            "SELECT lease_id, task_id, holder_session_id, lease_hash,
                    lease_json, state, expires_at_ms
             FROM control_work_leases ORDER BY lease_id",
        )?;
        let lease_rows = lease_statement.query_map([], |row| {
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
        for row in lease_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::decode_work_lease_row(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_work_lease:{}", stored.lease_id));
            }
        }

        let mut operation_statement = self.connection.prepare(
            "SELECT sequence, session_id, operation, idempotency_key,
                    intent_hash, intent_json, result_hash, result_json
             FROM control_operation_results ORDER BY sequence",
        )?;
        let operation_rows = operation_statement.query_map([], |row| {
            Ok(StoredControlOperation {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                operation: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                intent_json: row.get(5)?,
                result_hash: row.get(6)?,
                result_json: row.get(7)?,
            })
        })?;
        for row in operation_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_operation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_operation:{}", stored.sequence));
            }
        }
        let mut policy_operation_statement = self.connection.prepare(
            "SELECT sequence, operation, idempotency_key, intent_hash, intent_json,
                    result_hash, result_json
             FROM control_policy_operation_results ORDER BY sequence",
        )?;
        let policy_operation_rows = policy_operation_statement.query_map([], |row| {
            Ok(StoredControlPolicyOperation {
                sequence: row.get(0)?,
                operation: row.get(1)?,
                idempotency_key: row.get(2)?,
                intent_hash: row.get(3)?,
                intent_json: row.get(4)?,
                result_hash: row.get(5)?,
                result_json: row.get(6)?,
            })
        })?;
        for row in policy_operation_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_policy_operation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_policy_operation:{}", stored.sequence));
            }
        }
        let (checked_work_records, invalid_work_records) = self.verify_work_projections()?;
        report.checked_work_records = checked_work_records;
        report.invalid_work_records = invalid_work_records;
        let (checked_graph_snapshot_audits, invalid_graph_snapshot_audits) =
            super::graph_snapshot::verify_work_graph_snapshot_saved_events_on(&self.connection)?;
        report.checked_graph_snapshot_audits = checked_graph_snapshot_audits;
        report.invalid_graph_snapshot_audits = invalid_graph_snapshot_audits;
        Ok(report)
    }

    fn verify_control_policy_records_on(
        connection: &Connection,
        report: &mut IntegrityReport,
    ) -> Result<(), StoreError> {
        report.checked_control_records += 1;
        match Self::verify_control_policy_history(connection) {
            Ok(policy) => {
                let active_rules_are_valid = policy.state_schema_version
                    == CONTROL_POLICY_STATE_SCHEMA_VERSION
                    && Self::load_obligation_rule_set_on(connection, &policy.obligation_rule_set)
                        .is_ok();
                if !active_rules_are_valid {
                    report
                        .invalid_control_records
                        .push("control_policy_state:active".into());
                }
            }
            Err(error @ StoreError::Sqlite(_)) => return Err(error),
            Err(_) => report
                .invalid_control_records
                .push("control_policy_state:active".into()),
        }
        let active_policy_epoch = connection
            .query_row(
                "SELECT policy_epoch FROM control_policy_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let policy_rows = {
            let mut statement = connection.prepare(
                "SELECT policy_hash, policy_epoch
                 FROM control_policy_versions ORDER BY policy_epoch",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (stored_hash, projected_epoch) in policy_rows {
            report.checked_control_records += 1;
            let valid = ObjectHash::from_stored(stored_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))
                .and_then(|hash| Self::load_control_policy_version(connection, &hash))
                .and_then(|(policy, authority)| {
                    Self::load_obligation_rule_set_on(connection, &policy.obligation_rule_set)?;
                    Ok((policy, authority))
                });
            let is_orphaned_successor =
                active_policy_epoch.is_none_or(|active_epoch| projected_epoch > active_epoch);
            let is_invalid = match valid {
                Ok(_) => false,
                Err(error @ StoreError::Sqlite(_)) => return Err(error),
                Err(_) => true,
            };
            if is_orphaned_successor || is_invalid {
                report
                    .invalid_control_records
                    .push(format!("control_policy_version:{stored_hash}"));
            }
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the recovery scanner keeps active-head and per-version diagnostics in one visibly read-only pass"
    )]
    pub(super) fn diagnose_control_policy_records_on(
        connection: &Connection,
    ) -> Result<ControlPolicyRecoveryReport, StoreError> {
        const GUIDANCE: &str = "ordinary Engram open remains fail-closed; restore a verified backup or inspect the named immutable bindings before an explicit operator-directed repair; this command did not select, rewrite, or activate a policy";
        let mut report = ControlPolicyRecoveryReport {
            checked_control_records: 1,
            invalid_control_records: Vec::new(),
            guidance: GUIDANCE.into(),
        };
        let has_state = Self::sqlite_table_exists(connection, "control_policy_state")?;
        let has_versions = Self::sqlite_table_exists(connection, "control_policy_versions")?;
        let has_objects = Self::sqlite_table_exists(connection, "objects")?;

        let state_shape = if has_state {
            Self::diagnose_control_policy_table_shape(
                connection,
                "control_policy_state",
                &[
                    ("singleton", "INTEGER"),
                    ("schema_version", "INTEGER"),
                    ("policy_epoch", "INTEGER"),
                    ("required_assurance", "TEXT"),
                    ("supported_effects_json", "TEXT"),
                    ("grant_ttl_seconds", "INTEGER"),
                    ("policy_hash", "TEXT"),
                ],
                &mut report,
            )?
        } else {
            false
        };
        let versions_shape = if has_versions {
            Self::diagnose_control_policy_table_shape(
                connection,
                "control_policy_versions",
                &[
                    ("policy_hash", "TEXT"),
                    ("policy_epoch", "INTEGER"),
                    ("authority_hash", "TEXT"),
                    ("policy_json", "BLOB"),
                ],
                &mut report,
            )?
        } else {
            false
        };
        let objects_shape = if has_objects {
            Self::diagnose_control_policy_table_shape(
                connection,
                "objects",
                &[
                    ("object_hash", "TEXT"),
                    ("object_kind", "TEXT"),
                    ("canonical_json", "BLOB"),
                ],
                &mut report,
            )?
        } else {
            false
        };

        if !has_state {
            report
                .invalid_control_records
                .push(ControlPolicyRecoveryFinding {
                    record: "control_policy_state:active".into(),
                    detail: "control_policy_state table is missing".into(),
                });
        } else if !has_versions || !has_objects {
            let missing = [
                (!has_versions).then_some("control_policy_versions"),
                (!has_objects).then_some("objects"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            report
                .invalid_control_records
                .push(ControlPolicyRecoveryFinding {
                    record: "control_policy_state:active".into(),
                    detail: format!("active control policy cannot be verified; missing {missing}"),
                });
        } else if state_shape
            && versions_shape
            && objects_shape
            && let Err(error) = Self::verify_control_policy_history(connection).and_then(|policy| {
                Self::load_obligation_rule_set_on(connection, &policy.obligation_rule_set)
                    .map(|_| ())
            })
        {
            report
                .invalid_control_records
                .push(ControlPolicyRecoveryFinding {
                    record: "control_policy_state:active".into(),
                    detail: error.to_string(),
                });
        }

        let active_policy_epoch = if state_shape {
            match connection
                .query_row(
                    "SELECT policy_epoch FROM control_policy_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            {
                Ok(epoch) => epoch,
                Err(error) => {
                    report
                        .invalid_control_records
                        .push(ControlPolicyRecoveryFinding {
                            record: "control_policy_state:active".into(),
                            detail: format!("active selector cannot be decoded: {error}"),
                        });
                    None
                }
            }
        } else {
            None
        };
        if versions_shape && objects_shape {
            let policy_rows = (|| -> Result<Vec<(String, i64)>, rusqlite::Error> {
                let mut statement = connection.prepare(
                    "SELECT policy_hash, policy_epoch
                     FROM control_policy_versions ORDER BY policy_epoch, policy_hash",
                )?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })();
            let policy_rows = match policy_rows {
                Ok(rows) => rows,
                Err(error) => {
                    report
                        .invalid_control_records
                        .push(ControlPolicyRecoveryFinding {
                            record: "control_policy_versions:schema".into(),
                            detail: format!("version rows cannot be decoded: {error}"),
                        });
                    Vec::new()
                }
            };
            for (stored_hash, projected_epoch) in policy_rows {
                report.checked_control_records += 1;
                let valid = ObjectHash::from_stored(stored_hash.clone())
                    .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))
                    .and_then(|hash| Self::load_control_policy_version(connection, &hash))
                    .and_then(|(policy, authority)| {
                        Self::load_obligation_rule_set_on(
                            connection,
                            &policy.obligation_rule_set,
                        )?;
                        match policy.previous_policy.as_ref() {
                            Some(previous_hash) => {
                                let (previous, _) =
                                    Self::load_control_policy_version(connection, previous_hash)?;
                                if previous.policy_epoch.0.checked_add(1)
                                    != Some(policy.policy_epoch.0)
                                {
                                    return Err(StoreError::InvalidControlProjection(format!(
                                        "control policy {stored_hash} is not bound to a contiguous predecessor epoch"
                                    )));
                                }
                                Self::validate_control_policy_transition(
                                    Some(&previous),
                                    &policy,
                                    &authority,
                                )?;
                            }
                            None => Self::validate_control_policy_transition(
                                None,
                                &policy,
                                &authority,
                            )?,
                        }
                        Ok(())
                    });
                let invalid_detail = match valid {
                    Err(error) => Some(error.to_string()),
                    Ok(()) if active_policy_epoch.is_some_and(|epoch| projected_epoch > epoch) => {
                        Some(format!(
                            "policy epoch {projected_epoch} is an orphaned successor of the active selector"
                        ))
                    }
                    Ok(()) => None,
                };
                if let Some(detail) = invalid_detail {
                    report
                        .invalid_control_records
                        .push(ControlPolicyRecoveryFinding {
                            record: format!("control_policy_version:{stored_hash}"),
                            detail,
                        });
                }
            }
        }
        report.invalid_control_records.sort_by(|left, right| {
            left.record
                .cmp(&right.record)
                .then_with(|| left.detail.cmp(&right.detail))
        });
        report.invalid_control_records.dedup();
        Ok(report)
    }

    pub(super) fn decode_control_observation(
        stored: &StoredControlObservation,
    ) -> Result<ObservedTurnDecision, StoreError> {
        let input_hash = ObjectHash::from_stored(stored.input_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored.input_hash.clone()))?;
        let input: TurnEvaluationInput =
            CanonicalObject::verify(&input_hash, stored.input_json.clone())?.decode()?;
        let expected_intent = CanonicalObject::freeze(&TurnObservationIntentFingerprint {
            control_schema_version: input.control_schema_version,
            session_id: &input.session_id,
            task_id: input.task_id,
            intent: &input.intent,
        })?;
        let decision_hash = ObjectHash::from_stored(stored.decision_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored.decision_hash.clone()))?;
        let observation: ObservedTurnDecision =
            CanonicalObject::verify(&decision_hash, stored.decision_json.clone())?.decode()?;

        let input_task = input.task_id.map(|task_id| task_id.0.to_string());
        let row_matches = expected_intent.hash().as_str() == stored.intent_hash
            && observation.control_schema_version == CONTROL_SCHEMA_VERSION
            && input.session_id.0 == stored.session_id
            && input_task == stored.task_id
            && input.intent.idempotency_key == stored.idempotency_key
            && input.evaluated_at.timestamp_millis() == stored.observed_at_ms
            && observation.request_key == stored.idempotency_key
            && observation.observed_at.timestamp_millis() == stored.observed_at_ms;
        if !row_matches {
            return Err(StoreError::InvalidControlObservation(format!(
                "row {} does not match its input and decision",
                stored.sequence
            )));
        }

        let schema_matches = input.control_schema_version == CONTROL_SCHEMA_VERSION
            || matches!(
                &observation.decision,
                TurnDecision::Refuse { directive }
                    if directive.code == crate::domain::ControlRefusalCode::UnknownControlSchema
            );
        let decision_matches = schema_matches
            && match &observation.decision {
                TurnDecision::Grant { basis } => {
                    Some(basis.task_id) == input.task_id
                        && basis.session_id == input.session_id
                        && basis.purpose == input.intent.purpose
                        && basis.intent_fingerprint == input.intent.intent_fingerprint
                }
                TurnDecision::Refuse { directive } => {
                    directive.directive_id
                        == format!("{}:{}", stored.idempotency_key, directive.code.as_str())
                }
                TurnDecision::Defer { deferral } => !deferral.wake_condition.trim().is_empty(),
            };
        if !decision_matches {
            return Err(StoreError::InvalidControlObservation(format!(
                "decision {} is not bound to its input",
                stored.sequence
            )));
        }

        Ok(observation)
    }

    fn verify_control_turn_result(stored: &StoredControlTurnResult) -> Result<(), StoreError> {
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let decision: ControlTurnDecision =
            Self::decode_canonical_projection(&stored.decision_hash, stored.decision_json.clone())?;
        let row_matches = intent.get("session_id").and_then(serde_json::Value::as_str)
            == Some(stored.session_id.as_str())
            && intent.get("task_id").and_then(serde_json::Value::as_str)
                == Some(stored.task_id.as_str())
            && intent
                .get("intent")
                .and_then(|value| value.get("idempotency_key"))
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str());
        let decision_matches = match decision {
            ControlTurnDecision::Grant { grant } => {
                grant.control_schema_version == CONTROL_SCHEMA_VERSION
                    && grant.request_key == stored.idempotency_key
                    && grant.basis.session_id.0 == stored.session_id
                    && grant.basis.task_id.0.to_string() == stored.task_id
            }
            ControlTurnDecision::Refuse { directive } => directive
                .directive_id
                .starts_with(&format!("{}:", stored.idempotency_key)),
            ControlTurnDecision::Defer { deferral } => !deferral.wake_condition.trim().is_empty(),
        };
        if !row_matches || !decision_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn result {} is not bound to its row",
                stored.sequence
            )));
        }
        Ok(())
    }

    fn verify_control_grant_row(stored: &StoredControlGrantRow) -> Result<(), StoreError> {
        let grant: IssuedTurnGrant =
            Self::decode_canonical_projection(&stored.grant_hash, stored.grant_json.clone())?;
        let state = parse_enum::<TurnGrantState>(&stored.state)?;
        let delivery_matches = crate::control::delivery_matches_grant(&grant);
        let row_matches = grant.control_schema_version == CONTROL_SCHEMA_VERSION
            && grant.grant_id == stored.grant_id
            && grant.request_key == stored.request_key
            && grant.basis.session_id.0 == stored.session_id
            && grant.basis.task_id.0.to_string() == stored.task_id
            && grant.issued_at.timestamp_millis() == stored.issued_at_ms
            && grant.basis.expires_at.timestamp_millis() == stored.expires_at_ms
            && stored.expires_at_ms > stored.issued_at_ms
            && matches!(
                state,
                TurnGrantState::Issued
                    | TurnGrantState::Begun
                    | TurnGrantState::Completed
                    | TurnGrantState::Expired
                    | TurnGrantState::Superseded
            );
        if !row_matches || !delivery_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn grant {:?} is not bound to its row",
                stored.grant_id
            )));
        }
        Ok(())
    }

    fn verify_turn_grant_supersession(
        connection: &Connection,
        stored: &StoredTurnGrantSupersession,
    ) -> Result<(), StoreError> {
        let transition: TurnGrantSupersession = Self::decode_canonical_projection(
            &stored.supersession_hash,
            stored.supersession_json.clone(),
        )?;
        let grant = connection
            .query_row(
                "SELECT session_id, task_id, request_key, state
                 FROM control_turn_grants WHERE grant_id = ?1",
                [&stored.superseded_grant_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "superseded turn grant {:?} is missing",
                    stored.superseded_grant_id
                ))
            })?;
        let replacement_decision = connection
            .query_row(
                "SELECT decision_hash FROM control_turn_results
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![stored.session_id, stored.replacement_request_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "replacement turn result {:?} is missing",
                    stored.replacement_request_key
                ))
            })?;
        let row_matches = transition.control_schema_version == CONTROL_SCHEMA_VERSION
            && transition.session_id.0 == stored.session_id
            && transition.task_id.0.to_string() == stored.task_id
            && transition.superseded_grant_id == stored.superseded_grant_id
            && transition.superseded_request_key == grant.2
            && transition.replacement_request_key == stored.replacement_request_key
            && transition.replacement_decision.as_str() == stored.replacement_decision_hash
            && transition.replacement_decision.as_str() == replacement_decision
            && transition.reason == TurnGrantSupersessionReason::FreshEvaluation
            && transition.superseded_at.timestamp_millis() == stored.superseded_at_ms
            && grant.0 == stored.session_id
            && grant.1 == stored.task_id
            && grant.3 == "superseded";
        if !row_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn grant supersession {:?} is not bound to its grant and replacement",
                stored.superseded_grant_id
            )));
        }
        Ok(())
    }

    fn verify_control_operation(stored: &StoredControlOperation) -> Result<(), StoreError> {
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let result = Self::decode_canonical_value(&stored.result_hash, stored.result_json.clone())?;
        let row_matches = intent.get("session_id").and_then(serde_json::Value::as_str)
            == Some(stored.session_id.as_str())
            && intent
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str())
            && match stored.operation.as_str() {
                "turn_begin" | "turn_checkpoint" | "lease_acquire" | "obligation_waive" => result
                    .get("decision")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "lease_release" => result
                    .get("lease_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                _ => false,
            };
        if !row_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "control operation {} is not bound to its row",
                stored.sequence
            )));
        }
        Ok(())
    }

    fn verify_control_policy_operation(
        stored: &StoredControlPolicyOperation,
    ) -> Result<(), StoreError> {
        if stored.intent_json.len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES
            || stored.result_json.len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation {} exceeds its canonical byte limits",
                stored.sequence
            )));
        }
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let row_matches = intent
            .get("fingerprint_schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(
                CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
            ))
            && intent.get("operation").and_then(serde_json::Value::as_str)
                == Some(stored.operation.as_str())
            && intent
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str());
        if !row_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation {} is not bound to its row",
                stored.sequence
            )));
        }
        match stored.operation.as_str() {
            "set_required_assurance" => {
                Self::decode_canonical_projection::<ControlPolicyUpdateReceipt>(
                    &stored.result_hash,
                    stored.result_json.clone(),
                )?;
            }
            "set_obligation_rule_set" => {
                Self::decode_canonical_projection::<ObligationRuleSetUpdateReceipt>(
                    &stored.result_hash,
                    stored.result_json.clone(),
                )?;
            }
            _ => {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy operation {} has unknown operation {:?}",
                    stored.sequence, stored.operation
                )));
            }
        }
        Ok(())
    }

    fn decode_canonical_value(
        stored_hash: &str,
        bytes: Vec<u8>,
    ) -> Result<serde_json::Value, StoreError> {
        Self::decode_canonical_projection(stored_hash, bytes)
    }

    /// Applies one canonical memory assertion to disposable heads and FTS.
    ///
    /// Canonical rebuilds preserve a previously replayed terminal head when an
    /// older non-terminal assertion sorts later. Live writes instead refuse a
    /// suppressed projection so callers never mistake a no-op for success.
    pub(super) fn apply_memory_projection(
        transaction: &Transaction<'_>,
        version_hash: &ObjectHash,
        assertion_hash: &ObjectHash,
        version: &MemoryVersion,
        assertion: &MemoryAssertionEvent,
        mode: MemoryProjectionMode,
    ) -> Result<(), StoreError> {
        if version.schema_version != SCHEMA_VERSION
            || assertion.schema_version != SCHEMA_VERSION
            || version.memory_id != assertion.memory_id
            || &assertion.version != version_hash
        {
            return Err(StoreError::InvalidMemoryProjection(
                "version and assertion identities do not agree".into(),
            ));
        }
        validate_keyed_project_memory_shape(version, assertion)?;

        let (scope_kind, project_id, task_id, work_id, agent_id) = match &version.scope {
            Scope::Project { project } => ("project", &project.0, None, None, None),
            Scope::Task { project, task } => {
                ("task", &project.0, Some(task.0.to_string()), None, None)
            }
            Scope::Work { project, work } => {
                ("work", &project.0, None, Some(work.0.to_string()), None)
            }
            Scope::Agent {
                project,
                task,
                work,
                agent,
            } => (
                "agent",
                &project.0,
                task.map(|value| value.0.to_string()),
                work.map(|value| value.0.to_string()),
                Some(agent.as_str()),
            ),
        };
        let changed = transaction.execute(
            "INSERT INTO memory_heads (
                 memory_id, version_hash, assertion_hash, schema_version,
                 status, scope_kind, project_id, task_id, work_id, agent_id,
                 memory_kind, authority, delivery, sensitivity, title, body,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17
             )
             ON CONFLICT(memory_id) DO UPDATE SET
                 version_hash = excluded.version_hash,
                 assertion_hash = excluded.assertion_hash,
                 schema_version = excluded.schema_version,
                 status = excluded.status,
                 scope_kind = excluded.scope_kind,
                 project_id = excluded.project_id,
                 task_id = excluded.task_id,
                 work_id = excluded.work_id,
                 agent_id = excluded.agent_id,
                 memory_kind = excluded.memory_kind,
                 authority = excluded.authority,
                 delivery = excluded.delivery,
                 sensitivity = excluded.sensitivity,
                 title = excluded.title,
                 body = excluded.body,
                 created_at_ms = excluded.created_at_ms
             WHERE memory_heads.status NOT IN ('retracted', 'expired', 'tombstoned')
                OR excluded.status IN ('retracted', 'expired', 'tombstoned')",
            params![
                version.memory_id.0.to_string(),
                version_hash.as_str(),
                assertion_hash.as_str(),
                i64::from(version.schema_version),
                enum_name(assertion.status)?,
                scope_kind,
                project_id,
                task_id,
                work_id,
                agent_id,
                enum_name(version.kind)?,
                enum_name(version.authority)?,
                enum_name(version.delivery)?,
                enum_name(version.sensitivity)?,
                version.title,
                version.body,
                version.created_at.timestamp_millis(),
            ],
        )?;
        if changed == 0 {
            if mode == MemoryProjectionMode::Replay {
                // Rebuild order is not a lifecycle clock, so an older live
                // assertion must never replace a terminal head merely because
                // its object hash sorts later.
                return Ok(());
            }
            return Err(StoreError::InvalidMemoryProjection(format!(
                "live assertion {assertion_hash} cannot replace terminal memory head {}",
                version.memory_id.0
            )));
        }
        transaction.execute(
            "DELETE FROM object_fts WHERE object_hash = ?1",
            [version_hash.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO object_fts (object_hash, title, body) VALUES (?1, ?2, ?3)",
            params![version_hash.as_str(), version.title, version.body],
        )?;
        Ok(())
    }
}
