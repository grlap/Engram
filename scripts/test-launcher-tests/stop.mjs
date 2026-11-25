import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { basename, dirname, join } from "node:path";
import test, { after } from "node:test";
import { pathToFileURL } from "node:url";
import { removeWithRetry, requestStop, runJobCommand, stopController, stopProtocol, withStopLock } from "../launcher-stop.mjs";
import { warningSink } from "../operation-warning.mjs";
import { createRun, executable, executeRun, machineRecord, notifyRun, processIdentity, recoverRun, sameCreation, summarize } from "../test-launcher.mjs";
import { fixtureHome, removeFixtureHomes } from "../test-temp.mjs";

export function registerStopTests(support) {
  const { env, launcher, stage, json, logPath, git, repository, copyLauncher, directoryBytes, rewriteResults, parkingBroker } = support;
test("stop addresses this worktree's run id, refuses unsupported runs, and retains the first intent", async () => {
  await repository(async (root) => {
    const ask = (runId, actor = "operator-a", reason = "superseded input") => requestStop(runId, { root, actor, reason });
    await assert.rejects(ask("../outside"), /run id/u);
    await assert.rejects(ask(`test-${randomUUID()}`), /does not exist/u);
    const unsupported = await createRun({ root, stages: [stage("unused")] }, env);
    const before = directoryBytes(unsupported);
    await assert.rejects(ask(basename(unsupported)), /no owned Windows job/u);
    assert.deepEqual(directoryBytes(unsupported), before);
    const runDir = await createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
    // Protocol publication is tested on every OS, without launching a process.
    const path = join(runDir, "request.json");
    writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
    await assert.rejects(ask(basename(runDir), "", "reason"), /actor/u);
    await assert.rejects(ask(basename(runDir), "operator", "reason\nline"), /reason/u);
    const [first, second] = await Promise.all([ask(basename(runDir)), ask(basename(runDir), "operator-b", "later")]);
    assert.equal(first.intent.actor, "operator-a");
    assert.deepEqual(first.intent, second.intent);
    assert.equal(second.duplicate, true);
    assert.equal(json(join(runDir, "results.json")).state, "running", "request publication is not cleanup");
    for (const state of ["passed", "failed"]) {
      rewriteResults(runDir, (result) => ({ ...result, state, exitCode: state === "passed" ? 0 : 1, ended: "done" }));
      const bytes = directoryBytes(runDir);
      const result = await ask(basename(runDir));
      assert.equal(result.terminal, true);
      assert.equal(result.requested, false);
      assert.deepEqual(directoryBytes(runDir), bytes, "terminal no-op changes no byte");
    }
  });
});

for (const [caseName, malformed] of [
  ["invalid-json", "{not json"],
  ["wrong-run-id", JSON.stringify({ protocol: stopProtocol, runId: "wrong-run", id: "fixture", requestedAt: new Date().toISOString(), actor: "operator", reason: "bad" })],
]) {
  test(`malformed stop request ${caseName} has a named terminal completion reason`, async () => {
    await repository(async (root) => {
      const runDir = await createRun({ root, stages: [stage("never")], notifyTo: "fixture-parent" }, env);
      const requestPath = join(runDir, "request.json");
      writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), stopProtocol }));
      writeFileSync(join(runDir, "stop.request.json"), malformed);
      const result = await executeRun(runDir, env);
      assert.deepEqual([result.state, result.reason, result.interrupted], ["failed", "stop-request-invalid", undefined]);
      assert.match(await summarize(runDir), /^reason: stop-request-invalid$/mu);
      await notifyRun(runDir, env, async (_command, args) => {
        const body = readFileSync(args[args.indexOf("--message-file") + 1], "utf8");
        assert.match(body, /^reason: stop-request-invalid$/mu);
        return { code: 0 };
      });
    });
  });
}

test("unreadable stop requests have a named terminal completion reason", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("never")], notifyTo: "fixture-parent" }, env);
    const requestPath = join(runDir, "request.json");
    writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), stopProtocol }));
    mkdirSync(join(runDir, "stop.request.json"));
    const result = await executeRun(runDir, env);
    assert.deepEqual([result.state, result.reason, result.interrupted], ["failed", "stop-request-unreadable", undefined]);
    assert.match(await summarize(runDir), /^reason: stop-request-unreadable$/mu);
    await notifyRun(runDir, env, async (_command, args) => {
      const body = readFileSync(args[args.indexOf("--message-file") + 1], "utf8");
      assert.match(body, /^reason: stop-request-unreadable$/mu);
      return { code: 0 };
    });
  });
});

