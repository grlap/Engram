//! Derive a public folder envelope without changing retained task document bytes.

use super::{
    BTreeMap, ExportProfile, FolderExchangeDocument, FolderExportBasis, FolderProjectDocument,
    INCREMENTAL_FORMAT, IncrementalBasis, Result, TaskSyncRefusal, TaskTree, admit_limits,
    documents_fingerprint, pretty,
};

/// The project document of a complete folder export, with its ordered
/// fingerprint for the complete import route and the membership root the next
/// incremental export continues from. Returns the bytes and the fingerprint.
///
/// # Errors
/// Refuses an invalid profile or tree, or a disclosure the project document
/// does not match.
pub(crate) fn complete_envelope(
    tree: &TaskTree,
    profile: &ExportProfile,
    incremental: IncrementalBasis,
) -> Result<(Vec<u8>, String)> {
    profile.options().validate()?;
    if tree.project.notes_included != profile.include_notes
        || incremental.format != INCREMENTAL_FORMAT
    {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder profile and tree disclosure differ")
            .into());
    }
    let fingerprint = documents_fingerprint(
        &tree
            .folder_documents()
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?,
    );
    let envelope = FolderProjectDocument {
        project: tree.project.clone(),
        profile: profile.clone(),
        folder_exchange: FolderExchangeDocument {
            complete: true,
            export_basis: Some(FolderExportBasis {
                inventory_fingerprint: fingerprint.clone(),
            }),
            incremental: Some(incremental),
        },
    };
    let bytes = pretty(&envelope).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok((bytes, fingerprint))
}

pub(crate) fn prepare_folder_documents(
    documents: &BTreeMap<String, Vec<u8>>,
    profile: &ExportProfile,
) -> Result<(BTreeMap<String, Vec<u8>>, String)> {
    profile.options().validate()?;
    let tree = TaskTree::from_stored_documents(documents)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if tree.project.notes_included != profile.include_notes {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder profile and tree disclosure differ")
            .into());
    }
    // The exchange basis describes the same canonical planning content used by
    // the folder reader. Retained raw bytes remain separately authoritative.
    let export_basis = documents_fingerprint(&tree.folder_documents()?);
    let envelope = FolderProjectDocument {
        project: tree.project,
        profile: profile.clone(),
        folder_exchange: FolderExchangeDocument {
            complete: true,
            export_basis: Some(FolderExportBasis {
                inventory_fingerprint: export_basis.clone(),
            }),
            incremental: None,
        },
    };
    let mut published = documents.clone();
    published.insert(
        "project.json".into(),
        pretty(&envelope).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?,
    );
    admit_limits(&published)?;
    Ok((published, export_basis))
}
