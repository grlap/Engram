//! The operator `graph save` and `graph load` words and their snapshot-file
//! publication rules.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use engram::{
    LocalWorkService, ProjectId, SessionId, WorkGraphSnapshotCut, WorkGraphSnapshotDestinationKind,
};

use crate::{GraphCommand, WorkContext};

mod publication;
use publication::{GraphSnapshotWriteOutcome, write_graph_snapshot_file};

use super::attribution::{ShellWorkAttribution, resolve_shell_work_attribution};

const MAX_GRAPH_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) fn run_graph_from_cli(
    database: PathBuf,
    project_id: ProjectId,
    actor_id: Option<String>,
    session_id: Option<String>,
    actor_context: Option<String>,
    source_skill: Option<String>,
    operation: GraphCommand,
) -> Result<()> {
    let attribution = resolve_shell_work_attribution(actor_id, session_id)?;
    run_graph(
        WorkContext {
            database,
            project_id,
            actor_id: attribution.actor_id,
            session_id: SessionId(attribution.session_id),
            actor_context,
            attribution_defaults: attribution.defaults,
            source_skill,
        },
        operation,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the operator graph dispatcher keeps save disclosure and load recreation sequencing explicit"
)]
fn run_graph(context: WorkContext, operation: GraphCommand) -> Result<()> {
    match operation {
        GraphCommand::Save {
            out,
            stdout,
            include_restricted,
            reason,
        } => {
            if include_restricted != reason.is_some() {
                bail!("--include-restricted and --reason must be supplied together");
            }
            let attribution = ShellWorkAttribution {
                actor_id: context.actor_id.clone(),
                session_id: context.session_id.0.clone(),
                defaults: context.attribution_defaults,
            };
            let database = context.database.clone();
            let destination_kind = if stdout {
                WorkGraphSnapshotDestinationKind::Stdout
            } else if out.is_some() {
                WorkGraphSnapshotDestinationKind::File
            } else {
                WorkGraphSnapshotDestinationKind::DefaultFile
            };
            let service = LocalWorkService::new_with_attribution(
                context.database,
                context.project_id,
                context.actor_id,
                context.session_id,
                context.source_skill,
                context.actor_context,
                context.attribution_defaults,
            );
            let export = service.save_work_graph_snapshot(
                reason.as_deref(),
                destination_kind,
                chrono::Utc::now(),
            )?;
            let mut bytes = serde_json::to_vec_pretty(&export.document)?;
            bytes.push(b'\n');

            // All notices are deliberately delayed until the disclosure audit
            // above has committed. A failed audit therefore prints no bytes.
            attribution.print_notices();
            eprintln!("REDACTOR: {}", export.redactor_status);
            if include_restricted {
                let widened_memories = export
                    .document
                    .body
                    .memories
                    .iter()
                    .filter(|memory| {
                        matches!(
                            &memory.state,
                            engram::WorkGraphSnapshotMemoryState::Active {
                                sensitivity: engram::Sensitivity::Restricted,
                                ..
                            }
                        )
                    })
                    .count();
                // Widening reasons are store-validated (no control/format
                // characters) before this notice. This is not the error/refusal
                // renderer and is not a second character table.
                eprintln!(
                    "WARNING: --include-restricted widened {widened_memories} restricted project-memory bod{} into this disclosure because: {}",
                    if widened_memories == 1 { "y" } else { "ies" },
                    reason.as_deref().unwrap_or_default()
                );
            }
            if stdout {
                let mut output = io::stdout().lock();
                output.write_all(&bytes)?;
                output.flush()?;
            } else {
                let out = match out {
                    Some(out) => out,
                    None => graph_snapshot_default_path(
                        &database,
                        &export.document.body.summary.as_of,
                        &export.body_sha256,
                    )?,
                };
                let outcome =
                    write_graph_snapshot_file(&database, &out, &bytes).with_context(|| {
                        format!(
                            "failed to save snapshot to requested destination {}",
                            out.display()
                        )
                    })?;
                match outcome {
                    GraphSnapshotWriteOutcome::Saved => println!("{}", out.display()),
                    GraphSnapshotWriteOutcome::AlreadySaved => {
                        println!("already saved: {}", out.display());
                    }
                }
            }
        }
        GraphCommand::Load { file, dry_run } => {
            let metadata = fs::metadata(&file)
                .with_context(|| format!("failed to inspect snapshot {}", file.display()))?;
            if metadata.len() > MAX_GRAPH_SNAPSHOT_BYTES {
                bail!(
                    "snapshot {} exceeds the {MAX_GRAPH_SNAPSHOT_BYTES}-byte load limit",
                    file.display()
                );
            }
            let bytes = fs::read(&file)
                .with_context(|| format!("failed to read snapshot {}", file.display()))?;
            let service = LocalWorkService::new_with_attribution(
                context.database,
                context.project_id,
                context.actor_id,
                context.session_id,
                context.source_skill,
                context.actor_context,
                context.attribution_defaults,
            );
            let result = service.load_work_graph_snapshot(&bytes, dry_run, chrono::Utc::now())?;
            if dry_run {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "loaded {} work items, {} records, and {} memories from {}",
                    result.preview.summary.section_counts.items,
                    result.preview.summary.section_counts.records,
                    result.preview.summary.section_counts.memories,
                    file.display()
                );
            }
        }
    }
    Ok(())
}