test("stop intent reads retry transient errors for a minute before stopping or latching failure", () => {
  const request = { runId: `test-${randomUUID()}` };
  let calls = 0, cancellations = 0, clock = 0;
  const now = () => clock;
  const intent = { actor: "operator", reason: "superseded" };
  const transient = () => Object.assign(new Error("scanner holds request"), { code: "EPERM" });
  const control = stopController("fixture", request, { now, readIntent() {
    calls += 1;
    if (calls < 3) throw transient();
    return intent;
  } });
  try {
    control.register(() => { cancellations += 1; });
    assert.doesNotThrow(() => control.check());
    assert.throws(() => control.check(), (error) => error.reason === "stopped" && error.stop === intent);
    assert.deepEqual([calls, cancellations], [3, 1]);
  } finally { control.close(); }

  // Refusals within the minute neither end the run nor latch; one warning
  // marks ten seconds of them, and the minute's end latches the failure.
  let failedCalls = 0;
  const warnings = [];
  clock = 0;
  const unreadable = stopController("fixture", request, { now, warn: (text) => warnings.push(text), readIntent() {
    failedCalls += 1;
    throw Object.assign(new Error("scanner holds request"), { code: "EBUSY" });
  } });
  try {
    for (const at of [0, 200, 10_000, 30_000, 59_999]) {
      clock = at;
      assert.doesNotThrow(() => unreadable.check(), `refused at ${at} ms`);
    }
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /^warning: stop request read is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=10000\n$/u);
    clock = 60_000;
    assert.throws(() => unreadable.check(), (error) => error.reason === "stop-request-unreadable");
    assert.equal(failedCalls, 6);
  } finally { unreadable.close(); }

  // An episode past ten seconds is named once however it ends: by a read that
  // succeeds, by a refusal first seen past the minute, or by a permanent error.
  for (const [steps, outcome] of [
    [[[0, "busy"], [9_990, "busy"], [10_020, "read"]], "read"],
    [[[0, "busy"], [60_000, "busy"]], "unreadable"],
    [[[0, "busy"], [9_990, "busy"], [10_020, "invalid"]], "invalid"],
  ]) {
    warnings.length = 0;
    let state = "busy";
    const episode = stopController("fixture", request, { now, warn: (text) => warnings.push(text), readIntent() {
      if (state === "read") return null;
      if (state === "invalid") throw Object.assign(new Error("malformed request"), { reason: "stop-request-invalid" });
      throw Object.assign(new Error("scanner holds request"), { code: "EBUSY" });
    } });
    try {
      let ended = "read";
      for (const [at, next] of steps) {
        clock = at;
        state = next;
        try { episode.check(); } catch (error) {
          ended = { "stop-request-unreadable": "unreadable", "stop-request-invalid": "invalid" }[error.reason] ?? "other";
        }
      }
      assert.equal(ended, outcome);
      assert.equal(warnings.length, 1, `${outcome}: ${warnings.join("")}`);
    } finally { episode.close(); }
  }

  // A single read that itself takes eleven seconds is named too, whether it
  // finds no request or a malformed one.
  for (const [read, outcome] of [[() => null, "read"],
    [() => { throw Object.assign(new Error("malformed request"), { reason: "stop-request-invalid" }); }, "invalid"]]) {
    warnings.length = 0;
    clock = 0;
    const slow = stopController("fixture", request, { now, warn: (text) => warnings.push(text), readIntent() {
      clock += 11_000;
      return read();
    } });
    try {
      let ended = "read";
      try { slow.check(); } catch (error) { ended = error.reason === "stop-request-invalid" ? "invalid" : "other"; }
      assert.equal(ended, outcome);
      assert.equal(warnings.length, 1, `${outcome}: ${warnings.join("")}`);
      assert.match(warnings[0], /elapsed_ms=11000\n$/u);
    } finally { slow.close(); }
  }
});

test("final stop check settles a transient request before terminal commitment", async () => {
  const request = { runId: `test-${randomUUID()}` };
  const intent = { actor: "operator", reason: "superseded" };
  let calls = 0;
  const control = stopController("fixture", request, { retryDelayMs: 0, readIntent() {
    calls += 1;
    if (calls === 1) throw Object.assign(new Error("scanner holds request"), { code: "EPERM" });
    return intent;
  } });
  try {
    await assert.rejects(control.settle(), (error) => error.reason === "stopped" && error.stop === intent);
    assert.equal(calls, 2);
  } finally { control.close(); }
});

test("published stop intents warn after bounded temporary cleanup retries", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
    const requestPath = join(runDir, "request.json");
    writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), stopProtocol }));
    let temporaryAttempts = 0;
    const stopped = await requestStop(basename(runDir), {
      root, actor: "operator", reason: "superseded", removeAttempts: 3, removeDelayMs: 0,
      remove(path, options) {
        if (basename(path).startsWith("stop-")) {
          temporaryAttempts += 1;
          throw Object.assign(new Error("scanner holds temporary"), { code: "EBUSY" });
        }
        rmSync(path, options);
      },
    });
    assert.equal(stopped.requested, true);
    assert.match(stopped.warning, /cleanup failed after publication: EBUSY/u);
    assert.equal(temporaryAttempts, 3);
    assert.deepEqual(json(join(runDir, "stop.request.json")), stopped.intent);
    assert.deepEqual(readdirSync(runDir).filter((name) => name.startsWith("stop-")).sort(), [`stop-${stopped.intent.id}.tmp`]);
  });
});

