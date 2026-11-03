import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { basename, join } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION, WINDOWS_LIMITATION } from "../review-freeze-fingerprint.mjs";
import { completionMetadata, createRun, diagnostics, executeRun, inspectFailure, notifyRun, requiredStages, summarize } from "../test-launcher.mjs";

export function registerCompletionDiagnostics(support) {
  const { env, stage, git, repository } = support;
test("warnings stay in artifacts without entering completion or converting success to failure", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("warning", "console.error('warning: fixture caution')")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.exitCode, 0);
    assert.match(result.stages[0].diagnostics.text, /warning: fixture caution/u);
    assert.doesNotMatch(await summarize(runDir), /warning: fixture caution/u);
  });
});

test("failure diagnostics take priority over earlier warning-heavy passing stages", async () => {
  await repository(async (root) => {
    const warnings = "warning: earlier caution " + "w".repeat(3000);
    const runDir = await createRun({ root, stages: [
      ...[1, 2, 3].map((index) => stage(`noisy-${index}`, `console.error(${JSON.stringify(warnings)})`)),
      stage("broken", "console.error('error: essential failure detail'); process.exitCode = 17"),
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 17);
    assert.deepEqual(result.stages.map(({ state }) => state), ["passed", "passed", "passed", "failed"]);
    const summary = await summarize(runDir);
    assert.doesNotMatch(summary, /essential failure detail|earlier caution/u);
    assert.match(await inspectFailure(runDir, "broken"), /error: essential failure detail/u);
    assert.doesNotMatch(summary, /\n\n/u);
    assert.match(summary, /stages: total=4 passed=3 failed=1/u);
    assert.ok(Buffer.byteLength(summary) < 12_288);
  });
});

test("an error within a failed stage precedes that stage's long warning excerpt", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("mixed",
      `console.error('warning: ' + 'w'.repeat(3000)); console.error('error: essential failure after warning'); process.exitCode = 11`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 11);
    assert.ok(result.stages[0].diagnostics.text.startsWith("error: essential failure after warning\n"));
    assert.equal(result.stages[0].diagnostics.text.length, 2400);
    assert.equal(result.stages[0].diagnostics.truncated, true);
    assert.doesNotMatch(await summarize(runDir), /essential failure after warning/u);
    assert.match(await inspectFailure(runDir, "mixed"), /essential failure after warning/u);
  });
});

test("long logical-line continuations cannot become diagnostic or passing headers", async () => {
  await repository(async (root) => {
    const log = join(root, ".git", "long-line.log");
    for (const ending of ["error: false fragment", "ok 2 - false fragment"]) {
      const output = `ok 1 - ${"x".repeat(30_000)}${ending}\nerror: actual next line\n  genuine context\n`;
      writeFileSync(log, output);
      assert.deepEqual(await diagnostics(log, false), {
        text: "error: actual next line\n  genuine context\n", truncated: true,
      });
      assert.equal(readFileSync(log, "utf8"), output);
    }
  });
});

