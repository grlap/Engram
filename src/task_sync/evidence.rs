//! Inert, bounded execution-evidence interchange. Importing facts grants no authority.

pub mod acceptance;
pub mod closure;
pub mod consumption;
pub mod inventory;
pub mod limits;
mod pack;
pub(crate) mod path;
mod record;
pub mod verification;
pub mod witnesses;

pub use pack::EvidencePack;
pub use record::{RecordEnvelope, RecordKind};

use crate::ObjectId;

/// Structural refusals contain locators, never canonical bodies or serde values.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("evidence dependency {object}: {problem}")]
    Dependency {
        object: ObjectId,
        problem: &'static str,
    },
    #[error("evidence witness: {0:?}")]
    Witness(witnesses::WitnessProblem),
    #[error("{0}")]
    Oversize(#[from] limits::Oversize),
    #[error("unsupported evidence record kind: {0}")]
    UnknownKind(String),
    #[error("invalid evidence JSON ({category}, line {line}, column {column})")]
    Json {
        category: &'static str,
        line: usize,
        column: usize,
    },
    #[error("evidence record {0} is not canonical JSON")]
    NonCanonical(ObjectId),
    #[error("evidence record {0} has conflicting kind or bytes")]
    RecordConflict(ObjectId),
    #[error("invalid evidence pack: {0}")]
    InvalidPack(&'static str),
    #[error("evidence input/output failed: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for EvidenceError {
    fn from(error: serde_json::Error) -> Self {
        use serde_json::error::Category;
        Self::Json {
            category: match error.classify() {
                Category::Io => "io",
                Category::Syntax => "syntax",
                Category::Data => "data",
                Category::Eof => "eof",
            },
            line: error.line(),
            column: error.column(),
        }
    }
}

#[cfg(test)]
mod tests;