test("stop and terminal commitment share a lock, so a completed run is never signalled", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
    const path = join(runDir, "request.json");
    writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
    let pending;
    await withStopLock(runDir, async () => {
      pending = requestStop(basename(runDir), { root, actor: "operator", reason: "racing completion" });
      rewriteResults(runDir, (result) => ({ ...result, state: "passed", exitCode: 0, ended: "done" }));
    });
    assert.equal((await pending).requested, false);
    assert.equal(existsSync(join(runDir, "stop.request.json")), false);
  });
});

test("stop lock removal retries transient refusals without losing an action's result", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    let calls = 0;
    const result = await withStopLock(runDir, () => "committed", { remove(path) {
      calls++;
      if (calls < 3) throw Object.assign(new Error("scanner holds lock"), { code: "EPERM" });
      unlinkSync(path);
    } });
    assert.equal(result, "committed");
    assert.equal(calls, 3);
    assert.equal(existsSync(join(runDir, "stop.control.lock")), false);
    // A refusal lasting a whole second, well past the old twenty tries, is
    // still waited out, so no lock is left for the next stop to wait on.
    const refusedUntil = performance.now() + 1_000;
    assert.equal(await withStopLock(runDir, () => "waited out", { remove(path) {
      if (performance.now() < refusedUntil) throw Object.assign(new Error("scanner holds lock"), { code: "EBUSY" });
      unlinkSync(path);
    } }), "waited out");
    assert.equal(existsSync(join(runDir, "stop.control.lock")), false);
    // One refused past its window is reported, and the action still committed.
    const failures = [];
    assert.equal(await withStopLock(runDir, () => "still committed", {
      releaseWindowMs: 0,
      remove() { throw Object.assign(new Error("retained"), { code: "EBUSY" }); },
      onReleaseError(error) { failures.push(error.code); },
    }), "still committed");
    assert.deepEqual(failures, ["EBUSY"]);
  });
});

test("a removal refused past the warning time names itself once and still completes", async (t) => {
  const warnings = [];
  t.mock.method(warningSink, "write", (text) => { warnings.push(text); });
  const refusedUntil = performance.now() + 200;
  let removed = false;
  await removeWithRetry("fixture", { warnAfter: 50, operation: "stop control lock release", remove() {
    if (performance.now() < refusedUntil) throw Object.assign(new Error("scanner holds lock"), { code: "EPERM" });
    removed = true;
  } });
  assert.equal(removed, true);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0], /^warning: stop control lock release is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=\d+\n$/u);
});

// A synchronous removal can hold the warning timer back past its time; the
// removal still names itself once as it settles, by success or by refusal.
test("a removal whose last call crosses ten seconds is named as it settles", async (t) => {
  const warnings = [];
  t.mock.method(warningSink, "write", (text) => { warnings.push(text); });
  for (const [steps, outcome] of [
    [[[9_990, "EPERM"], [20, null]], "removed"],
    [[[11_000, "ENOENT"]], "refused"],
  ]) {
    warnings.length = 0;
    let clock = 0, call = 0;
    let ended = "removed";
    try {
      await removeWithRetry("fixture", { now: () => clock, delayMs: 0, remove() {
        const [took, code] = steps[call++];
        clock += took;
        if (code) throw Object.assign(new Error(code), { code });
      } });
    } catch { ended = "refused"; }
    assert.equal(ended, outcome);
    assert.equal(warnings.length, 1, `${outcome}: ${warnings.join("")}`);
    assert.match(warnings[0], new RegExp(`elapsed_ms=${steps.reduce((sum, [took]) => sum + took, 0)}\n$`, "u"));
  }
});

for (const code of ["EPERM", "EACCES", "EBUSY"]) test(`stop lock retries ${code} acquisition and still saves its terminal action`, async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    let attempts = 0, commits = 0;
    await withStopLock(runDir, () => {
      commits++;
      rewriteResults(runDir, (result) => ({ ...result, state: "passed", exitCode: 0, ended: "done" }));
    }, { open(...args) {
      attempts++;
      if (attempts < 3) throw Object.assign(new Error("injected sharing refusal"), { code });
      return openSync(...args);
    } });
    assert.equal(attempts, 3);
    assert.equal(commits, 1);
    assert.equal(json(join(runDir, "results.json")).state, "passed");
    assert.equal(existsSync(join(runDir, "stop.control.lock")), false);
  });
});

