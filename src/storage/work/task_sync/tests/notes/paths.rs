use super::*;

fn glued_paths() -> Vec<String> {
    let glues = [
        "", " ", "=", ":", "::", "@", "-", "_", "--opt=", "'", "\"", "`", "(", "[", ",", ".", "%",
        "/", "x/",
    ];
    let paths = [
        "/abs",
        "~/x",
        "$VAR/x",
        "%VAR%\\x",
        "C:\\x",
        "C:x",
        "C:/x",
        "file:x",
        "file:///x",
        "\\\\server\\share",
        "user@host:/x",
    ];
    glues
        .iter()
        .flat_map(|glue| paths.iter().map(move |path| format!("{glue}{path}")))
        .collect()
}

#[test]
fn task_sync_notes_plain_labels_and_principals_survive_glue_detection() {
    for plain in [
        "Decision: ok",
        "Note: fixed",
        "Result: pass",
        "Engram::Fable",
        "Termal::Fable2",
        "Engram::Coder",
        "greg/claude",
        "pass/fail",
        "and/or",
        "read/write",
        "read/write.",
        "and/or.",
        "pass/fail.",
    ] {
        assert_eq!(
            crate::task_sync::notes::prose_exclusion(plain),
            None,
            "{plain}"
        );
        assert_eq!(
            crate::task_sync::notes::author_exclusion(plain),
            None,
            "{plain}"
        );
        let mut tree = note_candidate();
        tree.tasks[0].notes[0].body = plain.into();
        tree.tasks[0].notes[0].author.actor_id = plain.into();
        tree.validate().unwrap();
    }
    // A dot-bearing slash sequence is a documented path shape, even as a version.
    assert_eq!(
        crate::task_sync::notes::prose_exclusion("1.2/3"),
        Some("repository or evidence path")
    );
    assert_eq!(
        crate::task_sync::notes::author_exclusion("org.example/codex"),
        None
    );
}

