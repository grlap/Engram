import assert from "node:assert/strict";
import { observeSource } from "../work-session-fixture.mjs";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { after } from "node:test";
import { assertTerseShow } from "../terse-show-assertions.mjs";

export function registerCriterionLinks(support) {
  const { test, fixtureHome, removeFixtureHomes, receipt, structuredError, buildAndInit, cliWord, cliJson, McpClient } = support;
test("criterion links reuse existing records and fence the authors read on CLI MCP", async (t) => {
  const engramHome = fixtureHome("engram-criterion-links-", t);
  const session = "criterion-link-author";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    for (const surface of ["cli", "mcp"]) {
      const ref = cliJson(engramHome, session, "add", `Explicit links ${surface}`,
        "--accept", "First outcome", "--accept", "Second outcome").work.short_ref;
      cliJson(engramHome, session, "claim", ref);
      cliJson(engramHome, session, "note", ref, "Existing artifact establishes the first outcome");
      const shown = cliJson(engramHome, session, "show", ref);
      const records = receipt(await client.call("show", {work_ref: ref, notes: true, gates: true}));
      const locator = records.notes[0].locator;
      const args = {work_ref: ref, summary: "Delivered", link_basis: shown.acceptance_basis,
        links: [{criterion: 1, locator}]};
      for (const malformed of ["missing-separator", "word=12345678"]) {
        const output = cliWord(engramHome, session, "done", ref, "--link", malformed,
          "--link-basis", String(args.link_basis), "--json");
        assert.notEqual(output.status, 0);
        const error = JSON.parse(output.stderr).error;
        assert.equal(error.code, "work_criterion_link_invalid");
        assert.equal(error.details.criterion, undefined);
        assert.deepEqual(error.next, [`engram work show ${ref}`, `engram work show ${ref} --notes --gates`]);
        assert.doesNotMatch(output.stderr, /criterion 0|linked-completion:/);
      }
      for (const shape of [
        {work_ref: ref, links: args.links},
        {...args, links: Array.from({length: 65}, () => args.links[0])},
        {...args, links: [{criterion: 0, locator}]},
      ]) {
        const error = structuredError(await client.call("done", shape), "work_criterion_link_invalid");
        assert.equal(error.details.criterion, undefined);
        assert.doesNotMatch(JSON.stringify(error), /criterion 0|linked-completion:/);
      }
      const complete = () => surface === "cli"
        ? Promise.resolve(cliJson(engramHome, session, "done", ref, "Delivered", "--link", `1=${locator}`, "--link-basis", String(args.link_basis)))
        : client.call("done", args).then(receipt);
      const first = await complete();
      assert.doesNotMatch(JSON.stringify(first), /linked-completion:/);
      assert.deepEqual(first.acceptance_evidence.unlinked_positions, [2]);
      assert.equal(first.acceptance_evidence.links[0].locator, locator);
      assert.match(first.acceptance_evidence.links[0].preview, /Existing artifact/);
      assert.equal(first.acceptance_evidence.link_label, "author-linked evidence; not verification");
      assert.deepEqual((await complete()).acceptance_evidence, first.acceptance_evidence);
      const otherAuthor = cliWord(engramHome, "different-link-session", "done", ref, "Delivered",
        "--link", `1=${locator}`, "--link-basis", String(args.link_basis), "--json");
      assert.notEqual(otherAuthor.status, 0);
      const otherError = JSON.parse(otherAuthor.stderr).error;
      assert.equal(otherError.code, "work_criterion_link_invalid");
      assert.match(otherError.details.reason, /completion is frozen/);
      assert.doesNotMatch(otherAuthor.stderr, /linked-completion:/);
      assert.deepEqual(receipt(await client.call("show", {work_ref: ref})).acceptance_evidence, first.acceptance_evidence);
      const text = cliWord(engramHome, session, "show", ref);
      assert.equal(text.status, 0, text.stderr);
      assert.match(text.stdout, /criterion 2: no evidence linked to this criterion/);
      assert.match(text.stdout, /author-linked evidence; not verification/);

      const stale = cliJson(engramHome, session, "add", `Stale basis ${surface}`).work.short_ref;
      cliJson(engramHome, session, "claim", stale);
      cliJson(engramHome, session, "note", stale, "Real current-run evidence");
      const prior = cliJson(engramHome, session, "show", stale, "--notes");
      cliJson(engramHome, session, "update", stale, "--accept", "Revised criterion");
      let error;
      if (surface === "cli") {
        const refused = cliWord(engramHome, session, "done", stale, "No capture should occur", "--link", `1=${prior.notes[0].locator}`, "--link-basis", String(prior.acceptance_basis), "--json");
        assert.notEqual(refused.status, 0);
        error = JSON.parse(refused.stderr).error;
      } else {
        error = structuredError(await client.call("done", {work_ref: stale, summary: "No capture should occur",
          links: [{criterion: 1, locator: prior.notes[0].locator}], link_basis: prior.acceptance_basis}), "work_criterion_link_invalid");
      }
      assert.equal(error.code, "work_criterion_link_invalid");
      assert.match(error.details.reason, /basis changed/);
      assert.deepEqual(error.next, [`engram work show ${stale}`, `engram work show ${stale} --notes --gates`]);
      assert.equal(cliJson(engramHome, session, "show", stale).status.work.lifecycle, "open");
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerAcceptanceEvaluation(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, structuredError, buildAndInit, cliWord, cliText, cliJson, McpClient } = support;
test("evaluated acceptance policy over the real transports: locators, source freshness, provenance", async (t) => {
  const engramHome = fixtureHome("engram-evaluated-policy-", t);
  const holder = "evaluated-holder";
  const peer = "evaluated-peer";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session,independent-session", "--mechanical-basis", "asserted",
      "--require-source-freshness", "--authorized-by", "dogfood-operator",
      "--idempotency-key", "dogfood-enable-evaluation",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    client = new McpClient(engramHome, holder);
    await client.initialize();
    const isGate = (row) => String(row.family).toLowerCase() === "gates";

    // MCP: same-session evaluation citing the printed gate locator; an
    // observation locator refuses; done without, with a changed, and with
    // the matching fingerprint.
    const ref = cliJson(engramHome, holder, "add", "Evaluated over MCP", "--accept", "the build passes").work.short_ref;
    // A supplied evaluation_mode reaches the store only through the
    // evaluation_mode action; any other action refuses it before effects.
    const beforeMisroute = receipt(await client.call("show", { work_ref: ref }));
    const peekBefore = receipt(await client.call("next", { peek: true }));
    assert.equal(typeof peekBefore.read_cut.project_position, "number");
    for (const misrouted of ["same_session", ""]) {
      const wrong = structuredError(
        await client.call("update", { work_ref: ref, action: "revise", title: "Misrouted mode", evaluation_mode: misrouted }),
        "invalid_argument",
      );
      assert.equal(wrong.details.field, "evaluation_mode");
      // No effect: the item, the project feed, and the focus are unchanged.
      assert.deepEqual(receipt(await client.call("show", { work_ref: ref })), beforeMisroute);
      const peekAfter = receipt(await client.call("next", { peek: true }));
      assert.equal(peekAfter.read_cut.project_position, peekBefore.read_cut.project_position, JSON.stringify(misrouted));
      assert.deepEqual(peekAfter.focus, peekBefore.focus, JSON.stringify(misrouted));
    }
    receipt(await client.call("update", { work_ref: ref, action: "evaluation_mode", evaluation_mode: "same_session" }));
    assert.match(cliWord(engramHome, holder, "show", ref).stdout, /evaluation mode: same_session/u);
    receipt(await client.call("update", { work_ref: ref, action: "evaluation_mode" }));
    assert.doesNotMatch(cliWord(engramHome, holder, "show", ref).stdout, /evaluation mode:/u);
    // A peer marks the task for same-session evaluation before the holder
    // takes it; a mark the holder set itself would not waive independence.
    cliJson(engramHome, peer, "update", ref, "--evaluation-mode", "same_session");
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", ref);
    cliJson(engramHome, peer, "note", ref, "peer observation without holding the run");
    // A refused evaluated completion carries a recovery command the CLI
    // parses and runs read-only: navigation to the criteria and evidence,
    // never an evaluation template with a pre-filled verdict.
    const runRecovery = (refusalJson, expectedCode) => {
      assert.equal(refusalJson.code, expectedCode);
      const command = refusalJson.next[0];
      const parts = command.split(" ");
      assert.deepEqual(parts.slice(0, 2), ["engram", "work"], command);
      const run = cliWord(engramHome, holder, ...parts.slice(2));
      assert.equal(run.status, 0, `${command}\n${run.stderr}`);
      assert.equal(parts[2], "show", command);
      assert.ok(parts.includes("--notes"), command);
      assert.doesNotMatch(command, /=pass/u);
    };
    const missingRefusal = cliWord(engramHome, holder, "done", ref, "Delivered", "--json");
    assert.equal(missingRefusal.status, 2, missingRefusal.stderr);
    const revisionBefore = cliJson(engramHome, holder, "show", ref).status.work.revision;
    runRecovery(JSON.parse(missingRefusal.stdout), "missing_acceptance_evaluation");
    assert.equal(cliJson(engramHome, holder, "show", ref).status.work.revision, revisionBefore);
    const shown = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(typeof shown.evidence_basis, "number");
    const records = receipt(await client.call("show", { work_ref: ref, notes: true, gates: true }));
    const gate = records.notes.find(isGate);
    const observation = records.notes.find((row) => row.non_holder === true);
    assert.ok(gate && observation, JSON.stringify(records.notes));
    const verdicts = (evidence) => [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the gate passed", evidence }];
    const base = { work_ref: ref, mode: "same_session", acceptance_basis: shown.acceptance_basis, evidence_basis: shown.evidence_basis };
    const refused = structuredError(await client.call("evaluate", { ...base, verdicts: verdicts([observation.locator]) }), "acceptance_evaluation_refused");
    assert.match(JSON.stringify(refused), /observation/);
    assert.equal(refused.details.cause.kind, "citation");
    assert.equal(refused.details.cause.mismatch, "not_on_run");
    assert.equal(refused.details.cause.citation, observation.locator);
    assert.equal(refused.details.cause.evaluated_cut, base.evidence_basis);
    assert.equal(refused.details.cause.remedy, "read_run_evidence");
    assert.ok(refused.next.some((command) => command.includes(ref)));
    const pinned = structuredError(await client.call("evaluate", { ...base, mode: "independent_session", verdicts: verdicts([gate.locator]) }), "acceptance_evaluation_refused");
    assert.equal(pinned.details.cause.kind, "eligibility");
    assert.equal(pinned.details.cause.mismatch, "task_pin_mismatch");
    assert.equal(pinned.details.cause.task_mark, "same_session");
    const cliPinned = cliWord(engramHome, holder, "evaluate", ref,
      "--mode", "independent-session", "--acceptance-basis", String(base.acceptance_basis),
      "--evidence-basis", String(base.evidence_basis), "--verdict", "1=pass:asserted",
      "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`, "--json");
    assert.equal(cliPinned.status, 1, cliPinned.stderr);
    const pinError = JSON.parse(cliPinned.stderr).error;
    assert.equal(pinError.code, pinned.code);
    assert.equal(pinError.message, pinned.message);
    assert.deepEqual(pinError.details, pinned.details);
    const cliCitation = cliWord(engramHome, holder, "evaluate", ref,
      "--mode", "same-session", "--acceptance-basis", String(base.acceptance_basis),
      "--evidence-basis", String(base.evidence_basis), "--verdict", "1=pass:asserted",
      "--rationale", "1=the gate passed", "--evidence", `1=${observation.locator}`, "--json");
    assert.equal(cliCitation.status, 1, cliCitation.stderr);
    // The transports agree on every detail; the remedy names the show
    // arguments as each caller passes them.
    const cliCitationDetails = JSON.parse(cliCitation.stderr).error.details;
    assert.deepEqual({ ...cliCitationDetails, remedy: null }, { ...refused.details, remedy: null });
    assert.match(cliCitationDetails.remedy, /^read this run.s show --notes --gates and cite/u);
    assert.match(refused.details.remedy, /^read this run.s show with notes and gates and cite/u);
    const evaluated = receipt(await client.call("evaluate", { ...base, source_fingerprint: "sha256:tree-a", verdicts: verdicts([gate.locator]) }));
    assert.equal(evaluated.evaluation.passed, 1);
    assert.match(evaluated.evaluation.evaluation, /^[0-9a-f]{32}(?:[0-9a-f]{32})?$/u);
    assert.equal(evaluated.evaluation.hash, undefined);
    assert.equal(evaluated.evaluation.source_fingerprint, "sha256:tree-a");
    const replayedEvaluation = receipt(await client.call("evaluate", { ...base, source_fingerprint: "sha256:tree-a", verdicts: verdicts([gate.locator]) }));
    assert.equal(replayedEvaluation.evaluation.replayed, true);
    assert.equal(replayedEvaluation.evaluation.evaluation, evaluated.evaluation.evaluation);
    assert.equal(replayedEvaluation.evaluation.hash, undefined);
    const fullEvaluation = receipt(await client.call("show", { work_ref: ref, full: true }));
    assert.equal(fullEvaluation.work.evaluation.evaluation, evaluated.evaluation.evaluation);
    assert.equal(fullEvaluation.work.evaluation.hash, undefined);
    assert.deepEqual(fullEvaluation.work.acceptance, ["the build passes"]);
    assert.equal(fullEvaluation.work.evaluation.verdicts[0].rationale, "the gate passed");
    assert.deepEqual(fullEvaluation.work.evaluation.verdicts[0].citations, [gate.locator]);
    const currentEvaluation = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(currentEvaluation.acceptance_evaluation.evaluation, evaluated.evaluation.evaluation);
    assert.equal(currentEvaluation.acceptance_evaluation.hash, undefined);
    assert.equal(currentEvaluation.acceptance_evaluation.source_fingerprint, "sha256:tree-a");
    for (const full of [false, true]) {
      const cliShown = cliJson(engramHome, holder, "show", ref, ...(full ? ["--full"] : []));
      const projected = full ? cliShown.work.evaluation : cliShown.acceptance_evaluation;
      assert.equal(projected.evaluation, evaluated.evaluation.evaluation);
      assert.equal(projected.hash, undefined);
    }
    const cliReplayed = cliJson(engramHome, holder, "evaluate", ref,
      "--mode", "same-session", "--acceptance-basis", String(base.acceptance_basis),
      "--evidence-basis", String(base.evidence_basis), "--verdict", "1=pass:asserted",
      "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`,
      "--source-fingerprint", "sha256:tree-a");
    assert.equal(cliReplayed.evaluation.replayed, true);
    assert.equal(cliReplayed.evaluation.evaluation, evaluated.evaluation.evaluation);
    assert.equal(cliReplayed.evaluation.hash, undefined);
    assert.equal(cliReplayed.evaluation.source_fingerprint, "sha256:tree-a");
    assert.match(cliWord(engramHome, holder, "show", ref).stdout, /evaluation: same_session [0-9a-f]{12}/u);
    assert.equal(evaluated.evaluation.verdicts_total, 1);
    const unmeasured = receipt(await client.call("done", { work_ref: ref, summary: "Delivered" }));
    assert.equal(unmeasured.code, "acceptance_evaluation_stale");
    assert.deepEqual(unmeasured.recovery.cause, { kind: "acceptance_evaluation_stale", reason: "source" });
    assert.equal(unmeasured.recovery.source.mismatch, "completion_measurement_missing");
    assert.equal(unmeasured.recovery.source.remedy, "measure_source_and_retry");
    assert.equal(unmeasured.recovery.source.evaluation, evaluated.evaluation.evaluation);
    assert.match(unmeasured.remedy, /fresh source measurement/u);
    const unmeasuredShow = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(unmeasuredShow.acceptance_evaluation.source_recovery, undefined);
    assert.ok(unmeasured.reminders.some((line) => line.includes("retry done with it (source_fingerprint)")), JSON.stringify(unmeasured.reminders));
    const changed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered", source_fingerprint: "sha256:tree-b" }));
    assert.equal(changed.code, "acceptance_evaluation_stale");
    assert.equal(changed.recovery.source.mismatch, "completion_fingerprint_mismatch");
    assert.equal(changed.recovery.source.remedy, "evaluate_current_source");
    assert.equal(changed.recovery.source.expected_fingerprint, "sha256:tree-a");
    assert.equal(changed.recovery.source.presented_fingerprint, "sha256:tree-b");
    assert.match(changed.remedy, /new acceptance evaluation/u);
    assert.match(changed.remedy, /copying.*insufficient/u);
    assert.ok(changed.reminders.some((line) => line.includes("(source)")), JSON.stringify(changed.reminders));
    assert.equal(receipt(await client.call("show", { work_ref: ref })).status.work.lifecycle, "open");
    const sealed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered", source_fingerprint: "sha256:tree-a" }));
    assert.equal(sealed.work.lifecycle, "completed");
    assert.equal(sealed.acceptance.provenance, "evaluated");
    assert.equal(sealed.acceptance.mode, "same_session");
    assert.equal(sealed.acceptance.evaluation, evaluated.evaluation.evaluation);
    // The evaluator label is the display identity: the caller's own session
    // is "you"; any other session is an opaque peer label, never its id.
    assert.equal(sealed.acceptance.evaluator, "you");
    const completedShow = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(completedShow.acceptance.provenance, "evaluated");
    assert.equal(completedShow.acceptance.evaluation, evaluated.evaluation.evaluation);

    // CLI: an independent peer evaluates over the CLI; the holder's done
    // refuses without a fingerprint and seals with --source-fingerprint.
    const cliRef = cliJson(engramHome, holder, "add", "Evaluated over CLI", "--accept", "the build passes").work.short_ref;
    cliJson(engramHome, holder, "claim", cliRef);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", cliRef);
    const cliShown = cliJson(engramHome, holder, "show", cliRef);
    const cliGate = cliJson(engramHome, peer, "show", cliRef, "--notes", "--gates").notes.find(isGate);
    assert.ok(cliGate);
    const cliEvaluated = cliJson(engramHome, peer, "evaluate", cliRef,
      "--mode", "independent-session",
      "--acceptance-basis", String(cliShown.acceptance_basis),
      "--evidence-basis", String(cliShown.evidence_basis),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed",
      "--evidence", `1=${cliGate.locator}`, "--source-fingerprint", "sha256:tree-c");
    assert.equal(cliEvaluated.evaluation.passed, 1);
    const cliRefused = cliWord(engramHome, holder, "done", cliRef, "Delivered", "--json");
    assert.equal(cliRefused.status, 2, cliRefused.stderr);
    assert.equal(JSON.parse(cliRefused.stdout).code, "acceptance_evaluation_stale");
    assert.equal(JSON.parse(cliRefused.stdout).recovery.source.mismatch, "completion_measurement_missing");
    assert.equal(JSON.parse(cliRefused.stdout).recovery.source.remedy, "measure_source_and_retry");
    runRecovery(JSON.parse(cliRefused.stdout), "acceptance_evaluation_stale");
    const cliSealed = cliJson(engramHome, holder, "done", cliRef, "Delivered", "--source-fingerprint", "sha256:tree-c");
    assert.equal(cliSealed.work.lifecycle, "completed");
    assert.equal(cliSealed.acceptance.provenance, "evaluated");
    assert.equal(cliSealed.acceptance.mode, "independent_session");
    assert.match(cliSealed.acceptance.evaluator, /^peer-[0-9a-f]+$/u);
    assert.doesNotMatch(cliSealed.acceptance.evaluator, /evaluated-peer/u);
    const cliText = cliWord(engramHome, holder, "show", cliRef);
    assert.equal(cliText.status, 0, cliText.stderr);
    assert.match(cliText.stdout, /acceptance: evaluated \(independent_session; recorded under asserted identity\) by peer-[0-9a-f]+/u);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("open obligations guide evaluation timing across MCP, host observations, and completion", async (t) => {
  const engramHome = fixtureHome("engram-evaluation-obligations-", t);
  const projectId = readFileSync(join(root, ".engram-project"), "utf8").trim();
  const fingerprint = (value) => createHash("sha256").update(value).digest("hex");
  const isGate = (row) => String(row.family).toLowerCase() === "gates";
  const clients = [];
  const controlClients = [];
  const run = (args) => {
    const result = spawnSync(binary, ["--home", engramHome, ...args], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  const core = (session, operation, input) => run([
    "work", "--actor-id", session, "--session-id", session,
    "core", operation, "--input", JSON.stringify(input),
  ]);
  const focus = (session, ref) => run([
    "work", "--actor-id", session, "--session-id", session, "core", "focus", ref,
  ]);
  class ControlClient {
    constructor(session) {
      this.pending = [];
      this.buffer = "";
      this.stderr = "";
      this.child = spawn(binary, [
        "--home", engramHome, "control", "--work-session", "--actor-id", session,
        "--session-id", session, "--source-skill", "engram-dogfood",
      ], { cwd: root, stdio: ["pipe", "pipe", "pipe"] });
      this.child.stdout.on("data", (chunk) => {
        this.buffer += chunk.toString("utf8");
        for (;;) {
          const newline = this.buffer.indexOf("\n");
          if (newline < 0) break;
          const line = this.buffer.slice(0, newline).trim();
          this.buffer = this.buffer.slice(newline + 1);
          if (line) this.pending.shift()?.(JSON.parse(line));
        }
      });
      this.child.stderr.on("data", (chunk) => { this.stderr += chunk.toString("utf8"); });
    }
    request(payload) {
      return new Promise((resolvePromise) => {
        this.pending.push(resolvePromise);
        this.child.stdin.write(`${JSON.stringify(payload)}\n`);
      }).then((response) => {
        assert.equal(response.status, "ok", `${JSON.stringify(response)} ${this.stderr}`);
        return response.result;
      });
    }
    close() {
      return new Promise((resolvePromise) => {
        this.child.once("close", resolvePromise);
        this.child.stdin.end();
      });
    }
  }
  try {
    buildAndInit(engramHome);
    run([
      "control-policy", "set-acceptance-evaluation", "--modes", "same-session",
      "--mechanical-basis", "asserted", "--authorized-by", "dogfood-operator",
      "--idempotency-key", "evaluation-obligation-policy",
    ]);
    const selectRule = (ruleId, key) => run([
      "control-policy", "set-obligation-rule-set", "--input", JSON.stringify({
        schema_version: 1,
        rules: [{
          rule: { rule_id: ruleId, rule_version: 1 },
          trigger: "source_changed",
          requirement: { check_kind: "test" },
        }],
      }), "--authorized-by", "dogfood-operator", "--idempotency-key", key,
    ]);
    const createChangedItem = async (suffix, sourceChanges = 1) => {
      const session = `obligations-${suffix}`;
      const proposed = core(session, "propose", {
        kind: "root", title: `Evaluate ${suffix}`, outcome: "The evidence is judged",
        acceptance: ["A test gate passes"], work_kind: "task", idempotency_key: `add-${suffix}`,
      });
      const ref = proposed.work.short_ref;
      const claimed = core(session, "update", { kind: "claim", ttl_seconds: 300, idempotency_key: `claim-${suffix}` });
      assert.ok(claimed.receipt.control_binding);
      const control = new ControlClient(session);
      controlClients.push(control);
      assert.equal((await control.request({ operation: "work_session_read" })).work_session, true);
      for (let index = 0; index < sourceChanges; index += 1) {
        await observeSource({ request: (frame) => control.request(frame), binding: claimed.receipt.control_binding,
          policy: () => run(["control-policy", "show"]), key: `change-${suffix}-${index}`,
          source: { workspace_id: `workspace-${suffix}`, source_revision: `revision-${suffix}-${index}` },
          changed: true });
      }
      cliJson(engramHome, session, "gate", "cargo-test", "--work-ref", ref);
      const mcp = new McpClient(engramHome, session);
      clients.push(mcp);
      await mcp.initialize();
      const shown = receipt(await mcp.call("show", { work_ref: ref }));
      assertTerseShow(shown);
      assert.equal(shown.evaluation_obligations.open_total, sourceChanges);
      assert.equal(shown.evaluation_obligations.omitted_open,
        sourceChanges - shown.evaluation_obligations.items.length);
      assert.equal(shown.evaluation_obligations.read_cut, shown.evidence_basis);
      const next = receipt(await mcp.call("next", { peek: true }));
      assert.match(JSON.stringify(next.evaluation_obligations), /before evaluation/u);
      const notes = receipt(await mcp.call("show", { work_ref: ref, notes: true, gates: true }));
      const gate = notes.notes.find(isGate);
      assert.ok(gate);
      const verdicts = [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the test gate passed", evidence: [gate.locator] }];
      const evaluate = async () => {
        const current = receipt(await mcp.call("show", { work_ref: ref }));
        return receipt(await mcp.call("evaluate", {
          work_ref: ref, mode: "same_session", acceptance_basis: current.acceptance_basis,
          evidence_basis: current.evidence_basis, verdicts,
        }));
      };
      return { session, ref, mcp, shown, next, evaluate };
    };
    const waive = (item, key) => {
      const open = focus(item.session, item.ref).obligation_page.items.find((row) => row.state === "open");
      assert.ok(open);
      return run([
        "authority", "waive-obligation", "--obligation-id", open.obligation_id,
        "--expected-definition", open.definition, "--waived-by", "dogfood-human",
        "--reason", "reviewed this exact test obligation", "--idempotency-key", key,
      ]);
    };

    selectRule("source_mutation_requires_operator_test", "operator-test-rule");
    const late = await createChangedItem("late-waiver");
    assert.equal(late.shown.evaluation_obligations.items[0].action_required_before_evaluation, true);
    assert.equal(late.next.next.some((command) => command.startsWith("engram work done ")), false);
    const first = await late.evaluate();
    assert.equal(first.evaluation_obligations.open_total, 1);
    assert.equal(first.evaluation_obligations.items.length, 1);
    assert.equal(first.evaluation_obligations.read_cut, undefined);
    assert.match(JSON.stringify(first.reminders), /open obligations: 1 total, 0 not shown/u);
    assert.equal(first.next.some((command) => command.startsWith("engram work done ")), false);
    assert.match(JSON.stringify(first.reminders), /resolve obligations needing action/u);
    assert.equal(first.evaluation.passed, 1);
    waive(late, "waive-after-evaluation");
    const stale = receipt(await late.mcp.call("done", { work_ref: late.ref, summary: "Delivered" }));
    assert.equal(stale.code, "acceptance_evaluation_stale");
    await late.evaluate();
    assert.equal(receipt(await late.mcp.call("done", { work_ref: late.ref, summary: "Delivered" })).work.lifecycle, "completed");

    const early = await createChangedItem("early-waiver");
    waive(early, "waive-before-evaluation");
    assert.equal((await early.evaluate()).evaluation.passed, 1);
    assert.equal(receipt(await early.mcp.call("done", { work_ref: early.ref, summary: "Delivered" })).work.lifecycle, "completed");

    selectRule("source_mutation_requires_test", "stock-test-rule");
    const stock = await createChangedItem("stock-waiver", 12);
    assert.ok(stock.shown.evaluation_obligations.omitted_open > 0);
    assert.equal(stock.shown.evaluation_obligations.action_required_total, 0);
    const verboseStock = receipt(await stock.mcp.call("next", { peek: true, verbose: true }));
    assert.equal(verboseStock.evaluation_obligations.open_total, 12);
    assert.equal(verboseStock.evaluation_obligations.action_required_total, 0);
    assert.equal(verboseStock.evaluation_obligations.omitted_open,
      12 - verboseStock.evaluation_obligations.items.length);
    assert.equal(stock.shown.evaluation_obligations.items[0].action_required_before_evaluation, false);
    assert.match(stock.shown.evaluation_obligations.items[0].remedy, /no action before evaluation/u);
    await stock.evaluate();
    const candidate = cliJson(engramHome, "ready-candidate-author", "add", "Ready candidate", "--accept", "A criterion");
    const stockNext = receipt(await stock.mcp.call("next", { peek: true }));
    assert.ok(stockNext.ready.some((item) => item.ref === candidate.work.short_ref));
    assert.ok(stockNext.next.some((command) => command.startsWith(`engram work done ${stock.ref} `)));
    assert.doesNotMatch(JSON.stringify(stockNext.reminders), /request acceptance evaluation/u);
    assert.equal(receipt(await stock.mcp.call("done", { work_ref: stock.ref, summary: "Delivered" })).work.lifecycle, "completed");
  } finally {
    await Promise.all(clients.map((client) => client.close()));
    await Promise.all(controlClients.map((client) => client.close()));
    removeFixtureHomes(engramHome);
  }
});

test("a carried failure over the real transports: shown, refused until another evaluator names it, then recorded", async (t) => {
  const engramHome = fixtureHome("engram-carried-failure-", t);
  const holder = "carried-holder";
  const reviewer = "carried-reviewer";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session,independent-session", "--mechanical-basis", "asserted",
      "--authorized-by", "dogfood-operator", "--idempotency-key", "dogfood-carried-failure",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    const ref = cliJson(engramHome, holder, "add", "Carried item", "--accept", "the report lists every store").work.short_ref;
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test");
    const shown = cliJson(engramHome, holder, "show", ref);
    const bases = (read) => ["--acceptance-basis", String(read.acceptance_basis), "--evidence-basis", String(read.evidence_basis)];
    // A session that never held the run records the failure.
    const failed = cliJson(engramHome, "carried-judge", "evaluate", ref, "--mode", "independent-session", ...bases(shown),
      "--verdict", "1=fail:judgment", "--rationale", "1=it lists one store");
    const failedId = failed.evaluation.evaluation;
    // The executor rewords the criterion the evaluation failed.
    cliJson(engramHome, holder, "update", ref, "--accept", "the report lists some stores");
    const carried = cliJson(engramHome, holder, "show", ref);
    assert.deepEqual(carried.acceptance_evaluation.carried_failure, {
      evaluation: failedId,
      revised_by: "executor",
      judged_revision: failed.evaluation.work_revision,
      failing: 1,
      supersedes_required: true,
    });
    const gate = cliJson(engramHome, holder, "show", ref, "--notes", "--gates").notes
      .find((row) => String(row.family).toLowerCase() === "gates");
    assert.ok(gate);

    client = new McpClient(engramHome, holder);
    await client.initialize();
    const request = {
      work_ref: ref, mode: "same_session",
      acceptance_basis: carried.acceptance_basis, evidence_basis: carried.evidence_basis,
      verdicts: [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the gate passed", evidence: [gate.locator] }],
    };
    const refused = structuredError(await client.call("evaluate", request), "acceptance_evaluation_refused");
    assert.equal(refused.details.reason, "carried_failure_unacknowledged");
    assert.equal(refused.details.failed_evaluation, failedId);
    assert.match(refused.details.remedy, /submit with supersedes RECORD_ID/u);
    // The executor naming its own failure is not someone else accepting
    // the revision.
    const selfNamed = structuredError(await client.call("evaluate", { ...request, supersedes: failedId }), "acceptance_evaluation_refused");
    assert.equal(selfNamed.details.reason, "carried_failure_self_acknowledged");
    assert.equal(selfNamed.details.failed_evaluation, failedId);
    assert.match(selfNamed.details.remedy, /independent_session/u);
    // A reviewer that never held the run names it over the CLI.
    const recorded = cliJson(engramHome, reviewer, "evaluate", ref, "--mode", "independent-session", ...bases(carried),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`,
      "--supersedes", failedId);
    assert.equal(recorded.evaluation.supersedes, failedId);

    // Once a newer evaluation ends the carry, the CLI flag names nothing.
    const after = cliJson(engramHome, holder, "show", ref);
    assert.equal(after.acceptance_evaluation.carried_failure, undefined);
    assert.equal(after.acceptance_evaluation.supersedes, failedId);
    const nothing = cliWord(engramHome, holder, "evaluate", ref, "--mode", "same-session", ...bases(after),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`,
      "--supersedes", failedId, "--json");
    assert.notEqual(nothing.status, 0);
    assert.equal(JSON.parse(nothing.stderr || nothing.stdout).error.details.reason, "nothing_to_supersede");
    assert.equal(cliJson(engramHome, holder, "done", ref, "Delivered").work.lifecycle, "completed");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerEvaluationHistory(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, cliText, cliJson, McpClient } = support;
test("evaluation history over MCP: a failing record then a passing one from one session", async (t) => {
  const engramHome = fixtureHome("engram-evaluation-history-", t);
  const holder = "history-holder";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session", "--mechanical-basis", "asserted",
      "--authorized-by", "dogfood-operator",
      "--idempotency-key", "dogfood-history-evaluation",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    client = new McpClient(engramHome, holder);
    await client.initialize();
    const ref = cliJson(engramHome, holder, "add", "Evaluated twice", "--accept", "the build passes").work.short_ref;
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", ref);
    const records = receipt(await client.call("show", { work_ref: ref, notes: true, gates: true }));
    const gate = records.notes.find((row) => String(row.family).toLowerCase() === "gates");
    assert.ok(gate, JSON.stringify(records.notes));
    const evaluate = async (verdict) => {
      const shown = receipt(await client.call("show", { work_ref: ref }));
      const evaluated = receipt(await client.call("evaluate", {
        work_ref: ref,
        mode: "same_session",
        acceptance_basis: shown.acceptance_basis,
        evidence_basis: shown.evidence_basis,
        verdicts: [{ criterion: 1, verdict, basis: "asserted", rationale: `the gate says ${verdict}`, evidence: [gate.locator] }],
      }));
      return evaluated.evaluation.evaluation;
    };
    const failed = await evaluate("fail");
    // The failure stands until new evidence: a correction precedes the pass.
    receipt(await client.call("note", { work_ref: ref, text: "correction: the build is fixed" }));
    const passed = await evaluate("pass");

    // The window lists both records of the run in run-feed order, from the
    // same evaluator session; only the newer one is the newest.
    const window = receipt(await client.call("show", { work_ref: ref, evaluations: true }));
    assert.equal(window.evaluations_window.total, 2);
    assert.equal(window.evaluations_window.shown, 2);
    assert.equal(window.evaluations_window.omitted, 0);
    assert.equal(window.evaluations_window.after, null);
    const [older, newer] = window.evaluations;
    assert.deepEqual([older.evaluation, newer.evaluation], [failed, passed]);
    assert.ok(older.run_position < newer.run_position, JSON.stringify(window.evaluations));
    assert.equal(older.evaluator_session, "you");
    assert.equal(newer.evaluator_session, older.evaluator_session);
    assert.equal(older.verdicts[0].verdict, "fail");
    assert.equal(newer.verdicts[0].verdict, "pass");
    assert.equal(older.newest, false);
    assert.equal(newer.newest, true);
    assert.equal(older.stale, null, "an older record is not stale merely because a newer one exists");
    assert.equal(newer.stale, null);

    // The CLI gives the same rows; the detail reads the older record complete.
    const viaCli = cliJson(engramHome, holder, "show", ref, "--evaluations");
    assert.deepEqual(viaCli.evaluations.map((row) => row.evaluation), [failed, passed]);
    const detail = receipt(await client.call("show", { work_ref: ref, evaluation: failed }));
    assert.equal(detail.evaluation.evaluation, failed);
    assert.equal(detail.evaluation.verdicts[0].verdict, "fail");
    assert.equal(detail.evaluation.verdicts[0].rationale, "the gate says fail");
    assert.deepEqual(detail.evaluation.verdicts[0].citations, [gate.locator]);

    // Ordinary show and done keep reading only the newest record.
    const sealed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered" }));
    assert.equal(sealed.work.lifecycle, "completed");
    assert.equal(sealed.acceptance.evaluation, passed);
    const afterDone = receipt(await client.call("show", { work_ref: ref, evaluations: true }));
    assert.deepEqual(afterDone.evaluations.map((row) => row.evaluation), [failed, passed]);
    // Completion revised the item: the ended run's records are listed but not
    // judged, so the record the seal consumed is never shown as stale.
    assert.equal(afterDone.evaluations_window.stale_judged, false);
    for (const row of afterDone.evaluations) {
      assert.equal(row.stale, null, JSON.stringify(row));
      assert.equal(row.stale_judged, false);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});


test("typed terse gates and generic vocabulary agree on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-receipt-facts-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "fact-reader");
    await client.initialize();
    const work = receipt(await client.call("add", { title: "Receipt facts" })).work.short_ref;
    receipt(await client.call("claim", { work_ref: work }));
    receipt(await client.call("gate", { work_ref: work, name: "Checks", failed: ["failed check"] }));
    receipt(await client.call("gate", { work_ref: work, name: "Checks" }));
    const mcp = receipt(await client.call("show", { work_ref: work }));
    const cli = cliJson(engramHome, "fact-reader", "show", work);
    for (const value of [mcp, cli]) {
      assert.deepEqual(value.notes.map(row => row.gate), [
        { name: "checks", passed: false }, { name: "checks", passed: true },
      ]);
      assert.deepEqual(value.notes.map(row => row.kind), ["generic", "generic"]);
      assertTerseShow(value);
    }
    assert.match(cliText(engramHome, "fact-reader", "show", work), /latest generic/u);
    for (const value of [
      receipt(await client.call("show", { work_ref: work, notes: true, gates: true })),
      cliJson(engramHome, "fact-reader", "show", work, "--notes", "--gates"),
    ]) {
      assert.equal(value.notes.length, 2);
      assert.deepEqual(value.notes.map(row => row.gate), [
        { name: "checks", passed: false }, { name: "checks", passed: true },
      ]);
      assert.equal(value.notes_window.total, 2);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});
}
