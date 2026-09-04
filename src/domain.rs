//! Domain records shared by storage, context assembly, and tracker adapters.

mod control;
mod identity;
mod memory;
mod provenance;
mod task;
mod work;
mod work_requests;

pub use crate::schema::{
    COMPLETION_ENVIRONMENT_SCHEMA_VERSION, COMPLETION_OBLIGATION_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, OBLIGATION_RULE_SET_SCHEMA_VERSION, SCHEMA_VERSION,
};
pub use control::*;
pub use identity::*;
pub use memory::*;
pub use provenance::*;
pub use task::*;
pub use work::*;
pub use work_requests::*;
