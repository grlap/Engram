// Test fixtures send facts through the ordinary host evidence protocol.
// They assert a registered producer; they do not attest that a command ran.
import { createHash } from "node:crypto";

export function gitRevision(label) {
  return `git-commit:${createHash("sha256").update(label).digest("hex").slice(0, 40)}`;
}

export async function observeSource({ request, policy, binding, key, source, changed = false }) {
  const root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  const time = new Date().toISOString();
  const occurrence = changed ? {
    kind: "inter_turn_change",
    source_change: { detection: "assumed_missing_baseline", workspace_id: source.workspace_id,
      sighting: { source_basis: source, observed_at: time } },
  } : { kind: "source_sighting", source_basis: source, observed_at: time };
  const currentPolicy = policy();
  return request({
    operation: "execution_observe", idempotency_key: key, binding,
    root_basis: { capture_run_cut: root.read_cut.position, latest_event: root.latest_event?.event ?? null,
      state: root.named_root },
    observed_interval: { from: time, through: time }, occurrence,
    causality: { kind: "unknown" },
    policy_basis: { mode: "account_if_eligible", project_policy_epoch: currentPolicy.epoch,
      policy: currentPolicy.policy, obligation_rule_set: currentPolicy.obligation_rules },
  });
}

export async function ensureRoot({ request, binding, key, source }) {
  let root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  if (root.named_root.state === "none") {
    await request({ operation: "named_root_bind", claim_id: binding.claim_id, claim_fence: binding.claim_fence,
      workspace_id: source.workspace_id, generation: 1, named_at: new Date().toISOString(),
      kind: "bound", idempotency_key: `${key}-root` });
    root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  }
  return root;
}

export async function recordCheck({ request, binding, session, key, source, checkFingerprint }) {
  const root = await ensureRoot({ request, binding, key, source });
  const time = new Date().toISOString();
  const command = "fixture registered check";
  const hostRun = `test-${key}`;
  return request({
    operation: "verification_record", operation_key: `termal-run:${hostRun}`,
    registration: { host_run_id: hostRun, request_identity: key, registered_at: time,
      initiating_session_id: session, owning_session_id: session, binding,
      root: { workspace_id: source.workspace_id, generation: root.named_root.generation, state: "named" } },
    input: { kind: "commit", commit: source.source_revision.slice("git-commit:".length) },
    run: { mode: "focused", command, started_at: time, finished_at: time, outcome: "passed" },
    checks: [{ stage: "focused", check_kind: "test", command,
      check_fingerprint: checkFingerprint ?? createHash("sha256").update(command).digest("hex"),
      started_at: time, completed_at: time, outcome: "passed", summary: "fixture host check", refs: [] }],
    post_run: { outcome: "clean", clean_before: true, clean_after: true, dirty_paths: [] },
  });
}
