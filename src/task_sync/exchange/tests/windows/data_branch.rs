//! Real HTTPS transport and data-worktree integration, in an isolated child.

use super::*;

mod evidence;
mod fixture;
mod incoming;
mod orphan;
mod outgoing;
#[path = "data_branch/diagnostics.rs"]
mod timing;
mod workflow;

#[tokio::test]
async fn task_exchange_windows_data_branch_uses_confined_https_and_exact_retained_inputs() {
    supervise("transport").await;
}

#[tokio::test]
async fn task_exchange_windows_strict_delta_uses_real_https_rejection_and_offline_replay() {
    supervise_phases("strict-recover").await;
}

#[tokio::test]
async fn task_exchange_windows_delta_revalidation_refuses_captured_head_drift() {
    supervise("drift").await;
}

#[tokio::test]
async fn task_exchange_windows_publication_observation_reads_the_selected_remote_tip() {
    supervise("observation").await;
}

#[tokio::test]
async fn task_exchange_windows_retry_observation_reads_the_selected_remote_tip() {
    supervise_phases("retry-observation").await;
}

#[tokio::test]
async fn task_exchange_windows_incoming_delta_uses_public_retention_and_offline_replay() {
    supervise("incoming").await;
}

#[tokio::test]
async fn task_exchange_windows_incoming_evidence_applies_through_the_public_step_and_replays_offline()
 {
    supervise("evidence").await;
}

#[tokio::test]
async fn task_exchange_windows_mismatched_evidence_selections_stop_and_a_later_request_applies() {
    supervise("evidence-mismatch").await;
}

#[tokio::test]
async fn task_exchange_windows_rejected_fetched_carrier_has_audited_recovery() {
    supervise("orphan-rejected").await;
}

#[tokio::test]
async fn task_exchange_windows_superseded_fetched_binding_has_audited_recovery() {
    supervise("orphan-superseded").await;
}

#[tokio::test]
async fn task_exchange_windows_unrequested_fetch_requires_explicit_abandonment() {
    supervise("orphan-unrequested").await;
}

/// Each child's supervisor bounds one real HTTPS workflow on a host whose Git,
/// TLS and Node processes it does not share with another such workflow. The
/// harness may run several of these tests at once, so they take turns here;
/// a failed child releases its turn without poisoning the others.
static CHILDREN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn supervise(case: &str) {
    let _turn = CHILDREN.lock().await;
    child(&Fixture::new(), case, None).await;
}

/// A strict publication into its actual refusal, then, only after that child
/// succeeded, a second child over the same fixture that requires the exact
/// handoff the first wrote. Each runs under its own supervisor.
async fn supervise_phases(recovery: &str) {
    let _turn = CHILDREN.lock().await;
    let fixture = Fixture::new();
    child(&fixture, "publish-into-refusal", None).await;
    let digest = outgoing::handoff_digest(&fixture.0);
    child(&fixture, recovery, Some(&digest)).await;
}

async fn child(fixture: &Fixture, case: &str, handoff: Option<&str>) {
    let mut command = fixture.isolated_config_command_for(
        "task_sync::exchange::tests::windows::data_branch::data_branch_fixture",
    );
    command.env("ENGRAM_DATA_BRANCH_CASE", case);
    command.env("ENGRAM_EXCHANGE_PHASE_TRACE", "1");
    if let Some(digest) = handoff {
        outgoing::require(&mut command, digest);
    }
    let output = diagnostics::supervise(command, supervisor_limits(), &Cancellation::default())
        .await
        .unwrap_or_else(|report| panic!("isolated HTTPS data workflow {case} failed:\n{report}"));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("data-branch-workflow complete"),
        "isolated HTTPS data workflow {case} did not complete"
    );
}

#[test]
#[ignore = "isolated subprocess entry; exercised by the confined HTTPS workflow"]
fn data_branch_fixture() {
    let _phase = timing::phase("child-entry");
    let root = PathBuf::from(std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            match std::env::var("ENGRAM_DATA_BRANCH_CASE").unwrap().as_str() {
                "transport" => workflow::run(&root).await,
                "publish-into-refusal" => {
                    Box::pin(outgoing::publish_into_refusal(&root)).await;
                }
                "strict-recover" => Box::pin(outgoing::recover(&root)).await,
                "drift" => Box::pin(outgoing::refuses_captured_head_drift(&root)).await,
                "observation" => {
                    Box::pin(outgoing::observes_the_selected_remote_tip(&root)).await;
                }
                "retry-observation" => {
                    Box::pin(outgoing::observes_the_retry_remote_tip(&root)).await;
                }
                "incoming" => Box::pin(workflow::incoming(&root)).await,
                "evidence" => Box::pin(evidence::run(&root)).await,
                "evidence-mismatch" => Box::pin(evidence::run_mismatches(&root)).await,
                "orphan-rejected" => Box::pin(orphan::run(&root, "rejected")).await,
                "orphan-superseded" => Box::pin(orphan::run(&root, "superseded")).await,
                "orphan-unrequested" => Box::pin(orphan::run(&root, "unrequested")).await,
                _ => panic!("unknown owned data workflow"),
            }
        });
}
