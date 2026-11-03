import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { dirname, join } from "node:path";
import test, { after, describe } from "node:test";
import { fileURLToPath } from "node:url";
import { completionMetadata, createRun, executeRun, notifyRun, processHost, processIdentity, recoverRun, renameWithRetry, requiredStages, sameCreation, startDetached, summarize, systemProbes, wellFormedCreated, wmiFileTime } from "../test-launcher.mjs";

export function registerRecoveryLiveness(support) {
  const { env, entryUrl, launcher, launcherSupportFiles, launcherFixtureFiles, stage, json, git, repository, directoryBytes, parkingBroker } = support;
test("an incomplete run is never a pass and cannot notify a successful completion", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("not-started")], notifyTo: "fixture-parent" }, env);
    const created = Date.parse(json(join(runDir, "results.json")).heartbeat.at);
    assert.match(await summarize(runDir, { now: created }), /^RUNNING .* exit=unknown$/mu);
    assert.match(await summarize(runDir, { now: created + 3_600_000 }), /^INTERRUPTED .* exit=unknown$/mu);
    let sent = false;
    await assert.rejects(notifyRun(runDir, env, async () => { sent = true; return { code: 0 }; }), /terminal|running|incomplete/iu);
    assert.equal(sent, false);
  });
});

test("liveness comes from the heartbeat, never from a process at the recorded pid", async () => {
  await repository(async (root) => {
    await assert.rejects(() => createRun({ root, stages: [stage("refused")] },
      { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "5" }), /ENGRAM_LAUNCHER_HEARTBEAT_MS/u);
    assert.equal(existsSync(join(root, ".git", "review-runs")), false, "an invalid cadence refuses before creating a run");
    const runDir = await createRun({ root, stages: [stage("never-started")] },
      { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "1000" });
    const path = join(runDir, "results.json");
    const created = json(path);
    assert.equal(created.heartbeat.everyMs, 1000);
    const at = Date.parse(created.heartbeat.at);
    assert.match(await summarize(runDir, { now: at + 3000 }), /^RUNNING /u);
    assert.match(await summarize(runDir, { now: at + 3001 }), /^INTERRUPTED /u);
    // A live process now holding the recorded pid does not revive the run.
    writeFileSync(path, JSON.stringify({ ...created, pid: process.pid }));
    assert.match(await summarize(runDir, { now: at + 3001 }), /^INTERRUPTED /u);
    const { heartbeat: _, ...older } = created;
    writeFileSync(path, JSON.stringify(older));
    assert.match(await summarize(runDir, { now: at + 3001 }), /^UNKNOWN /u);
  });
});

test("a recorded cadence outside the setting's bounds falls back to the worker's own", async () => {
  await repository(async (root) => {
    for (const recorded of [5, 10 ** 12]) {
      const runDir = await createRun({ root, stages: [stage("clean")] }, env);
      const path = join(runDir, "results.json");
      writeFileSync(path, JSON.stringify({ ...json(path), heartbeat: { at: new Date().toISOString(), everyMs: recorded } }));
      const result = await executeRun(runDir, { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "2000" });
      assert.equal(result.state, "passed");
      assert.equal(json(path).heartbeat.everyMs, 2000, `recorded ${recorded}`);
    }
  });
});

test("a transient rename refusal is retried as I/O, and a persistent or other one still fails", () => {
  const refusing = (code, successAt = Infinity) => {
    let calls = 0;
    const rename = () => {
      calls += 1;
      if (calls < successAt) throw Object.assign(new Error(`${code} on attempt ${calls}`), { code });
    };
    return { rename, calls: () => calls };
  };
  const transient = refusing("EPERM", 3);
  renameWithRetry("from", "to", { rename: transient.rename, delayMs: 1 });
  assert.equal(transient.calls(), 3);
  const persistent = refusing("EBUSY");
  assert.throws(() => renameWithRetry("from", "to", { rename: persistent.rename, attempts: 4, delayMs: 1 }), /EBUSY on attempt 4/u);
  assert.equal(persistent.calls(), 4);
  const missing = refusing("ENOENT");
  assert.throws(() => renameWithRetry("from", "to", { rename: missing.rename, delayMs: 1 }), /ENOENT on attempt 1/u);
  assert.equal(missing.calls(), 1);
});

