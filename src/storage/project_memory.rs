use super::{
    ActorContext, Authority, CanonicalObject, Connection, Delivery, ForgetProjectMemoryRequest,
    MAX_CONTEXT_GENERATION_BYTES, MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT,
    MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES, MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES,
    MAX_PROJECT_MEMORY_BODY_BYTES, MAX_PROJECT_MEMORY_KEY_BYTES,
    MAX_PROJECT_MEMORY_PROVENANCE_LINKS, MemoryAssertionEvent, MemoryHeadProjectionRow, MemoryId,
    MemoryKind, MemoryProjectionMode, MemoryStatus, MemoryVersion, ObjectHash, OptionalExtension,
    PROJECT_MEMORY_FIRST_LINE_BYTES, PROJECT_MEMORY_LIST_LIMIT, PreparedProjectMemory,
    ProjectMemoryAdvertisement, ProjectMemoryFull, ProjectMemoryList, ProjectMemoryListRow,
    ProjectMemoryMutationReceipt, Redactor, RememberProjectMemoryRequest, SCHEMA_VERSION, Scope,
    Sensitivity, SessionId, SqliteStore, StoreError, StoredProjectMemory, TransactionBehavior,
    fts_query, normalize_project_memory_query, params,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Creates one attributed project episode or replays the identical create.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key, size,
    /// redaction, or create-only lifecycle admission fails.
    #[cfg(test)]
    pub fn remember_project_memory<R: Redactor>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        redactor: &R,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.remember_project_memory_with_admission(request, redactor, |_| Ok(()))
    }

    pub(crate) fn remember_project_memory_with_admission<R, A>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        redactor: &R,
        admit_full_response: A,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError>
    where
        R: Redactor,
        A: FnOnce(&ProjectMemoryFull) -> Result<(), StoreError>,
    {
        validate_project_memory_authorization(&request.session_id, &request.actor)?;
        let actor = validated_project_memory_actor(&request.actor, redactor)?;
        validate_project_memory_authorization(&request.session_id, &actor)?;
        if request.body.trim().is_empty() {
            return Err(StoreError::InvalidProjectMemory(
                "memory body must not be empty".into(),
            ));
        }
        if request.body.len() > MAX_PROJECT_MEMORY_BODY_BYTES {
            return Err(StoreError::InvalidProjectMemory(format!(
                "memory body exceeds {MAX_PROJECT_MEMORY_BODY_BYTES} UTF-8 bytes"
            )));
        }
        redactor
            .inspect(&request.body)
            .map_err(StoreError::RedactionRefused)?;
        let key = match request.key.as_deref() {
            Some(key) => validate_project_memory_key(key)?,
            None => slug_project_memory_key(&request.body)?,
        };
        let mut request = request.clone();
        request.actor = actor;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _ = project_memory_state_on(&transaction, &request.project_id)?;
        if let Some(existing) = lookup_project_memory_on(&transaction, &request.project_id, &key)? {
            return match existing.assertion.status {
                MemoryStatus::Tombstoned => Err(StoreError::ProjectMemoryRetired(key)),
                MemoryStatus::Active
                    if existing.version.body == request.body
                        && existing.version.actor.actor_id == request.actor.actor_id
                        && existing.version.actor.session_id == request.actor.session_id =>
                {
                    let stored_full = ProjectMemoryFull {
                        key: key.clone(),
                        body: existing.version.body.clone(),
                        remembered_at: existing.version.created_at,
                        actor_id: existing.version.actor.actor_id.clone(),
                        actor_context: existing
                            .version
                            .actor
                            .attribution_context()
                            .map(str::to_owned),
                        session_id: existing.version.actor.session_id.clone(),
                    };
                    admit_full_response(&stored_full)?;
                    Ok(ProjectMemoryMutationReceipt {
                        key,
                        remembered_at: existing.version.created_at,
                        forgotten_at: None,
                        duplicate: true,
                    })
                }
                MemoryStatus::Active => Err(StoreError::ProjectMemoryExists(key)),
                status => Err(StoreError::InvalidMemoryProjection(format!(
                    "project memory key has unsupported status {status:?}"
                ))),
            };
        }

        let full = ProjectMemoryFull {
            key: key.clone(),
            body: request.body.clone(),
            remembered_at: request.created_at,
            actor_id: request.actor.actor_id.clone(),
            actor_context: request.actor.attribution_context().map(str::to_owned),
            session_id: request.actor.session_id.clone(),
        };
        admit_full_response(&full)?;

        let prepared = prepare_project_memory(&request, &key)?;
        Self::insert_project_memory_version_object(
            &transaction,
            &prepared.version_object,
            &request.project_id,
            &key,
        )?;
        Self::insert_object(
            &transaction,
            "memory_assertion_event",
            &prepared.assertion_object,
        )?;
        Self::apply_memory_projection(
            &transaction,
            prepared.version_object.hash(),
            prepared.assertion_object.hash(),
            &prepared.version,
            &prepared.assertion,
            MemoryProjectionMode::Live,
        )?;
        advance_project_memory_state_on(&transaction, &request.project_id, 1)?;
        transaction.commit()?;
        Ok(ProjectMemoryMutationReceipt {
            key,
            remembered_at: request.created_at,
            forgotten_at: None,
            duplicate: false,
        })
    }

    /// Appends an attributed terminal tombstone for one project-memory key.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key
    /// resolution, or terminal lifecycle validation fails.
    pub fn forget_project_memory<R: Redactor>(
        &mut self,
        request: &ForgetProjectMemoryRequest,
        redactor: &R,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        validate_project_memory_authorization(&request.session_id, &request.actor)?;
        let actor = validated_project_memory_actor(&request.actor, redactor)?;
        validate_project_memory_authorization(&request.session_id, &actor)?;
        let key = validate_project_memory_key(&request.key)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _ = project_memory_state_on(&transaction, &request.project_id)?;
        let existing = lookup_project_memory_on(&transaction, &request.project_id, &key)?
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            return Ok(ProjectMemoryMutationReceipt {
                key,
                remembered_at: existing.version.created_at,
                forgotten_at: Some(existing.assertion.created_at),
                duplicate: true,
            });
        }
        if existing.assertion.status != MemoryStatus::Active {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "project memory key has unsupported status {:?}",
                existing.assertion.status
            )));
        }
        if request.created_at < existing.version.created_at {
            return Err(StoreError::InvalidProjectMemory(format!(
                "forget timestamp {} precedes the remembered timestamp {} for project memory {key}",
                request.created_at, existing.version.created_at
            )));
        }
        let assertion = MemoryAssertionEvent {
            schema_version: SCHEMA_VERSION,
            memory_id: existing.version.memory_id,
            version: existing.version_hash.clone(),
            status: MemoryStatus::Tombstoned,
            policy_reason: "explicit project-memory forget".into(),
            actor,
            created_at: request.created_at,
        };
        let assertion_object = CanonicalObject::freeze(&assertion)?;
        Self::insert_object(&transaction, "memory_assertion_event", &assertion_object)?;
        Self::apply_memory_projection(
            &transaction,
            &existing.version_hash,
            assertion_object.hash(),
            &existing.version,
            &assertion,
            MemoryProjectionMode::Live,
        )?;
        advance_project_memory_state_on(&transaction, &request.project_id, -1)?;
        transaction.commit()?;
        Ok(ProjectMemoryMutationReceipt {
            key,
            remembered_at: existing.version.created_at,
            forgotten_at: Some(request.created_at),
            duplicate: false,
        })
    }

    /// Returns a dedicated bounded full-read envelope for one live key.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key
    /// resolution, lifecycle, or stored-envelope validation fails.
    pub fn project_memory_full(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        key: &str,
    ) -> Result<ProjectMemoryFull, StoreError> {
        validate_project_memory_authorization(session_id, actor)?;
        let key = validate_project_memory_key(key)?;
        let existing = lookup_project_memory_on(&self.connection, project_id, &key)?
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            return Err(StoreError::ProjectMemoryRetired(key));
        }
        if existing.assertion.status != MemoryStatus::Active {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "project memory key has unsupported status {:?}",
                existing.assertion.status
            )));
        }
        let full = ProjectMemoryFull {
            key,
            body: existing.version.body,
            remembered_at: existing.version.created_at,
            actor_id: existing.version.actor.actor_id.clone(),
            actor_context: existing
                .version
                .actor
                .attribution_context()
                .map(str::to_owned),
            session_id: existing.version.actor.session_id,
        };
        Ok(full)
    }

    /// Lists live project memories without returning their bodies.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, cursor, or
    /// query validation fails, or when the stored projection is invalid.
    pub fn project_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        query: Option<&str>,
        after: Option<&str>,
    ) -> Result<ProjectMemoryList, StoreError> {
        validate_project_memory_authorization(session_id, actor)?;
        let normalized_query = normalize_project_memory_query(query)?;
        if normalized_query.is_some() && after.is_some() {
            return Err(StoreError::InvalidProjectMemory(
                "filtered memory search does not accept --after; refine the query instead".into(),
            ));
        }
        let normalized_after = after.map(validate_project_memory_key).transpose()?;
        let transaction = self.connection.unchecked_transaction()?;
        let (rows, total_matches) = project_memory_rows_on(
            &transaction,
            project_id,
            normalized_query,
            normalized_after.as_deref(),
            PROJECT_MEMORY_LIST_LIMIT + 1,
        )?;
        let has_more = rows.len() > PROJECT_MEMORY_LIST_LIMIT;
        let memories = rows
            .into_iter()
            .take(PROJECT_MEMORY_LIST_LIMIT)
            .collect::<Vec<_>>();
        let omitted_count = total_matches
            .unwrap_or(memories.len())
            .saturating_sub(memories.len());
        let next_after = if normalized_query.is_none() && has_more {
            memories.last().map(|row| row.key.clone())
        } else {
            None
        };
        let exhausted = if normalized_query.is_some() {
            omitted_count == 0
        } else {
            next_after.is_none()
        };
        transaction.commit()?;
        Ok(ProjectMemoryList {
            memories,
            next_after,
            omitted_count,
            exhausted,
        })
    }

    /// Returns and advances the advisory content-free memory signal for next.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when the context generation is
    /// invalid or the advisory projection cannot be read or updated.
    #[cfg(test)]
    pub(crate) fn project_memory_advertisement(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        context_generation: Option<&str>,
    ) -> Result<(usize, bool), StoreError> {
        let advertisement = self.project_memory_advertisement_candidate(
            project_id,
            session_id,
            context_generation,
        )?;
        let result = (advertisement.count, advertisement.changed);
        if advertisement.changed {
            self.acknowledge_project_memory_advertisement(project_id, session_id, &advertisement)?;
        }
        Ok(result)
    }

    pub(crate) fn project_memory_advertisement_candidate(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        context_generation: Option<&str>,
    ) -> Result<ProjectMemoryAdvertisement, StoreError> {
        if context_generation.is_some_and(|value| {
            value.len() > MAX_CONTEXT_GENERATION_BYTES || value.chars().any(char::is_control)
        }) {
            return Err(StoreError::InvalidProjectMemory(format!(
                "context_generation must be at most {MAX_CONTEXT_GENERATION_BYTES} bytes without control characters"
            )));
        }
        let transaction = self.connection.unchecked_transaction()?;
        let (count, change_position) = project_memory_state_on(&transaction, project_id)?;
        let context_generation_digest =
            context_generation.map(project_memory_context_generation_digest);
        let prior = transaction
            .query_row(
                "SELECT memory_position, context_generation_digest
                 FROM project_memory_advertisements
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        transaction.commit()?;
        let changed = prior
            .as_ref()
            .is_none_or(|(prior_position, prior_generation)| {
                *prior_position != change_position
                    || context_generation_digest
                        .as_deref()
                        .is_some_and(|digest| prior_generation.as_deref() != Some(digest))
            });
        Ok(ProjectMemoryAdvertisement {
            count,
            changed,
            change_position,
            context_generation_digest,
        })
    }

    pub(crate) fn acknowledge_project_memory_advertisement(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        advertisement: &ProjectMemoryAdvertisement,
    ) -> Result<(), StoreError> {
        if !advertisement.changed {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context_generation_digest =
            advertisement
                .context_generation_digest
                .clone()
                .or(transaction
                    .query_row(
                        "SELECT context_generation_digest FROM project_memory_advertisements
                     WHERE project_id = ?1 AND session_id = ?2",
                        params![project_id.0, session_id.0],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten());
        transaction.execute(
            "DELETE FROM project_memory_advertisements
             WHERE project_id = ?1
               AND session_id != ?2
               AND rowid NOT IN (
                   SELECT rowid FROM project_memory_advertisements
                   WHERE project_id = ?1 AND session_id != ?2
                   ORDER BY rowid DESC
                   LIMIT ?3
               )",
            params![
                project_id.0,
                session_id.0,
                MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT - 1
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO project_memory_advertisements (
                 project_id, session_id, context_generation_digest, memory_position
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                project_id.0,
                session_id.0,
                context_generation_digest,
                advertisement.change_position
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn bounded_project_memory_attribution_text(value: &str, label: &str) -> Result<String, StoreError> {
    if value.trim().is_empty() || value.len() > MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "{label} must contain from 1 through {MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES} UTF-8 bytes"
        )));
    }
    Ok(value.to_owned())
}

fn bounded_optional_project_memory_attribution_text(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, StoreError> {
    value
        .map(|value| bounded_project_memory_attribution_text(value, label))
        .transpose()
}

fn validated_project_memory_actor<R: Redactor>(
    actor: &ActorContext,
    redactor: &R,
) -> Result<ActorContext, StoreError> {
    validate_project_memory_actor_shape(actor)?;
    let validated = actor.clone();
    for prose in [
        Some(validated.actor_id.as_str()),
        Some(validated.actor_kind.as_str()),
        Some(validated.reason.as_str()),
        validated.run_id.as_deref(),
        validated
            .session_id
            .as_ref()
            .map(|session| session.0.as_str()),
        validated.source_tool.as_deref(),
        validated.source_skill.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        redactor
            .inspect(prose)
            .map_err(StoreError::RedactionRefused)?;
    }
    for link in &validated.provenance_chain {
        redactor
            .inspect(&link.source)
            .map_err(StoreError::RedactionRefused)?;
        if let Some(reference) = link.reference.as_deref() {
            redactor
                .inspect(reference)
                .map_err(StoreError::RedactionRefused)?;
        }
    }
    Ok(validated)
}

fn validate_project_memory_actor_shape(actor: &ActorContext) -> Result<(), StoreError> {
    actor.validate_attribution_context().map_err(|detail| {
        StoreError::InvalidProjectMemory(format!(
            "project-memory attribution has invalid actor context: {detail}"
        ))
    })?;
    if actor.provenance_chain.len() > MAX_PROJECT_MEMORY_PROVENANCE_LINKS {
        return Err(StoreError::InvalidProjectMemory(format!(
            "project-memory attribution must contain at most {MAX_PROJECT_MEMORY_PROVENANCE_LINKS} provenance links"
        )));
    }
    bounded_project_memory_attribution_text(&actor.actor_id, "project-memory actor")?;
    bounded_project_memory_attribution_text(&actor.actor_kind, "project-memory actor kind")?;
    bounded_project_memory_attribution_text(&actor.reason, "project-memory attribution reason")?;
    bounded_optional_project_memory_attribution_text(
        actor.run_id.as_deref(),
        "project-memory run",
    )?;
    let session = actor.session_id.as_ref().ok_or_else(|| {
        StoreError::InvalidProjectMemory(
            "project-memory attribution requires a nonblank session".into(),
        )
    })?;
    bounded_project_memory_attribution_text(&session.0, "project-memory session")?;
    bounded_optional_project_memory_attribution_text(
        actor.source_tool.as_deref(),
        "project-memory source tool",
    )?;
    bounded_optional_project_memory_attribution_text(
        actor.source_skill.as_deref(),
        "project-memory source skill",
    )?;
    for (index, link) in actor.provenance_chain.iter().enumerate() {
        bounded_project_memory_attribution_text(
            &link.source,
            &format!("project-memory provenance source {index}"),
        )?;
        bounded_optional_project_memory_attribution_text(
            link.reference.as_deref(),
            &format!("project-memory provenance reference {index}"),
        )?;
    }
    let canonical_candidate = CanonicalObject::freeze(actor)?;
    if canonical_candidate.bytes().len() > MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "project-memory attribution exceeds the {MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES}-byte canonical limit"
        )));
    }
    Ok(())
}

