//! Schema generations understood and emitted by this release.
//!
//! Keeping these markers together makes a version bump an explicit,
//! reviewable change instead of allowing storage projections and canonical
//! objects to drift independently.

/// Canonical memory, work, evidence, and lifecycle object schema.
pub const SCHEMA_VERSION: u16 = 1;
/// Behavioral-control protocol schema.
pub const CONTROL_SCHEMA_VERSION: u16 = 1;
/// Mutable control-policy selector/projection schema.
pub const CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 4;
/// Stock selector schema emitted before immutable policy history existed.
pub const LEGACY_STOCK_CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 1;
/// Legacy selector schema before durable policy-operation replay was added.
pub const LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 3;
/// Legacy selector schema that first introduced immutable policy versions.
pub const LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 2;
/// Historical immutable control-policy V1 object schema.
pub const CONTROL_POLICY_SCHEMA_VERSION_V1: u16 = 1;
/// Immutable control-policy object schema emitted by current writers.
pub const CONTROL_POLICY_SCHEMA_VERSION: u16 = CONTROL_POLICY_SCHEMA_VERSION_V1;
/// Historical immutable control-policy V1 authority-decision schema.
pub const CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1: u16 = 1;
/// Immutable control-policy authority-decision schema emitted by current writers.
pub const CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION: u16 = CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1;
/// Control protocol recorded in historical V1 policy objects.
pub const CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1: u16 = 1;
/// First-class local-work SQLite projection schema.
pub const WORK_SCHEMA_VERSION: i64 = 11;
/// Completion-seal obligation basis emitted by current writers.
pub const COMPLETION_OBLIGATION_SCHEMA_VERSION: u16 = 1;
/// Completion-seal environment basis emitted by current writers.
pub const COMPLETION_ENVIRONMENT_SCHEMA_VERSION: u16 = 1;
/// Immutable obligation-rule-set schema emitted by current writers.
pub const OBLIGATION_RULE_SET_SCHEMA_VERSION: u16 = 1;
/// Canonical intent schema for project-policy administration operations.
pub const CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION: u16 = 1;
/// Canonical intent schema for bind-scoped work-lease acquisition.
pub const WORK_LEASE_ACQUIRE_FINGERPRINT_SCHEMA_VERSION: u16 = 2;