test("a killed launcher's run reads as interrupted once its heartbeat stops", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
    }
    const everyMs = 1000;
    // The broker tells this test when the stage is parked, releases it on
    // request, and reports when the stage process has gone.
    const { address, next: fromBroker, release, close } = parkingBroker(t, root);
    // The stage parks only after the heartbeat has advanced during the stage,
    // so the launcher is killed mid-stage with its timer demonstrably beating.
    const source = `
      const fs = require('node:fs'), path = require('node:path');
      const runs = path.join('.git', 'review-runs');
      const file = path.join(runs, fs.readdirSync(runs)[0], 'results.json');
      const beat = () => { try { return JSON.parse(fs.readFileSync(file, 'utf8')).heartbeat.at; } catch { return undefined; } };
      const deadline = Date.now() + 20000;
      let first;
      const wait = () => {
        const now = beat();
        first ??= now;
        if (first && now && now !== first) {
          const socket = require('node:net').connect(${JSON.stringify(address)});
          socket.on('data', () => process.exit(0));
          socket.on('close', () => process.exit(0));
          socket.on('error', () => process.exit(1));
        } else if (Date.now() > deadline) process.exit(3);
        else setTimeout(wait, 10);
      };
      wait();`;
    let child, completion, gone;
    try {
      assert.equal((await fromBroker())[0], "listening");
      child = spawn(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused", "--", process.execPath, "-e", source],
        { cwd: root, env: { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: String(everyMs) }, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
      completion = once(child, "close");
      let output = "";
      child.stdout.on("data", (chunk) => { output += chunk; });
      child.stderr.resume();
      const parked = await Promise.race([fromBroker(),
        completion.then(([code]) => [`launcher ended before the stage parked (${code})`])]);
      assert.equal(parked[0], "connected");
      // The only later broker message is the stage's exit; listen before
      // anything can trigger it.
      gone = fromBroker();
      child.kill("SIGKILL");
      await completion;
      const runDir = /^STARTED (.+)$/mu.exec(output)?.[1];
      assert.ok(runDir, output);
      const killed = json(join(runDir, "results.json"));
      assert.equal(killed.state, "running", "a killed launcher writes no terminal result");
      assert.equal(killed.stages[0].state, "running");
      assert.equal(killed.heartbeat.everyMs, everyMs);
      const at = Date.parse(killed.heartbeat.at);
      assert.match(await summarize(runDir, { now: at + 3 * everyMs }), /^RUNNING /u);
      assert.match(await summarize(runDir, { now: at + 3 * everyMs + 1 }), /^INTERRUPTED .* exit=unknown$/mu);
      // With the default clock, once the heartbeat has had time to go stale.
      await new Promise((done) => { setTimeout(done, 4 * everyMs); });
      assert.match(await summarize(runDir), /^INTERRUPTED /u);
      assert.equal(json(join(runDir, "results.json")).heartbeat.at, killed.heartbeat.at, "nothing beats after the kill");
      // The recorded pid now naming a live, unrelated process changes nothing.
      writeFileSync(join(runDir, "results.json"), JSON.stringify({ ...killed, pid: process.pid }));
      assert.match(await summarize(runDir), /^INTERRUPTED /u);
    } finally {
      if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      if (completion) await completion;
      // Release the parked stage and wait for it to go, without letting a
      // cleanup failure replace the test's own error.
      if (gone) release();
      if (gone) await gone.catch(() => {});
      await close();
    }
  });
});

test("admitted display metadata survives rewritten requests and an unpublished notification retry", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "execution-count");
    const runDir = await createRun({ root, notifyTo: "fixture-parent", item: "original item",
      nextTransition: { owner: "fixture-next", action: "Original pending action." },
      stages: [stage("rewrite", `const fs = require('node:fs'), path = require('node:path');
        const runs = path.join('.git', 'review-runs');
        for (const name of fs.readdirSync(runs)) {
          const file = path.join(runs, name, 'request.json');
          const request = JSON.parse(fs.readFileSync(file, 'utf8'));
          Object.assign(request, { item: 'rewritten item', nextTransition: { owner: 'wrong-owner', action: 'Wrong action.' },
            expectedFingerprint: 'changed', started: 'invalid' });
          fs.writeFileSync(file, JSON.stringify(request));
        }
        fs.appendFileSync(${JSON.stringify(counter)}, 'ran');`)] }, env);
    const snapshot = readFileSync(join(runDir, "completion.request.json"));
    await executeRun(runDir, env);
    const summary = await summarize(runDir);
    assert.match(summary, /^PASS /u);
    assert.match(summary, /ITEM "original item"\nNEXT OWNER "fixture-next"\nNEXT ACTION "Original pending action\."/u);
    assert.doesNotMatch(summary, /rewritten item|Wrong action|wrong-owner/u);
    await assert.rejects(notifyRun(runDir, { ...env, TERMAL_CLI: "missing" }), /TERMAL_CLI/u);
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
    writeFileSync(join(runDir, "request.json"), "{");
    assert.equal(await summarize(runDir), summary);
    assert.equal((await recoverRun(runDir)).settled, false);
    const recovered = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(recovered.status, 0, recovered.stderr);
    assert.equal(recovered.stdout, `${summary}Already terminal; tests not rerun.\n`);
    const sent = [];
    await notifyRun(runDir, env, async (_command, args) => {
      sent.push(args[args.indexOf("--to") + 1]);
      assert.equal(readFileSync(args[args.indexOf("--message-file") + 1], "utf8"), summary);
      return { code: 0 };
    });
    assert.deepEqual(sent, ["fixture-parent", "fixture-next"]);
    assert.deepEqual(readFileSync(join(runDir, "completion.request.json")), snapshot);
    assert.equal(readFileSync(counter, "utf8"), "ran");
  });
});

test("stale-heartbeat sign-off names the running stage and distinguishes inferred interruption", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("active")], nextTransition: { owner: "fixture-next", action: "Recover the result." } }, env);
    const result = json(join(runDir, "results.json"));
    const at = Date.parse(result.heartbeat.at);
    result.stages[0].state = "running";
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const summary = await summarize(runDir, { now: at + 40_000 });
    assert.match(summary, /^INTERRUPTED .*exit=unknown/u);
    assert.match(summary, /^reason: heartbeat-stale$/mu);
    assert.match(summary, /^BLOCKER \{"reason":"interrupted","stage":"active"\}$/mu);
    assert.equal(completionMetadata(json(join(runDir, "completion.request.json")), { ...result,
      state: "failed", exitCode: 1, ended: new Date(at + 40_000).toISOString(), interrupted: true }).reason, "interrupted");
  });
});

test("CLI recover distinguishes lock, mismatched result and unpublished executor without writes", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    const original = json(join(runDir, "results.json"));
    // A live pid with an impossible old creation time is deterministically a
    // later process. Never depend on a terminated pid remaining unallocated.
    const created = { win32: "win32:1", linux: "linux:1", darwin: "ps:fixture-before-boot" }[process.platform];
    const executor = { pid: process.pid, created: created ?? null, host: processHost() };
    for (const [reason, result, lock] of [
      ["results-mismatch", { ...original, runId: "different-run" }, false],
      ["executor-unpublished", original, false],
      ["executor-unidentifiable", { ...original, executor: { pid: 0 } }, false],
      [created ? "recovery-lock-held" : "executor-unknown", { ...original, executor }, true],
    ]) {
      writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      if (lock) writeFileSync(join(runDir, "recovery.lock"), "");
      const before = directoryBytes(runDir);
      const refused = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(refused.status, 1);
      assert.ok(refused.stderr.includes(`REFUSED recover; ${reason}; nothing changed`), refused.stderr);
      assert.ok(refused.stderr.includes(`results: ${join(runDir, "results.json")}`));
      if (reason === "recovery-lock-held") assert.ok(refused.stderr.includes(`lock: ${join(runDir, "recovery.lock")}`));
      assert.deepEqual(directoryBytes(runDir), before);
    }
  });
});

