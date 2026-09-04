use super::{
    ActorContext, AssuranceLevel, BUILTIN_CONTROL_GRANT_TTL_SECONDS, BackupManifest,
    CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
    CONTROL_POLICY_STATE_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CORE_REBUILDABLE_SCHEMA_OBJECTS,
    CanonicalObject, Connection, ControlAssurance, ControlPolicy, ControlPolicyRecoveryFinding,
    ControlPolicyRecoveryReport, Duration, EffectClass, HashMap, HashSet, HostPathPolicy,
    InitialControlPolicy, IntegrityReport, MAX_CONTROL_POLICY_AUTHORITY_BYTES,
    MemoryAssertionEvent, MemoryHeadProjectionRow, MemoryStatus, MemoryVersion,
    OBLIGATION_RULE_SET_SCHEMA_VERSION, ObjectHash, ObligationRuleSet, OpenWriteNeed,
    OptionalExtension, Path, ProjectPolicyAuthorityDecision, ProjectPolicyEpoch,
    ProjectPolicyOperation, Redactor, SCHEMA_VERSION, SchemaDurability, SchemaOwner, Scope,
    SqliteStore, StoreError, TransactionBehavior, Utc, current_schema_definition_issue,
    derived_project_memory_state_rows_on, describe_host_path_policy, different_build_store_error,
    drop_schema_object, enum_name, fts_query, immutable_uri, normalize_control_policy_actor,
    normalize_control_text, params, parse_enum, publish_without_replacing, remove_store_files,
    require_current_schema_marker, store_sidecars, unique_sibling_path,
    validate_keyed_project_memory_shape, work,
};