fn graph_snapshot_default_path(
    database: &Path,
    cut: &WorkGraphSnapshotCut,
    body_sha256: &engram::ObjectId,
) -> Result<PathBuf> {
    let (home, digest) = engram_home_and_project_digest(database)?;
    let body_prefix = body_sha256
        .as_str()
        .get(..12)
        .context("snapshot body digest is shorter than twelve hex digits")?;
    Ok(home.join("snapshots").join(digest).join(format!(
        "graph-{}-{}-{body_prefix}.json",
        cut.work_feed, cut.project_memory
    )))
}

pub(crate) fn engram_home_and_project_digest(database: &Path) -> Result<(&Path, &std::ffi::OsStr)> {
    let project_dir = database
        .parent()
        .context("store path has no project directory")?;
    let digest = project_dir
        .file_name()
        .context("store path has no project digest")?;
    let home = project_dir
        .parent()
        .and_then(Path::parent)
        .context("store path is not below an Engram home")?;
    Ok((home, digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram::graph_snapshot_files_are_equivalent;

    #[test]
    fn graph_default_path_uses_the_project_digest_and_cut() {
        let database = Path::new("engram-home")
            .join("projects")
            .join("project-digest")
            .join("engram.db");
        let cut = WorkGraphSnapshotCut {
            work_feed: 17,
            project_memory: 4,
        };
        let body_sha256 = engram::CanonicalObject::freeze(&serde_json::json!({
            "derived": "snapshot body"
        }))
        .expect("canonical fixture")
        .key()
        .clone();
        let body_prefix = body_sha256.as_str().get(..12).expect("body hash prefix");
        assert_eq!(
            graph_snapshot_default_path(&database, &cut, &body_sha256)
                .expect("default snapshot path"),
            Path::new("engram-home")
                .join("snapshots")
                .join("project-digest")
                .join(format!("graph-17-4-{body_prefix}.json"))
        );
    }

    #[test]
    fn graph_snapshot_writer_refuses_duplicate_member_destinations() {
        let directory = crate::test_support::temp_home().expect("tempdir");
        let database = directory.path().join("projects/digest/engram.db");
        fs::create_dir_all(database.parent().expect("database parent")).expect("project directory");
        let output = directory.path().join("snapshots/graph.json");
        fs::create_dir_all(output.parent().expect("snapshot parent")).expect("directory");
        let bytes = br#"{"body":{"items":[]},"manifest":{"exported_at":"first","exporting_build":"build"}}"#;
        let text = std::str::from_utf8(bytes).expect("fixture text");
        for poisoned in [
            text.replacen('{', "{\"body\":null,", 1),
            text.replacen("\"items\":", "\"items\":null,\"items\":", 1),
        ] {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&poisoned).expect("collapsed JSON"),
                serde_json::from_slice::<serde_json::Value>(bytes).expect("original JSON")
            );
            fs::write(&output, &poisoned).expect("poisoned destination");
            let error = write_graph_snapshot_file(&database, &output, bytes)
                .expect_err("duplicate destination is not an already-saved snapshot");
            assert!(error.to_string().contains("different bytes"));
            assert_eq!(
                fs::read(&output).expect("existing bytes"),
                poisoned.as_bytes()
            );
            assert!(!graph_snapshot_files_are_equivalent(
                poisoned.as_bytes(),
                poisoned.as_bytes()
            ));
        }
        let varied = text
            .replace("first", "second")
            .replace("\"build\"", "\"other\"");
        assert!(graph_snapshot_files_are_equivalent(
            bytes,
            varied.as_bytes()
        ));
    }

    #[test]
    fn graph_snapshot_writer_never_replaces_store_or_destination_bytes() {
        let directory = crate::test_support::temp_home().expect("tempdir");
        let home = directory.path().join("engram-home");
        let project_dir = home.join("projects").join("project-digest");
        fs::create_dir_all(&project_dir).expect("project directory");
        let database = project_dir.join("engram.db");
        fs::write(&database, b"canonical-store").expect("store fixture");
        let wal = project_dir.join("engram.db-wal");
        fs::write(&wal, b"committed-wal").expect("wal fixture");
        let output = home
            .join("snapshots")
            .join("project-digest")
            .join("graph.json");

        let store_error = write_graph_snapshot_file(&database, &database, b"snapshot")
            .expect_err("active store destination");
        assert!(
            store_error
                .to_string()
                .contains("outside Engram's project stores")
        );
        let wal_error =
            write_graph_snapshot_file(&database, &wal, b"snapshot").expect_err("wal destination");
        assert!(
            wal_error
                .to_string()
                .contains("outside Engram's project stores")
        );
        assert_eq!(
            fs::read(&database).expect("store preserved"),
            b"canonical-store"
        );
        assert_eq!(fs::read(&wal).expect("wal preserved"), b"committed-wal");

        let snapshot = br#"{"body":{},"manifest":{"exported_at":"now","exporting_build":"build"}}"#;
        assert_eq!(
            write_graph_snapshot_file(&database, &output, snapshot).expect("publish snapshot"),
            GraphSnapshotWriteOutcome::Saved
        );
        assert_eq!(
            write_graph_snapshot_file(&database, &output, snapshot).expect("identical retry"),
            GraphSnapshotWriteOutcome::AlreadySaved
        );
        let replacement_error = write_graph_snapshot_file(&database, &output, b"different\n")
            .expect_err("different replacement");
        assert!(replacement_error.to_string().contains("different bytes"));
        assert_eq!(fs::read(&output).expect("snapshot preserved"), snapshot);
        let staged_files = fs::read_dir(output.parent().expect("snapshot parent"))
            .expect("snapshot directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".graph-save-"))
            .count();
        assert_eq!(staged_files, 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output)
                    .expect("snapshot metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
