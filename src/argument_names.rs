//! Prose that tells a caller to pass an argument, spelled for each surface.
//!
//! A CLI caller passes `--revise`; an MCP caller passes the `revise` field.
//! A sentence that names arguments is written once per site as a [`Twin`]:
//! its CLI spelling is what the core raises and every CLI surface prints,
//! and its MCP spelling is what the agent projection shows an MCP caller.
//! Runnable `engram work …` commands are never twinned: they stay CLI syntax
//! on every surface.

/// How guidance names a word's arguments: as the CLI flag, or as the MCP
/// field the caller actually passes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArgumentNames {
    #[default]
    Cli,
    Mcp,
}

/// One sentence that names arguments, in its CLI and its MCP spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Twin {
    pub cli: &'static str,
    pub mcp: &'static str,
}

impl Twin {
    /// The spelling for a surface.
    #[must_use]
    pub const fn spelled(self, names: ArgumentNames) -> &'static str {
        match names {
            ArgumentNames::Cli => self.cli,
            ArgumentNames::Mcp => self.mcp,
        }
    }
}
