//! Narrow proposed setup edits; all unrelated bytes remain untouched.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

const BEGIN: &str = "# BEGIN ENGRAM TASK SYNC";
const END: &str = "# END ENGRAM TASK SYNC";
const IGNORE: &str = "!/.engram/\n/.engram/*\n!/.engram/project.json\n!/.engram/tasks/\n/.engram/tasks/*\n!/.engram/tasks/*.json\n";
const ATTRIBUTES: &str =
    "/.engram/** text eol=lf -filter diff merge -working-tree-encoding -ident\n";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

pub(super) fn propose(path: &str, before: Option<Vec<u8>>) -> Result<Edit> {
    let text = std::str::from_utf8(before.as_deref().unwrap_or_default())?;
    ensure!(
        !text.contains(BEGIN) && !text.contains(END),
        "existing task-sync setup block requires its original intent"
    );
    let mut after = Vec::new();
    for line in text.split_inclusive('\n') {
        // Remove only the known blanket local-state rule. Other rules stay
        // byte-identical; the final allowlist overrides earlier blanket rules.
        if path != ".gitignore"
            || !matches!(
                line.trim_end_matches(['\r', '\n']),
                "/.engram/" | "/.engram"
            )
        {
            after.extend_from_slice(line.as_bytes());
        }
    }
    if !after.is_empty() && !after.ends_with(b"\n") {
        after.push(b'\n');
    }
    after.extend_from_slice(format!("{BEGIN}\n").as_bytes());
    after.extend_from_slice(
        if path == ".gitignore" {
            IGNORE
        } else {
            ATTRIBUTES
        }
        .as_bytes(),
    );
    after.extend_from_slice(format!("{END}\n").as_bytes());
    Ok(Edit {
        path: path.into(),
        before,
        after,
    })
}