#[test]
fn task_sync_notes_quoted_and_relative_paths_are_omitted_and_refused_on_import() {
    let mut paths = [
        "`/home/operator/file`",
        "`src/module.rs`",
        "`~/.ssh/id`",
        "`$HOME/private`",
        "`\\\\server\\share`",
        "`file:///etc/private`",
        "**/home/operator/file**",
        "“/home/operator/file”",
        "Ωsrc/module.rs",
        "./src/module.rs",
        "./target/debug",
        "../secret",
        "|docs/design.md|",
        ">evidence/report",
        "*scratch/local*",
        "_/home/operator/file_",
        "__src/module.rs__",
        "_\\\\server\\share_",
        "_$HOME/private_",
        ".\\private\\report",
        "..\\private\\report",
        "\\Users\\operator\\file",
        "$env:HOME\\file",
        "%HOME%\\private",
        "%HOMEPATH%",
        "%APPDATA%",
        "%LOCALAPPDATA%",
        "%HOMEDRIVE%\\private",
        "$env:APPDATA\\private",
        "$env:LOCALAPPDATA\\private",
        "${env:HOME}\\private",
        "${env:USERPROFILE}\\private",
        "folder/report.txt",
        "folder/report.txt.",
        "_folder/report.txt_",
        "**folder/report.txt**",
        "‘folder/report.txt’",
        "https://example.invalid/report",
        "%CUSTOM_HOME%",
        "$CUSTOM_HOME/report",
        "~operator",
        "C:private",
        "progress\\fragment",
        "src/x.rs",
        "a/b/c",
        "folder/",
        "/folder",
        "root=/private",
        "root=$HOME/private",
        "cache=%CUSTOM_HOME%",
        "drive=C:private",
        "--out=/srv",
        "at=D:/data",
        "dir=~/notes",
        "dir=$HOME/notes",
        "x=%APPDATA%",
        "dir=%APPDATA%/engram",
        "op@buildhost:/srv",
        "at@/srv",
        "at+/srv",
        "at#/srv",
        "at«/srv",
        "at→/srv",
        "at::/srv",
        "at::C:private",
        "at::file:private",
        "cache--/srv",
        "-o/srv",
        "-L/opt",
        "build-$HOME/notes",
        "CFLAGS=-I$HOME/include",
        "pre-%APPDATA%",
        "x-~/notes",
        "a_/srv",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    let matrix = glued_paths();
    let matrix_count = matrix.len();
    paths.extend(matrix);
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("quoted-paths", "notes", 0);
    request.notes = vec![
        "Decision: ok Note: fixed Result: pass Engram::Fable Termal::Fable2 Engram::Coder Ω pass/fail read/write and/or for w-abc123def456".into(),
    ];
    let item = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    for (index, path) in paths.iter().enumerate() {
        source
            .record_work_observation(
                &RecordWorkObservationRequest {
                    status: index % 2 == 0,
                    project_id: item.project_id.clone(),
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    session_id: SessionId("peer".into()),
                    summary: format!("See {path}"),
                    refs: vec![],
                    actor: crate::domain::non_holder_note_actor(actor("peer")),
                    idempotency_key: format!("path-{index}"),
                    recorded_at: at(1),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let exported = source
        .export_task_tree_with_notes(&item.project_id, "host-A")
        .unwrap();
    assert_eq!(exported.tree.tasks[0].notes.len(), 1);
    assert_eq!(
        exported.omissions.counts.values().sum::<usize>(),
        paths.len()
    );
    assert_eq!(
        exported.omissions.counts["machine or user path"],
        56 + matrix_count
    );
    assert_eq!(exported.omissions.counts["repository or evidence path"], 18);
    let documents = exported.tree.documents().unwrap();
    let shared = documents
        .values()
        .flat_map(|bytes| bytes.iter().copied())
        .collect::<Vec<_>>();
    let shared = String::from_utf8(shared).unwrap();
    let ids = source
        .connection
        .prepare("SELECT object_id FROM objects WHERE object_kind = 'work_observation'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let retained = exported.tree.tasks[0].notes[0].id.as_str();
    assert_eq!(ids.len(), paths.len() + 1);
    for id in ids.iter().filter(|id| id.as_str() != retained) {
        assert!(!shared.contains(id));
    }
    for path in paths {
        // JSON escaping must not hide a disclosure from this assertion.
        let escaped = serde_json::to_string(&format!("See {path}")).unwrap();
        assert!(!shared.contains(&escaped));
        let mut candidate = note_candidate();
        candidate.tasks[0].notes[0].body = format!("See {path}");
        let mut target = SqliteStore::open_in_memory().unwrap();
        let before = state(&target);
        for dry in [true, false] {
            let error = target
                .import_task_tree(
                    &candidate.project.project_id,
                    &actor("importer"),
                    &candidate,
                    dry,
                    at(5),
                )
                .unwrap_err()
                .to_string();
            assert!(
                error == "local work input is invalid: shared note refused: machine or user path"
                    || error
                        == "local work input is invalid: shared note refused: repository or evidence path"
            );
            assert!(!error.contains(&path));
            assert_eq!(state(&target), before);
        }
    }
}

#[test]
fn task_sync_notes_keep_actor_principals_but_refuse_path_bearing_authors() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("author-paths", "notes", 0);
    request.actor = actor("greg/codex");
    request.notes = vec!["Plain progress".into()];
    let item = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let exported = source
        .export_task_tree_with_notes(&item.project_id, "host-A")
        .unwrap();
    assert!(exported.omissions.counts.is_empty());
    assert_eq!(
        exported.tree.tasks[0].notes[0].author.actor_id,
        "greg/codex"
    );
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &exported.tree);
    assert_eq!(
        target
            .export_task_tree_with_notes(&item.project_id, "host-B")
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        exported.tree.documents().unwrap()
    );
    let mut authors = [
        "src/module.rs",
        "_/home/operator/file_",
        "prefix\\private",
        "%CUSTOM_HOME%",
        "C:private",
        "root=/private",
        "root=$HOME/private",
        "cache=%CUSTOM_HOME%",
        "drive=C:private",
        "op@buildhost:/srv",
        "at::C:private",
        "at::file:private",
        "cache--/srv",
        "-o/srv",
        "-L/opt",
        "build-$HOME/notes",
        "CFLAGS=-I$HOME/include",
        "pre-%APPDATA%",
        "x-~/notes",
        "a_/srv",
        "x./home/operator/secret",
        "x%/Users/operator/AppData",
        ".ssh/id_rsa",
        "greg/.ssh/id_rsa",
        "x/home/operator/secret",
        "team/Users/operator/AppData",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    authors.extend(glued_paths());
    for author in authors {
        let mut candidate = exported.tree.clone();
        candidate.tasks[0].notes[0].author.actor_id = author.clone();
        let mut target = SqliteStore::open_in_memory().unwrap();
        let before = state(&target);
        for dry in [true, false] {
            assert!(
                target
                    .import_task_tree(&item.project_id, &actor("importer"), &candidate, dry, at(5))
                    .is_err()
            );
            assert_eq!(state(&target), before);
        }
        let mut source = SqliteStore::open_in_memory().unwrap();
        request.actor = actor(&author);
        let item = source
            .create_work(&request, &crate::DevelopmentNoopRedactor)
            .unwrap();
        let omitted = source
            .export_task_tree_with_notes(&item.project_id, "host-A")
            .unwrap();
        assert_eq!(omitted.tree.tasks[0].notes, [] as [SharedNote; 0]);
        assert_eq!(omitted.omissions.counts.values().sum::<usize>(), 1);
    }
}