test("launch snapshots do not require hard links, including a notification route", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    const local = join(root, "scripts", "test-launcher.mjs");
    const source = readFileSync(launcher, "utf8").replace("linkSync(temporary, path);",
      'throw Object.assign(new Error("fixture: hard links unavailable"), { code: "ENOTSUP" });');
    assert.notEqual(source, readFileSync(launcher, "utf8"));
    writeFileSync(local, source);
    for (const name of launcherSupportFiles) copyFileSync(join(dirname(launcher), name), join(root, "scripts", name));
    git(["add", "scripts"], root);
    for (const notify of [false, true]) {
      const result = spawnSync(process.execPath, [local, "focused", ...(notify ? ["--notify", "fixture-parent"] : []),
        "--item", "portable launch", "--next-owner", "fixture-next", "--next-action", "Review the result.",
        "--", process.execPath, "-e", "console.log('stage executed')"], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(result.status, notify ? 1 : 0, result.stderr);
      assert.match(result.stdout, /^PASS test-/mu);
      const resultPath = /^results: (.+)$/mu.exec(result.stdout)?.[1];
      assert.ok(resultPath, result.stdout);
      const runDir = dirname(resultPath);
      assert.equal(json(resultPath).state, "passed");
      assert.equal(json(join(runDir, "completion.request.json")).item, "portable launch");
      assert.deepEqual(json(join(runDir, "completion.request.json")).nextTransition,
        { owner: "fixture-next", action: "Review the result." });
      assert.match(readFileSync(join(runDir, "focused.log"), "utf8"), /stage executed/u);
      if (notify) {
        assert.equal(json(join(runDir, "notification.target.json")).nextOwner, "fixture-next");
        assert.match(result.stderr, /notification failed; tests were NOT rerun/u);
        assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
      }
    }
  });
});

test("launch snapshot publication failure saves terminal failure and a public CLI locator", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    const local = join(root, "scripts", "test-launcher.mjs");
    const source = readFileSync(launcher, "utf8").replace('save(join(runDir, "completion.request.json"), {',
      'throw new Error("fixture private publication diagnostic"); save(join(runDir, "completion.request.json"), {');
    assert.notEqual(source, readFileSync(launcher, "utf8"));
    writeFileSync(local, source);
    for (const name of launcherSupportFiles) copyFileSync(join(dirname(launcher), name), join(root, "scripts", name));
    git(["add", "scripts"], root);
    const result = spawnSync(process.execPath, [local, "focused", "--", process.execPath, "-e", ""],
      { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 1);
    assert.doesNotMatch(result.stdout, /STARTED/u);
    assert.match(result.stderr, /FAIL launcher: request-publication-failed/u);
    assert.doesNotMatch(result.stderr, /private publication diagnostic|request-or-runtime-failed/u);
    const resultPath = /^results: (.+)$/mu.exec(result.stderr)?.[1];
    assert.ok(resultPath, result.stderr);
    assert.equal(json(resultPath).state, "failed");
    assert.equal(json(resultPath).reason, "request-publication-failed");
    assert.equal(json(resultPath).stages[0].state, "unrun");
    assert.equal(existsSync(join(dirname(resultPath), "focused.log")), false);
  });
});

test("CLI heartbeat bounds name the setting before creating a run", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(join(dirname(launcher), name), join(root, "scripts", name));
    }
    const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused", "--", process.execPath, "-e", ""],
      { cwd: root, env: { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "5" }, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /ENGRAM_LAUNCHER_HEARTBEAT_MS must be an integer from 1000 to 600000/u);
    assert.doesNotMatch(result.stderr, /request-or-runtime-failed/u);
    assert.equal(result.stdout, "");
    assert.equal(existsSync(join(root, ".git", "review-runs")), false);
  });
});

test("CLI summary, inspection and recovery name unreadable result artifacts without mutation", async () => {
  await repository(async (root) => {
    for (const corrupt of [false, true]) {
      const runDir = join(root, ".git", "review-runs", `test-${randomUUID()}`);
      if (corrupt) {
        mkdirSync(runDir, { recursive: true });
        writeFileSync(join(runDir, "results.json"), "{ private corrupt diagnostic");
      }
      for (const args of [["summary", runDir], ["summary", runDir, "--stage", "unused"], ["recover", runDir]]) {
        const result = spawnSync(process.execPath, [launcher, ...args], { cwd: root, env, encoding: "utf8", windowsHide: true });
        assert.equal(result.status, 1);
        assert.match(result.stderr, /results-unreadable/u);
        assert.ok(result.stderr.includes(`results: ${join(runDir, "results.json")}`), result.stderr);
        assert.doesNotMatch(result.stderr, /ENOENT|SyntaxError|private corrupt diagnostic|request-or-runtime-failed/u);
        assert.equal(result.stdout, "");
        assert.equal(existsSync(runDir), corrupt);
        if (corrupt) {
          assert.equal(readFileSync(join(runDir, "results.json"), "utf8"), "{ private corrupt diagnostic");
          assert.deepEqual(readdirSync(runDir), ["results.json"]);
        }
      }
    }
  });
});

