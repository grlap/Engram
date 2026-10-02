//! Off-host backup on the operator side: the `BackupAdapter` port and its
//! adapters. They live outside the core library, which produces the copy and
//! its manifest; an adapter only moves bytes and reports what the target
//! confirmed.

pub(crate) mod adapter;
pub(crate) mod directory;
pub(crate) mod push;

#[cfg(test)]
mod tests;