fn validate_project_memory_authorization(
    session_id: &SessionId,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    if actor.actor_id.trim().is_empty()
        || session_id.0.trim().is_empty()
        || actor
            .session_id
            .as_ref()
            .is_none_or(|value| value.0.trim().is_empty())
        || actor.session_id.as_ref() != Some(session_id)
    {
        return Err(StoreError::ProjectMemoryBindingInvalid);
    }
    Ok(())
}

fn project_memory_context_generation_digest(value: &str) -> String {
    const DOMAIN: &[u8] = b"engram-project-memory-context-generation-v1\0";
    let mut input = Vec::with_capacity(DOMAIN.len() + value.len());
    input.extend_from_slice(DOMAIN);
    input.extend_from_slice(value.as_bytes());
    let digest = <sha2::Sha256 as sha2::Digest>::digest(input);
    format!("{digest:x}")
}

fn prepare_project_memory(
    request: &RememberProjectMemoryRequest,
    key: &str,
) -> Result<PreparedProjectMemory, StoreError> {
    let memory_id = MemoryId::new();
    let version = MemoryVersion {
        schema_version: SCHEMA_VERSION,
        memory_id,
        project_key: Some(key.to_owned()),
        parents: Vec::new(),
        kind: MemoryKind::Episode,
        authority: Authority::Soft,
        delivery: Delivery::OnDemand,
        scope: Scope::Project {
            project: request.project_id.clone(),
        },
        title: format!("Project memory {key}"),
        body: request.body.clone(),
        structured_value: None,
        tags: vec!["project-memory".into()],
        evidence: Vec::new(),
        refs: Vec::new(),
        source_snapshot: None,
        confidence: None,
        sensitivity: Sensitivity::Internal,
        classification_reason: "explicit project episode".into(),
        delivery_override_reason: None,
        valid_from: None,
        valid_until: None,
        review_by: None,
        last_verified: None,
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let version_object = CanonicalObject::freeze(&version)?;
    let assertion = MemoryAssertionEvent {
        schema_version: SCHEMA_VERSION,
        memory_id,
        version: version_object.hash().clone(),
        status: MemoryStatus::Active,
        policy_reason: "project episodes are active immediately".into(),
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let assertion_object = CanonicalObject::freeze(&assertion)?;
    Ok(PreparedProjectMemory {
        version,
        assertion,
        version_object,
        assertion_object,
    })
}

fn validate_project_memory_key(value: &str) -> Result<String, StoreError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_PROJECT_MEMORY_KEY_BYTES
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit());
    let tail_valid = bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte));
    if !valid || !tail_valid {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory key must be 1-{MAX_PROJECT_MEMORY_KEY_BYTES} ASCII bytes matching [a-z0-9][a-z0-9._-]*"
        )));
    }
    Ok(value.to_owned())
}

