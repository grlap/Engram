use super::{
    Connection, MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS, MemorySummary,
    OptionalExtension, SessionId, SqliteStore, StoreError, params, work,
};

#[cfg(test)]
use super::{
    CanonicalObject, HashMap, MemoryAssertionEvent, MemoryProjectionMode, MemoryVersion, ObjectId,
    SCHEMA_VERSION, Scope, TransactionBehavior,
};

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Returns current memories bound to one local work item and visible to
    /// the requesting actor. Shared work memories are visible to every actor
    /// focused on the item; agent-scoped work memories remain private.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work belongs to another project, a
    /// canonical projection is invalid, or SQLite cannot perform the query.
    pub fn search_work_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        work_id: crate::domain::WorkId,
        session_id: &SessionId,
        agent_id: &str,
        query: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<MemorySummary>, StoreError> {
        let read_guard = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let transaction = &self.connection;
        let (focused_work_id, _) =
            Self::focused_work_for_session_on(transaction, project_id, session_id)?;
        if focused_work_id != Some(work_id) {
            return Err(StoreError::InvalidWork(
                "work-memory query must match the session's persisted focus".into(),
            ));
        }
        let (work_project, work_root_id) = work::verified_work_identity(transaction, work_id)?;
        if work_project != *project_id {
            return Err(StoreError::InvalidWork(
                "work-memory query must stay within the bound project".into(),
            ));
        }
        let visibility = "h.project_id = ?1 AND h.work_id = ?2 AND
             h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'stale') AND
             (h.scope_kind = 'agent' AND h.agent_id = ?3)";
        let root_visibility = "h.project_id = ?1 AND
             h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'stale') AND
             h.scope_kind = 'work' AND h.work_id IN (
                 SELECT item.work_id FROM work_items item
                 WHERE item.project_id = ?1 AND item.root_id = ?4
             )";
        let visibility = format!("(({visibility}) OR ({root_visibility}))");
        let limit = limit.map_or(i64::MAX, |limit| i64::from(limit.clamp(1, 1_000)));
        let search = query
            .filter(|value| !value.trim().is_empty())
            .map(fts_query)
            .transpose()?;
        let rows = match search {
            // A query with no searchable fragment finds nothing.
            Some(None) => Vec::new(),
            Some(Some(fts_query)) => {
                let sql = format!(
                    "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_id = f.object_id
                 WHERE {visibility} AND object_fts MATCH ?5
                 ORDER BY bm25(object_fts), h.created_at_ms DESC LIMIT ?6"
                );
                let mut statement = transaction.prepare(&sql)?;
                let mapped = statement.query_map(
                    params![
                        project_id.0,
                        work_id.0.to_string(),
                        agent_id,
                        work_root_id.0.to_string(),
                        fts_query,
                        limit
                    ],
                    Self::decode_memory_summary,
                )?;
                mapped.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let sql = format!(
                    "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM memory_heads h WHERE {visibility}
                 ORDER BY h.created_at_ms DESC, h.memory_id LIMIT ?5"
                );
                let mut statement = transaction.prepare(&sql)?;
                let mapped = statement.query_map(
                    params![
                        project_id.0,
                        work_id.0.to_string(),
                        agent_id,
                        work_root_id.0.to_string(),
                        limit
                    ],
                    Self::decode_memory_summary,
                )?;
                mapped.collect::<Result<Vec<_>, _>>()?
            }
        };
        let memories = rows
            .into_iter()
            .map(Self::parse_memory_summary)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(read_guard) = read_guard {
            read_guard.commit()?;
        }
        Ok(memories)
    }

    /// Rebuilds all disposable memory projections from verified canonical
    /// assertion and version objects. Unsupported schemas remain stored but
    /// are intentionally not activated.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical objects fail verification or the
    /// derived tables cannot be replaced atomically.
    #[cfg(test)]
    pub(super) fn rebuild_memory_index(&mut self) -> Result<usize, StoreError> {
        let assertions = {
            let mut statement = self.connection.prepare(
                "SELECT object_id, canonical_json FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 ORDER BY created_at, object_id",
            )?;
            let mapped = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM memory_heads", [])?;
        transaction.execute("DELETE FROM object_fts", [])?;
        let mut activated = 0;
        // Canonical objects do not change during this rebuild transaction.
        // Validate each complete keyed chain once before selecting its head;
        // ordinary per-assertion validation below still applies to every row.
        let mut project_heads = HashMap::new();
        for (stored_hash, bytes) in assertions {
            let assertion_id = ObjectId::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
            let assertion_object = CanonicalObject::stored(&assertion_id, bytes)?;
            let value: serde_json::Value = serde_json::from_slice(assertion_object.bytes())?;
            if value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let assertion: MemoryAssertionEvent = assertion_object.decode()?;
            let version_bytes: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT canonical_json FROM objects
                     WHERE object_id = ?1 AND object_kind = 'memory_version'",
                    [assertion.version.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(version_bytes) = version_bytes else {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "assertion {assertion_id} references missing version {}",
                    assertion.version
                )));
            };
            let version_object = CanonicalObject::stored(&assertion.version, version_bytes)?;
            let version_value: serde_json::Value = serde_json::from_slice(version_object.bytes())?;
            if version_value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let version: MemoryVersion = version_object.decode()?;
            if let (Some(key), Scope::Project { project }) = (&version.project_key, &version.scope)
            {
                let identity = (project.clone(), key.clone());
                if !project_heads.contains_key(&identity) {
                    let history = super::project_memory::project_memory_history_on(
                        &transaction,
                        project,
                        key,
                    )?;
                    let head = history.last().ok_or_else(|| {
                        StoreError::InvalidMemoryProjection(
                            "rebuilt project memory has no canonical head".into(),
                        )
                    })?;
                    project_heads.insert(identity.clone(), head.version_id.clone());
                }
                if project_heads.get(&identity) != Some(&assertion.version) {
                    activated += 1;
                    continue;
                }
            }
            Self::apply_memory_projection(
                &transaction,
                &assertion.version,
                &assertion_id,
                &version,
                &assertion,
                MemoryProjectionMode::Replay,
            )?;
            activated += 1;
        }
        Self::rebuild_object_fts_from_heads_on(&transaction)?;
        Self::rebuild_project_memory_state_on(&transaction)?;
        transaction.commit()?;
        Ok(activated)
    }

    pub(super) fn focused_work_for_session_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<(Option<crate::domain::WorkId>, Option<crate::domain::WorkId>), StoreError> {
        let stored = connection
            .query_row(
                "SELECT focused_work_id FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(stored) = stored else {
            return Ok((None, None));
        };
        let work_id = uuid::Uuid::parse_str(&stored)
            .map(crate::domain::WorkId)
            .map_err(|_| {
                StoreError::InvalidWorkProjection(format!(
                    "work session focus contains invalid work id {stored}"
                ))
            })?;
        let (work_project, root_id) = work::verified_work_identity(connection, work_id)?;
        if work_project != *project_id {
            return Err(StoreError::InvalidWorkProjection(
                "focused work crosses its session project binding".into(),
            ));
        }
        Ok((Some(work_id), Some(root_id)))
    }
}