// Retained limit: this waits out the launcher's real one-minute stop lock, which is
// product behaviour. The test timeout is only a hang guard well above it.
test("a leftover stop lock cannot discard a terminal result or its notification", { timeout: 180_000 }, async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, full: true, detached: true, stages: [stage("never")], notifyTo: "fixture-parent" },
      { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "1000" });
    const path = join(runDir, "request.json");
    writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
    await requestStop(basename(runDir), { root, actor: "operator", reason: "superseded" });
    writeFileSync(join(runDir, "stop.control.lock"), "left by a dead caller");
    // While the terminal save waits out the lock, the saved record stays a
    // running one whose heartbeat keeps moving.
    const beats = new Set();
    let provisional = false;
    const watch = setInterval(() => {
      try {
        const saved = json(join(runDir, "results.json"));
        if (saved.ended) return;
        beats.add(saved.heartbeat.at);
        if (saved.exitCode !== undefined && saved.exitCode !== null) provisional = true;
      } catch { /* the launcher may be mid-save */ }
    }, 250);
    let result;
    try { result = await executeRun(runDir, env); }
    finally { clearInterval(watch); }
    assert.ok(beats.size >= 10, `the heartbeat moved through the lock wait: ${beats.size} beats`);
    assert.equal(provisional, false, "no provisional terminal field was saved before the terminal save");
    assert.deepEqual([result.state, result.reason, result.interrupted], ["failed", "stopped", true]);
    assert.ok(result.ended);
    assert.match(result.stopLockWarning, /timed out/u);
    assert.equal(result.stop.actor, "operator");
    assert.equal(existsSync(join(runDir, "stop.control.lock")), true, "never guess a lock owner's lifetime");
    await notifyRun(runDir, env, async (_command, args) => {
      const body = readFileSync(args[args.indexOf("--message-file") + 1], "utf8");
      assert.match(body, /^INTERRUPTED /u);
      assert.match(body, /^reason: stopped$/mu);
      return { code: 0 };
    });
    assert.equal(json(join(runDir, "notification.json")).ok, true);
  });
});

test("stopped completion keeps the interrupted wire vocabulary and notification retry payload", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("rust"), stage("later")], notifyTo: "fixture-parent" }, env);
    const request = json(join(runDir, "request.json"));
    const result = { ...json(join(runDir, "results.json")), state: "failed", exitCode: 1, interrupted: true,
      reason: "stopped", error: "stopped by operator: fixture-owner: superseded", ended: new Date().toISOString(),
      stop: { actor: "fixture-owner", reason: "superseded", requestedAt: new Date().toISOString() },
      stages: [{ name: "rust", state: "failed", outcome: "unknown" }, { name: "later", state: "unrun" }] };
    rewriteResults(runDir, () => result);
    const summary = await summarize(runDir);
    assert.match(summary, /^INTERRUPTED test-\S+ exit=1$/mu);
    assert.match(summary, /^reason: stopped$/mu);
    assert.match(summary, /stop: actor="fixture-owner" reason="superseded"/u);
    const record = machineRecord(request, result);
    assert.match(record, /overall=interrupted .*reason=stopped/u);
    assert.doesNotMatch(record, /overall=passed|state=passed/u);
    let payload, key;
    await assert.rejects(notifyRun(runDir, env, async (_command, args) => {
      payload = readFileSync(args[args.indexOf("--message-file") + 1]);
      key = args[args.indexOf("--idempotency-key") + 1];
      return { code: 1 };
    }), /notification failed/u);
    await notifyRun(runDir, env, async (_command, args) => {
      assert.deepEqual(readFileSync(args[args.indexOf("--message-file") + 1]), payload);
      assert.equal(args[args.indexOf("--idempotency-key") + 1], key);
      return { code: 0 };
    });
    assert.deepEqual(json(join(runDir, "results.json")), result);
    assert.equal((await recoverRun(runDir)).settled, false);
  });
});

