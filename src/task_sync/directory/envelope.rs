//! Derive a public folder envelope without changing retained task document bytes.

use super::{
    BTreeMap, ExportProfile, FolderExchangeDocument, FolderExportBasis, FolderProjectDocument,
    Result, TaskSyncRefusal, TaskTree, admit_limits, documents_fingerprint, pretty,
};

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
        memory_profile: None,
        project: tree.project,
        profile: profile.clone(),
        folder_exchange: FolderExchangeDocument {
            complete: true,
            export_basis: FolderExportBasis {
                inventory_fingerprint: export_basis.clone(),
            },
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