/// The full-text query for `query`: every search fragment as a quoted prefix
/// term, all of them required. `None` when `query` holds no fragment at all,
/// so the caller finds nothing rather than matching some stand-in phrase.
pub(super) fn fts_query(query: &str) -> Result<Option<String>, StoreError> {
    let tokens: Vec<_> = fts_tokens(query)?
        .into_iter()
        .map(|token| format!("\"{token}\"*"))
        .collect();
    Ok((!tokens.is_empty()).then(|| tokens.join(" AND ")))
}

/// Search fragments of `query`, split where the full-text tokenizer that
/// indexed the memories splits text, so a word holding a combining mark or a
/// private-use character stays one fragment. An underscore stays inside a
/// fragment too, so `engram_check` remains one quoted phrase. A fragment the
/// tokenizer reads no token from, such as a lone `_` or a mark on its own, is
/// dropped: a query term made of it could never match.
///
/// ASCII letters and digits are token characters and other ASCII characters
/// separators; every other character is classified by the tokenizer itself,
/// in a private in-memory table, never by an approximation of its rules.
fn fts_tokens(query: &str) -> Result<Vec<&str>, StoreError> {
    let mut wider: Vec<char> = query.chars().filter(|ch| !ch.is_ascii()).collect();
    wider.sort_unstable();
    wider.dedup();
    let tokenizer = (!wider.is_empty()).then(QueryTokenizer::open).transpose()?;
    let token_characters = match &tokenizer {
        Some(tokenizer) => tokenizer.token_characters(&wider)?,
        None => Vec::new(),
    };
    let fragments: Vec<&str> = query
        .split(|ch: char| {
            !(ch.is_ascii_alphanumeric()
                || ch == '_'
                || token_characters.binary_search(&ch).is_ok())
        })
        .filter(|fragment| !fragment.is_empty())
        .collect();
    let Some(tokenizer) = tokenizer else {
        return Ok(fragments
            .into_iter()
            .filter(|fragment| fragment.chars().any(|ch| ch.is_ascii_alphanumeric()))
            .collect());
    };
    let yields = tokenizer.yields_tokens(&fragments)?;
    Ok(fragments
        .into_iter()
        .zip(yields)
        .filter_map(|(fragment, yields)| yields.then_some(fragment))
        .collect())
}