// The supported capability is Windows job containment. Other platforms still
// exercise all intent/terminal/notification rules above; their stop is refused.
if (process.platform === "win32") for (const active of ["cargo-check", "rust", "rust-scale-claims"]) {
  test(`stop during ${active} reaps a detached grandchild before completion and notification`, { timeout: 180_000 }, async (t) => {
    await repository(async (root) => {
      // Hermetic stages, not real compilation. Only cargo-version preflights
      // are replaced; admission, containment, stop and finish remain the real path.
      const local = copyLauncher(root, (source) => source.replace("if (request.full) {", "if (false) {"));
      const broker = parkingBroker(t, root);
      let child, completion;
      try {
        assert.equal((await broker.next())[0], "listening");
        const pidFile = join(root, ".git", "stage.pid");
        const descendantFile = join(root, ".git", "descendant.pid");
        const nested = `require('node:fs').writeFileSync(${JSON.stringify(descendantFile)}, String(process.pid)); ${broker.parked}`;
        const tree = `require('node:fs').writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
          const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(nested)}],
            { detached: true, stdio: 'ignore', windowsHide: true });
          child.on('close', () => process.exit(0));`;
        writeFileSync(join(root, "mailbox"), `
          const fs = require('node:fs');
          const path = require('node:path');
          const message = process.argv[process.argv.indexOf('--message-file') + 1];
          const results = JSON.parse(fs.readFileSync(path.join(path.dirname(message), 'results.json'), 'utf8'));
          if (!results.interrupted || results.reason !== 'stopped' || results.processCleanup.some(x => !x.confirmed)) process.exitCode = 8;
          const body = fs.readFileSync(message, 'utf8');
          if (!body.startsWith('INTERRUPTED ')) process.exitCode = 9;
        `);
        const runDir = await createRun({ root, full: true, detached: true, notifyTo: "fixture-parent",
          stages: [stage("first"), stage(active, tree), stage("later", "throw Error('must not run')")] }, env);
        child = spawn(process.execPath, [local, "_run", runDir], { cwd: root, env, windowsHide: true, stdio: "ignore" });
        completion = once(child, "close");
        const ready = await Promise.race([broker.next(), completion.then(([code]) => [`launcher ended (${code}): ${readFileSync(join(runDir, "results.json"), "utf8")}`])]);
        assert.equal(ready[0], "connected");
        const pids = [pidFile, descendantFile].map((path) => Number(readFileSync(path, "utf8")));
        const identities = pids.map((pid) => processIdentity(pid));
        assert.ok(identities.every(({ state }) => state === "alive"));
        const prior = json(join(runDir, "results.json"));
        const priorLog = readFileSync(logPath(runDir, prior.stages[0]));
        const stopped = await requestStop(basename(runDir), { root, actor: "fixture-owner", reason: "superseded input" });
        assert.equal(stopped.requested, true);
        assert.equal((await completion)[0], 1);
        const result = json(join(runDir, "results.json"));
        assert.deepEqual([result.state, result.interrupted, result.reason, result.exitCode], ["failed", true, "stopped", 1]);
        assert.deepEqual(result.stages[0], prior.stages[0]);
        assert.deepEqual(readFileSync(logPath(runDir, result.stages[0])), priorLog);
        assert.deepEqual(result.stages.map(({ state }) => state), ["passed", "failed", "unrun"]);
        assert.ok(result.processCleanup.length >= 2);
        assert.ok(result.processCleanup.every(({ confirmed, activeProcesses }) => confirmed && activeProcesses === 0));
        assert.equal(json(join(runDir, "notification.json")).ok, true);
        assert.equal(broker.release() === false, false, "the unrelated broker is still alive");
        for (const [index, pid] of pids.entries()) {
          const found = processIdentity(pid);
          assert.ok(found.state === "gone" || (found.state === "alive" && !sameCreation(identities[index].created, found)), JSON.stringify(found));
        }
        const terminal = directoryBytes(runDir);
        const noOp = await requestStop(basename(runDir), { root, actor: "late", reason: "too late" });
        assert.equal(noOp.terminal, true);
        assert.deepEqual(directoryBytes(runDir), terminal);
        const cli = spawnSync(process.execPath, [local, "stop", basename(runDir), "--actor", "late", "--reason", "already finished"],
          { cwd: root, env, encoding: "utf8", windowsHide: true });
        assert.equal(cli.status, 0, cli.stderr);
        assert.match(cli.stdout, /^Already terminal; nothing stopped\./u);
        assert.deepEqual(directoryBytes(runDir), terminal);
      } finally {
        // Release only our fixture's socket; its root exits and the job then
        // cleans up. No arbitrary process-name or PID kill is used.
        await broker.close();
        if (completion) await completion;
      }
    });
  });
}

if (process.platform === "win32") test("a job cleans a detached descendant after its stage parent exits normally", { timeout: 90_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let pending, control;
    try {
      assert.equal((await broker.next())[0], "listening");
      const childSource = `const socket = require('node:net').connect(${JSON.stringify(broker.address)});
        socket.on('connect', () => process.send('ready')); socket.on('error', () => process.exit(1));`;
      const parentSource = `const child = require('node:child_process').spawn(process.execPath,
        ['-e', ${JSON.stringify(childSource)}], { detached: true, stdio: ['ignore', 'ignore', 'ignore', 'ipc'] });
        child.on('message', () => process.exit(0));`;
      const runDir = await createRun({ root, full: true, detached: true, stages: [stage("parent-exit")] }, env);
      control = stopController(runDir, json(join(runDir, "request.json")));
      pending = runJobCommand(process.execPath, ["-e", parentSource], { cwd: root, env, runDir, control,
        log: join(runDir, "parent-exit.log"),
        powerShell: join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe") });
      const result = await pending;
      assert.equal(result.code, 0);
      assert.equal(result.stopped, false);
      assert.deepEqual([result.cleanup.confirmed, result.cleanup.activeProcesses], [true, 0]);
    } finally {
      control?.close();
      await broker.close();
      if (pending) await pending;
    }
  });
});