test("successful Node warning headers remain visible without passing names", async () => {
  await repository(async (root) => {
    const warnings = [
      `(node:${process.pid}) [DEP0040] DeprecationWarning: fixture deprecation`,
      `(node:${process.pid}) ExperimentalWarning: fixture experiment`,
      `# (node:${process.pid}) [CUSTOM] MaxListenersExceededWarning: fixture listeners`,
    ];
    const output = [...warnings, "ok 1 - ExperimentalWarning passing name", " ✓ DeprecationWarning passing name"].join("\n");
    const runDir = await createRun({ root, stages: [stage("node-warnings", `console.error(${JSON.stringify(output)})`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 0);
    assert.equal(result.stages[0].diagnostics.text, `${warnings.join("\n")}\n`);
    assert.doesNotMatch(await summarize(runDir), /passing name/u);
  });
});

test("normalization limitations are platform-independent with a separate Windows caveat", () => {
  assert.deepEqual(fingerprintLimitations("linux"), [NORMALIZATION_LIMITATION]);
  assert.deepEqual(fingerprintLimitations("darwin"), [NORMALIZATION_LIMITATION]);
  assert.deepEqual(fingerprintLimitations("win32"), [NORMALIZATION_LIMITATION, WINDOWS_LIMITATION]);
});

}

export function registerCompletionReceipts(support) {
  const { env, entryUrl, launcher, launcherFixtureFiles, stage, json, git, repository, directoryBytes } = support;
test("launch metadata validation refuses before any run is created", async () => {
  await repository(async (root) => {
    const runs = join(root, ".git", "review-runs");
    for (const invalid of [
      { nextTransition: { owner: "fixture-owner" } }, { nextTransition: { action: "continue" } },
      { nextTransition: { owner: " fixture-owner", action: "continue" } },
      { nextTransition: { owner: "fixture-owner", action: " \t " } },
      { nextTransition: { owner: "fixture-owner", action: "line\nline" } },
      { nextTransition: { owner: "fixture-owner", action: "界".repeat(667) } },
      { nextTransition: { owner: "-option", action: "continue" } },
      { item: "" }, { item: "界".repeat(86) }, { item: "line\rline" },
    ]) {
      await assert.rejects(() => createRun({ root, stages: [stage("unused")], ...invalid }, env), /requires/u);
      assert.equal(existsSync(runs), false);
    }
    const item = "  verify release  ";
    const nextTransition = { owner: "fixture-owner", action: "  inspect \"quoted\" path\\name\u0085\u2028\u2029  " };
    const runDir = await createRun({ root, stages: [stage("clean")], item, nextTransition }, env);
    const requestBytes = readFileSync(join(runDir, "request.json"));
    await executeRun(runDir, env);
    assert.deepEqual(readFileSync(join(runDir, "request.json")), requestBytes, "launcher never republishes the launch request during execution");
    assert.equal(json(join(runDir, "request.json")).item, item);
    assert.deepEqual(json(join(runDir, "request.json")).nextTransition, nextTransition);
    const summary = await summarize(runDir);
    const value = (prefix) => JSON.parse(summary.split("\n").find((line) => line.startsWith(prefix)).slice(prefix.length));
    assert.equal(value("ITEM "), item);
    assert.equal(value("NEXT OWNER "), nextTransition.owner);
    assert.equal(value("NEXT ACTION "), nextTransition.action);
    assert.doesNotMatch(summary, /[\u0085\u2028\u2029]/u);
  });
});

test("CLI metadata flags preserve command arguments and refuse invalid pairs without a run", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
    }
    const local = join(root, "scripts", "test-launcher.mjs");
    const runs = join(root, ".git", "review-runs");
    const command = ["--", process.execPath, "-e", "console.log('--next-action is a command argument')"];
    for (const flags of [
      ["--next-owner", "fixture-owner"], ["--next-action", "continue"],
      ["--item", "one", "--item", "two"],
      ["--next-owner", "fixture-owner", "--next-owner", "fixture-owner", "--next-action", "continue"],
      ["--next-owner", " fixture-owner", "--next-action", "continue"],
      ["--next-owner", "fixture-owner", "--next-action", "line\nline"],
      ["--next-owner", "fixture-owner", "--next-action", "--detach"], ["--item", "--detach"],
    ]) {
      const result = spawnSync(process.execPath, [local, "focused", ...flags, ...command],
        { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(result.status, 1, JSON.stringify(flags));
      assert.equal(existsSync(runs), false);
    }
    const result = spawnSync(process.execPath, [local, "focused", "--item", "release validation",
      "--next-owner", "fixture-owner", "--next-action", "  Review the exact input.  ", ...command],
      { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readdirSync(runs).length, 1);
    const runDir = join(runs, readdirSync(runs)[0]);
    assert.deepEqual(json(join(runDir, "request.json")).nextTransition,
      { owner: "fixture-owner", action: "  Review the exact input.  " });
    assert.match(result.stdout, /NEXT ACTION "  Review the exact input\.  "/u);
    assert.doesNotMatch(result.stdout, /is a command argument/u);
    assert.match(readFileSync(join(runDir, "focused.log"), "utf8"), /--next-action is a command argument/u);
    const record = result.stdout.trimEnd().split("\n").at(-1);
    assert.match(record, /^test-launcher\/v1 .*overall=passed scope=focused stages=1$/u);
  });
});

test("sign-off carries exact transition metadata for pass, failure, interruption and input mismatch", async () => {
  await repository(async (root) => {
    const nextTransition = { owner: "fixture-owner", action: "Review the frozen input and consolidate the pair." };
    const runDir = await createRun({ root, stages: [stage("first"), stage("later")], item: "release validation", nextTransition }, env);
    const request = json(join(runDir, "request.json"));
    const fingerprint = request.expectedFingerprint;
    const passed = { runId: basename(runDir), state: "passed", exitCode: 0,
      started: request.started, ended: new Date(Date.parse(request.started) + 2000).toISOString(),
      expectedFingerprint: fingerprint, before: fingerprint, after: fingerprint,
      stages: request.stages.map(({ name }) => ({ name, state: "passed", code: 0 })) };
    for (const [expected, result] of [
      ["PASS", passed],
      ["FAIL", { ...passed, state: "failed", exitCode: 13, stages: [{ name: "first", state: "failed", code: 13 }, { name: "later", state: "unrun" }] }],
      ["INTERRUPTED", { ...passed, state: "failed", exitCode: 1, interrupted: true, recovered: { phase: "stage", stage: "first" } }],
      ["INTERRUPTED", { ...passed, stages: [{ name: "first", state: "failed", signal: "SIGTERM", code: null }] }],
      ["FAIL", { ...passed, after: `${fingerprint[0] === "0" ? "1" : "0"}${fingerprint.slice(1)}` }],
      ["FAIL", { ...passed, reason: "input-changed", expectedFingerprint: undefined, before: undefined, after: undefined }],
      ["UNKNOWN", { ...passed, after: undefined }],
      ["UNKNOWN", { ...passed, state: "running", ended: undefined, heartbeat: undefined }],
      ["FAIL", { ...passed, preflight: [{ name: "probe", code: 0, error: "fixture diagnostic" }] }],
      ["FAIL", { ...passed, stages: [{ name: "first", state: "failed", code: 0, tests: { failures: 1 } }] }],
    ]) {
      writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      const summary = await summarize(runDir);
      assert.equal(summary.split("\n")[0], `${expected} ${basename(runDir)} exit=${result.ended ? result.exitCode : "unknown"}`);
      assert.equal(summary.split("\n")[1], `results: ${join(runDir, "results.json")}`);
      assert.ok(summary.includes(`NEXT OWNER ${JSON.stringify(nextTransition.owner)}\nNEXT ACTION ${JSON.stringify(nextTransition.action)}\n`));
      const blocker = summary.split("\n").find((line) => line.startsWith("BLOCKER "));
      if (expected === "PASS") assert.equal(blocker, undefined);
      else assert.ok(["stage-failed", "runner-failed", "interrupted", "no-terminal-result"].includes(JSON.parse(blocker.slice(8)).reason));
      if (expected === "UNKNOWN" && result.state === "passed") {
        assert.match(summary, /^reason: input-unverified$/mu);
        assert.deepEqual(JSON.parse(blocker.slice(8)), { reason: "no-terminal-result", basis: "input-unverified" });
        assert.doesNotMatch(summary, /launcher-error|runner-failed/u);
      }
      if (expected === "FAIL" && result.state === "passed" && !result.stages.some((entry) => entry.state === "failed")) {
        assert.deepEqual(JSON.parse(blocker.slice(8)), { reason: "runner-failed", exit: 0,
          basis: result.preflight ? "contradictory-result" : "input-changed" });
      }
      if (expected === "FAIL" && completionMetadata(request, result).input.comparison === "mismatch") {
        assert.deepEqual(summary.split("\n").slice(2, 4), [
          `first: passed exit=0 log=${join(runDir, "first.log")}`,
          `later: passed exit=0 log=${join(runDir, "later.log")}`,
        ]);
        assert.match(summary, /^reason: input-changed$/mu);
      }
      assert.doesNotMatch(summary, /test-launcher\/v1/u);
    }
    assert.deepEqual(completionMetadata(request, passed).elapsed, { ms: 2000, basis: "launch-to-terminal" });
    for (const started of [undefined, "invalid", new Date(Date.parse(passed.ended) + 1).toISOString()]) {
      assert.equal(completionMetadata({ ...request, started }, { ...passed, started }).elapsed.ms, "unknown");
    }
    assert.deepEqual(completionMetadata(request, { ...passed, interrupted: true }).elapsed, { ms: "unknown", basis: "recovery-end" });
  });
});

test("completion output ignores arbitrary saved prose and does not open stage logs", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("broken")], notifyTo: "fixture-parent",
      nextTransition: { owner: "fixture-owner", action: "Inspect the failure." } }, env);
    const result = await executeRun(runDir, env);
    const marker = "NEVER-INJECT-DIAGNOSTIC-TEXT";
    Object.assign(result, { state: "failed", exitCode: 1, error: marker, limitations: [marker], reason: marker });
    Object.assign(result.stages[0], { state: "failed", code: 1, error: marker, reported: marker,
      diagnostics: { text: marker.repeat(20_000), fallback: marker, truncated: true } });
    const logBytes = readFileSync(result.stages[0].log);
    unlinkSync(result.stages[0].log);
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const saved = readFileSync(join(runDir, "results.json"));
    const summary = await summarize(runDir);
    assert.doesNotMatch(summary, /NEVER-INJECT/u);
    assert.ok(Buffer.byteLength(summary) < 3000);
    await notifyRun(runDir, env, async (_command, args) => {
      assert.equal(readFileSync(args[args.indexOf("--message-file") + 1], "utf8"), summary);
      return { code: 0 };
    });
    const excerpt = await inspectFailure(runDir, "broken", 100);
    assert.ok(excerpt.includes(marker));
    const bytes = readFileSync(join(runDir, "notification.message.txt"));
    writeFileSync(join(runDir, "request.json"), "{");
    await notifyRun(runDir, env, async (_command, args) => {
      assert.equal(args[args.indexOf("--to") + 1], "fixture-parent");
      assert.deepEqual(readFileSync(args[args.indexOf("--message-file") + 1]), bytes);
      return { code: 0 };
    });
    assert.deepEqual(readFileSync(join(runDir, "results.json")), saved);
    assert.equal(existsSync(result.stages[0].log), false);
    assert.equal(logBytes.length, 0);
  });
});