test("CLI safe failures retain startup locators, notification retry and inspection guidance", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    const local = join(root, "scripts", "test-launcher.mjs");
    // Trigger a real child admission failure through the public detached CLI.
    // The only fixture injection corrupts results just before startDetached.
    const source = readFileSync(launcher, "utf8").replace("await startDetached(runDir);",
      'writeFileSync(join(runDir, "results.json"), "{"); await startDetached(runDir);');
    writeFileSync(local, source);
    for (const name of launcherSupportFiles) copyFileSync(join(dirname(launcher), name), join(root, "scripts", name));
    git(["add", "scripts"], root);
    const failed = spawnSync(process.execPath, [local, "focused", "--detach", "--notify", "fixture-parent",
      "--", process.execPath, "-e", ""], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(failed.status, 1);
    assert.doesNotMatch(failed.stdout, /STARTED/u);
    assert.match(failed.stderr, /FAIL launcher: worker-startup-failed/u);
    const resultPath = /^results: (.+)$/mu.exec(failed.stderr)?.[1];
    assert.ok(resultPath, failed.stderr);
    const runDir = dirname(resultPath);
    assert.ok(failed.stderr.includes(`launcher.log: ${join(runDir, "launcher.log")}`));
    assert.equal(json(resultPath).state, "failed");
    const retry = spawnSync(process.execPath, [launcher, "notify", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(retry.status, 1);
    assert.match(retry.stderr, /notification failed; tests were NOT rerun/u);
    assert.ok(retry.stderr.includes(`retry: notify ${JSON.stringify(runDir)}`));
    for (const [args, reason] of [
      [["--stage", "missing"], "inspection selector is unavailable"],
      [["--stage", "focused"], "inspection requires a failed stage or preflight probe"],
      [["--max-chars", "100"], "explicit inspection requires --stage NAME"],
      [["--stage", "unused", "--max-chars", "6001"], "--max-chars requires an integer from 1 to 6000"],
      [["--stage", "unused", "--stage", "unused"], "duplicate inspection option"],
      [["--invalid"], "invalid inspection option"],
    ]) {
      const inspected = spawnSync(process.execPath, [launcher, "summary", runDir, ...args], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(inspected.status, 1);
      assert.ok(inspected.stderr.includes(reason), inspected.stderr);
      assert.doesNotMatch(inspected.stderr, /request-or-runtime-failed|SyntaxError|stack/u);
    }
    const noResult = await createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
    const early = spawnSync(process.execPath, [launcher, "notify", noResult], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(early.status, 1);
    assert.match(early.stderr, /cannot notify before terminal results are saved/u);
  });
});

}

export function registerRecoveryProcesses(support) {
  const { env, launcher, stage, json, logPath, repository, directoryBytes, rewriteResults, parkingBroker } = support;

// The host the recover unit tests recover from: the Windows system their
// records describe, whatever system runs them.
const fixtureHost = { platform: "win32", hostname: "fixture-host", pidNamespace: null };
// Whether this system's identity query answers for a living process.
const identifies = ["win32", "linux", "darwin"].includes(process.platform);

test("the system tells a living process by its creation time and a finished one from it", async () => {
  if (!identifies) {
    // Where start times move with the clock, a living process is unknown.
    assert.equal(processIdentity(process.pid).state, "unknown");
    return;
  }
  const self = processIdentity(process.pid);
  assert.equal(self.state, "alive", JSON.stringify(self));
  assert.equal(wellFormedCreated(self.created), true, self.created);
  assert.deepEqual(processIdentity(process.pid), self, "the creation time is stable");
  const child = spawn(process.execPath, ["-e", "process.stdin.resume()"], { windowsHide: true, stdio: ["pipe", "ignore", "ignore"] });
  const exited = once(child, "exit");
  let living;
  try {
    await once(child, "spawn");
    living = processIdentity(child.pid);
    assert.equal(living.state, "alive", JSON.stringify(living));
  } finally {
    // The child ends however the assertions went.
    child.stdin.end();
    await exited;
  }
  // Once it has ended, its id names no process, or, if the system has already
  // given the id to another, a later one.
  const after = processIdentity(child.pid);
  assert.ok(after.state === "gone" || (after.state === "alive" && !sameCreation(living.created, after)), JSON.stringify(after));
  for (const pid of [0, -1, 1.5, Number.NaN]) assert.equal(processIdentity(pid).state, "unknown", String(pid));
});

test("only the system's own no-such-process answer reads as gone", () => {
  const probes = (overrides) => ({ ...systemProbes, ...overrides });
  const ask = (platform, overrides) => processIdentity(4242, { platform, probes: probes(overrides) });
  const run = (status, stdout, stderr = "", extra = {}) => () => ({ status, stdout, stderr, signal: null, ...extra });
  // Windows: the system PowerShell's answer, and every failure is unknown.
  assert.deepEqual(ask("win32", { powershell: run(0, "alive 133\r\n") }), { state: "alive", created: "win32:133" });
  assert.deepEqual(ask("win32", { powershell: run(0, "gone\r\n") }), { state: "gone" });
  // A process the query may not open is asked of WMI, to the microsecond.
  assert.deepEqual(ask("win32", { powershell: run(0, "coarse 20260929182308.939671-420\r\n") }),
    { state: "alive", created: "win32:134352049889396710", coarse: true });
  assert.equal(ask("win32", { powershell: run(0, "coarse 20261399000000.000000+000\r\n") }).state, "unknown");
  for (const failure of [run(1, ""), run(0, "unknown Win32Exception"), run(0, "garbage"), run(null, "", "", { signal: "SIGTERM" }),
    () => ({ error: new Error("SystemRoot names no Windows directory") })]) {
    assert.equal(ask("win32", { powershell: failure }).state, "unknown");
  }
  // Linux: absence only from the signal-0 answer; an unreadable status of an
  // existing process is unknown, a zombie is gone.
  const stat = (state, start) => () => `4242 (node (x)) ${state} 1 ${Array.from({ length: 17 }, () => "0").join(" ")} ${start} 0`;
  assert.deepEqual(ask("linux", { exists: () => "absent" }), { state: "gone" });
  assert.deepEqual(ask("linux", { exists: () => "exists", procStat: stat("S", "777") }), { state: "alive", created: "linux:777" });
  assert.deepEqual(ask("linux", { exists: () => "exists", procStat: stat("Z", "777") }), { state: "gone" });
  const hidden = ask("linux", { exists: () => "exists", procStat: () => { throw Object.assign(new Error("hidden"), { code: "ENOENT" }); } });
  assert.equal(hidden.state, "unknown", "a hidden /proc says nothing of an end");
  assert.equal(ask("linux", { exists: () => "EINVAL" }).state, "unknown");
  // Elsewhere: ps, whose failure is unknown whatever it printed.
  assert.deepEqual(ask("darwin", { exists: () => "exists", ps: run(0, "Wed  Sep 30 01:02:03 2026\n") }),
    { state: "alive", created: "ps:Wed Sep 30 01:02:03 2026" });
  assert.equal(ask("darwin", { exists: () => "exists", ps: run(1, "", "ps: illegal option -- o") }).state, "unknown");
  assert.equal(ask("darwin", { exists: () => "exists", ps: run(1, "") }).state, "unknown");
  assert.deepEqual(ask("darwin", { exists: () => "absent" }), { state: "gone" });
  // Where a start time moves with the clock, an existing process is unknown
  // and only absence settles.
  assert.equal(ask("freebsd", { exists: () => "exists", ps: () => assert.fail("not asked") }).state, "unknown");
  assert.deepEqual(ask("freebsd", { exists: () => "absent" }), { state: "gone" });
  // The creation-time forms that identify a process, and some that do not.
  for (const created of ["win32:133", "linux:777", "ps:Wed Sep 30 01:02:03 2026"]) assert.equal(wellFormedCreated(created), true, created);
  for (const created of ["win32:", "win32:garbage", "linux:1.5", "ps:", "ps:two  spaces", "other:1", "", 7, null,
    "toString:1", "constructor:1", "__proto__:1", "hasOwnProperty:1"]) {
    assert.equal(wellFormedCreated(created), false, String(created));
  }
});

test("WMI's local creation time is read with the offset it carries", () => {
  // Measured on one host: WMI's CreationDate and .NET's exact StartTime for
  // one process, 2026-09-30 01:23:08.9396716 UTC.
  assert.equal(wmiFileTime("20260929182308.939671-420"), 134352049889396710n);
  assert.equal(sameCreation("win32:134352049889396716", { state: "alive", created: "win32:134352049889396710", coarse: true }), true);
  // The same instant written with another offset is the same time.
  assert.equal(wmiFileTime("20260930012308.939671+000"), 134352049889396710n);
  assert.equal(wmiFileTime("20260930032308.939671+120"), 134352049889396710n);
  for (const text of ["", "20260929182308.939671", "20260929182308-420", "20261329182308.939671-420",
    "20260231000000.000000+000", "20260929182308.93967-420", "2026092918230x.939671-420"]) {
    assert.equal(wmiFileTime(text), null, text);
  }
});

test("a coarse creation time is the exact one cut to the microsecond", () => {
  const coarse = (created) => ({ state: "alive", created, coarse: true });
  assert.equal(sameCreation("win32:1345", { state: "alive", created: "win32:1345" }), true);
  assert.equal(sameCreation("win32:1345", { state: "alive", created: "win32:1340" }), false, "an exact answer must match exactly");
  for (const exact of ["win32:1340", "win32:1345", "win32:1349"]) assert.equal(sameCreation(exact, coarse("win32:1340")), true, exact);
  for (const exact of ["win32:1339", "win32:1350"]) assert.equal(sameCreation(exact, coarse("win32:1340")), false, exact);
  // Beyond a double's precision, as FILETIMEs are.
  assert.equal(sameCreation("win32:134352045952779645", coarse("win32:134352045952779640")), true);
  assert.equal(sameCreation("win32:134352045952779655", coarse("win32:134352045952779640")), false);
  assert.equal(sameCreation("linux:5", coarse("linux:5")), true, "an identical value is the same whatever its precision");
  assert.equal(sameCreation("linux:6", coarse("linux:5")), false);
});

test("a process the query may not open is still identified", { skip: process.platform !== "win32" }, () => {
  // The system process: an ordinary user may not open it for query, and an
  // administrator may; either way it is alive with a creation time.
  const system = processIdentity(4);
  assert.equal(system.state, "alive", JSON.stringify(system));
  assert.equal(wellFormedCreated(system.created), true, system.created);
});

test("the executor publishes its process id and creation time before it reports ready", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("clean")] }, env);
    let published;
    const result = await executeRun(runDir, env, () => { published = json(join(runDir, "results.json")).executor; });
    assert.equal(result.state, "passed");
    const self = processIdentity(process.pid);
    assert.equal(published.pid, process.pid);
    assert.deepEqual(published.host, processHost());
    if (self.state === "alive" && !self.coarse) assert.equal(published.created, self.created);
    else assert.deepEqual([published.created, typeof published.unidentified], [null, "string"]);
  });
});