if (process.platform === "win32") test("a retained stop before admission starts no preflight or stage", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, full: true, detached: true, notifyTo: "fixture-parent", stages: [stage("never")] }, env);
    await requestStop(basename(runDir), { root, actor: "fixture-owner", reason: "superseded before spawn" });
    const result = await executeRun(runDir, env);
    assert.deepEqual([result.state, result.interrupted, result.reason], ["failed", true, "stopped"]);
    assert.ok(result.stages.every(({ state }) => state === "unrun"));
    assert.deepEqual(result.preflight, []);
    assert.equal(result.processCleanup, undefined);
  });
});

// These fixtures run the copied real worker and supervisor. Only failure
// boundaries or OS observations are injected; no real stuck driver is needed.
async function containedFixture(root, transform = (source) => source, source = "") {
  const local = copyLauncher(root, transform);
  const module = await import(pathToFileURL(local).href);
  const runDir = await module.createRun({ root, stages: [stage("contained", source)] }, env);
  const path = join(runDir, "request.json");
  writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
  return { module, runDir, scripts: dirname(local) };
}

if (process.platform === "win32") test("a stop accepted at the pre-spawn boundary retains its classification and actor", async () => {
  await repository(async (root) => {
    const { module, runDir } = await containedFixture(root, (source) => {
      const changed = source.replace(/control\?\.check\(\);\r?\n    let outcome;/u, `control?.check();
    await requestStop(request.runId, { root: request.root, actor: 'boundary-owner', reason: 'superseded at spawn' });
    let outcome;`);
      assert.notEqual(changed, source);
      return changed;
    });
    const requestPath = join(runDir, "request.json");
    writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), full: true, detached: true }));
    const result = await module.executeRun(runDir, env);
    assert.deepEqual([result.reason, result.interrupted, result.stop.actor], ["stopped", true, "boundary-owner"]);
    assert.equal(result.processCleanup, undefined);
    assert.equal(readdirSync(runDir).some((name) => name.startsWith("job-")), false);
  });
});

if (process.platform === "win32") test("a transient final stop read under the commit lock surfaces the retained stop, not a pass", async () => {
  await repository(async (root) => {
    // Only the final read's boundary is injected: the copied worker's stop
    // reads see no request until commit holds the stop lock. There the first
    // read is refused as a scanner would refuse it, and the next one finds a
    // retained request.
    const anchor = "control = stopController(runDir, request);";
    const { module, runDir } = await containedFixture(root, (source) => {
      const changed = source.replace(anchor, `control = stopController(runDir, request, { retryDelayMs: 0, readIntent: (() => {
      let finalReads = 0;
      return (dir, runId) => {
        if (!existsSync(join(dir, "stop.control.lock"))) return null;
        finalReads += 1;
        if (finalReads === 1) {
          writeFileSync(join(dir, "final-read-eperm.marker"), "");
          throw Object.assign(new Error("scanner holds request"), { code: "EPERM" });
        }
        return { protocol: stopProtocol, runId, id: "final-read-fixture", actor: "final-owner",
          reason: "superseded at commit", requestedAt: new Date().toISOString() };
      };
    })() });`);
      assert.notEqual(changed, source);
      return changed;
    });
    const result = await module.executeRun(runDir, env);
    assert.ok(existsSync(join(runDir, "final-read-eperm.marker")), "the final read was refused once under the lock");
    assert.deepEqual([result.state, result.reason, result.interrupted], ["failed", "stopped", true]);
    assert.deepEqual([result.stop.id, result.stop.actor], ["final-read-fixture", "final-owner"]);
    assert.deepEqual(result.stages.map(({ state }) => state), ["passed"]);
    assert.deepEqual([result.processCleanup?.[0]?.confirmed, result.processCleanup?.[0]?.activeProcesses], [true, 0]);
    const saved = json(join(runDir, "results.json"));
    assert.deepEqual([saved.state, saved.reason, saved.interrupted, saved.stop.id], ["failed", "stopped", true, "final-read-fixture"]);
  });
});

if (process.platform === "win32") test("helper compilation failure is definite startup failure, not uncertain cleanup", async () => {
  await repository(async (root) => {
    const local = copyLauncher(root);
    writeFileSync(join(root, "scripts", "launcher-process-job.cs"), "invalid C# fixture");
    const module = await import(pathToFileURL(local).href);
    const runDir = await module.createRun({ root, stages: [stage("contained")] }, env);
    const path = join(runDir, "request.json");
    writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
    const result = await module.executeRun(runDir, env);
    assert.equal(result.reason, "helper-start-failed");
    assert.equal(result.interrupted, undefined);
    assert.ok(result.ended);
    const receipt = readdirSync(runDir).find((name) => /^job-.*\.json$/u.test(name));
    assert.equal(json(join(runDir, receipt)).Phase, "helper");
    assert.match(await module.summarize(runDir), /^reason: helper-start-failed$/mu);
    assert.deepEqual(readdirSync(join(root, "target", "tmp", "launcher-jobs")), []);
  });
});