test("explicit failure inspection has an exact bound, selector and no side effects", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("failure", "console.error('error: ' + 'x'.repeat(4000));process.exitCode=2"), stage("later")] }, env);
    await executeRun(runDir, env);
    const before = directoryBytes(runDir);
    const output = await inspectFailure(runDir, "failure", 37);
    assert.ok(output.includes(`\n${"error: " + "x".repeat(30)}\n`));
    assert.match(output, /inspection: truncated/u);
    for (const size of [0, 6001, 1.5, NaN]) await assert.rejects(inspectFailure(runDir, "failure", size), /1 to 6000/u);
    await assert.rejects(inspectFailure(runDir, "later"), /requires a failed/u);
    await assert.rejects(inspectFailure(runDir, "missing"), /unavailable/u);
    assert.deepEqual(directoryBytes(runDir), before);
  });
});

test("explicit failure inspection accepts projected failures without changing saved stages", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("focused")] }, env);
    const original = json(join(runDir, "results.json"));
    const marker = `private-inspection-${randomUUID()}`;
    assert.equal(runDir.includes(marker), false);
    for (const [label, changes, inspectable] of [
      ["saved error", { error: marker }, true],
      ["nonzero exit", { code: 17 }, true],
      ["failed tests", { tests: { failed: 1 } }, true],
      ["counted failures", { tests: { failures: 1 } }, true],
      ["raw failure with unknown outcome", { state: "failed", outcome: "unknown" }, true],
      ["signal with unknown outcome", { signal: "SIGTERM", outcome: "unknown" }, true],
      ["healthy", {}, false],
      ["unknown outcome", { outcome: "unknown", error: marker }, false],
      ["unrun", { state: "skipped", error: marker }, false],
    ]) {
      const entry = { name: "focused", state: "passed", code: 0,
        diagnostics: { text: marker, truncated: false }, ...changes };
      writeFileSync(join(runDir, "results.json"), JSON.stringify({ ...original, stages: [entry] }));
      const before = directoryBytes(runDir);
      assert.equal((await summarize(runDir)).includes(marker), false, label);
      if (inspectable) {
        const output = await inspectFailure(runDir, "focused", marker.length);
        assert.deepEqual(output.split("\n"), [
          `inspection: "focused" max-chars=${marker.length}`,
          `results: ${join(runDir, "results.json")}`,
          marker,
          ...(entry.error ? ["inspection: truncated; full log at locator"] : []),
          "",
        ], label);
      } else await assert.rejects(inspectFailure(runDir, "focused"), /requires a failed/u, label);
      assert.deepEqual(directoryBytes(runDir), before, label);
    }
  });
});