fn slug_project_memory_key(body: &str) -> Result<String, StoreError> {
    let mut slug = String::new();
    let mut between_words = false;
    for byte in body.bytes() {
        if byte.is_ascii_alphanumeric() {
            if between_words && !slug.is_empty() && slug.len() < MAX_PROJECT_MEMORY_KEY_BYTES {
                slug.push('-');
            }
            if slug.len() >= MAX_PROJECT_MEMORY_KEY_BYTES {
                break;
            }
            slug.push(char::from(byte.to_ascii_lowercase()));
            between_words = false;
        } else if !slug.is_empty() {
            between_words = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(StoreError::InvalidProjectMemory(
            "memory body cannot produce a safe key; pass --key KEY".into(),
        ));
    }
    validate_project_memory_key(&slug)
}

pub(super) fn validate_keyed_project_memory_shape(
    version: &MemoryVersion,
    assertion: &MemoryAssertionEvent,
) -> Result<(), StoreError> {
    let Some(key) = version.project_key.as_deref() else {
        return Ok(());
    };
    let invalid = |detail: &str| {
        StoreError::InvalidMemoryProjection(format!(
            "keyed project memory has invalid canonical shape: {detail}"
        ))
    };
    validate_project_memory_key(key).map_err(|error| invalid(&error.to_string()))?;
    let Scope::Project { .. } = &version.scope else {
        return Err(invalid("project_key requires project scope"));
    };
    let restored = version.source_snapshot.as_ref().is_some_and(|source| {
        (source.source_ref == super::graph_snapshot::RESTORED_MEMORY_SOURCE
            || source.source_ref == super::graph_snapshot::RESTORED_REDACTED_MEMORY_SOURCE)
            && ObjectHash::from_stored(source.fingerprint.clone()).is_some()
    });
    if version.parents.is_empty()
        && version.kind == MemoryKind::Episode
        && version.authority == Authority::Soft
        && version.delivery == Delivery::OnDemand
        && version.title == format!("Project memory {key}")
        && !version.body.trim().is_empty()
        && version.body.len() <= MAX_PROJECT_MEMORY_BODY_BYTES
        && version.structured_value.is_none()
        && version.tags.len() == 1
        && version.tags[0] == "project-memory"
        && version.evidence.is_empty()
        && version.refs.is_empty()
        && (version.source_snapshot.is_none() || restored)
        && version.confidence.is_none()
        && (version.sensitivity == Sensitivity::Internal || restored)
        && version.classification_reason
            == if restored {
                "restored project episode"
            } else {
                "explicit project episode"
            }
        && version.delivery_override_reason.is_none()
        && version.valid_from.is_none()
        && version.valid_until.is_none()
        && version.review_by.is_none()
        && version.last_verified.is_none()
    {
        validate_project_memory_actor_shape(&version.actor)
            .map_err(|error| invalid(&error.to_string()))?;
        validate_project_memory_actor_shape(&assertion.actor)
            .map_err(|error| invalid(&error.to_string()))?;
    } else {
        return Err(invalid(
            "version fields do not match the fixed project-episode contract",
        ));
    }
    let lifecycle_matches = match assertion.status {
        MemoryStatus::Active => {
            assertion.policy_reason
                == if restored {
                    "restored project episode is active immediately"
                } else {
                    "project episodes are active immediately"
                }
                && assertion.actor == version.actor
                && assertion.created_at == version.created_at
        }
        MemoryStatus::Tombstoned => {
            (assertion.policy_reason == "explicit project-memory forget"
                || (restored && assertion.policy_reason == "restored project-memory tombstone"))
                && assertion.created_at >= version.created_at
        }
        _ => false,
    };
    if !lifecycle_matches {
        return Err(invalid(
            "assertion does not match the active-or-terminal project-memory lifecycle",
        ));
    }
    Ok(())
}

pub(super) fn lookup_project_memory_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Option<StoredProjectMemory>, StoreError> {
    // The hard index requirement makes a missing or incompatible rebuildable
    // projection fail closed; open/doctor names the explicit repair command.
    let stored = connection
        .query_row(
            "SELECT head.memory_id, head.version_hash, head.assertion_hash,
                    head.schema_version, head.status, head.scope_kind,
                    head.project_id, head.task_id, head.work_id, head.agent_id,
                    head.memory_kind, head.authority, head.delivery,
                    head.sensitivity, head.title, head.body, head.created_at_ms
             FROM objects AS object INDEXED BY objects_project_memory_key
             JOIN memory_heads AS head ON head.version_hash = object.object_hash
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_extract(object.canonical_json, '$.project_key') = ?2",
            params![project_id.0, key],
            |row| {
                Ok(MemoryHeadProjectionRow {
                    memory_id: row.get(0)?,
                    version_hash: row.get(1)?,
                    assertion_hash: row.get(2)?,
                    schema_version: row.get(3)?,
                    status: row.get(4)?,
                    scope_kind: row.get(5)?,
                    project_id: row.get(6)?,
                    task_id: row.get(7)?,
                    work_id: row.get(8)?,
                    agent_id: row.get(9)?,
                    memory_kind: row.get(10)?,
                    authority: row.get(11)?,
                    delivery: row.get(12)?,
                    sensitivity: row.get(13)?,
                    title: row.get(14)?,
                    body: row.get(15)?,
                    created_at_ms: row.get(16)?,
                })
            },
        )
        .optional()?;
    let Some(stored) = stored else {
        let reserved = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                   AND json_extract(canonical_json, '$.project_key') = ?2
             )",
            params![project_id.0, key],
            |row| row.get::<_, bool>(0),
        )?;
        if reserved {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory key is reserved but its durable head is missing".into(),
            ));
        }
        return Ok(None);
    };
    let version_hash = ObjectHash::from_stored(stored.version_hash.clone())
        .ok_or_else(|| StoreError::InvalidStoredHash(stored.version_hash.clone()))?;
    let assertion_hash = ObjectHash::from_stored(stored.assertion_hash.clone())
        .ok_or_else(|| StoreError::InvalidStoredHash(stored.assertion_hash.clone()))?;
    let version: MemoryVersion =
        SqliteStore::get_typed_object_on(connection, &version_hash, "memory_version")?.ok_or_else(
            || StoreError::InvalidMemoryProjection("project memory version is missing".into()),
        )?;
    let assertion: MemoryAssertionEvent =
        SqliteStore::get_typed_object_on(connection, &assertion_hash, "memory_assertion_event")?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection("project memory assertion is missing".into())
            })?;
    validate_keyed_project_memory_shape(&version, &assertion)?;
    let expected_status =
        SqliteStore::expected_memory_head_status_on(connection, &version_hash, assertion.status)?;
    let expected = SqliteStore::expected_memory_head_projection(
        &version_hash,
        &assertion_hash,
        &version,
        &assertion,
        expected_status,
    )?;
    let shape_matches = stored == expected
        && version.project_key.as_deref() == Some(key)
        && matches!(&version.scope, Scope::Project { project } if project == project_id)
        && version.kind == MemoryKind::Episode
        && version.authority == Authority::Soft
        && version.delivery == Delivery::OnDemand
        && assertion.memory_id == version.memory_id
        && assertion.version == version_hash;
    if !shape_matches {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory key projection does not match its canonical objects".into(),
        ));
    }
    Ok(Some(StoredProjectMemory {
        version_hash,
        version,
        assertion,
    }))
}