test("recover refuses, changing no file, a run whose executor is alive, unknown, unpublished or locked", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("first"), stage("second")] }, env);
    const refuses = async (identify, pattern, label) => {
      const before = directoryBytes(runDir);
      await assert.rejects(recoverRun(runDir, { host: fixtureHost, identify }), (error) => error.refused === true && pattern.test(error.message), label);
      assert.deepEqual(directoryBytes(runDir), before, `${label}: no file changed`);
    };
    const never = () => assert.fail("no executor to ask about");
    await refuses(never, /has not published its executor/u, "unpublished");
    const hourAgo = new Date(Date.now() - 3_600_000).toISOString();
    rewriteResults(runDir, (result) => ({ ...result, heartbeat: { at: hourAgo, everyMs: 1000 },
      pid: 4242, executor: { pid: 4242, created: "win32:100", host: fixtureHost },
      stages: [{ name: "first", state: "running" }, { name: "second", state: "unrun" }] }));
    assert.match(await summarize(runDir), /^INTERRUPTED /u, "the heartbeat alone reads as interrupted");
    await refuses(() => ({ state: "alive", created: "win32:100" }), /executor 4242 is alive/u, "alive with a stale heartbeat");
    await refuses(() => ({ state: "unknown", why: "access is denied" }), /cannot tell .*access is denied/u, "unknown");
    await refuses(() => ({ state: "alive", created: "linux:5" }), /cannot be compared/u, "another clock");
    await refuses(() => ({ state: "alive", created: "win32:garbage" }), /cannot be compared/u, "a malformed answer");
    await refuses(() => ({ state: "alive", created: "win32:100", coarse: true }), /executor 4242 is alive/u, "alive, coarsely");
    // An executor that ran on another system, or recorded a creation time this
    // system does not give, is not asked about here.
    for (const [other, label] of [
      [{ ...fixtureHost, platform: "linux" }, "another platform"],
      [{ ...fixtureHost, hostname: "container" }, "another host"],
      [{ ...fixtureHost, pidNamespace: "pid:[4026531836]" }, "another pid namespace"],
    ]) {
      rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: "win32:100", host: other } }));
      await refuses(never, /ran as .*so its id cannot be asked here/u, label);
    }
    rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: "linux:100", host: fixtureHost } }));
    await refuses(never, /not one win32 gives, so it ran elsewhere/u, "another platform's creation time");
    rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: "win32:100", host: { platform: "win32" } } }));
    await refuses(never, /malformed/u, "a malformed host");
    rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: "win32:100" } }));
    await refuses(never, /malformed/u, "an executor record without its host");
    rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: "win32:100", host: fixtureHost } }));
    await refuses(() => ({ state: "alive", created: "win32:100" }), /executor 4242 is alive/u, "the same host");
    // A run directory whose results name another run is not recovered.
    const { runId } = json(join(runDir, "results.json"));
    rewriteResults(runDir, (result) => ({ ...result, runId: "test-another" }));
    await refuses(never, /does not describe run/u, "another run's results");
    rewriteResults(runDir, (result) => ({ ...result, runId }));
    // A recovery that holds the lock, or a killed one that left it, makes the
    // next refuse and name the file.
    writeFileSync(join(runDir, "recovery.lock"), "");
    await refuses(() => ({ state: "gone" }), /another recovery .*recovery\.lock/u, "locked");
    unlinkSync(join(runDir, "recovery.lock"));
    for (const executor of [null, { pid: "4242", created: "win32:100" }, { pid: 4242, created: 7 }, { pid: 4242 },
      { pid: 4242, created: "win32:" }, { pid: 4242, created: "win32:garbage" }, { pid: 4242, created: "toString:1" },
      { pid: 4242, created: "__proto__:1" }]) {
      rewriteResults(runDir, (result) => ({ ...result, executor }));
      await refuses(never, /malformed/u, `malformed ${JSON.stringify(executor)}`);
    }
    // An executor whose own query failed is recorded without a creation
    // time, as a run from before creation times were recorded names its id
    // alone: both are recovered only when no process has that id.
    rewriteResults(runDir, (result) => ({ ...result, executor: { pid: 4242, created: null, host: fixtureHost, unidentified: "access is denied" } }));
    await refuses(() => ({ state: "alive", created: "win32:1" }), /recorded no creation time/u, "unidentified executor alive");
    rewriteResults(runDir, (result) => { const { executor: _, ...older } = result; return older; });
    await refuses(() => ({ state: "alive", created: "win32:1" }), /recorded no creation time/u, "historical id alive");
    const { settled, result } = await recoverRun(runDir, { host: fixtureHost, identify: () => ({ state: "gone" }) });
    assert.equal(settled, true);
    assert.equal(result.interrupted, true);
    assert.deepEqual(result.recovered.executor, { pid: 4242, created: null });
    assert.equal(existsSync(join(runDir, "recovery.lock")), false, "the lock is released");
  });
});