test("notification follows durable completion and retries the same payload without rerunning tests", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "execution-count");
    const runDir = await createRun({ root, stages: [stage("count-once",
      `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    const resultBytes = readFileSync(join(runDir, "results.json"), "utf8");
    const calls = [];
    const send = async (command, args, options) => {
      assert.equal(json(join(runDir, "results.json")).state, "passed");
      assert.equal(command, process.execPath);
      assert.equal(options.env.TERMAL_SESSION_ID, "fixture-owner");
      calls.push(args);
      return { code: calls.length === 1 ? 7 : 0, signal: null };
    };
    await assert.rejects(notifyRun(runDir, env, send), /notification failed/iu);
    const message = readFileSync(join(runDir, "notification.message.txt"), "utf8");
    await notifyRun(runDir, env, send);
    assert.equal(calls.length, 2);
    assert.deepEqual(calls[1], calls[0]);
    const args = calls[0];
    assert.equal(args[args.indexOf("--to") + 1], "fixture-parent");
    assert.equal(args[args.indexOf("--idempotency-key") + 1], `engram-tests:${basename(runDir)}`);
    assert.equal(readFileSync(join(runDir, "notification.message.txt"), "utf8"), message);
    assert.equal(readFileSync(join(runDir, "results.json"), "utf8"), resultBytes);
    assert.equal(readFileSync(counter, "utf8"), "run\n");
  });
});

test("notification delivers one frozen sign-off to distinct owners with stable separate keys", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "execution-count");
    const runDir = await createRun({ root, notifyTo: "fixture-parent",
      nextTransition: { owner: "fixture-next", action: "Inspect this exact result." },
      stages: [stage("count-once", `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)] }, env);
    await executeRun(runDir, env);
    const saved = readFileSync(join(runDir, "results.json"));
    const calls = [];
    const send = async (_command, args) => {
      calls.push({ args, body: readFileSync(args[args.indexOf("--message-file") + 1]) });
      // Failure of the coordinator send must not withhold the owner's body.
      return { code: calls.length === 1 || calls.length === 4 ? 7 : 0 };
    };
    await assert.rejects(notifyRun(runDir, env, send), /notification failed/u);
    assert.equal(calls.length, 2);
    assert.deepEqual(calls[0].body, calls[1].body);
    const receipt = json(join(runDir, "notification.json"));
    assert.equal(receipt.ok, false);
    assert.deepEqual(receipt.deliveries.map(({ to, code }) => [to, code]), [["fixture-parent", 7], ["fixture-next", 0]]);
    assert.deepEqual(calls.map(({ args }) => args[args.indexOf("--idempotency-key") + 1]),
      [`engram-tests:${basename(runDir)}`, `engram-tests:${basename(runDir)}:next-owner`]);
    writeFileSync(join(runDir, "request.json"), "{");
    await assert.rejects(notifyRun(runDir, env, send), /notification failed/u);
    assert.deepEqual(calls[2], calls[0]);
    assert.deepEqual(calls[3], calls[1]);
    assert.deepEqual(json(join(runDir, "notification.json")).deliveries.map(({ code }) => code), [0, 7]);
    assert.equal(json(join(runDir, "notification.json")).ok, false);
    await notifyRun(runDir, env, send);
    assert.deepEqual(calls[4], calls[0]);
    assert.equal(json(join(runDir, "notification.json")).ok, true);
    assert.deepEqual(calls[5], calls[1]);
    assert.deepEqual(readFileSync(join(runDir, "results.json")), saved);
    assert.equal(readFileSync(counter, "utf8"), "run\n");
  });
});

test("notification suppresses a next-owner self-send and a repeated target", async () => {
  await repository(async (root) => {
    for (const owner of [env.TERMAL_SESSION_ID, "fixture-parent"]) {
      const runDir = await createRun({ root, notifyTo: "fixture-parent", nextTransition: { owner, action: "Continue." },
        stages: [stage("clean")] }, env);
      await executeRun(runDir, env);
      const calls = [];
      await notifyRun(runDir, env, async (_command, args) => { calls.push(args); return { code: 0 }; });
      assert.equal(calls.length, 1);
      assert.equal(calls[0][calls[0].indexOf("--to") + 1], "fixture-parent");
      assert.equal(calls[0][calls[0].indexOf("--idempotency-key") + 1], `engram-tests:${basename(runDir)}`);
      assert.ok(readFileSync(join(runDir, "notification.message.txt"), "utf8").includes(`NEXT OWNER ${JSON.stringify(owner)}`));
    }
  });
});

test("notification publication ignores interrupted temporary bytes and freezes one complete body", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("clean")], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    const expected = await summarize(runDir, { compact: true });
    const abandoned = `notification.message.txt.${randomUUID()}.tmp`;
    writeFileSync(join(runDir, abandoned), "partial interrupted body", { mode: 0o600 });
    const bodies = [];
    const send = async (_command, args) => {
      bodies.push(readFileSync(args[args.indexOf("--message-file") + 1], "utf8"));
      return { code: 0, signal: null };
    };
    // Both callers enter before async summarization finishes. Exclusive
    // publication selects one complete body, and both sends use its bytes.
    await Promise.all([notifyRun(runDir, env, send), notifyRun(runDir, env, send)]);
    assert.deepEqual(bodies, [expected, expected]);
    assert.equal(readFileSync(join(runDir, "notification.message.txt"), "utf8"), expected);
    assert.deepEqual(readdirSync(runDir).filter((name) => name.endsWith(".tmp")), [abandoned]);
  });
});

