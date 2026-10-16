//! Select an admitted journal or capture one immutable prepared candidate.

use super::{
    Attribution, Directory, MaterializeSelection, Path, Result, candidate, ensure, journal,
    progress, repository,
};

pub(super) fn select(
    root: &Directory,
    admin: &Directory,
    setup: super::super::Intent,
    selection: &MaterializeSelection,
    enabled: bool,
    attribution: Attribution,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<(Directory, journal::Record, bool)> {
    let private_name = format!(
        "engram-task-sync-materialize-{}-{}",
        selection.setup_operation, selection.prepared_operation
    );
    match admin.dir().symlink_metadata(&private_name) {
        Ok(_) => {
            let private = admin.child(Path::new(&private_name))?;
            let record = journal::read(&private, &setup.basis.git_directory.join(&private_name))?;
            ensure!(
                record.selection == *selection && record.basis == setup.basis,
                "materialization selection differs from admitted operation"
            );
            Ok((private, record, true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                enabled,
                "task sync is not enabled; completed setup preparation does not admit a new materialization"
            );
            let current = repository::inspect(
                &setup.basis.project_file,
                &setup.basis.project_id,
                &selection.repository,
                &setup.basis.branch,
                setup.basis.assertions.clone(),
            )?;
            ensure!(
                current == setup.basis,
                "setup basis changed before materialization"
            );
            let prepared = progress::read_prepared(
                &selection.progress_directory,
                &progress::PreparedSelection {
                    operation: selection.prepared_operation,
                    destination: selection.candidate_directory.clone(),
                    project: setup.basis.project_id.clone(),
                    profile: progress::ExportProfile {
                        include_notes: setup.basis.assertions.include_notes,
                        origin_machine: setup.basis.assertions.origin_machine.clone(),
                    },
                },
            )?;
            candidate::disclose(&prepared.documents)?;
            let index = repository::index_basis(&setup.basis)?;
            repository::recheck(&setup.basis, &index)?;
            checkpoint("admitted-before-private")?;
            let private = admin.create_child(Path::new(&private_name))?;
            checkpoint("private-created")?;
            let record = journal::initialize(
                &private,
                root,
                admin,
                setup.basis,
                selection.clone(),
                attribution,
                prepared,
                index,
                checkpoint,
            )?;
            checkpoint("journal-published")?;
            Ok((private, record, false))
        }
        Err(error) => Err(error.into()),
    }
}
