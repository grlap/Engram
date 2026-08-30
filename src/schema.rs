//! Schema markers understood and emitted by the current build.
//!
//! Engram has one pre-release schema. These declarations stay together so
//! storage projections and canonical objects cannot drift independently.

/// Canonical memory, work, evidence, and lifecycle object schema.
pub const SCHEMA_VERSION: u16 = 1;
/// Behavioral-control protocol schema.
pub const CONTROL_SCHEMA_VERSION: u16 = 1;
/// Mutable control-policy selector/projection schema.
pub const CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 1;
/// Immutable control-policy object schema emitted by current writers.
pub const CONTROL_POLICY_SCHEMA_VERSION: u16 = 1;
/// Immutable control-policy authority-decision schema emitted by current writers.
pub const CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION: u16 = 1;
/// First-class local-work SQLite projection schema.
pub const WORK_SCHEMA_VERSION: i64 = 1;
/// Completion-seal obligation basis emitted by current writers.
pub const COMPLETION_OBLIGATION_SCHEMA_VERSION: u16 = 1;
/// Completion-seal environment basis emitted by current writers.
pub const COMPLETION_ENVIRONMENT_SCHEMA_VERSION: u16 = 1;
/// Immutable obligation-rule-set schema emitted by current writers.
pub const OBLIGATION_RULE_SET_SCHEMA_VERSION: u16 = 1;
/// Canonical intent schema for project-policy administration operations.
pub const CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION: u16 = 1;
/// Canonical intent schema for bind-scoped work-lease acquisition.
pub const WORK_LEASE_ACQUIRE_FINGERPRINT_SCHEMA_VERSION: u16 = 1;