test("summary never renders a shared runner or stage error", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("broken")] }, env);
    const result = json(join(runDir, "results.json"));
    const error = "fixture shared failure detail";
    Object.assign(result, { state: "failed", ended: new Date().toISOString(), exitCode: 1, error });
    Object.assign(result.stages[0], { state: "failed", code: 1, error });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const summary = await summarize(runDir);
    assert.doesNotMatch(summary, /fixture shared failure detail/u);
    assert.match(summary, /stages: total=1 passed=0 failed=1/u);
    assert.ok((await inspectFailure(runDir, "broken")).includes(error));
  });
});

test("all default summaries are metadata-only and preserve boundary evidence", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: requiredStages(), notifyTo: "fixture-parent" }, env);
    const result = json(join(runDir, "results.json"));
    const fingerprint = json(join(runDir, "input.json")).fingerprint;
    Object.assign(result, { state: "passed", ended: new Date().toISOString(), exitCode: 0,
      expectedFingerprint: fingerprint, before: fingerprint, after: fingerprint,
      preflight: ["cargo-version", "cargo-fmt", "cargo-clippy"].map((name) => ({ name, code: 0,
        log: join(runDir, `preflight-${name}.log`) })) });
    for (const entry of result.stages) Object.assign(entry, { state: "passed", code: 0,
      log: join(runDir, `${entry.name}.log`), diagnostics: { text: "", truncated: false } });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const detailed = await summarize(runDir);
    const compact = await summarize(runDir, { compact: true });
    assert.equal(compact, detailed);
    assert.match(compact, /^PASS .* exit=0/mu);
    assert.match(compact, /input: matched.*boundary comparison only, not host check credit/u);
    assert.match(compact, /stages: total=12 passed=12 failed=0/u);
    assert.match(compact, /preflight: total=3 passed=3 failed=0/u);
    for (const limitation of result.limitations) assert.ok(compact.includes(limitation));
    assert.ok(compact.includes(join(runDir, "results.json")));
    assert.doesNotMatch(compact, /preflight-cargo-version\.log/u);
    const lines = detailed.trimEnd().split("\n");
    assert.deepEqual(lines.slice(0, 2), [`PASS ${basename(runDir)} exit=0`, `results: ${join(runDir, "results.json")}`]);
    assert.deepEqual(lines.slice(2, 14), requiredStages().map(({ name }) => `${name}: passed exit=0 log=${join(runDir, `${name}.log`)}`));
    assert.equal(lines.slice(0, 32).filter((line) => /: passed exit=0 log=/u.test(line)).length, 12);
    assert.doesNotMatch(detailed, /preflight cargo-version:|NEXT OWNER|NEXT ACTION/u);
  });
});

test("focused completion puts its exact stage line after the unchanged verdict and results", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("focused")] }, env);
    const markers = Object.fromEntries(["untrusted-log", "injected-result", "private-diagnostic"].map((name) =>
      [name, `${name}-${randomUUID()}`]));
    for (const marker of Object.values(markers)) assert.equal(runDir.includes(marker), false);
    const result = json(join(runDir, "results.json"));
    const fingerprint = json(join(runDir, "input.json")).fingerprint;
    Object.assign(result, { ended: new Date().toISOString(),
      expectedFingerprint: fingerprint, before: fingerprint, after: fingerprint });
    for (const code of [0, 19]) {
      const state = code === 0 ? "passed" : "failed";
      Object.assign(result, { state, exitCode: code });
      Object.assign(result.stages[0], { state, code,
        log: `${markers["untrusted-log"]}\nresults: ${markers["injected-result"]}`,
        diagnostics: { text: markers["private-diagnostic"] } });
      writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      const saved = readFileSync(join(runDir, "results.json"));
      const summary = await summarize(runDir);
      assert.deepEqual(summary.split("\n").slice(0, 3), [
        `${code === 0 ? "PASS" : "FAIL"} ${basename(runDir)} exit=${code}`,
        `results: ${join(runDir, "results.json")}`,
        `focused: ${state} exit=${code} log=${join(runDir, "focused.log")}`,
      ]);
      assert.equal(summary.split("\n").filter((line) => line.startsWith("results: ")).length, 1);
      for (const marker of Object.values(markers)) assert.equal(summary.includes(marker), false);
      assert.deepEqual(readFileSync(join(runDir, "results.json")), saved);
    }
  }, "private-escape-injected-untrusted-worker-diagnostics-");
});