#[cfg(test)]
use super::{building_schema_reference, fail_cold_schema_after_ddl};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Opens or creates a local database and applies idempotent schema setup
    /// under the running target's conservative path policy. Embedding hosts
    /// and the CLI should prefer [`Self::open_with_host_path_identity`] with
    /// the project root's probed or host-supplied identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, Some(HostPathPolicy::host_default()))
    }

    /// Opens a local database without asserting the project root's filesystem
    /// identity: work and memory operations proceed against any persisted
    /// policy, and path-bearing leases fail closed. Agent-facing services that
    /// never lease paths open this way so they cannot disagree with the
    /// resolved policy the host bound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store.
    pub fn open_unresolved(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, None)
    }

    /// Inspects only the control-policy family through a read-only connection.
    ///
    /// This entry point intentionally returns a report rather than a
    /// [`SqliteStore`]. It cannot enable MCP, control, work, grants, schema
    /// initialization, or any other mutation surface, and it never chooses or
    /// rewrites an active policy. Ordinary store open remains fail-closed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the database cannot be opened read-only or
    /// SQLite cannot enumerate its schema.
    pub fn diagnose_control_policy_recovery(
        path: &Path,
    ) -> Result<ControlPolicyRecoveryReport, StoreError> {
        if !path.is_file() {
            return Err(StoreError::InvalidControlProjection(format!(
                "control-policy recovery target {} is not an existing file",
                path.display()
            )));
        }
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "query_only", true)?;
        Self::diagnose_control_policy_records_on(&connection)
    }

    /// Rebuilds only declared indexes, triggers, and full-text projections.
    ///
    /// Ordinary open never invokes this path. The existing store must already
    /// have the exact current durable schema and policy bindings before the
    /// single repair transaction begins. Full integrity verification follows
    /// the rebuild and never rewrites durable state.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the target is absent, durable state is not
    /// current and valid, repair cannot commit, or post-repair verification
    /// fails.
    pub fn repair_rebuildable_projections(path: &Path) -> Result<IntegrityReport, StoreError> {
        if !path.is_file() {
            return Err(StoreError::InvalidControlProjection(format!(
                "projection-repair target {} is not an existing file",
                path.display()
            )));
        }
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;",
        )?;
        if let Some(issue) = Self::current_core_durable_schema_issue(&connection)? {
            return Err(StoreError::InvalidControlProjection(format!(
                "projection repair refused because durable state is invalid: {issue}"
            )));
        }
        work::preflight_schema(&connection, false)?;
        Self::require_task_local_cursor_schema(&connection)?;
        Self::preflight_host_path_policy(&connection, None)?;
        Self::preflight_control_policy_schema(&connection)?;
        Self::require_current_contradiction_edges(&connection)?;

        let work_schema_version = work::schema_version(&connection)?;
        // Keep repair and exhaustive verification in one writer transaction.
        // Owning the connection through `SqliteStore` lets `verify_all` reuse
        // the active snapshot; any error drops the connection and rolls back.
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        Self::repair_core_rebuildable_schema_on(&connection)?;
        work::repair_rebuildable_schema_on(&connection)?;
        let store = Self {
            connection,
            work_schema_version,
            host_path_policy: None,
        };
        let after = store.verify_all()?;
        if !after.is_healthy() {
            return Err(StoreError::InvalidControlProjection(format!(
                "projection repair refused because verification found {} invalid object(s), {} invalid control record(s), and {} invalid work record(s)",
                after.invalid_objects.len(),
                after.invalid_control_records.len(),
                after.invalid_work_records.len()
            )));
        }
        store.connection.execute_batch("COMMIT;")?;
        Ok(after)
    }

    /// Writes a consistent copy of this store to `path` through SQLite's own
    /// online backup (`VACUUM INTO`), then opens the copy and verifies every
    /// immutable object and hash-bound record in it. The copy is a full store:
    /// it carries host-private state and private scratch and must be kept where the
    /// store itself may be kept.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the copy cannot be written, opened, or
    /// fails verification; a failed copy is removed.
    pub fn backup_to(&self, path: &Path) -> Result<BackupManifest, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                StoreError::InvalidWork(format!(
                    "cannot create backup directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        // The copy is written to a path only this invocation knows, verified
        // and hashed there, and then published under the requested name
        // without ever replacing an existing file.
        let staged = unique_sibling_path(path, "backup");
        let target = staged.to_string_lossy().into_owned();
        if let Err(error) = self.connection.execute("VACUUM INTO ?1", [&target]) {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        // The staged copy is ours: one ordinary open settles its journal mode
        // so the copy verifies and restores through read-only opens later.
        // Closing that connection folds and removes its log; any sidecar left
        // behind is empty and must not travel with the copy.
        if let Err(error) = Self::open_with_host_path_identity(&staged, None) {
            let _ = remove_store_files(&staged);
            return Err(error);
        }
        for sidecar in store_sidecars(&staged) {
            if sidecar.exists()
                && let Err(error) = std::fs::remove_file(&sidecar)
            {
                let _ = remove_store_files(&staged);
                return Err(StoreError::InvalidWork(format!(
                    "cannot remove {}: {error}",
                    sidecar.display()
                )));
            }
        }
        let manifest = match Self::verify_backup(&staged) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = std::fs::remove_file(&staged);
                return Err(error);
            }
        };
        if let Err(error) = publish_without_replacing(&staged, path) {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        Ok(BackupManifest {
            path: path.to_path_buf(),
            ..manifest
        })
    }

    /// Verifies an existing backup file without creating, transforming, or
    /// modifying anything. The bytes are hashed first, then the file is
    /// opened read-only and every immutable object and hash-bound record is
    /// checked. Only the current store schema is accepted.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the path is not an existing regular file,
    /// cannot be opened read-only as a current store, or fails verification.
    pub fn verify_backup(path: &Path) -> Result<BackupManifest, StoreError> {
        if !path.is_file() {
            return Err(StoreError::InvalidWork(format!(
                "backup {} is not an existing file",
                path.display()
            )));
        }
        // A backup is one self-contained file. Log sidecars beside it mean it
        // was opened read-write after it was written, so its main file may
        // not hold everything; refuse rather than verify a stale picture.
        for sidecar in store_sidecars(path) {
            if sidecar.exists() {
                return Err(StoreError::InvalidWork(format!(
                    "backup {} has a log sidecar {}; it was opened after it was written",
                    path.display(),
                    sidecar.display()
                )));
            }
        }
        let bytes = std::fs::read(path).map_err(|error| {
            StoreError::InvalidWork(format!("cannot read backup {}: {error}", path.display()))
        })?;
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
        // `immutable=1` reads exactly the hashed bytes: no shared-memory or log
        // file is consulted or created, so a read-only directory works too.
        let immutable = || {
            Connection::open_with_flags(
                immutable_uri(path)?,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(StoreError::from)
        };
        let store = Self::from_connection(immutable()?, None, None)?;
        let report = store.verify_all()?;
        if !report.is_healthy() {
            return Err(StoreError::InvalidWork(format!(
                "backup {} failed verification: {} object(s), {} control record(s), {} work record(s) invalid",
                path.display(),
                report.invalid_objects.len(),
                report.invalid_control_records.len(),
                report.invalid_work_records.len()
            )));
        }
        Ok(BackupManifest {
            path: path.to_path_buf(),
            file_sha256: format!("{digest:x}"),
            file_bytes: bytes.len() as u64,
            checked_objects: report.checked_objects,
            checked_control_records: report.checked_control_records,
            checked_work_records: report.checked_work_records,
            created_at: Utc::now(),
        })
    }

    /// Opens or creates a local database with the project root's resolved
    /// filesystem identity, probed or host-supplied, or `None` when it could
    /// not be resolved. The first resolved opener persists the policy; later
    /// resolved openers must present the same one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store
    /// or the persisted policy differs from the resolved one.
    pub fn open_with_host_path_identity(
        path: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, identity, None)
    }

    /// The filesystem identity this opener resolved, if any.
    #[must_use]
    pub const fn host_path_identity(&self) -> Option<HostPathPolicy> {
        self.host_path_policy
    }

    /// Active local-work projection schema verified when this store opened.
    #[must_use]
    pub const fn work_schema_version(&self) -> i64 {
        self.work_schema_version
    }

    /// Reads the policy persisted by the first resolved opener, if any.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the policy row cannot be read.
    pub fn stored_host_path_policy(&self) -> Result<Option<HostPathPolicy>, StoreError> {
        Self::stored_host_path_policy_on(&self.connection)
    }

    fn stored_host_path_policy_on(
        connection: &Connection,
    ) -> Result<Option<HostPathPolicy>, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_host_path_policy")? {
            return Ok(None);
        }
        Ok(connection
            .query_row(
                "SELECT case_fold_paths, windows_alias_rules
                 FROM control_host_path_policy WHERE singleton = 1",
                [],
                |row| {
                    Ok(HostPathPolicy {
                        case_fold_paths: row.get::<_, bool>(0)?,
                        windows_alias_rules: row.get::<_, bool>(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// The policy that normalizes one lease subject: logical subjects never
    /// need one, path subjects need the resolved identity.
    pub(super) fn path_policy_for(
        &self,
        subject: &crate::domain::ResourceSubject,
    ) -> Result<HostPathPolicy, StoreError> {
        match subject {
            crate::domain::ResourceSubject::Logical { .. } => Ok(HostPathPolicy {
                case_fold_paths: false,
                windows_alias_rules: false,
            }),
            crate::domain::ResourceSubject::Path { .. } => self
                .host_path_policy
                .ok_or(StoreError::HostPathIdentityUnresolved),
        }
    }

    /// Creates a store with an explicit bootstrap control-assurance requirement.
    ///
    /// The requested value and asserted operator context apply only while
    /// installing the first policy. Reconfiguring an existing store requires
    /// an attributed policy update.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when initialization fails or an existing policy
    /// has a different assurance requirement.
    pub fn open_with_initial_control_assurance<R: Redactor>(
        path: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
        required_assurance: ControlAssurance,
        authorized_by: &ActorContext,
        reason: &str,
        redactor: &R,
    ) -> Result<Self, StoreError> {
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy bootstrap records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let reason = normalize_control_text(reason, "control policy bootstrap reason")?;
        redactor
            .inspect(&reason)
            .map_err(StoreError::RedactionRefused)?;
        let connection = Connection::open(path)?;
        Self::from_connection(
            connection,
            identity,
            Some(InitialControlPolicy {
                required_assurance,
                authorized_by,
                reason,
            }),
        )
    }

    /// Opens a store with an explicit embedding-host filesystem identity policy.
    ///
    /// The first opener persists the policy. Later openers must present the
    /// same policy so resource lease identities cannot drift between hosts.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when initialization fails or the stored policy differs.
    pub fn open_with_host_path_policy(
        path: impl AsRef<Path>,
        policy: HostPathPolicy,
    ) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, Some(policy))
    }

    /// Creates an isolated store for tests or ephemeral runs under the running
    /// target's conservative path policy.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot initialize the schema.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_in_memory_with_host_path_identity(Some(HostPathPolicy::host_default()))
    }

    /// Creates an isolated store with an explicit or unresolved filesystem
    /// identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot initialize the schema.
    pub fn open_in_memory_with_host_path_identity(
        identity: Option<HostPathPolicy>,
    ) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?, identity, None)
    }

    fn from_connection(
        connection: Connection,
        host_path_policy: Option<HostPathPolicy>,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<Self, StoreError> {
        Self::from_connection_with_busy_timeout(
            connection,
            host_path_policy,
            initial_control_policy,
            Duration::from_secs(5),
        )
    }

    #[allow(
        clippy::if_not_else,
        clippy::too_many_lines,
        reason = "the cold-schema branch stays adjacent to the complete idempotent DDL for auditability"
    )]
    fn from_connection_with_busy_timeout(
        mut connection: Connection,
        host_path_policy: Option<HostPathPolicy>,
        initial_control_policy: Option<InitialControlPolicy>,
        busy_timeout: Duration,
    ) -> Result<Self, StoreError> {
        connection.busy_timeout(busy_timeout)?;
        let store_has_schema = Self::sqlite_user_schema_exists(&connection)?;
        let core_store_exists = Self::sqlite_table_exists(&connection, "objects")?;
        if store_has_schema && !core_store_exists {
            return Err(different_build_store_error());
        }
        if core_store_exists && Self::current_core_durable_schema_issue(&connection)?.is_some() {
            return Err(different_build_store_error());
        }
        let allow_initialization = !store_has_schema;
        work::preflight_schema(&connection, allow_initialization)?;
        if Self::sqlite_table_exists(&connection, "task_changes")? {
            Self::require_task_local_cursor_schema(&connection)?;
        }
        Self::preflight_host_path_policy(&connection, host_path_policy)?;
        Self::preflight_control_policy_schema(&connection)?;
        let control_policy_preexisted = Self::control_policy_preexisted(&connection)?;
        Self::preflight_initial_control_assurance(
            &connection,
            control_policy_preexisted,
            initial_control_policy
                .as_ref()
                .map(|policy| policy.required_assurance),
        )?;
        let core_schema_complete = Self::current_core_schema_is_complete(&connection)?;
        if core_store_exists && !core_schema_complete {
            let issue = Self::current_core_rebuildable_schema_issue(&connection)?
                .unwrap_or_else(|| "the current schema inventory is incomplete".into());
            return Err(StoreError::InvalidControlProjection(format!(
                "the store is missing a rebuildable projection: {issue}; run `engram doctor --repair-projections` explicitly"
            )));
        }
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;",
        )?;
        let journal_mode =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        if !matches!(journal_mode.as_str(), "wal" | "memory") {
            connection.execute_batch("PRAGMA journal_mode = WAL;")?;
        }
        if !core_schema_complete {
            connection.execute_batch("BEGIN IMMEDIATE;")?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS objects (
                 object_hash TEXT PRIMARY KEY,
                 object_kind TEXT NOT NULL,
                 canonical_json BLOB NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             ) STRICT;
             CREATE TABLE IF NOT EXISTS publication_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 report_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 external_ref TEXT,
                 state TEXT NOT NULL,
                 last_error TEXT,
                 attempt_count INTEGER NOT NULL DEFAULT 0,
                 receipt_json TEXT
             ) STRICT;
             CREATE VIRTUAL TABLE IF NOT EXISTS object_fts USING fts5(
                 object_hash UNINDEXED,
                 title,
                 body
             );
             CREATE UNIQUE INDEX IF NOT EXISTS objects_project_memory_key
                 ON objects(
                     json_extract(canonical_json, '$.scope.project'),
                     json_extract(canonical_json, '$.project_key')
                 )
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text';
             CREATE TABLE IF NOT EXISTS memory_heads (
                 memory_id TEXT PRIMARY KEY,
                 version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 assertion_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 schema_version INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 scope_kind TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 task_id TEXT,
                 work_id TEXT,
                 agent_id TEXT,
                 memory_kind TEXT NOT NULL,
                 authority TEXT NOT NULL,
                 delivery TEXT NOT NULL,
                 sensitivity TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_heads_scope
                 ON memory_heads(project_id, task_id, work_id, agent_id, status);
             CREATE TABLE IF NOT EXISTS note_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 request_hash TEXT NOT NULL,
                 receipt_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS project_context_revisions (
                 project_id TEXT PRIMARY KEY,
                 revision INTEGER NOT NULL CHECK(revision >= 0)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS agent_context_revisions (
                 project_id TEXT NOT NULL,
                 agent_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK(revision >= 0),
                 PRIMARY KEY(project_id, agent_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS project_memory_advertisements (
                 project_id TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 context_generation_digest TEXT,
                 memory_position INTEGER NOT NULL CHECK(memory_position >= 0),
                 PRIMARY KEY(project_id, session_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS project_memory_state (
                 project_id TEXT PRIMARY KEY,
                 active_count INTEGER NOT NULL CHECK(active_count >= 0),
                 change_position INTEGER NOT NULL CHECK(change_position >= 0)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS memory_contradictions (
                 contradiction_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 left_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 right_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 UNIQUE(left_version_hash, right_version_hash),
                 CHECK(left_version_hash < right_version_hash)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_contradictions_versions
                 ON memory_contradictions(left_version_hash, right_version_hash);
             CREATE TABLE IF NOT EXISTS memory_contradiction_edges (
                 contradiction_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 project_id TEXT NOT NULL,
                 task_id TEXT,
                 work_root_id TEXT,
                 left_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 right_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 UNIQUE(left_version_hash, right_version_hash),
                 CHECK(left_version_hash < right_version_hash)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_contradiction_edges_context
                 ON memory_contradiction_edges(project_id, task_id, work_root_id);
             CREATE TABLE IF NOT EXISTS contradiction_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 request_hash TEXT NOT NULL,
                 receipt_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS tasks (
                 task_id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL,
                 external_ref TEXT NOT NULL,
                 title TEXT NOT NULL,
                 state TEXT NOT NULL,
                 event_cursor INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 UNIQUE(project_id, external_ref)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_participants (
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 session_id TEXT NOT NULL,
                 joined_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(task_id, session_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS session_bindings (
                 session_id TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 bound_at_ms INTEGER NOT NULL
              ) STRICT;
              CREATE TABLE IF NOT EXISTS task_changes (
                  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                  task_id TEXT NOT NULL,
                  task_cursor INTEGER NOT NULL CHECK(task_cursor > 0),
                  object_kind TEXT NOT NULL,
                  object_hash TEXT NOT NULL REFERENCES objects(object_hash),
                  UNIQUE(task_id, task_cursor),
                  UNIQUE(task_id, object_hash)
              ) STRICT;
              CREATE TABLE IF NOT EXISTS task_claims (
                 task_id TEXT PRIMARY KEY,
                 lease_id TEXT NOT NULL UNIQUE,
                 holder_session_id TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 revision INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_claim_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL,
                 holder_session_id TEXT NOT NULL,
                 lease_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_observations (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 task_id TEXT,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 input_hash TEXT NOT NULL,
                 input_json BLOB NOT NULL,
                 decision_hash TEXT NOT NULL,
                 decision_json BLOB NOT NULL,
                 observed_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, idempotency_key)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_observations_session_sequence
                 ON control_observations(session_id, sequence);
             CREATE TABLE IF NOT EXISTS control_policy_state (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 schema_version INTEGER NOT NULL,
                 policy_epoch INTEGER NOT NULL,
                 required_assurance TEXT NOT NULL,
                 supported_effects_json TEXT NOT NULL,
                 grant_ttl_seconds INTEGER NOT NULL,
                 policy_hash TEXT REFERENCES objects(object_hash)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_policy_versions (
                 policy_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 policy_epoch INTEGER NOT NULL UNIQUE CHECK(policy_epoch > 0),
                 authority_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 policy_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_policy_operation_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 operation TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 result_hash TEXT NOT NULL,
                 result_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(operation, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_control_state (
                 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
                 admission_epoch INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_connections (
                 session_id TEXT PRIMARY KEY,
                 connection_token TEXT NOT NULL,
                 opened_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_host_path_policy (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 case_fold_paths INTEGER NOT NULL CHECK(case_fold_paths IN (0, 1)),
                 windows_alias_rules INTEGER NOT NULL CHECK(windows_alias_rules IN (0, 1))
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_sessions (
                 session_id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 root_execution_id TEXT,
                 work_id TEXT,
                 run_id TEXT,
                 work_revision INTEGER,
                 claim_id TEXT,
                 claim_fence INTEGER,
                 routing_token TEXT NOT NULL,
                 actor_json BLOB NOT NULL,
                 bind_key TEXT NOT NULL,
                 bind_intent_hash TEXT NOT NULL,
                 bind_intent_json BLOB NOT NULL,
                 phase TEXT NOT NULL,
                 assurance TEXT NOT NULL,
                 mediated_effects_json TEXT NOT NULL,
                 confirmed_cursor INTEGER NOT NULL,
                 tentative_cursor INTEGER,
                 project_policy_epoch INTEGER NOT NULL,
                 task_admission_epoch INTEGER NOT NULL,
                 blocking_watermark INTEGER NOT NULL,
                 capability_map_revision INTEGER NOT NULL,
                 revision INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_sessions_work_run
                 ON control_sessions(project_id, run_id, session_id);
             CREATE TABLE IF NOT EXISTS control_turn_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 decision_hash TEXT NOT NULL,
                 decision_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_turn_grants (
                 grant_id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 request_key TEXT NOT NULL,
                 grant_hash TEXT NOT NULL,
                 grant_json BLOB NOT NULL,
                 state TEXT NOT NULL,
                 issued_at_ms INTEGER NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 begun_at_ms INTEGER,
                 completed_at_ms INTEGER,
                 UNIQUE(session_id, request_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_turn_grant_supersessions (
                 superseded_grant_id TEXT PRIMARY KEY
                     REFERENCES control_turn_grants(grant_id),
                 session_id TEXT NOT NULL,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 replacement_request_key TEXT NOT NULL,
                 replacement_decision_hash TEXT NOT NULL,
                 supersession_hash TEXT NOT NULL,
                 supersession_json BLOB NOT NULL,
                 superseded_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, replacement_request_key),
                 FOREIGN KEY(session_id, replacement_request_key)
                     REFERENCES control_turn_results(session_id, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_work_leases (
                 lease_id TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 holder_session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 lease_hash TEXT NOT NULL,
                 lease_json BLOB NOT NULL,
                 state TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 UNIQUE(holder_session_id, lease_id)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_work_leases_task_state
                 ON control_work_leases(task_id, state, expires_at_ms);
             CREATE TABLE IF NOT EXISTS control_operation_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 operation TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 result_hash TEXT NOT NULL,
                 result_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, operation, idempotency_key)
             ) STRICT;",
            )?;
            #[cfg(test)]
            if fail_cold_schema_after_ddl() && !building_schema_reference() {
                return Err(StoreError::InvalidControlProjection(
                    "injected cold-schema failure after DDL".into(),
                ));
            }
        }
        if core_schema_complete {
            Self::bind_host_path_policy(&mut connection, host_path_policy)?;
        } else if let Some(policy) = host_path_policy {
            Self::bind_host_path_policy_on(&connection, policy)?;
        } else {
            Self::preflight_host_path_policy(&connection, None)?;
        }
        if !core_schema_complete {
            connection.execute(
                "CREATE INDEX IF NOT EXISTS memory_heads_work_scope
                 ON memory_heads(project_id, work_id, agent_id, status)",
                [],
            )?;
            connection.execute_batch(
                "INSERT INTO project_context_revisions (project_id, revision)
                 SELECT project_id, COUNT(*)
                 FROM (
                     SELECT project_id FROM memory_heads WHERE scope_kind = 'project'
                     UNION ALL
                     SELECT project_id FROM memory_contradiction_edges
                 )
                 GROUP BY project_id
                 ON CONFLICT(project_id) DO NOTHING;
                 INSERT INTO agent_context_revisions (project_id, agent_id, revision)
                 SELECT project_id, agent_id, COUNT(*)
                 FROM memory_heads
                 WHERE scope_kind = 'agent' AND agent_id IS NOT NULL
                 GROUP BY project_id, agent_id
                 ON CONFLICT(project_id, agent_id) DO NOTHING;",
            )?;
        }
        Self::require_task_local_cursor_schema(&connection)?;
        if !core_schema_complete {
            connection.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS task_changes_task_cursor
                 ON task_changes(task_id, task_cursor)",
                [],
            )?;
        }
        if core_schema_complete {
            Self::initialize_control_policy(
                &mut connection,
                control_policy_preexisted,
                initial_control_policy,
            )?;
        } else {
            Self::initialize_control_policy_on(&connection, initial_control_policy)?;
            connection.execute_batch("COMMIT;")?;
        }
        work::initialize_schema(&mut connection, allow_initialization)?;
        let work_schema_version = work::schema_version(&connection)?;
        Self::require_current_contradiction_edges(&connection)?;
        Ok(Self {
            connection,
            work_schema_version,
            host_path_policy,
        })
    }

    fn control_policy_row_exists(connection: &Connection) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_state")? {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM control_policy_state WHERE singleton = 1
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    pub(super) fn sqlite_table_exists(
        connection: &Connection,
        table: &str,
    ) -> Result<bool, StoreError> {
        connection
            .query_row(
                "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = ?1
             )",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn sqlite_user_schema_exists(connection: &Connection) -> Result<bool, StoreError> {
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_schema
                     WHERE name NOT LIKE 'sqlite_%'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn control_policy_family_exists(connection: &Connection) -> Result<bool, StoreError> {
        for table in [
            "control_policy_state",
            "control_policy_versions",
            "control_policy_operation_results",
            "control_sessions",
            "control_turn_grants",
            "control_turn_grant_supersessions",
        ] {
            if Self::sqlite_table_exists(connection, table)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn canonical_control_policy_objects_exist(connection: &Connection) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "objects")? {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM objects
                     WHERE object_kind IN (
                         'control_policy', 'project_policy_authority_decision'
                     )
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn control_policy_preexisted(connection: &Connection) -> Result<bool, StoreError> {
        Self::control_policy_row_exists(connection)
    }

    fn control_policy_state_columns(
        connection: &Connection,
    ) -> Result<HashSet<String>, StoreError> {
        let mut statement = connection.prepare("PRAGMA table_info('control_policy_state')")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    fn sqlite_table_column_types(
        connection: &Connection,
        table: &str,
    ) -> Result<HashMap<String, String>, StoreError> {
        let quoted = table.replace('"', "\"\"");
        let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{quoted}\")"))?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        Ok(rows
            .map(|row| {
                row.map(|(name, declared_type)| (name, declared_type.trim().to_ascii_uppercase()))
            })
            .collect::<Result<HashMap<_, _>, _>>()?)
    }

    pub(super) fn diagnose_control_policy_table_shape(
        connection: &Connection,
        table: &str,
        required: &[(&str, &str)],
        report: &mut ControlPolicyRecoveryReport,
    ) -> Result<bool, StoreError> {
        let columns = Self::sqlite_table_column_types(connection, table)?;
        let mut valid = true;
        for (column, expected_type) in required {
            let detail = match columns.get(*column) {
                None => Some(format!("required column {column:?} is missing")),
                Some(stored_type) if stored_type != expected_type => Some(format!(
                    "column {column:?} has declared type {stored_type:?}; expected {expected_type}"
                )),
                Some(_) => None,
            };
            if let Some(detail) = detail {
                valid = false;
                report
                    .invalid_control_records
                    .push(ControlPolicyRecoveryFinding {
                        record: format!("{table}:schema"),
                        detail,
                    });
            }
        }
        Ok(valid)
    }

    fn control_policy_operation_results_table_is_complete(
        connection: &Connection,
    ) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_operation_results")? {
            return Ok(false);
        }
        let mut statement =
            connection.prepare("PRAGMA table_info('control_policy_operation_results')")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        Ok([
            "sequence",
            "operation",
            "idempotency_key",
            "intent_hash",
            "intent_json",
            "result_hash",
            "result_json",
            "created_at_ms",
        ]
        .iter()
        .all(|column| columns.contains(*column)))
    }

    fn preflight_control_policy_schema(connection: &Connection) -> Result<(), StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_state")? {
            if Self::control_policy_family_exists(connection)?
                || Self::canonical_control_policy_objects_exist(connection)?
            {
                return Err(StoreError::InvalidControlProjection(
                    "control policy state is missing from an established store".into(),
                ));
            }
            return Ok(());
        }
        if !Self::control_policy_row_exists(connection)? {
            return Err(StoreError::InvalidControlProjection(
                "control policy singleton is missing from an established store".into(),
            ));
        }
        let columns = Self::control_policy_state_columns(connection)?;
        for required in [
            "singleton",
            "schema_version",
            "policy_epoch",
            "required_assurance",
            "supported_effects_json",
            "grant_ttl_seconds",
            "policy_hash",
        ] {
            if !columns.contains(required) {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy state is missing required column {required:?}"
                )));
            }
        }
        let schema_version = connection.query_row(
            "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        require_current_schema_marker(schema_version, CONTROL_POLICY_STATE_SCHEMA_VERSION)?;
        if !Self::sqlite_table_exists(connection, "control_policy_versions")? {
            return Err(StoreError::InvalidControlProjection(
                "current control policy version table is missing".into(),
            ));
        }
        if !Self::control_policy_operation_results_table_is_complete(connection)? {
            return Err(StoreError::InvalidControlProjection(
                "current control policy operation-result table is missing".into(),
            ));
        }
        let snapshot = connection.unchecked_transaction()?;
        let policy = Self::verify_control_policy_history(&snapshot)?;
        Self::load_obligation_rule_set_on(&snapshot, &policy.obligation_rule_set)?;
        snapshot.commit()?;
        Ok(())
    }

    fn preflight_initial_control_assurance(
        connection: &Connection,
        policy_preexisted: bool,
        initial_required_assurance: Option<ControlAssurance>,
    ) -> Result<(), StoreError> {
        let Some(requested) = initial_required_assurance else {
            return Ok(());
        };
        if !policy_preexisted {
            return Ok(());
        }
        let required_assurance: String = connection.query_row(
            "SELECT required_assurance
             FROM control_policy_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let projected_assurance: ControlAssurance = parse_enum(&required_assurance)?;
        if projected_assurance != requested {
            return Err(StoreError::InvalidControlProjection(
                "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                    .into(),
            ));
        }
        Ok(())
    }

    fn initialize_control_policy(
        connection: &mut Connection,
        policy_preexisted: bool,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<(), StoreError> {
        if policy_preexisted {
            Self::preflight_initial_control_assurance(
                connection,
                true,
                initial_control_policy
                    .as_ref()
                    .map(|policy| policy.required_assurance),
            )?;
            return Ok(());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::control_policy_row_exists(&transaction)? {
            let policy = Self::verify_control_policy_history(&transaction)?;
            if initial_control_policy
                .as_ref()
                .is_some_and(|initial| initial.required_assurance != policy.required_assurance)
            {
                return Err(StoreError::InvalidControlProjection(
                    "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                        .into(),
                ));
            }
        } else {
            Self::initialize_control_policy_on(&transaction, initial_control_policy)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn initialize_control_policy_on(
        connection: &Connection,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<(), StoreError> {
        if !Self::control_policy_operation_results_table_is_complete(connection)? {
            return Err(StoreError::InvalidControlProjection(
                "current control policy operation-result table is missing".into(),
            ));
        }
        let now = Utc::now();
        let (required_assurance, authorized_by, reason) =
            if let Some(initial) = initial_control_policy {
                (
                    initial.required_assurance,
                    initial.authorized_by,
                    initial.reason,
                )
            } else {
                let source = "engram:init";
                let reason = "install the default project bootstrap control policy";
                (
                    ControlAssurance::TurnGated,
                    ActorContext {
                        actor_id: source.into(),
                        actor_kind: "system".into(),
                        assurance: AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: None,
                        source_tool: Some(source.into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: reason.into(),
                    },
                    reason.to_owned(),
                )
            };
        let obligation_rule_set = Self::insert_builtin_obligation_rule_set(connection)?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
            operation: ProjectPolicyOperation::SetRequiredAssurance,
            policy_epoch: ProjectPolicyEpoch(1),
            previous_policy: None,
            required_assurance,
            obligation_rule_set: obligation_rule_set.clone(),
            authorized_by,
            reason,
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            connection,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION,
            control_schema_version: CONTROL_SCHEMA_VERSION,
            policy_epoch: ProjectPolicyEpoch(1),
            previous_policy: None,
            required_assurance,
            supported_effects: Self::builtin_control_effects(),
            grant_ttl_seconds: BUILTIN_CONTROL_GRANT_TTL_SECONDS,
            obligation_rule_set,
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(connection, "control_policy", &policy_object)?;
        connection.execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        connection.execute(
            "INSERT INTO control_policy_state (
                 singleton, schema_version, policy_epoch, required_assurance,
                 supported_effects_json, grant_ttl_seconds, policy_hash
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
            ],
        )?;
        Self::verify_control_policy_history(connection)?;
        Ok(())
    }

    pub(super) fn builtin_control_effects() -> Vec<EffectClass> {
        vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
        ]
    }

    fn insert_builtin_obligation_rule_set(
        connection: &Connection,
    ) -> Result<ObjectHash, StoreError> {
        let rule_set = crate::control::builtin_obligation_rule_set();
        Self::validate_obligation_rule_set(&rule_set)?;
        let object = CanonicalObject::freeze(&rule_set)?;
        Self::insert_object(connection, "obligation_rule_set", &object)?;
        Ok(object.hash().clone())
    }

    pub(crate) fn load_obligation_rule_set_on(
        connection: &Connection,
        hash: &ObjectHash,
    ) -> Result<ObligationRuleSet, StoreError> {
        let bytes = Self::load_control_object_bytes(connection, hash, "obligation_rule_set")?;
        let rule_set: ObligationRuleSet = CanonicalObject::verify(hash, bytes)?.decode()?;
        Self::validate_obligation_rule_set(&rule_set)?;
        Ok(rule_set)
    }

    pub(crate) fn obligation_rule_set_for_policy_on(
        connection: &Connection,
        policy_hash: &ObjectHash,
    ) -> Result<(ObjectHash, ObligationRuleSet), StoreError> {
        let (policy, _) = Self::load_control_policy_version(connection, policy_hash)?;
        let hash = policy.obligation_rule_set;
        let rule_set = Self::load_obligation_rule_set_on(connection, &hash)?;
        Ok((hash, rule_set))
    }

    pub(super) fn obligation_rule_set_for_policy_epoch_on(
        connection: &Connection,
        epoch: ProjectPolicyEpoch,
    ) -> Result<(ObjectHash, ObligationRuleSet), StoreError> {
        let stored_hash = connection
            .query_row(
                "SELECT policy_hash FROM control_policy_versions WHERE policy_epoch = ?1",
                [epoch.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "turn grant names missing control policy epoch {}",
                    epoch.0
                ))
            })?;
        let policy_hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        Self::obligation_rule_set_for_policy_on(connection, &policy_hash)
    }

    pub(super) fn validate_obligation_rule_set(
        rule_set: &ObligationRuleSet,
    ) -> Result<(), StoreError> {
        const MAX_RULES: usize = 64;
        let mut identities = HashSet::new();
        let valid = rule_set.schema_version == OBLIGATION_RULE_SET_SCHEMA_VERSION
            && rule_set.rules.len() <= MAX_RULES
            && rule_set.rules.iter().all(|definition| {
                let id = definition.rule.rule_id.as_str();
                !id.is_empty()
                    && id.len() <= 128
                    && id.trim() == id
                    && definition.rule.rule_version > 0
                    && !matches!(
                        definition.trigger,
                        crate::domain::BuiltinObligationTrigger::Unknown
                    )
                    && identities.insert((id.to_owned(), definition.rule.rule_version))
            });
        if !valid {
            return Err(StoreError::InvalidControlProjection(
                "canonical obligation rule set has an unsupported or ambiguous shape".into(),
            ));
        }
        Ok(())
    }

    fn current_core_durable_schema_issue(
        connection: &Connection,
    ) -> Result<Option<String>, StoreError> {
        current_schema_definition_issue(connection, SchemaOwner::Core, SchemaDurability::Durable)
    }

    fn current_core_rebuildable_schema_issue(
        connection: &Connection,
    ) -> Result<Option<String>, StoreError> {
        current_schema_definition_issue(
            connection,
            SchemaOwner::Core,
            SchemaDurability::Rebuildable,
        )
    }

    fn repair_core_rebuildable_schema_on(connection: &Connection) -> Result<bool, StoreError> {
        let mut checked_heads = 0;
        let mut invalid_heads = Vec::new();
        Self::verify_memory_head_projections_on(
            connection,
            &mut checked_heads,
            &mut invalid_heads,
        )?;
        if !invalid_heads.is_empty() {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "projection repair refused because durable memory heads are invalid: {}",
                invalid_heads.join(", ")
            )));
        }
        for object in CORE_REBUILDABLE_SCHEMA_OBJECTS {
            drop_schema_object(connection, object)?;
        }
        connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS object_fts USING fts5(
                 object_hash UNINDEXED,
                 title,
                 body
             );
             CREATE UNIQUE INDEX IF NOT EXISTS objects_project_memory_key
                 ON objects(
                     json_extract(canonical_json, '$.scope.project'),
                     json_extract(canonical_json, '$.project_key')
                 )
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text';
             CREATE INDEX IF NOT EXISTS memory_heads_scope
                 ON memory_heads(project_id, task_id, work_id, agent_id, status);
             CREATE INDEX IF NOT EXISTS memory_heads_work_scope
                 ON memory_heads(project_id, work_id, agent_id, status);
             CREATE INDEX IF NOT EXISTS memory_contradictions_versions
                 ON memory_contradictions(left_version_hash, right_version_hash);
             CREATE INDEX IF NOT EXISTS memory_contradiction_edges_context
                 ON memory_contradiction_edges(project_id, task_id, work_root_id);
             CREATE UNIQUE INDEX IF NOT EXISTS task_changes_task_cursor
                 ON task_changes(task_id, task_cursor);
             CREATE INDEX IF NOT EXISTS control_observations_session_sequence
                 ON control_observations(session_id, sequence);
             CREATE INDEX IF NOT EXISTS control_sessions_work_run
                 ON control_sessions(project_id, run_id, session_id);
             CREATE INDEX IF NOT EXISTS control_work_leases_task_state
                  ON control_work_leases(task_id, state, expires_at_ms);
             CREATE TABLE IF NOT EXISTS project_memory_advertisements (
                 project_id TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 context_generation_digest TEXT,
                 memory_position INTEGER NOT NULL CHECK(memory_position >= 0),
                 PRIMARY KEY(project_id, session_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS project_memory_state (
                 project_id TEXT PRIMARY KEY,
                 active_count INTEGER NOT NULL CHECK(active_count >= 0),
                 change_position INTEGER NOT NULL CHECK(change_position >= 0)
             ) STRICT;",
        )?;
        Self::rebuild_object_fts_from_heads_on(connection)?;
        Self::rebuild_project_memory_state_on(connection)?;
        let mut checked = 0;
        let mut invalid = Vec::new();
        Self::verify_object_fts_on(connection, &mut checked, &mut invalid)?;
        Self::verify_project_memory_state_on(connection, &mut checked, &mut invalid)?;
        if !invalid.is_empty() {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "explicit projection repair did not rebuild: {}",
                invalid.join(", ")
            )));
        }
        if Self::current_core_rebuildable_schema_issue(connection)?.is_some() {
            return Err(StoreError::InvalidControlProjection(
                "explicit core projection repair did not restore the current schema".into(),
            ));
        }
        Ok(true)
    }

    pub(super) fn verify_memory_head_projections_on(
        connection: &Connection,
        checked: &mut usize,
        invalid: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let mut statement = connection.prepare(
            "SELECT memory_id, version_hash, assertion_hash, schema_version,
                    status, scope_kind, project_id, task_id, work_id, agent_id,
                    memory_kind, authority, delivery, sensitivity, title, body,
                    created_at_ms
             FROM memory_heads ORDER BY memory_id",
        )?;
        let rows = statement
            .query_map([], |row| {
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
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        for stored in rows {
            *checked += 1;
            let record = format!("memory_head:{}", stored.memory_id);
            let valid = (|| {
                let version_hash = ObjectHash::from_stored(stored.version_hash.clone())
                    .ok_or_else(|| StoreError::InvalidStoredHash(stored.version_hash.clone()))?;
                let assertion_hash = ObjectHash::from_stored(stored.assertion_hash.clone())
                    .ok_or_else(|| StoreError::InvalidStoredHash(stored.assertion_hash.clone()))?;
                let version: MemoryVersion =
                    Self::get_typed_object_on(connection, &version_hash, "memory_version")?
                        .ok_or_else(|| {
                            StoreError::InvalidMemoryProjection(format!(
                                "memory head {} references missing version {}",
                                stored.memory_id, version_hash
                            ))
                        })?;
                let assertion: MemoryAssertionEvent = Self::get_typed_object_on(
                    connection,
                    &assertion_hash,
                    "memory_assertion_event",
                )?
                .ok_or_else(|| {
                    StoreError::InvalidMemoryProjection(format!(
                        "memory head {} references missing assertion {}",
                        stored.memory_id, assertion_hash
                    ))
                })?;
                let status = Self::expected_memory_head_status_on(
                    connection,
                    &version_hash,
                    assertion.status,
                )?;
                let expected = Self::expected_memory_head_projection(
                    &version_hash,
                    &assertion_hash,
                    &version,
                    &assertion,
                    status,
                )?;
                if stored != expected {
                    return Err(StoreError::InvalidMemoryProjection(format!(
                        "memory head {} does not match its canonical version and assertion",
                        stored.memory_id
                    )));
                }
                Ok(())
            })();
            if valid.is_err() {
                invalid.push(record);
            }
        }
        Ok(())
    }

    pub(super) fn expected_memory_head_status_on(
        connection: &Connection,
        version_hash: &ObjectHash,
        asserted: MemoryStatus,
    ) -> Result<MemoryStatus, StoreError> {
        if !matches!(asserted, MemoryStatus::Active | MemoryStatus::Stale) {
            return Ok(asserted);
        }
        let contradicted = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM memory_contradiction_edges
                 WHERE left_version_hash = ?1 OR right_version_hash = ?1
             )",
            [version_hash.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(if contradicted {
            MemoryStatus::Contested
        } else {
            asserted
        })
    }

    pub(super) fn expected_memory_head_projection(
        version_hash: &ObjectHash,
        assertion_hash: &ObjectHash,
        version: &MemoryVersion,
        assertion: &MemoryAssertionEvent,
        status: MemoryStatus,
    ) -> Result<MemoryHeadProjectionRow, StoreError> {
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
            Scope::Project { project } => ("project", project.0.clone(), None, None, None),
            Scope::Task { project, task } => (
                "task",
                project.0.clone(),
                Some(task.0.to_string()),
                None,
                None,
            ),
            Scope::Work { project, work } => (
                "work",
                project.0.clone(),
                None,
                Some(work.0.to_string()),
                None,
            ),
            Scope::Agent {
                project,
                task,
                work,
                agent,
            } => (
                "agent",
                project.0.clone(),
                task.map(|value| value.0.to_string()),
                work.map(|value| value.0.to_string()),
                Some(agent.clone()),
            ),
        };
        Ok(MemoryHeadProjectionRow {
            memory_id: version.memory_id.0.to_string(),
            version_hash: version_hash.as_str().to_owned(),
            assertion_hash: assertion_hash.as_str().to_owned(),
            schema_version: i64::from(version.schema_version),
            status: enum_name(status)?,
            scope_kind: scope_kind.into(),
            project_id,
            task_id,
            work_id,
            agent_id,
            memory_kind: enum_name(version.kind)?,
            authority: enum_name(version.authority)?,
            delivery: enum_name(version.delivery)?,
            sensitivity: enum_name(version.sensitivity)?,
            title: version.title.clone(),
            body: version.body.clone(),
            created_at_ms: version.created_at.timestamp_millis(),
        })
    }

    pub(super) fn rebuild_object_fts_from_heads_on(
        connection: &Connection,
    ) -> Result<(), StoreError> {
        connection.execute("DELETE FROM object_fts", [])?;
        connection.execute(
            "INSERT INTO object_fts (object_hash, title, body)
             SELECT version_hash, title, body FROM memory_heads ORDER BY version_hash",
            [],
        )?;
        Ok(())
    }

    pub(super) fn rebuild_project_memory_state_on(
        connection: &Connection,
    ) -> Result<(), StoreError> {
        connection.execute("DELETE FROM project_memory_state", [])?;
        for (project_id, active_count, change_position) in
            derived_project_memory_state_rows_on(connection)?
        {
            connection.execute(
                "INSERT INTO project_memory_state (
                     project_id, active_count, change_position
                 ) VALUES (?1, ?2, ?3)",
                params![project_id, active_count, change_position],
            )?;
        }
        Ok(())
    }

    pub(super) fn verify_project_memory_state_on(
        connection: &Connection,
        checked: &mut usize,
        invalid: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let expected = derived_project_memory_state_rows_on(connection)?;
        let actual = connection
            .prepare(
                "SELECT project_id, active_count, change_position
                 FROM project_memory_state ORDER BY project_id",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        *checked += expected.len().max(actual.len());
        if actual != expected {
            invalid.push("project_memory_state:projection_binding".into());
        }
        Ok(())
    }

    pub(super) fn verify_object_fts_on(
        connection: &Connection,
        checked: &mut usize,
        invalid: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let mut statement = connection
            .prepare("SELECT version_hash, title, body FROM memory_heads ORDER BY version_hash")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (version_hash, title, body) = row?;
            *checked += 1;
            let mut fts_statement =
                connection.prepare("SELECT title, body FROM object_fts WHERE object_hash = ?1")?;
            let stored = fts_statement
                .query_map([version_hash.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if stored.as_slice() != [(title.clone(), body.clone())] {
                invalid.push(format!("object_fts:{version_hash}:projection_binding"));
                continue;
            }
            let query = fts_query(&format!("{title} {body}"));
            if query != "\"__engram_no_match__\""
                && !connection
                    .query_row(
                        "SELECT EXISTS(
                             SELECT 1 FROM object_fts
                             WHERE object_hash = ?1 AND object_fts MATCH ?2
                         )",
                        params![version_hash, query],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            {
                invalid.push(format!("object_fts:{version_hash}:fts_index"));
            }
        }
        drop(statement);
        let orphaned = connection.query_row(
            "SELECT COUNT(*) FROM object_fts fts
             WHERE NOT EXISTS (
                 SELECT 1 FROM memory_heads head WHERE head.version_hash = fts.object_hash
             )",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if orphaned != 0 {
            invalid.push("object_fts:orphaned_rows".into());
        }
        Ok(())
    }

    fn current_core_schema_is_complete(connection: &Connection) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "objects")? {
            return Ok(false);
        }
        if Self::current_core_durable_schema_issue(connection)?.is_some()
            || Self::current_core_rebuildable_schema_issue(connection)?.is_some()
        {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM control_policy_state WHERE singleton = 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn require_current_contradiction_edges(connection: &Connection) -> Result<(), StoreError> {
        let missing = connection.query_row(
            "SELECT COUNT(*)
             FROM memory_contradictions contradiction
             JOIN tasks task ON task.task_id = contradiction.task_id
             LEFT JOIN memory_contradiction_edges edge
               ON edge.contradiction_hash = contradiction.contradiction_hash
              AND edge.project_id = task.project_id
              AND edge.task_id = contradiction.task_id
              AND edge.work_root_id IS NULL
              AND edge.left_version_hash = contradiction.left_version_hash
              AND edge.right_version_hash = contradiction.right_version_hash
             WHERE edge.contradiction_hash IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if missing != 0 {
            return Err(StoreError::InvalidMemoryProjection(
                "task contradiction projections do not match the current schema".into(),
            ));
        }
        Ok(())
    }

    fn preflight_host_path_policy(
        connection: &Connection,
        expected: Option<HostPathPolicy>,
    ) -> Result<(), StoreError> {
        let stored = Self::stored_host_path_policy_on(connection)?;
        let Some(stored) = stored else {
            if Self::has_path_bearing_control_state(connection)? {
                return Err(StoreError::InvalidControlSession(
                    "path-bearing control state exists without a bound host path policy".into(),
                ));
            }
            return Ok(());
        };
        // An unresolved opener asserts nothing and may read; only a resolved
        // opener that disagrees with the persisted identity is refused.
        if let Some(expected) = expected
            && stored != expected
        {
            return Err(StoreError::InvalidControlSession(format!(
                "the store's persisted host path policy ({}) differs from this opener's ({}); if the project moved to a different filesystem, supply --host-path-policy matching the store or re-initialize a fresh store",
                describe_host_path_policy(stored),
                describe_host_path_policy(expected)
            )));
        }
        Ok(())
    }

    fn bind_host_path_policy(
        connection: &mut Connection,
        expected: Option<HostPathPolicy>,
    ) -> Result<(), StoreError> {
        let Some(expected) = expected else {
            Self::preflight_host_path_policy(connection, None)?;
            return Ok(());
        };
        let snapshot = connection.unchecked_transaction()?;
        let need = Self::host_path_policy_write_need_on(&snapshot, expected)?;
        snapshot.commit()?;
        if need == OpenWriteNeed::Current {
            return Ok(());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::host_path_policy_write_need_on(&transaction, expected)?
            == OpenWriteNeed::NeedsWrite
        {
            Self::bind_host_path_policy_on(&transaction, expected)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn host_path_policy_write_need_on(
        connection: &Connection,
        expected: HostPathPolicy,
    ) -> Result<OpenWriteNeed, StoreError> {
        Self::preflight_host_path_policy(connection, Some(expected))?;
        Ok(if Self::stored_host_path_policy_on(connection)?.is_some() {
            OpenWriteNeed::Current
        } else {
            OpenWriteNeed::NeedsWrite
        })
    }

    fn bind_host_path_policy_on(
        connection: &Connection,
        expected: HostPathPolicy,
    ) -> Result<(), StoreError> {
        if Self::stored_host_path_policy_on(connection)? == Some(expected) {
            return Ok(());
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS control_host_path_policy (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 case_fold_paths INTEGER NOT NULL CHECK(case_fold_paths IN (0, 1)),
                 windows_alias_rules INTEGER NOT NULL CHECK(windows_alias_rules IN (0, 1))
             ) STRICT;",
        )?;
        if Self::has_path_bearing_control_state(connection)?
            && connection.query_row("SELECT COUNT(*) FROM control_host_path_policy", [], |row| {
                row.get::<_, i64>(0)
            })? == 0
        {
            return Err(StoreError::InvalidControlSession(
                "path-bearing control state exists without a bound host path policy".into(),
            ));
        }
        connection.execute(
            "INSERT OR IGNORE INTO control_host_path_policy (
                 singleton, case_fold_paths, windows_alias_rules
             ) VALUES (1, ?1, ?2)",
            params![
                i64::from(expected.case_fold_paths),
                i64::from(expected.windows_alias_rules)
            ],
        )?;
        Self::preflight_host_path_policy(connection, Some(expected))?;
        Ok(())
    }

    fn has_path_bearing_control_state(connection: &Connection) -> Result<bool, StoreError> {
        for (table, column) in [
            ("control_work_leases", "lease_json"),
            ("control_turn_grants", "grant_json"),
        ] {
            let table_exists = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )?;
            if table_exists {
                let query = format!(
                    "SELECT EXISTS(SELECT 1 FROM {table} WHERE CAST({column} AS TEXT) LIKE '%\"kind\":\"path\"%')"
                );
                if connection.query_row(&query, [], |row| row.get::<_, bool>(0))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn require_task_local_cursor_schema(connection: &Connection) -> Result<(), StoreError> {
        let has_task_cursor = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('task_changes')
                 WHERE name = 'task_cursor'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_task_cursor {
            return Ok(());
        }
        Err(StoreError::InvalidTaskProjection(
            "task_changes does not match the current task-local cursor schema".into(),
        ))
    }
}