test("recover writes the interrupted result TermAl's launcher writes and leaves a terminal run as it is", async () => {
  await repository(async (root) => {
    const recover = async (stages, identify = () => ({ state: "gone" })) => {
      const runDir = await createRun({ root, stages: stages.map(({ name }) => stage(name)) }, env);
      rewriteResults(runDir, (result) => ({ ...result, pid: 4242, executor: { pid: 4242, created: "win32:100", host: fixtureHost }, stages }));
      const outcome = await recoverRun(runDir, { host: fixtureHost, identify, now: () => new Date("2026-09-30T01:00:00.000Z") });
      return { runDir, ...outcome };
    };
    const passed = { name: "first", state: "passed", code: 0, signal: null, started: "s", ended: "e", log: "first.log",
      tests: { executed: 3, passed: 3, failed: 0, ignored: 0, runners: 1, filteredOut: 0, failures: 0 } };
    const running = { name: "second", state: "running", started: "s2", command: ["node"], log: "second.log" };
    const { runDir, settled, result } = await recover([passed, running, { name: "third", state: "unrun" }]);
    assert.equal(settled, true);
    // The run, as TermAl's `recover` writes it and its reader reads it.
    assert.equal(result.state, "failed");
    assert.equal(result.exitCode, 1);
    assert.equal(result.interrupted, true);
    assert.equal(result.ended, "2026-09-30T01:00:00.000Z");
    assert.equal(result.error, "interrupted: executor 4242 ended without saving a terminal result (no process has its id); no stage was rerun");
    assert.deepEqual(result.stages[0], passed, "a finished stage is kept as it was");
    assert.deepEqual(result.stages[1], { ...running, state: "failed", outcome: "unknown",
      error: "interrupted: executor 4242 ended without saving a terminal result (no process has its id); this stage's outcome is unknown" });
    assert.deepEqual(result.stages[2], { name: "third", state: "unrun" });
    for (const invented of ["code", "signal", "tests", "ended"]) assert.equal(Object.hasOwn(result.stages[1], invented), false, invented);
    assert.deepEqual(result.recovered, { at: "2026-09-30T01:00:00.000Z", phase: "stage", stage: "second",
      executor: { pid: 4242, created: "win32:100", host: fixtureHost }, found: "no process has its id", lastHeartbeat: result.heartbeat.at });
    assert.deepEqual(json(join(runDir, "results.json")), result);
    const summary = await summarize(runDir);
    assert.match(summary, /^INTERRUPTED test-\S+ exit=1$/mu);
    assert.match(summary, /elapsed: ms=unknown basis=recovery-end/u);
    assert.match(summary, /^recovery: phase=stage stage="second"$/mu);
    assert.match(summary, /unknown=1/u);
    assert.doesNotMatch(summary, /^(?:PASS|FAIL)\b|test-launcher\/v1/mu);

    // Recovering again, like recovering any terminal run, changes nothing.
    const before = directoryBytes(runDir);
    assert.equal((await recoverRun(runDir, { host: fixtureHost, identify: () => assert.fail("not asked") })).settled, false);
    assert.deepEqual(directoryBytes(runDir), before);

    // An id a system service took is told apart by its coarse creation time.
    const taken = await recover([running], () => ({ state: "alive", created: "win32:990", coarse: true }));
    assert.equal(taken.result.recovered.found, "its id now names a later process");

    // Where it stopped when no stage was running, and a reused id.
    const reused = await recover([{ name: "first", state: "unrun" }], () => ({ state: "alive", created: "win32:999" }));
    assert.deepEqual([reused.result.recovered.phase, reused.result.recovered.stage, reused.result.recovered.found],
      ["startup", null, "its id now names a later process"]);
    assert.match(await summarize(reused.runDir), /^INTERRUPTED /mu);
    assert.match(await summarize(reused.runDir), /^recovery: phase=startup stage=null$/mu);
    const between = await recover([passed, { name: "second", state: "unrun" }]);
    assert.deepEqual([between.result.recovered.phase, between.result.recovered.stage], ["between-stages", "first"]);
    assert.match(await summarize(between.runDir), /^INTERRUPTED /mu);
    assert.match(await summarize(between.runDir), /^recovery: phase=between-stages stage="first"$/mu);
    const finishing = await recover([passed]);
    assert.deepEqual([finishing.result.recovered.phase, finishing.result.recovered.stage], ["finishing", null]);
    assert.match(await summarize(finishing.runDir), /^recovery: phase=finishing stage=null$/mu);

    for (const state of ["passed", "failed"]) {
      const terminal = await createRun({ root, stages: [stage("only")] }, env);
      rewriteResults(terminal, (result) => ({ ...result, state, exitCode: state === "passed" ? 0 : 1, ended: "e" }));
      const bytes = directoryBytes(terminal);
      assert.equal((await recoverRun(terminal, { host: fixtureHost, identify: () => assert.fail("not asked") })).settled, false);
      assert.deepEqual(directoryBytes(terminal), bytes, state);
    }
  });
});