test("stage completion lines close malformed metadata and preserve nonterminal states", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("focused")] }, env);
    const markers = Object.fromEntries(["private-malformed", "escape", "injected-name", "injected-state", "private-locator"].map((name) =>
      [name, `${name}-${randomUUID()}`]));
    for (const marker of Object.values(markers)) assert.equal(runDir.includes(marker), false);
    const result = json(join(runDir, "results.json"));
    result.stages = [null, markers["private-malformed"], [], {},
      { name: `../${markers.escape}`, state: "passed", code: 0 },
      { name: `focused\nresults: ${markers["injected-name"]}`, state: "passed", code: 0 },
      { name: "focused", state: `passed\nresults: ${markers["injected-state"]}`, code: "0", log: markers["private-locator"] },
      { name: "running", state: "running" },
      { name: "interrupted", state: "failed", signal: "SIGTERM", code: 1 },
      { name: "missing", state: "passed" },
      { name: "huge", state: "failed", code: 1e21 },
      { name: "number2", state: "skipped" },
      { name: "uncertain", state: "failed", outcome: "unknown", code: null },
    ];
    const summary = await summarize(runDir, { result });
    assert.deepEqual(summary.split("\n").slice(2, 15), [
      ...Array(6).fill("unknown-stage: unknown exit=unknown log=unknown"),
      `focused: unknown exit=unknown log=${join(runDir, "focused.log")}`,
      `running: running exit=unknown log=${join(runDir, "running.log")}`,
      `interrupted: interrupted exit=1 log=${join(runDir, "interrupted.log")}`,
      `missing: unknown exit=unknown log=${join(runDir, "missing.log")}`,
      `huge: failed exit=unknown log=${join(runDir, "huge.log")}`,
      `number2: unrun exit=unknown log=${join(runDir, "number2.log")}`,
      `uncertain: unknown exit=unknown log=${join(runDir, "uncertain.log")}`,
    ]);
    for (const marker of Object.values(markers)) assert.equal(summary.includes(marker), false);
    assert.equal(summary.split("\n").filter((line) => line.startsWith("results: ")).length, 1);
    for (const stages of [undefined, null, {}]) {
      const metadata = completionMetadata({}, { ...result, stages });
      assert.deepEqual(metadata.stageDetails, []);
      assert.equal(metadata.stages.total, 0);
    }
  }, "private-escape-injected-untrusted-worker-diagnostics-");
});

test("saved passed stages normalize contradictions and preserve higher-priority states", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("focused")],
      nextTransition: { owner: "fixture-owner", action: "Inspect the saved result." } }, env);
    const request = json(join(runDir, "request.json"));
    const original = json(join(runDir, "results.json"));
    const marker = `private-stage-error-${randomUUID()}`;
    assert.equal(runDir.includes(marker), false);
    const passed = { ...original, state: "passed", exitCode: 0,
      ended: new Date(Date.parse(request.started) + 1000).toISOString(),
      expectedFingerprint: request.expectedFingerprint, before: request.expectedFingerprint, after: request.expectedFingerprint };
    for (const [label, changes, state, verdict] of [
      ["clean", { tests: { failed: 0, failures: 0 } }, "passed", "PASS"],
      ["missing exit", { code: undefined }, "unknown", "FAIL"],
      ["null exit", { code: null }, "unknown", "FAIL"],
      ["string exit", { code: "0" }, "unknown", "FAIL"],
      ["unsafe exit", { code: 1e21 }, "unknown", "FAIL"],
      ["nonzero exit", { code: 17 }, "failed", "FAIL"],
      ["error", { error: marker }, "failed", "FAIL"],
      ["known failures", { tests: { failed: 1 } }, "failed", "FAIL"],
      ["readable failures", { tests: { executed: "unknown", failures: 1 } }, "failed", "FAIL"],
      ["unknown outcome", { outcome: "unknown", error: marker, tests: { failures: 1 } }, "unknown", "FAIL"],
      ["signal", { signal: "SIGTERM", outcome: "unknown", error: marker, tests: { failures: 1 } }, "interrupted", "INTERRUPTED"],
      ["skipped", { state: "skipped", error: marker }, "unrun", "FAIL"],
    ]) {
      const entry = { name: "focused", state: "passed", code: 0, ...changes };
      const result = { ...passed, stages: [entry] };
      writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      const saved = readFileSync(join(runDir, "results.json"));
      const metadata = completionMetadata(request, result);
      const summary = await summarize(runDir);
      assert.deepEqual(summary.split("\n").slice(0, 3), [
        `${verdict} ${basename(runDir)} exit=0`,
        `results: ${join(runDir, "results.json")}`,
        `focused: ${state} exit=${Number.isSafeInteger(entry.code) ? entry.code : "unknown"} log=${join(runDir, "focused.log")}`,
      ], label);
      assert.equal(metadata.verdict, verdict, label);
      assert.deepEqual(metadata.stages, { total: 1, passed: 0, failed: 0, interrupted: 0, running: 0, unrun: 0, unknown: 0,
        [state]: 1 }, label);
      const blocker = summary.split("\n").find((line) => line.startsWith("BLOCKER "));
      if (verdict === "PASS") assert.equal(blocker, undefined, label);
      else assert.deepEqual(JSON.parse(blocker.slice(8)), state === "failed"
        ? { reason: "stage-failed", stage: "focused", exit: entry.code }
        : state === "interrupted" ? { reason: "interrupted", stage: "focused" }
          : { reason: "runner-failed", exit: 0, basis: "contradictory-result" }, label);
      assert.equal(summary.split("\n").filter((line) => line.startsWith("results: ")).length, 1, label);
      assert.equal(summary.includes(marker), false, label);
      assert.deepEqual(readFileSync(join(runDir, "results.json")), saved, label);
    }
  });
});

test("larger saved runs keep all stage completion lines in order", async () => {
  await repository(async (root) => {
    const stages = Array.from({ length: 35 }, (_, index) => stage(`stage-${index}`));
    const runDir = await createRun({ root, stages }, env);
    const result = json(join(runDir, "results.json"));
    for (const entry of result.stages) Object.assign(entry, { state: "passed", code: 0 });
    const lines = (await summarize(runDir, { result })).split("\n");
    assert.deepEqual(lines.slice(2, 37), stages.map(({ name }) => `${name}: passed exit=0 log=${join(runDir, `${name}.log`)}`));
    assert.match(lines[38], /^stages: total=35 passed=35 failed=0/u);
  });
});