/// A private in-memory full-text table with the memory index's tokenizer,
/// opened for one query and dropped with it.
struct QueryTokenizer(Connection);

impl QueryTokenizer {
    /// The table names no tokenizer, as the memory index does not, so both
    /// tokenize with the same default.
    fn open() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(text);
             CREATE VIRTUAL TABLE probe_tokens USING fts5vocab(probe, 'instance');",
        )?;
        Ok(Self(connection))
    }

    /// How many tokens the tokenizer reads from each of `texts`, in order.
    fn token_counts(&self, texts: &[String]) -> Result<Vec<usize>, StoreError> {
        self.0.execute("DELETE FROM probe", [])?;
        let mut insert = self
            .0
            .prepare("INSERT INTO probe (rowid, text) VALUES (?1, ?2)")?;
        for (row, text) in texts.iter().enumerate() {
            insert.execute(params![i64::try_from(row).unwrap_or(i64::MAX), text])?;
        }
        let mut counts = vec![0; texts.len()];
        let mut statement = self
            .0
            .prepare("SELECT doc, COUNT(*) FROM probe_tokens GROUP BY doc")?;
        let rows =
            statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (doc, count) = row?;
            if let (Ok(doc), Ok(count)) = (usize::try_from(doc), usize::try_from(count))
                && let Some(slot) = counts.get_mut(doc)
            {
                *slot = count;
            }
        }
        Ok(counts)
    }

    /// The sorted characters of `characters` that the tokenizer keeps inside
    /// a token: placed between two letters, such a character leaves one
    /// token where a separator would leave two.
    fn token_characters(&self, characters: &[char]) -> Result<Vec<char>, StoreError> {
        let probes: Vec<String> = characters.iter().map(|ch| format!("a{ch}a")).collect();
        Ok(characters
            .iter()
            .zip(self.token_counts(&probes)?)
            .filter_map(|(ch, count)| (count == 1).then_some(*ch))
            .collect())
    }

    /// Whether the tokenizer reads at least one token from each fragment.
    fn yields_tokens(&self, fragments: &[&str]) -> Result<Vec<bool>, StoreError> {
        let texts: Vec<String> = fragments
            .iter()
            .map(|fragment| (*fragment).to_owned())
            .collect();
        Ok(self
            .token_counts(&texts)?
            .into_iter()
            .map(|count| count > 0)
            .collect())
    }
}

pub(super) fn normalize_project_memory_query(
    query: Option<&str>,
) -> Result<Option<&str>, StoreError> {
    let Some(raw) = query else {
        return Ok(None);
    };
    if raw.len() > MAX_PROJECT_MEMORY_QUERY_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory query exceeds {MAX_PROJECT_MEMORY_QUERY_BYTES} UTF-8 bytes"
        )));
    }
    let query = raw.trim();
    if query.is_empty() {
        return Ok(None);
    }
    if fts_tokens(query)?.len() > MAX_PROJECT_MEMORY_QUERY_TOKENS {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory query exceeds {MAX_PROJECT_MEMORY_QUERY_TOKENS} search tokens"
        )));
    }
    Ok(Some(query))
}