test("a launcher that finishes while recover asks about it keeps its own result", async () => {
  await repository(async (root) => {
    const running = { name: "only", state: "running", started: "s", command: ["node"], log: "only.log" };
    const runDir = await createRun({ root, stages: [stage("only")] }, env);
    rewriteResults(runDir, (result) => ({ ...result, pid: 4242, executor: { pid: 4242, created: "win32:100", host: fixtureHost }, stages: [running] }));
    // The executor saves its terminal result and ends between recover's
    // first reading and the system's answer that it is gone.
    const outcome = await recoverRun(runDir, { host: fixtureHost, identify: () => {
      rewriteResults(runDir, (result) => ({ ...result, state: "passed", exitCode: 0, ended: "e",
        stages: [{ ...running, state: "passed", code: 0 }] }));
      return { state: "gone" };
    } });
    assert.equal(outcome.settled, false);
    const kept = json(join(runDir, "results.json"));
    assert.deepEqual([kept.state, kept.exitCode, Object.hasOwn(kept, "interrupted")], ["passed", 0, false]);
    assert.equal(existsSync(join(runDir, "recovery.lock")), false);
  });
});

test("a recovery whose write fails releases the lock and leaves the run as it was", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("only")] }, env);
    rewriteResults(runDir, (result) => ({ ...result, pid: 4242, executor: { pid: 4242, created: "win32:100", host: fixtureHost },
      stages: [{ name: "only", state: "running" }] }));
    const before = directoryBytes(runDir);
    await assert.rejects(recoverRun(runDir, { host: fixtureHost, identify: () => ({ state: "gone" }), write: () => { throw new Error("disk full"); } }),
      /disk full/u);
    assert.deepEqual(directoryBytes(runDir), before, "no lock or result is left behind");
    assert.equal((await recoverRun(runDir, { host: fixtureHost, identify: () => ({ state: "gone" }) })).settled, true, "a later recovery can settle it");
  });
});