test("failure completion retains numeric states and omits all warning and failure prose", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("warning"), stage("failure"), stage("later")] }, env);
    const result = json(join(runDir, "results.json"));
    Object.assign(result, { state: "failed", ended: new Date().toISOString(), exitCode: 23,
      clearedToolchainOverrides: ["RUSTUP_TOOLCHAIN"] });
    Object.assign(result.stages[0], { state: "passed", code: 0, log: join(runDir, "warning.log"),
      diagnostics: { text: `warning: preceding stage\n${"w".repeat(6100)}`, truncated: true } });
    Object.assign(result.stages[1], { state: "failed", code: 23, log: join(runDir, "failure.log"),
      diagnostics: { text: "error: essential failure detail", truncated: false } });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const compact = await summarize(runDir, { compact: true });
    assert.match(compact, /^FAIL .*exit=23/mu);
    assert.match(compact, /stages: total=3 passed=1 failed=1 interrupted=0 running=0 unrun=1 unknown=0/u);
    assert.deepEqual(compact.split("\n").slice(2, 5), [
      `warning: passed exit=0 log=${join(runDir, "warning.log")}`,
      `failure: failed exit=23 log=${join(runDir, "failure.log")}`,
      `later: unrun exit=unknown log=${join(runDir, "later.log")}`,
    ]);
    assert.doesNotMatch(compact, /preceding stage|essential failure detail|toolchain:|diagnostics truncated/u);
    assert.match(compact, /input: unknown/u);
    const saved = readFileSync(join(runDir, "results.json"));
    assert.match(await inspectFailure(runDir, "failure"), /essential failure detail/u);
    await assert.rejects(inspectFailure(runDir, "warning"), /requires a failed/u);
    assert.deepEqual(readFileSync(join(runDir, "results.json")), saved);
  });
});

test("exceptional saved success cannot hide an interruption or inject prose", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("exception"), stage("fallback"), stage("count")] }, env);
    const result = json(join(runDir, "results.json"));
    const shared = "shared runner and stage error";
    Object.assign(result, { state: "failed", ended: new Date().toISOString(), exitCode: 1, error: shared,
      preflight: [{ name: "silent", code: 9, log: join(runDir, "preflight-silent.log") }] });
    Object.assign(result.stages[0], { state: "passed", code: 0, signal: "SIGTERM", outcome: "unknown",
      error: shared, reported: "separate reported failure", log: join(runDir, "exception.log") });
    Object.assign(result.stages[1], { state: "passed", code: 0, log: join(runDir, "fallback.log"),
      diagnostics: { text: "", fallback: "no recognized diagnostic; bounded failure tail", truncated: true } });
    Object.assign(result.stages[2], { state: "passed", code: 0, log: join(runDir, "count.log"),
      tests: { executed: "unknown", why: "incomplete-summary", failures: 1 } });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const compact = await summarize(runDir, { compact: true });
    assert.match(compact, /^INTERRUPTED /u);
    assert.match(compact, /stages: total=3 passed=1 failed=1 interrupted=1/u);
    assert.match(compact, /preflight: total=1 passed=0 failed=1/u);
    assert.doesNotMatch(compact, /shared runner|separate reported failure|bounded failure tail|incomplete-summary/u);
    result.stages[0].error = "e".repeat(2500);
    result.error = "r".repeat(2500);
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const truncated = await summarize(runDir, { compact: true });
    assert.equal(truncated, compact);
  });
});

test("compact summaries preserve running, interrupted, recovered and unknown outcomes and input mismatches", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unfinished"), stage("later")] }, env);
    const result = json(join(runDir, "results.json"));
    const at = Date.now();
    Object.assign(result, { heartbeat: { at: new Date(at).toISOString(), everyMs: 1000 } });
    Object.assign(result.stages[0], { state: "running", log: join(runDir, "unfinished.log") });
    const saveResult = () => writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    saveResult();
    assert.match(await summarize(runDir, { compact: true, now: at }), /^RUNNING .* exit=unknown/mu);
    assert.match(await summarize(runDir, { compact: true, now: at + 4000 }), /^INTERRUPTED .* exit=unknown/mu);
    delete result.heartbeat;
    saveResult();
    assert.match(await summarize(runDir, { compact: true }), /^UNKNOWN .* exit=unknown/mu);
    const fingerprint = json(join(runDir, "input.json")).fingerprint;
    Object.assign(result, { state: "failed", ended: new Date(at).toISOString(), exitCode: 1, interrupted: true,
      recovered: { phase: "stage", stage: "unfinished", at: new Date(at).toISOString() },
      expectedFingerprint: fingerprint, before: fingerprint, after: `${fingerprint}-changed` });
    Object.assign(result.stages[0], { state: "failed", outcome: "unknown", error: "interrupted: outcome unknown" });
    saveResult();
    const recovered = await summarize(runDir, { compact: true });
    assert.match(recovered, /^INTERRUPTED .*exit=1/mu);
    assert.match(recovered, /stages: total=2 passed=0 failed=0 interrupted=0 running=0 unrun=1 unknown=1/u);
    assert.match(recovered, /elapsed: ms=unknown basis=recovery-end/u);
    assert.doesNotMatch(recovered, /interrupted: outcome unknown/u);
    assert.match(recovered, /input: mismatch/u);
    delete result.after;
    saveResult();
    assert.match(await summarize(runDir, { compact: true }), /input: unknown/u);
    let sent = false;
    result.state = "running";
    saveResult();
    await assert.rejects(notifyRun(runDir, env, async () => { sent = true; }), /terminal/u);
    assert.equal(sent, false);
  });
});

