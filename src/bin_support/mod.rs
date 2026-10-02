//! Operator command families dispatched from the clap graph in `main.rs`.

pub(crate) mod attribution;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "no operator word calls the adapter until `backup push`"
    )
)]
pub(crate) mod backup;
pub(crate) mod backup_target;
pub(crate) mod control;
pub(crate) mod control_session_inspect;
pub(crate) mod doctor;
pub(crate) mod graph;
pub(crate) mod import;
pub(crate) mod migration;
pub(crate) mod project;
pub(crate) mod readiness;
pub(crate) mod store_lifecycle;
pub(crate) mod terminal_errors;