pub(in crate::storage) fn validate_stored_project_memory_key(
    key: &str,
) -> Result<String, StoreError> {
    validate_project_memory_key(key).map_err(|_| {
        StoreError::InvalidMemoryProjection(
            "project memory list candidate has an unsafe canonical key".into(),
        )
    })
}

fn project_memory_rows_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    query: Option<&str>,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<ProjectMemoryListRow>, Option<usize>), StoreError> {
    let limit = i64::try_from(limit)
        .map_err(|_| StoreError::InvalidProjectMemory("memory list limit is invalid".into()))?;
    let (keys, total_matches) = if let Some(query) = query {
        let lowered_key_query = query.to_ascii_lowercase();
        let escaped_key_query = lowered_key_query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let fts_query = fts_query(query);
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key'),
                    COUNT(*) OVER()
             FROM object_fts AS f
             JOIN memory_heads AS head ON head.version_hash = f.object_hash
             JOIN objects AS object ON object.object_hash = head.version_hash
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND object_fts MATCH ?2
             ORDER BY
                 CASE
                     WHEN json_extract(object.canonical_json, '$.project_key') = ?3 THEN 0
                     WHEN lower(json_extract(object.canonical_json, '$.project_key'))
                         LIKE ?4 || '%' ESCAPE '\\' THEN 1
                     ELSE 2
                 END,
                 f.rank,
                 json_extract(object.canonical_json, '$.project_key')
             LIMIT ?5",
        )?;
        let matches = statement
            .query_map(
                params![
                    project_id.0,
                    fts_query,
                    lowered_key_query,
                    escaped_key_query,
                    limit
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let total = matches
            .first()
            .map_or(Ok(0), |(_, total)| usize::try_from(*total))
            .map_err(|_| {
                StoreError::InvalidMemoryProjection("memory match count is invalid".into())
            })?;
        (
            matches.into_iter().map(|(key, _)| key).collect(),
            Some(total),
        )
    } else {
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key')
             FROM memory_heads AS head
             JOIN objects AS object ON object.object_hash = head.version_hash
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND (?2 IS NULL OR json_extract(object.canonical_json, '$.project_key') > ?2)
             ORDER BY json_extract(object.canonical_json, '$.project_key')
             LIMIT ?3",
        )?;
        (
            statement
                .query_map(params![project_id.0, after, limit], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?,
            None,
        )
    };
    let rows = keys
        .into_iter()
        .map(|key| {
            let key = validate_stored_project_memory_key(&key)?;
            let stored =
                lookup_project_memory_on(connection, project_id, &key)?.ok_or_else(|| {
                    StoreError::InvalidMemoryProjection(
                        "project memory list candidate has no canonical binding".into(),
                    )
                })?;
            if stored.assertion.status != MemoryStatus::Active {
                return Err(StoreError::InvalidMemoryProjection(
                    "project memory list candidate is not active".into(),
                ));
            }
            Ok(project_memory_list_row(key, &stored))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok((rows, total_matches))
}

fn project_memory_list_row(key: String, stored: &StoredProjectMemory) -> ProjectMemoryListRow {
    ProjectMemoryListRow {
        key,
        first_line: project_memory_first_line(&stored.version.body),
        remembered_at: stored.version.created_at,
        actor_id: stored.version.actor.actor_id.clone(),
        actor_context: stored
            .version
            .actor
            .attribution_context()
            .map(str::to_owned),
    }
}

pub(super) fn project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(usize, i64), StoreError> {
    let state = connection
        .query_row(
            "SELECT active_count, change_position
             FROM project_memory_state WHERE project_id = ?1",
            [project_id.0.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let state = if let Some(state) = state {
        state
    } else {
        let has_project_memory = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                 LIMIT 1
             )",
            [project_id.0.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if has_project_memory {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory state is missing for a retained project key".into(),
            ));
        }
        (0, 0)
    };
    let count = usize::try_from(state.0).map_err(|_| {
        StoreError::InvalidMemoryProjection("project memory count is invalid".into())
    })?;
    Ok((count, state.1))
}

fn advance_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    active_delta: i64,
) -> Result<(), StoreError> {
    let changed = if active_delta == 1 {
        connection.execute(
            "INSERT INTO project_memory_state (
                 project_id, active_count, change_position
             ) VALUES (?1, 1, 1)
             ON CONFLICT(project_id) DO UPDATE SET
                 active_count = project_memory_state.active_count + 1,
                 change_position = project_memory_state.change_position + 1",
            [project_id.0.as_str()],
        )?
    } else if active_delta == -1 {
        connection.execute(
            "UPDATE project_memory_state
             SET active_count = active_count - 1,
                 change_position = change_position + 1
             WHERE project_id = ?1 AND active_count > 0",
            [project_id.0.as_str()],
        )?
    } else {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state delta must be exactly one".into(),
        ));
    };
    if changed != 1 {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state is missing or inconsistent".into(),
        ));
    }
    Ok(())
}

