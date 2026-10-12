//! Current-path preflight. Materialization must recheck every actual task path.

use super::repository::{git, ordinary_ancestors, read_optional};
use anyhow::{Result, ensure};
use std::{fs, path::Path};

pub(super) fn check(root: &Path, admin: &Path, marker_name: &str) -> Result<()> {
    // A deeper file would override the proposed managed root block. Refuse
    // occupancy, including ignored files, before Git reads it or runs status.
    let nested = root.join(".engram/.gitattributes");
    ordinary_ancestors(&nested)?;
    ensure!(
        fs::symlink_metadata(&nested).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "nested attribute file is occupied: .engram/.gitattributes"
    );
    // Inspect ordinary bounded files before Git's attribute reader accesses them.
    // In particular, status must not recurse into a directory named .gitignore
    // and apply an unchecked filter to one of its children.
    read_optional(&root.join(".gitignore"))?;
    read_optional(&root.join(".gitattributes"))?;
    let info = admin.join("info/attributes");
    ordinary_ancestors(&info)?;
    match fs::symlink_metadata(&info) {
        Ok(_) => {
            read_optional(&info)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let paths = [
        ".engram/project.json",
        ".engram/tasks/00000000-0000-7000-8000-000000000001.json",
        ".gitignore",
        ".gitattributes",
        marker_name,
    ];
    let mut args = vec![
        "check-attr",
        "-z",
        "filter",
        "diff",
        "merge",
        "working-tree-encoding",
        "eol",
        "ident",
        "--",
    ];
    args.extend(paths);
    let output = git(root, &args)?;
    let fields: Vec<_> = output.split(|byte| *byte == 0).collect();
    ensure!(
        fields.last() == Some(&&b""[..]) && (fields.len() - 1) % 3 == 0,
        "malformed Git attribute result"
    );
    ensure!(
        fields.len() - 1 == paths.len() * 6 * 3,
        "incomplete Git attribute result"
    );
    for record in fields[..fields.len() - 1].as_chunks::<3>().0 {
        let path = std::str::from_utf8(record[0])?;
        let attribute = std::str::from_utf8(record[1])?;
        let value = std::str::from_utf8(record[2])?;
        let safe = matches!(value, "unset" | "unspecified")
            || matches!(
                (attribute, value),
                ("eol", "lf") | ("diff" | "merge", "set")
            );
        ensure!(
            safe,
            "Git attribute {attribute}={value} applies to {path}; inspect and resolve before setup preparation"
        );
    }
    Ok(())
}