test("retained failure stays in the artifact and requires explicit inspection", async () => {
  await repository(async (root) => {
    // Sanitized from a real durable repair-gate notification; identities and
    // fingerprints come from this fixture, not from the historical run.
    const diagnostic = "test doctor_cli_refusals_are_json_and_leave_the_store_unchanged ... FAILED\n\nfailures:\n\n---- doctor_cli_refusals_are_json_and_leave_the_store_unchanged stdout ----\n\nthread 'doctor_cli_refusals_are_json_and_leave_the_store_unchanged' (79612) panicked at tests\\diagnostics_cli.rs:673:17:\nassertion `left == right` failed\n  left: Array [String(\"indexes\"), String(\"triggers\"), String(\"fts\"), String(\"derived_tables\")]\n right: Array [String(\"indexes\"), String(\"triggers\"), String(\"fts\")]\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\nfailures:\n    doctor_cli_refusals_are_json_and_leave_the_store_unchanged\n\ntest result: FAILED. 25 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.46s\n\nerror: test failed, to rerun pass `--test diagnostics_cli`\nTemp audit: run=<fixture scratch>; before=0 after=0; new=0; remaining=0";
    const runDir = await createRun({ root, stages: requiredStages() }, env);
    const result = json(join(runDir, "results.json"));
    const fingerprint = json(join(runDir, "input.json")).fingerprint;
    Object.assign(result, { state: "failed", ended: new Date().toISOString(), exitCode: 101,
      expectedFingerprint: fingerprint, before: fingerprint, after: fingerprint,
      clearedToolchainOverrides: ["RUSTUP_TOOLCHAIN"],
      preflight: ["cargo-version", "cargo-fmt", "cargo-clippy"].map((name) => ({ name, code: 0,
        log: join(runDir, `preflight-${name}.log`), diagnostics: { text: "", truncated: false } })) });
    for (const entry of result.stages.slice(0, 3)) Object.assign(entry, { state: "passed", code: 0,
      log: join(runDir, `${entry.name}.log`), diagnostics: { text: "", truncated: false } });
    Object.assign(result.stages[3], { state: "failed", code: 101, log: join(runDir, "rust.log"),
      tests: { executed: "unknown", why: "incomplete-summary", failures: 1 },
      diagnostics: { text: diagnostic, truncated: false } });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const detailed = await summarize(runDir);
    const compact = await summarize(runDir, { compact: true });
    assert.equal(compact, detailed);
    assert.doesNotMatch(compact, /doctor_cli_refusals|panicked|Temp audit/u);
    assert.match(compact, /^FAIL .* exit=101/mu);
    assert.match(compact, /stages: total=12 passed=3 failed=1 interrupted=0 running=0 unrun=8 unknown=0/u);
    assert.ok((await inspectFailure(runDir, "rust", 6000)).includes(diagnostic));
    assert.equal(json(join(runDir, "results.json")).stages[3].diagnostics.text, diagnostic);
    for (const limitation of result.limitations) assert.ok(compact.includes(limitation));
  });
});

test("pre-existing detailed notification retries keep frozen bytes, identity and results", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "execution-count");
    const runDir = await createRun({ root, stages: [stage("clean",
      `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'ran')`)], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    const saved = readFileSync(join(runDir, "results.json"));
    const detailed = `PASS ${basename(runDir)} exit=0\nresults: ${join(runDir, "results.json")}\nwarning: historical frozen diagnostic bytes\n`;
    assert.notEqual(detailed, await summarize(runDir));
    unlinkSync(join(runDir, "notification.target.json")); // Historical runs had only their original request route.
    unlinkSync(join(runDir, "completion.request.json"));
    writeFileSync(join(runDir, "notification.message.txt"), detailed);
    const calls = [];
    const send = async (_command, args) => {
      calls.push(args);
      assert.equal(readFileSync(args[args.indexOf("--message-file") + 1], "utf8"), detailed);
      return { code: calls.length === 1 ? 7 : 0 };
    };
    await assert.rejects(notifyRun(runDir, env, send), /notification failed/u);
    const originalTarget = readFileSync(join(runDir, "notification.target.json"));
    writeFileSync(join(runDir, "request.json"), JSON.stringify({ ...json(join(runDir, "request.json")),
      notifyTo: "rewritten-recipient", nextTransition: { owner: "new-owner", action: "wrong action" } }));
    await notifyRun(runDir, env, send);
    writeFileSync(join(runDir, "request.json"), "{");
    await notifyRun(runDir, env, send);
    assert.deepEqual(calls[0], calls[1]);
    assert.deepEqual(calls[0], calls[2]);
    assert.deepEqual(readFileSync(join(runDir, "notification.target.json")), originalTarget);
    assert.equal(calls[0][calls[0].indexOf("--idempotency-key") + 1], `engram-tests:${basename(runDir)}`);
    assert.deepEqual(readFileSync(join(runDir, "results.json")), saved);
    assert.equal(readFileSync(counter, "utf8"), "ran");
  });
});

test("self-send and changed notification sender are rejected before invoking transport", async () => {
  await repository(async (root) => {
    const selfRun = await createRun({ root, stages: [stage("unused")], notifyTo: "fixture-owner" }, env);
    const selfResult = await executeRun(selfRun, env);
    assert.equal(selfResult.state, "failed");
    assert.equal(selfResult.stages[0].state, "unrun");
    assert.match(selfResult.error, /self|same|sender/iu);
    const runDir = await createRun({ root, stages: [stage("clean")], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    let sent = false;
    const send = async () => { sent = true; return { code: 0 }; };
    await assert.rejects(notifyRun(selfRun, env, send), /self|same|sender/iu);
    await assert.rejects(notifyRun(runDir, { ...env, TERMAL_SESSION_ID: "different-owner" }, send), /session|sender|owner/iu);
    assert.equal(sent, false);
  });
});

}
