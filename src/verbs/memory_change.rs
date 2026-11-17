//! `remember`'s partial edits, and the change a revise reports so its writer
//! can confirm nothing was dropped.

use crate::domain::{ProjectMemoryChange, ProjectMemoryEdit, ProjectMemoryMutationReceipt};
use crate::storage::StoreError;

/// The edit `--append` or `--section NAME` asks for; neither is a whole-body
/// revise, and both together are refused.
///
/// # Errors
///
/// Refuses `--append` with `--section`.
pub(super) fn edit(append: bool, section: Option<&str>) -> Result<ProjectMemoryEdit, StoreError> {
    match (append, section.map(str::trim)) {
        (true, Some(_)) => Err(StoreError::InvalidProjectMemory(
            super::argument_wording::APPEND_WITH_SECTION_REFUSAL
                .cli
                .into(),
        )),
        (true, None) => Ok(ProjectMemoryEdit::Append),
        (false, Some(name)) => Ok(ProjectMemoryEdit::Section {
            name: name.to_owned(),
        }),
        (false, None) => Ok(ProjectMemoryEdit::Whole),
    }
}

/// The bounded change a revise's receipt adds. Historical navigation is
/// reserved for an explicitly requested revision read.
pub(super) fn lines(receipt: &ProjectMemoryMutationReceipt) -> Vec<String> {
    let Some(change) = &receipt.change else {
        return Vec::new();
    };
    let mut lines = vec![summary(change)];
    for (sign, text, bytes, omitted) in [
        (
            "-",
            &change.removed,
            change.removed_bytes,
            change.removed_omitted_bytes,
        ),
        (
            "+",
            &change.added,
            change.added_bytes,
            change.added_omitted_bytes,
        ),
    ] {
        if bytes == 0 {
            continue;
        }
        let shown = super::terminal_data_block(text);
        for line in shown.split('\n') {
            lines.push(format!("  {sign} {line}"));
        }
        if omitted > 0 {
            lines.push(format!("  {sign} … {omitted} more bytes"));
        }
    }
    lines
}

fn summary(change: &ProjectMemoryChange) -> String {
    let edit = match &change.section {
        Some(section) => format!("section {section}"),
        None => change.edit.clone(),
    };
    format!(
        "changed ({edit}): {} → {} bytes; {} removed and {} added at byte {}",
        change.before_bytes,
        change.after_bytes,
        change.removed_bytes,
        change.added_bytes,
        change.span_start
    )
}
