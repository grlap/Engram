use super::*;
use sha2::{Digest, Sha256};

async fn git(repository: &Path, args: &[&str]) -> Vec<u8> {
    local(
        repository,
        args,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap()
}

async fn commit(repository: &Path, message: &str) -> String {
    git(
        repository,
        &[
            "-c",
            "user.name=Exchange Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            message,
        ],
    )
    .await;
    String::from_utf8(git(repository, &["rev-parse", "HEAD"]).await)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn retention_pins_exact_commit_after_branch_advance_and_refuses_replacement() {
    let home = crate::test_support::temp_home().unwrap();
    let repository = home.path();
    git(repository, &["init", "--initial-branch=main", "."]).await;
    let first = commit(repository, "exact message\n\nsecond paragraph").await;
    let bytes = git(repository, &["cat-file", "commit", &first]).await;
    let operation = uuid::Uuid::new_v4();
    let retained = retain_commit(
        repository,
        operation,
        &first,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        retained.raw_bytes_fingerprint.as_str(),
        format!("{:x}", Sha256::digest(&bytes))
    );
    let second = commit(repository, "later source revision").await;
    verify_retained_commit(
        repository,
        &retained,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        git(repository, &["cat-file", "commit", &retained.retention_ref]).await,
        bytes
    );
    assert_eq!(
        retain_commit(
            repository,
            operation,
            &first,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap(),
        retained
    );
    assert!(
        retain_commit(
            repository,
            operation,
            &second,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(
        git(repository, &["rev-parse", "HEAD"]).await,
        format!("{second}\n").as_bytes()
    );
    assert_eq!(
        git(repository, &["status", "--porcelain"]).await,
        [] as [u8; 0]
    );
    // Reopening after the only pin disappeared must preserve uncertainty, even
    // though Git can still find the unreachable object in its object database.
    git(
        repository,
        &["update-ref", "-d", &retained.retention_ref, &first],
    )
    .await;
    assert!(
        verify_retained_commit(
            repository,
            &retained,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert!(
        retain_commit(
            repository,
            operation,
            &first,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .is_err(),
        "retry recreated a missing retained ref"
    );
    assert_eq!(
        git(
            repository,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &retained.retention_ref
            ]
        )
        .await,
        [] as [u8; 0]
    );
}
