use super::*;
use crate::task_sync::git::read_task_snapshot;

fn pack_path() -> String {
    crate::task_sync::evidence::path::pack(&crate::ObjectId::mint(), &crate::ObjectId::mint())
}

fn write_pack(f: &Fixture, path: &str) {
    let file = f.root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    // The planning reader validates the carrier shape, never the pack body.
    fs::write(file, b"opaque pack bytes, deliberately not JSON").unwrap();
}

#[test]
fn mixed_snapshots_and_evidence_only_deltas_never_open_pack_blobs() {
    let f = Fixture::new(2);
    let base = f.commit();
    let path = pack_path();
    write_pack(&f, &path);
    let mut changed = f.tree.tasks[0].clone();
    changed.title = "task edit beside evidence".into();
    f.write(&changed);
    let mixed = f.commit();
    reads();
    let snapshot = read_task_snapshot(&f.root, &f.tree.project.project_id, &mixed).unwrap();
    assert_eq!(snapshot.tree.tasks.len(), 2);
    assert!(
        reads()
            .iter()
            .all(|path| !path.starts_with(".engram/evidence/"))
    );
    let delta = f.read(&base, &mixed).unwrap();
    assert_eq!(delta.upserts.get(&changed.id), Some(&changed));
    assert_eq!(task_reads(), [Fixture::relative(changed.id)]);
    let second = pack_path();
    for action in ["add", "modify", "delete"] {
        match action {
            "add" => write_pack(&f, &second),
            "modify" => {
                fs::write(f.root.join(&second), b"different opaque bytes").unwrap();
            }
            "delete" => fs::remove_file(f.root.join(&second)).unwrap(),
            _ => unreachable!(),
        }
        let prior = f.text(&["rev-parse", "HEAD"]);
        let incoming = f.commit();
        let identity = f.identity();
        reads();
        let delta = f.read(&prior, &incoming).unwrap();
        assert!(delta.upserts.is_empty() && delta.removed.is_empty());
        assert_eq!(task_reads(), [] as [String; 0]);
        assert_eq!(f.identity(), identity);
    }
}

#[test]
fn evidence_carrier_unknown_names_and_unsafe_modes_still_refuse() {
    for fault in [
        "origin",
        "prefix",
        "mismatch",
        "leaf",
        "depth",
        "unknown",
        "executable",
        "link",
    ] {
        let f = Fixture::new(1);
        let base = f.commit();
        let valid = pack_path();
        let parts = valid.split('/').collect::<Vec<_>>();
        let path = match fault {
            "origin" => valid.replace(parts[2], "not-an-origin"),
            "prefix" => valid.replace(&format!("/{}/", parts[3]), "/zz/"),
            "mismatch" => valid.replace(
                &format!("/{}/", parts[3]),
                if parts[3] == "00" { "/01/" } else { "/00/" },
            ),
            "leaf" => format!("{valid}.extra"),
            "depth" => format!("{valid}/nested.json"),
            "unknown" => ".engram/unexpected/file.json".into(),
            _ => valid.clone(),
        };
        write_pack(&f, &path);
        f.git(&["add", "--all"]);
        if matches!(fault, "executable" | "link") {
            let blob = f.text(&["rev-parse", &format!(":{path}")]);
            f.git(&[
                "update-index",
                "--cacheinfo",
                if fault == "link" { "120000" } else { "100755" },
                &blob,
                &path,
            ]);
        }
        f.git(&["commit", "-m", "Invalid evidence carrier"]);
        let incoming = f.text(&["rev-parse", "HEAD"]);
        reads();
        assert!(f.read(&base, &incoming).is_err(), "{fault}");
        assert!(
            read_task_snapshot(&f.root, &f.tree.project.project_id, &incoming).is_err(),
            "{fault}"
        );
        assert!(
            reads()
                .iter()
                .all(|path| !path.starts_with(".engram/evidence/"))
        );
    }
}

#[test]
fn empty_unknown_evidence_trees_are_visible_to_delta_shape_validation() {
    let f = Fixture::new(1);
    let base = f.commit();
    let tree = |bytes: &[u8]| {
        String::from_utf8(
            crate::task_sync::git::query(
                &f.root,
                &["mktree"],
                false,
                Some(bytes),
                Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap()
        .trim()
        .to_owned()
    };
    let empty = tree(b"");
    let evidence = tree(format!("040000 tree {empty}\tnot-an-origin\n").as_bytes());
    let mut entries = f.git(&["ls-tree", &format!("{base}:.engram")]);
    entries.extend_from_slice(format!("040000 tree {evidence}\tevidence\n").as_bytes());
    let metadata = tree(&entries);
    let root = tree(format!("040000 tree {metadata}\t.engram\n").as_bytes());
    let incoming = f.text(&[
        "commit-tree",
        &root,
        "-p",
        &base,
        "-m",
        "Retain empty unknown directory",
    ]);
    assert!(f.read(&base, &incoming).is_err());
    assert!(read_task_snapshot(&f.root, &f.tree.project.project_id, &incoming).is_err());
}