pub(super) fn derived_project_memory_state_rows_on(
    connection: &Connection,
) -> Result<Vec<(String, i64, i64)>, StoreError> {
    Ok(connection
        .prepare(
            "WITH assertion_counts AS (
                 SELECT json_extract(canonical_json, '$.version') AS version_hash,
                        COUNT(*) AS assertion_count
                 FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 GROUP BY version_hash
             )
             SELECT json_extract(version.canonical_json, '$.scope.project') AS project_id,
                    SUM(CASE WHEN head.status = 'active' THEN 1 ELSE 0 END),
                    SUM(COALESCE(assertion_counts.assertion_count, 0))
             FROM objects AS version
             JOIN memory_heads AS head ON head.version_hash = version.object_hash
             LEFT JOIN assertion_counts ON assertion_counts.version_hash = version.object_hash
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
             GROUP BY project_id ORDER BY project_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

pub(super) fn derived_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(i64, i64), StoreError> {
    let heads = connection
        .prepare(
            "SELECT head.memory_id, head.status, head.version_hash
             FROM objects AS version INDEXED BY objects_project_memory_key
             JOIN memory_heads AS head ON head.version_hash = version.object_hash
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
               AND json_extract(version.canonical_json, '$.scope.project') = ?1
             ORDER BY json_extract(version.canonical_json, '$.project_key')",
        )?
        .query_map([project_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut active_count = 0_i64;
    let mut change_position = 0_i64;
    let mut assertion_statement = connection.prepare(
        "SELECT object_hash
         FROM objects INDEXED BY objects_memory_assertion_version
         WHERE object_kind = 'memory_assertion_event'
           AND json_extract(canonical_json, '$.version') = ?1
         ORDER BY object_hash",
    )?;
    for (memory_id, status, version_hash) in heads {
        active_count = active_count
            .checked_add(i64::from(status == "active"))
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory active count exceeds SQLite range".into(),
                )
            })?;
        let assertion_hashes = assertion_statement
            .query_map([version_hash.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in assertion_hashes {
            let assertion_hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
                connection,
                &assertion_hash,
                "memory_assertion_event",
            )?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_hash} is missing"
                ))
            })?;
            if assertion.schema_version != SCHEMA_VERSION
                || assertion.memory_id.0.to_string() != memory_id
                || assertion.version.as_str() != version_hash
            {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_hash} disagrees with its head"
                )));
            }
            change_position = change_position.checked_add(1).ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory change position exceeds SQLite range".into(),
                )
            })?;
        }
    }
    Ok((active_count, change_position))
}

fn project_memory_first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.len() <= PROJECT_MEMORY_FIRST_LINE_BYTES {
        return line;
    }
    let mut end = PROJECT_MEMORY_FIRST_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
}