if (process.platform === "win32") test("native cleanup confirmation returns an unconfirmed receipt when membership never settles", { timeout: 120_000 }, async () => {
  await repository(async (root) => {
    const local = copyLauncher(root);
    const path = join(root, "scripts", "launcher-process-job.cs");
    const source = readFileSync(path, "utf8");
    // The real job is empty; inject only the observed membership count and a
    // shorter confirmation allowance and warning time, simulating an OS
    // cleanup that stalls.
    // Keyed on the names, so the production values can change freely.
    const changed = source.replace(/(\bCleanupBudgetMs = )[^;]+;/u, (_, name) => `${name}100;`)
      .replace(/(\bWarnAfterMs = )[^;]+;/u, (_, name) => `${name}50;`)
      .replace("active = accounting.ActiveProcesses;", "active = accounting.ActiveProcesses + 1;");
    for (const injected of ["CleanupBudgetMs = 100;", "WarnAfterMs = 50;", "active = accounting.ActiveProcesses + 1;"]) {
      assert.ok(changed.includes(injected), injected);
    }
    writeFileSync(path, changed);
    const module = await import(pathToFileURL(local).href);
    const runDir = await module.createRun({ root, stages: [stage("contained")] }, env);
    const requestPath = join(runDir, "request.json");
    writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), stopProtocol }));
    const result = await module.executeRun(runDir, env);
    assert.equal(result.reason, "process-cleanup-unconfirmed");
    const receipt = readdirSync(runDir).find((name) => /^job-.*\.json$/u.test(name));
    assert.deepEqual([json(join(runDir, receipt)).CleanupConfirmed, json(join(runDir, receipt)).ActiveProcesses], [false, 1]);
    assert.ok(result.ended);
    // The stalled confirmation named itself once in the stage log.
    const warned = readFileSync(logPath(runDir, result.stages[0]), "utf8")
      .match(/^warning: process job cleanup confirmation is still running after 10 seconds; attempt=\d+\/0; elapsed_ms=\d+\r?$/gmu);
    assert.equal(warned?.length, 1);
  });
});

if (process.platform === "win32") for (const proof of ["missing", "unconfirmed"]) {
  test(`${proof} cleanup proof overrides a requested stop in result, summary and notification`, async () => {
    await repository(async (root) => {
      const local = copyLauncher(root);
      writeFileSync(join(root, "scripts", "launcher-process-job.ps1"), `param([string]$Spec)
$request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Spec)) | ConvertFrom-Json
$intent = @{ protocol = 'windows-job-v1'; runId = [IO.Path]::GetFileName([IO.Path]::GetDirectoryName($request.receipt));
 id = 'fixture'; actor = 'operator'; reason = 'superseded'; requestedAt = [DateTime]::UtcNow.ToString('o') }
[IO.File]::WriteAllText((Join-Path ([IO.Path]::GetDirectoryName($request.receipt)) 'stop.request.json'), ($intent | ConvertTo-Json))
${proof === "unconfirmed" ? "[IO.File]::WriteAllText($request.receipt, '{\"ExitCode\":1,\"Stopped\":true,\"CleanupConfirmed\":false,\"ActiveProcesses\":1}')" : ""}
exit 1
`);
      const module = await import(pathToFileURL(local).href);
      const runDir = await module.createRun({ root, stages: [stage("contained")], notifyTo: "fixture-parent" }, env);
      const path = join(runDir, "request.json");
      writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
      const result = await module.executeRun(runDir, env);
      assert.deepEqual([result.state, result.reason, result.interrupted, result.stop], ["failed", "process-cleanup-unconfirmed", undefined, undefined]);
      assert.match(await module.summarize(runDir), /^reason: process-cleanup-unconfirmed$/mu);
      assert.match(module.machineRecord(json(path), result), /reason=process-cleanup-unconfirmed/u);
      await module.notifyRun(runDir, env, async (_command, args) => {
        const body = readFileSync(args[args.indexOf("--message-file") + 1], "utf8");
        assert.match(body, /^FAIL /u);
        assert.match(body, /^reason: process-cleanup-unconfirmed$/mu);
        assert.doesNotMatch(body, /stop: actor=/u);
        return { code: 0 };
      });
    });
  });
}

