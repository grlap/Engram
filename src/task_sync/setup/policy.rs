//! Narrow proposed setup edits; all unrelated bytes remain untouched.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

const BEGIN: &str = "# BEGIN ENGRAM TASK SYNC";
const END: &str = "# END ENGRAM TASK SYNC";
const IGNORE: &str =
    "/*\n!/.gitignore\n!/.gitattributes\n!/project.json\n!/tasks/\n/tasks/*\n!/tasks/*.json\n";
const ATTRIBUTES: &str = "* text eol=lf -filter diff merge -working-tree-encoding -ident\n";

fn ignore() -> String {
    let id = "[0-9a-f]".repeat(32);
    let stored_id = "[0-9a-f]".repeat(64);
    let shard = "[0-9a-f]".repeat(2);
    // Admit only the carrier's UUID directories and JSON leaves. The evidence
    // reader still verifies shard agreement, ordinary modes and pack contents.
    format!(
        "{IGNORE}!/memories/\n/memories/*\n!/memories/*/\n/memories/*/*\n!/memories/*/head.json\n!/memories/*/{id}.json\n!/memories/*/{stored_id}.json\n!/evidence/\n/evidence/*\n!/evidence/{id}/\n/evidence/*/*\n!/evidence/{id}/{shard}/\n/evidence/*/*/*\n!/evidence/{id}/{shard}/{id}.json\n"
    )
}

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
    if path == ".gitignore" {
        after.extend_from_slice(ignore().as_bytes());
    } else {
        after.extend_from_slice(ATTRIBUTES.as_bytes());
    }
    after.extend_from_slice(format!("{END}\n").as_bytes());
    Ok(Edit {
        path: path.into(),
        before,
        after,
    })
}

// Prepared intents own their exact proposed bytes. Extending new allowlists
// must not invalidate an already prepared, unpublished operation.
pub(super) fn validate_recorded(edit: &Edit) -> Result<bool> {
    let current = propose(&edit.path, edit.before.clone())?;
    if current == *edit {
        return Ok(true);
    }
    if edit.path != ".gitignore" {
        return Ok(false);
    }
    let text = String::from_utf8(current.after)?;
    let (prefix, block) = text.split_at(
        text.find(BEGIN)
            .ok_or_else(|| anyhow::anyhow!("managed policy block missing"))?,
    );
    let id = "[0-9a-f]".repeat(32);
    let stored_id = "[0-9a-f]".repeat(64);
    let without_stored = block.replace(&format!("!/memories/*/{stored_id}.json\n"), "");
    let before_memories = without_stored.replace(&format!("!/memories/\n/memories/*\n!/memories/*/\n/memories/*/*\n!/memories/*/head.json\n!/memories/*/{id}.json\n"), "");
    Ok(
        edit.after == format!("{prefix}{without_stored}").into_bytes()
            || edit.after == format!("{prefix}{before_memories}").into_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_sync_generated_policy_admits_both_stored_identity_shapes() {
        let owned = crate::test_support::temp_home().unwrap();
        let root = owned.path().join("policy-repository");
        std::fs::create_dir(&root).unwrap();
        let query = |args: &[&str], missing, input: Option<&[u8]>| {
            crate::task_sync::git::query(
                &root,
                args,
                missing,
                input,
                crate::operation_time::MINIMUM,
            )
        };
        query(&["init", "-q", "--initial-branch=main"], false, None).unwrap();
        let directory = root.join(".engram/memories/rule");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            root.join(".engram/.gitignore"),
            propose(".gitignore", None).unwrap().after,
        )
        .unwrap();
        let ids = [
            crate::ObjectId::mint(),
            crate::ObjectId::from_canonical_bytes(b"historical policy identity"),
        ];
        let mut input = Vec::new();
        for name in
            std::iter::once("head.json".to_owned()).chain(ids.iter().map(|id| format!("{id}.json")))
        {
            std::fs::write(directory.join(&name), "fixture").unwrap();
            input.extend_from_slice(format!(".engram/memories/rule/{name}\0").as_bytes());
        }
        assert!(
            query(
                &["check-ignore", "--no-index", "-z", "--stdin"],
                true,
                Some(&input)
            )
            .unwrap()
            .is_empty()
        );
        let unowned = b".engram/memories/rule/unrelated.json\0";
        assert_eq!(
            query(
                &["check-ignore", "--no-index", "-z", "--stdin"],
                true,
                Some(unowned)
            )
            .unwrap(),
            unowned.to_vec()
        );
    }

    #[test]
    fn memory_sync_policy_keeps_prepared_intent_bytes_admissible() {
        let current = propose(".gitignore", Some(b"# operator rule\n".to_vec())).unwrap();
        assert!(validate_recorded(&current).unwrap());
        let id = "[0-9a-f]".repeat(32);
        let stored = "[0-9a-f]".repeat(64);
        let old = String::from_utf8(current.after.clone())
            .unwrap()
            .replace(&format!("!/memories/*/{stored}.json\n"), "");
        let mut recorded = current.clone();
        recorded.after = old.as_bytes().to_vec();
        assert!(validate_recorded(&recorded).unwrap());
        recorded.after = old.replace(&format!("!/memories/\n/memories/*\n!/memories/*/\n/memories/*/*\n!/memories/*/head.json\n!/memories/*/{id}.json\n"), "").into_bytes();
        assert!(validate_recorded(&recorded).unwrap());
        recorded.after.extend_from_slice(b"unexpected\n");
        assert!(!validate_recorded(&recorded).unwrap());
    }
}