test("a second recovery started while the first holds the lock refuses", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("only")] }, env);
    rewriteResults(runDir, (result) => ({ ...result, pid: 4242, executor: { pid: 4242, created: "win32:100", host: fixtureHost },
      stages: [{ name: "only", state: "running" }] }));
    // The second recovery starts while the first holds the lock, just before
    // the first writes its result.
    let second;
    const first = await recoverRun(runDir, { host: fixtureHost, identify: () => ({ state: "gone" }), now: () => {
      assert.equal(existsSync(join(runDir, "recovery.lock")), true, "the first holds the lock while it writes");
      second = recoverRun(runDir, { host: fixtureHost, identify: () => ({ state: "gone" }) });
      return new Date("2026-09-30T01:00:00.000Z");
    } });
    assert.equal(first.settled, true);
    await assert.rejects(second, (error) => error.refused === true && /another recovery .*recovery\.lock/u.test(error.message));
    assert.equal(json(join(runDir, "results.json")).recovered.at, "2026-09-30T01:00:00.000Z", "the first recovery's result stands");
    assert.equal(existsSync(join(runDir, "recovery.lock")), false);
  });
});

// A stage that parks until the broker releases it, and a broker that says
// when the stage has connected and when it has gone.
test("a launcher stopped during a Rust scale phase preserves the prior result without rerunning phases", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let child, completion;
    try {
      assert.equal((await broker.next())[0], "listening");
      const stages = requiredStages().slice(3, 7).map((entry, index) => ({ ...entry, command: process.execPath,
        args: ["-e", index === 1 ? broker.parked : "console.log('running 1 test'); console.log('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s')"] }));
      const runDir = await createRun({ root, full: true, stages }, env);
      child = spawn(process.execPath, [launcher, "_run", runDir], { cwd: root, env, windowsHide: true, stdio: ["ignore", "ignore", "ignore"] });
      completion = once(child, "close");
      const parked = await Promise.race([broker.next(), completion.then(([code]) => [`launcher ended (${code})`])]);
      assert.equal(parked[0], "connected");
      const before = json(join(runDir, "results.json"));
      assert.deepEqual(before.stages.map(({ state }) => state), ["passed", "running", "unrun", "unrun"]);
      assert.equal(before.stages[0].tests.passed, 1);
      const firstLog = readFileSync(logPath(runDir, before.stages[0]));
      child.kill("SIGKILL");
      await completion;

      const recovered = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(recovered.status, 0, recovered.stderr);
      assert.match(recovered.stdout, /^INTERRUPTED test-\S+ exit=1$/mu);
      assert.match(recovered.stdout, /^recovery: phase=stage stage="rust-scale-claims"$/mu);
      assert.match(recovered.stdout, /^Settled as interrupted; tests not rerun\.$/mu);
      assert.doesNotMatch(recovered.stdout, /^(?:PASS|FAIL)\b|test-launcher\/v1/mu);
      const after = json(join(runDir, "results.json"));
      assert.deepEqual([after.state, after.interrupted, after.exitCode], ["failed", true, 1]);
      assert.match(after.error, /^interrupted: /u);
      assert.deepEqual(after.stages[0], before.stages[0], "the first stage's fields are unchanged");
      assert.deepEqual(readFileSync(logPath(runDir, after.stages[0])), firstLog, "the first stage's log bytes are unchanged");
      assert.equal(after.stages[1].state, "failed");
      assert.equal(after.stages[1].outcome, "unknown");
      assert.deepEqual(after.stages.slice(2), before.stages.slice(2), "later phases remain unrun");
      assert.match(after.stages[1].error, /^interrupted: .*this stage's outcome is unknown$/u);
      // The system may already have given the killed launcher's id to another
      // process; either answer settles the run.
      assert.match(after.recovered.found, /^(?:no process has its id|its id now names a later process)$/u);
      assert.deepEqual(after.recovered.executor, before.executor);
      const again = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(again.status, 0, again.stderr);
      assert.match(again.stdout, /^Already terminal; tests not rerun\.$/mu);
      assert.deepEqual(json(join(runDir, "results.json")), after);
    } finally {
      if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      if (completion) await completion;
      // The parked stage outlived its launcher; release it.
      await broker.close();
    }
  });
});

test("recover refuses a living launcher whose heartbeat has gone stale, changing no file", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let child, completion;
    try {
      assert.equal((await broker.next())[0], "listening");
      const runDir = await createRun({ root, stages: [{ name: "parked", command: process.execPath, args: ["-e", broker.parked] }] },
        { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "600000" });
      child = spawn(process.execPath, [launcher, "_run", runDir], { cwd: root, env, windowsHide: true, stdio: ["ignore", "ignore", "ignore"] });
      completion = once(child, "close");
      const parked = await Promise.race([broker.next(), completion.then(([code]) => [`launcher ended (${code})`])]);
      assert.equal(parked[0], "connected");
      // The launcher lives; only its heartbeat reads an hour old.
      rewriteResults(runDir, (result) => ({ ...result, heartbeat: { ...result.heartbeat, at: new Date(Date.now() - 3_600_000).toISOString() } }));
      assert.match(await summarize(runDir), /^INTERRUPTED /u);
      const before = directoryBytes(runDir);
      const recovered = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(recovered.status, 1);
      // Where start times move with the clock the living executor is unknown,
      // and recover refuses for that.
      assert.match(recovered.stderr, identifies ? /^REFUSED recover; executor-alive; nothing changed$/mu
        : /^REFUSED recover; executor-unknown; nothing changed$/mu);
      assert.ok(recovered.stderr.includes(`results: ${join(runDir, "results.json")}`));
      assert.equal(recovered.stdout, "");
      assert.deepEqual(directoryBytes(runDir), before);
      broker.release();
      const [code] = await completion;
      assert.equal(code, 0);
      assert.equal(json(join(runDir, "results.json")).state, "passed", "the living launcher finished its run");
    } finally {
      if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      if (completion) await completion;
      await broker.close();
    }
  });
});

}
