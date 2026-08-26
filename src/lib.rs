//! Engram's local working-memory core.
//!
//! The crate deliberately separates local operational state from publication:
//! agents work against local immutable records, while a frozen report crosses
//! the external tracker boundary only through a receipted adapter call.

pub mod canonical;
pub mod domain;
pub mod mcp;
pub mod memory;
pub mod project;
pub mod storage;
pub mod tracker;

pub use canonical::{CanonicalObject, ObjectHash};
pub use domain::{
    ActorContext, Authority, ChangeCursor, ContextItem, ContextOmission, ContextPacket,
    ContextPacketHeader, ContextPacketPayload, Delivery, DeltaItem, FinalizationBarrier,
    FrozenReport, LocalTask, MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId,
    MemoryKind, MemoryRecord, MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest,
    NoteVisibility, ParticipantReadiness, ProjectId, Scope, Sensitivity, SessionId,
    TaskBindReceipt, TaskDelta, TaskId, TaskLease, TaskState,
};
pub use mcp::McpServer;
pub use memory::{DevelopmentNoopRedactor, Redactor};
pub use project::project_database_path;
pub use storage::{IntegrityReport, SqliteStore, TaskChange};
pub use tracker::{DummyTrackerAdapter, PublicationReceipt, TrackerAdapter};
