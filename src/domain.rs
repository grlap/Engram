//! Domain records shared by storage, context assembly, and tracker adapters.

mod acceptance_admission;
mod acceptance_binding_read;
mod acceptance_evaluation;
mod acceptance_source_recovery;
mod acceptance_verification_read;
mod control;
mod identity;
mod memory;
mod provenance;
mod source_observation;
mod task;
mod unadmitted_observation;
mod work;
mod work_import;
mod work_observation;
mod work_plan;
mod work_requests;

pub use crate::schema::{
    COMPLETION_ENVIRONMENT_SCHEMA_VERSION, COMPLETION_OBLIGATION_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, OBLIGATION_RULE_SET_SCHEMA_VERSION, SCHEMA_VERSION,
};
pub use acceptance_admission::*;
pub use acceptance_binding_read::*;
pub use acceptance_evaluation::*;
pub use acceptance_source_recovery::*;
pub use acceptance_verification_read::*;
pub use control::*;
pub use identity::*;
pub use memory::*;
pub use provenance::*;
pub use source_observation::*;
pub use task::*;
pub use unadmitted_observation::*;
pub use work::*;
pub use work_import::*;
pub use work_observation::*;
pub use work_plan::*;
pub use work_requests::*;
