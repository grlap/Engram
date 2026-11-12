use super::*;
use engram::{
    FeedId,
    storage::{NativeEvidenceCaptureRequest, NativeEvidenceCursor, NativeEvidenceFeed},
    task_sync::evidence::EvidencePack,
};

#[test]
fn native_capture_pages_stage_original_bytes_only_in_data_checkout() {
    let (owned, marker, folder, home, mut binding) = fixture();
    binding.profile.origin_machine = Some("fixture-host".into());
    let activation = json!({"action":"activate", "project_file":marker, "binding":binding});
    let activated = success(&run(&home, &marker, "step", &activation));
    let binding_id: ObjectId = serde_json::from_value(activated["binding"].clone()).unwrap();
    work(&home, &marker, &["add", "Native capture fixture"]);
    let operation = uuid::Uuid::new_v4().to_string();
    let origin = success(
        &command(&home, &marker)
            .env("ENGRAM_ACTOR_ID", "step-operator")
            .env("ENGRAM_SESSION_ID", "step-session")
            .args(["task-sync", "origin-enroll", "--operation", &operation])
            .output()
            .unwrap(),
    );
    let connection = rusqlite::Connection::open(&binding.store.database).unwrap();
    let (seed, original): (String, Vec<u8>) = connection.query_row(
        "SELECT entry.object_id,object.canonical_json FROM work_feed_entries entry JOIN objects object ON object.object_id=entry.object_id WHERE entry.feed_kind='project' AND entry.feed_id=?1 AND entry.object_kind='work_event' ORDER BY entry.position LIMIT 1",
        [&binding.project.0], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    let through = connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind='project' AND feed_id=?1",
            [&binding.project.0],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let source_before =
        engram::source::inspect(owned.path(), std::time::Duration::from_secs(30)).source_id;
    assert!(source_before.is_some());
    let index_before = fs::read(owned.path().join(".git/index")).unwrap();
    let capture_request = NativeEvidenceCaptureRequest {
        capture: ObjectId::mint(),
        origin: serde_json::from_value(origin["origin_id"].clone()).unwrap(),
        profile: binding.profile.clone(),
        roots: vec![seed.parse().unwrap()],
        feeds: vec![NativeEvidenceFeed {
            feed: FeedId::Project(binding.project.clone()),
            through,
        }],
        maximum_pack_records: 2,
    };
    let request =
        json!({"action":"capture_native", "binding":binding_id, "request":capture_request});
    let mut wrong_profile = request.clone();
    wrong_profile["request"]["profile"] =
        json!({"include_notes":true, "origin_machine":"chosen-host"});
    let refused = run(&home, &marker, "step", &wrong_profile);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("capture disclosure profile differs")
    );
    assert!(!folder.join("evidence").exists());
    let first = success(&run(&home, &marker, "step", &request));
    assert_eq!(first["capture"]["replayed"], false);
    let repeated = success(&run(&home, &marker, "step", &request));
    assert_eq!(repeated["capture"]["replayed"], true);
    assert_eq!(repeated["capture"]["packs"], first["capture"]["packs"]);
    let mut cursor: NativeEvidenceCursor =
        serde_json::from_value(first["capture"]["cursor"].clone()).unwrap();
    let mut staged = Vec::new();
    let mut found_original = false;
    loop {
        let page_request = json!({"action":"export_native", "binding":binding_id, "project_file":marker,
            "branch":"engram-data", "cursor":cursor, "maximum_packs":1});
        let page = success(&run(&home, &marker, "step", &page_request));
        assert_eq!(success(&run(&home, &marker, "step", &page_request)), page);
        let files = page["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let file = &files[0];
        let path = file["path"].as_str().unwrap();
        let bytes = fs::read(folder.parent().unwrap().join(path)).unwrap();
        assert_eq!(
            bytes.len(),
            usize::try_from(file["encoded_bytes"].as_u64().unwrap()).unwrap()
        );
        let pack = EvidencePack::read(bytes.as_slice()).unwrap();
        assert_eq!(pack.pack_id().as_str(), file["pack"].as_str().unwrap());
        for record in pack.records() {
            if record.object_id().as_str() == seed {
                assert_eq!(record.canonical_bytes(), original);
                found_original = true;
            }
        }
        staged.push(file["pack"].clone());
        if page["next"].is_null() {
            break;
        }
        cursor = serde_json::from_value(page["next"].clone()).unwrap();
        assert!(staged.len() <= first["capture"]["packs"].as_array().unwrap().len());
    }
    assert!(found_original);
    assert_eq!(json!(staged), first["capture"]["packs"]);
    assert!(!owned.path().join(".engram").exists());
    assert_eq!(
        fs::read(owned.path().join(".git/index")).unwrap(),
        index_before
    );
    assert!(
        engram::source::inspect(owned.path(), std::time::Duration::from_secs(30))
            .source_id
            .eq(&source_before)
    );
}