if (process.platform === "win32") for (const policy of ["Restricted", "AllSigned"]) test(`inline helper works under ${policy} policy and preserves workload environment and DWORD exits`, async () => {
  await repository(async (root) => {
    copyLauncher(root);
    const path = join(root, "scripts", "launcher-stop.mjs");
    const source = readFileSync(path, "utf8");
    const restricted = source.replace('["-NoProfile", "-NonInteractive", "-Command", bootstrap]',
      `["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "${policy}", "-Command", bootstrap]`);
    assert.notEqual(restricted, source);
    writeFileSync(path, restricted);
    const module = await import(pathToFileURL(path).href);
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    const control = stopController(runDir, json(join(runDir, "request.json")));
    const childEnv = { ...env, TEMP: root, TMP: root, PSModulePath: "fixture-original-modules" };
    const log = join(runDir, "environment.log");
    try {
      const result = await module.runJobCommand(process.execPath,
        ["-e", "console.log(JSON.stringify({ TEMP: process.env.TEMP, TMP: process.env.TMP, PSModulePath: process.env.PSModulePath })); process.exit(0xC0000005)"],
        { cwd: root, env: childEnv, runDir, control, log,
          powerShell: join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe") });
      assert.equal(result.code, 0xC0000005);
      assert.equal(result.cleanup.confirmed, true);
      const line = readFileSync(log, "utf8").split(/\r?\n/u).find((value) => value.startsWith('{"TEMP"'));
      assert.deepEqual(JSON.parse(line), { TEMP: root, TMP: root, PSModulePath: childEnv.PSModulePath });
      assert.deepEqual(readdirSync(join(root, "target", "tmp", "launcher-jobs")), []);
    } finally { control.close(); }
  });
});

if (process.platform === "win32") test("a missing helper executable reports startup failure without a possible workload", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    const control = stopController(runDir, json(join(runDir, "request.json")));
    try {
      await assert.rejects(runJobCommand(process.execPath, [], { cwd: root, env, runDir, control,
        log: join(runDir, "missing-helper.log"), powerShell: join(root, "missing-powershell.exe") }),
      (error) => error.reason === "helper-start-failed" && error.cleanupUnconfirmed !== true);
      assert.equal(readdirSync(runDir).some((name) => /^job-.*\.json$/u.test(name)), false);
      assert.deepEqual(readdirSync(join(root, "target", "tmp", "launcher-jobs")), []);
      const failedLaunch = await runJobCommand(join(root, "missing-stage.exe"), [], { cwd: root, env, runDir, control,
        log: join(runDir, "failed-launch.log"),
        powerShell: join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe") });
      assert.equal(failedLaunch.code, 1);
      assert.equal(typeof failedLaunch.error, "string");
      assert.equal(failedLaunch.cleanup.confirmed, true, "a failed CreateProcess has no uncertain workload");
      assert.deepEqual(readdirSync(join(root, "target", "tmp", "launcher-jobs")), []);
    } finally { control.close(); }
  });
});

if (process.platform === "win32") for (const ancestor of ["target", "target/tmp", "target/tmp/launcher-jobs"]) {
  test(`compiler scratch refuses a linked ${ancestor} ancestor before creating anything at its destination`, async () => {
    await repository(async (root) => {
      // The destination is outside this fixture repository, but still inside
      // the owning checkout's target/ as required for every test artifact.
      const destination = fixtureHome("launcher-junction-destination-");
      const link = join(root, ...ancestor.split("/"));
      const runDir = await createRun({ root, stages: [stage("unused")] }, env);
      const control = stopController(runDir, json(join(runDir, "request.json")));
      const log = join(runDir, "linked-scratch.log");
      try {
        writeFileSync(join(destination, "sentinel"), "must stay unchanged");
        const before = directoryBytes(destination);
        mkdirSync(dirname(link), { recursive: true });
        symlinkSync(destination, link, "junction");
        await assert.rejects(runJobCommand(process.execPath, [], { cwd: root, env, runDir, control, log,
          powerShell: join(root, "missing-powershell.exe") }),
        (error) => error.reason === "helper-start-failed" && /scratch ancestor is a link/u.test(error.message)
          && error.cleanupUnconfirmed !== true);
        assert.deepEqual(directoryBytes(destination), before);
        assert.equal(existsSync(log), false, "refusal precedes log creation and helper startup");
        assert.equal(readdirSync(runDir).some((name) => /^job-.*\.json$/u.test(name)), false);
      } finally {
        control.close();
        // Unlink this known fixture junction itself before recursive fixture
        // cleanup. No recursive deletion is performed through the link.
        if (existsSync(link) && lstatSync(link).isSymbolicLink()) unlinkSync(link);
        removeFixtureHomes(destination);
      }
    });
  });
}

// Retained limit: the same real one-minute stop lock, under the same hang guard.
if (process.platform === "win32") test("a leftover stop lock after a clean stage preserves the passing terminal save", { timeout: 180_000 }, async () => {
  await repository(async (root) => {
    const { module, runDir } = await containedFixture(root, (source) => source,
      "require('node:fs').writeFileSync(process.env.FIXTURE_STOP_LOCK, 'stale');");
    const result = await module.executeRun(runDir, { ...env, FIXTURE_STOP_LOCK: join(runDir, "stop.control.lock") });
    assert.equal(result.state, "passed");
    assert.equal(result.exitCode, 0);
    assert.match(result.stopLockWarning, /timed out/u);
    assert.ok(result.ended);
  });
});
}
