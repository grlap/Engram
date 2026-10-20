import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { chmodSync, constants, copyFileSync, existsSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, realpathSync, rmdirSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { devNull } from "node:os";
import { once } from "node:events";
import { basename, delimiter, dirname, isAbsolute, join, relative, resolve } from "node:path";
import test, { after, describe } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION, WINDOWS_LIMITATION } from "./review-freeze-fingerprint.mjs";
import { requestStop, runJobCommand, stopController, stopProtocol, withStopLock } from "./launcher-stop.mjs";
import {
  commandKind, completionMetadata, countTests, createRun, diagnostics, executable, executeRun, fileProbes, inspectFailure, machineRecord, notifyRun,
  processHost, processIdentity, recoverRun, renameWithRetry, requiredStages, sameCreation, selectsTests, startDetached, summarize,
  systemProbes, wellFormedCreated, wmiFileTime,
} from "./test-launcher.mjs";
import { fixtureHome, fixtureRoot, removeFixtureHomes, removeFixturePath, tempSnapshot, assertTempClean } from "./test-temp.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));
// This test file runs in its own Node test process. Isolate both explicit Git
// children and in-process fingerprint calls from developer configuration.
for (const key of Object.keys(process.env)) if (key.toUpperCase().startsWith("GIT_")) delete process.env[key];
// Restore the fixture ceiling the scrub removed: Git in a fixture never climbs
// out into this checkout.
Object.assign(process.env, { GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : devNull,
  GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0", GIT_CEILING_DIRECTORIES: fixtureRoot });
const env = { ...process.env, TERMAL_SESSION_ID: "fixture-owner", TERMAL_CLI: process.execPath };
const launcher = fileURLToPath(new URL("./test-launcher.mjs", import.meta.url));
const launcherSupportFiles = ["review-freeze-fingerprint.mjs", "launcher-stop.mjs", "launcher-process-job.ps1", "launcher-process-job.cs"];
const launcherFixtureFiles = ["test-launcher.mjs", ...launcherSupportFiles];
const stage = (name, source = "") => ({ name, command: process.execPath, args: ["-e", source] });
const json = (path) => JSON.parse(readFileSync(path, "utf8"));
const logPath = (runDir, entry) => isAbsolute(entry.log) ? entry.log : join(runDir, entry.log);

function git(args, cwd) {
  // Fixture Git writes must not start maintenance that outlives cleanup.
  return execFileSync("git", ["-c", "maintenance.auto=false", ...args], { cwd, encoding: "utf8" });
}

async function repository(callback, prefix = "engram-launcher-") {
  const root = fixtureHome(prefix);
  try {
    git(["init", "--quiet", "--template="], root);
    assert.equal(git(["config", "--bool", "--get", "maintenance.auto"], root).trim(), "false");
    git(["config", "core.autocrlf", "false"], root);
    writeFileSync(join(root, "tracked.txt"), "baseline\n");
    git(["add", "tracked.txt"], root);
    await callback(root);
  } finally {
    removeFixtureHomes(root);
  }
}

describe("documentation link checker", () => {
  function checkFixture(context, sourceParts) {
    const root = fixtureHome("doc-links-", context);
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("check-doc-links.mjs", import.meta.url)), join(scripts, "check-doc-links.mjs"));
    writeFileSync(join(root, "README.md"), "[Kept](kept.md)\n");
    writeFileSync(join(root, "kept.md"), "# Kept\n");
    const source = join(root, ...sourceParts);
    mkdirSync(dirname(source), { recursive: true });
    writeFileSync(source, "[Dead link](missing.md)\n");
    const result = spawnSync(process.execPath, [join(scripts, "check-doc-links.mjs")], {
      cwd: root, encoding: "utf8", windowsHide: true,
    });
    assert.ifError(result.error);
    return result;
  }

  test("skips the repository's .claude/worktrees subtree", (t) => {
    const result = checkFixture(t, [".claude", "worktrees", "child", "docs", "stale.md"]);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "Documentation links are valid.\n");
    assert.equal(result.stderr, "");
  });

  // Retained archives may contain copied Markdown whose original links no
  // longer resolve. Run the copied real checker on each nested fixture.
  for (const sourceParts of [
    [".evidence", "notes.md"],
    [".logs", "day.md"],
    [".evidence", "abc123", "wt", "review-runs", "run-1", "notes.md"],
    [".evidence", "provisional", "run-2", "wt", "seed.md"],
    [".logs", "session", "day.md"],
    [".worktrees", "other", "README.md"],
  ]) {
    test(`skips retained dead links under ${sourceParts.slice(0, -1).join("/")}`, (t) => {
      const result = checkFixture(t, sourceParts);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, "Documentation links are valid.\n");
      assert.equal(result.stderr, "");
    });
  }

  for (const sourceParts of [
    ["docs", "guide.md"],
    ["docs", ".evidence", "notes.md"],
    ["docs", ".logs", "day.md"],
    ["docs", ".evidence", "nested", "notes.md"],
    ["docs", ".logs", "nested", "day.md"],
    [".claude", "commands", "command.md"],
    ["worktrees", "document.md"],
    ["other", ".claude", "worktrees", "document.md"],
  ]) {
    test(`checks dead links under ${sourceParts.slice(0, -1).join("/")}`, (t) => {
      const result = checkFixture(t, sourceParts);
      assert.equal(result.status, 1, result.stderr);
      assert.equal(result.stderr, `${join(...sourceParts)}:1: missing missing.md\n`);
      assert.equal(result.stdout, "");
    });
  }
});

test("required stages preserve all gates with four independent Rust phases", () => {
  for (const platform of ["win32", "linux"]) {
    const stages = requiredStages(platform);
    assert.equal(stages.length, 12);
    assert.equal(new Set(stages.map(({ name }) => name)).size, 12);
    assert.deepEqual(stages.slice(0, 3).map(({ command, args }) => [command, ...args]), [
      ["cargo", "fmt", "--check"],
      ["cargo", "check"],
      ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ]);
    assert.match([stages[3].command, ...stages[3].args].join(" "), platform === "win32"
      ? /pwsh.*-NoProfile.*-File scripts\/test-rust\.ps1/u
      : /scripts\/test-rust\.sh/u);
    assert.deepEqual(stages.slice(3, 7).map(({ name, args, selects }) => [name, args.slice(-2), selects]), [
      ["rust", ["--phase", "ordinary"], false],
      ["rust-scale-claims", ["--phase", "scale-claims"], true],
      ["rust-scale-roots", ["--phase", "scale-roots"], true],
      ["rust-scale-planning", ["--phase", "scale-planning"], true],
    ]);
    assert.deepEqual(stages[7].args, ["--test", "scripts/review-freeze-fingerprint.test.mjs", "scripts/test-launcher.test.mjs"]);
    assert.deepEqual(stages.slice(8).map(({ args }) => args), [
      ["--test", "scripts/mcp-dogfood.test.mjs"],
      ["--test", "scripts/control-dogfood.test.mjs"],
      ["--test", "scripts/parity.test.mjs"],
      ["scripts/check-doc-links.mjs"],
    ]);
  }
});

const rustPhaseArgs = [
  ["--", "cargo", "test"],
  ["--", "cargo", "test", "claim_validated_mutations_are_bounded_at_project_scale", "--", "--ignored", "--nocapture"],
  ["--", "cargo", "test", "root_delta_scale_", "--", "--ignored", "--nocapture"],
  ["--", "cargo", "test", "planning_scale_", "--", "--ignored", "--nocapture"],
];
function rustWrapperFixture(root, platform) {
  mkdirSync(join(root, "scripts"));
  for (const file of ["test-rust.ps1", "test-rust.sh", "test-rust-host.sh"]) {
    copyFileSync(join(dirname(launcher), file), join(root, "scripts", file));
  }
  const callsPath = join(root, ".git", "phase-calls.json");
  writeFileSync(join(root, "scripts", "test-temp.mjs"), `
    import assert from 'node:assert/strict';
    import { existsSync, readFileSync, writeFileSync } from 'node:fs';
    const args = process.argv.slice(2);
    const calls = existsSync(process.env.PHASE_CALLS) ? JSON.parse(readFileSync(process.env.PHASE_CALLS, 'utf8')) : [];
    calls.push({ args, cwd: process.cwd(), threads: process.env.RUST_TEST_THREADS });
    writeFileSync(process.env.PHASE_CALLS, JSON.stringify(calls));
    console.log('     Running unittests src/lib.rs (target/debug/deps/fixture)');
    if (process.env.PHASE_RESULTS && args.includes('claim_validated_mutations_are_bounded_at_project_scale')) {
      const saved = JSON.parse(readFileSync(process.env.PHASE_RESULTS, 'utf8'));
      assert.deepEqual(saved.stages.map(({ state }) => state), ['passed', 'running', 'unrun', 'unrun']);
      const prior = saved.stages[0];
      assert.equal(prior.code, 0);
      assert.ok(prior.started && prior.ended);
      assert.equal(prior.tests.passed, 1);
      writeFileSync(process.env.PHASE_SNAPSHOT, JSON.stringify(prior));
      console.log('running 1 test');
      console.log('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s');
      process.exit(101);
    }
    console.log('running 1 test');
    console.log('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s');
  `);
  const runner = requiredStages(platform)[3];
  return { callsPath, command: runner.command, args: runner.args.slice(0, -2),
    env: { ...env, ENGRAM_TEST_THREADS: "2", PHASE_CALLS: callsPath } };
}

for (const platform of ["win32", "linux"]) {
  const supported = platform === "win32" ? process.platform === "win32" : process.platform !== "win32";
  test(`Rust phase wrapper (${platform}) preserves default, selected and Cargo passthrough execution`,
    { skip: !supported, timeout: 60_000 }, async () => {
      await repository(async (root) => {
        const fixture = rustWrapperFixture(root, platform);
        const invoke = (args) => {
          if (existsSync(fixture.callsPath)) unlinkSync(fixture.callsPath);
          const result = spawnSync(fixture.command, [...fixture.args, ...args],
            { cwd: root, env: fixture.env, encoding: "utf8", windowsHide: true });
          const calls = existsSync(fixture.callsPath) ? json(fixture.callsPath) : [];
          return { result, calls };
        };
        const assertCalls = (args, expected) => {
          const { result, calls } = invoke(args);
          assert.equal(result.status, 0, result.stderr);
          assert.deepEqual(calls, expected.map((args) => ({ args, cwd: root, threads: "2" })));
        };
        assertCalls([], rustPhaseArgs);
        for (const [index, phase] of ["ordinary", "scale-claims", "scale-roots", "scale-planning"].entries()) {
          assertCalls(["--phase", phase], [rustPhaseArgs[index]]);
        }
        assertCalls(["--lib", "contract", "--", "--exact"], [["--", "cargo", "test", "--lib", "contract", "--", "--exact"]]);
        assertCalls(["--PHASE", "ordinary"], [["--", "cargo", "test", "--PHASE", "ordinary"]]);
        for (const args of [["--phase"], ["--phase", "unknown"], ["--phase", "Ordinary"], ["--phase", "ordinary", "extra"]]) {
          const { result, calls } = invoke(args);
          assert.equal(result.status, 2, result.stderr);
          assert.match(result.stderr, /Usage: test-rust\.(?:ps1|sh) --phase/u);
          assert.deepEqual(calls, [], "invalid selectors never reach the harness");
        }
      });
    });

  test(`Rust phase launcher (${platform}) saves the ordinary result before a scale failure`,
    { skip: !supported, timeout: 60_000 }, async () => {
      await repository(async (root) => {
        const fixture = rustWrapperFixture(root, platform);
        const stages = requiredStages(platform).slice(3, 7);
        const runDir = createRun({ root, full: true, stages }, fixture.env);
        const snapshot = join(root, ".git", "phase-snapshot.json");
        const result = await executeRun(runDir, { ...fixture.env,
          PHASE_RESULTS: join(runDir, "results.json"), PHASE_SNAPSHOT: snapshot });
        assert.equal(result.state, "failed");
        assert.equal(result.exitCode, 101, JSON.stringify({ error: result.error,
          stages: result.stages.map(({ name, state, code, diagnostics }) => ({ name, state, code, diagnostics })) }));
        const saved = json(join(runDir, "results.json"));
        assert.deepEqual(saved.stages.map(({ state }) => state), ["passed", "failed", "unrun", "unrun"]);
        assert.deepEqual(saved.stages[0], json(snapshot), "the already durable ordinary result remains unchanged");
        assert.equal(saved.stages[1].code, 101);
        assert.equal(saved.stages[1].tests.failed, 1);
        assert.deepEqual(json(fixture.callsPath).map(({ args }) => args), rustPhaseArgs.slice(0, 2));
        const record = parseRecord(machineRecord(result.plan, result));
        assert.deepEqual(record.stages.map(({ name, state, exit, why }) => [name, state, exit, why]), [
          ["rust", "passed", "0", undefined], ["rust-scale-claims", "failed", "101", undefined],
          ["rust-scale-roots", "skipped", "none", "not-run"], ["rust-scale-planning", "skipped", "none", "not-run"],
        ]);
      });
    });
}

test("clean pass retains logs and summarizes without passing-test lists", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("clean", "console.log('ok 1 - passing-marker')")] }, env);
    assert.match(relative(join(realpathSync.native(root), ".git", "review-runs"), runDir), /^test-[^\\/]+$/u);
    assert.ok(existsSync(join(runDir, "request.json")));
    assert.equal(json(join(runDir, "results.json")).state, "running");
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.exitCode, 0);
    assert.equal(result.stages[0].state, "passed");
    assert.deepEqual(result.limitations, fingerprintLimitations());
    assert.match(readFileSync(logPath(runDir, result.stages[0]), "utf8"), /passing-marker/u);
    const summary = await summarize(runDir);
    assert.match(summary, /PASS/iu);
    assert.ok(summary.includes(NORMALIZATION_LIMITATION));
    assert.doesNotMatch(summary, /passing-marker/u);
    assert.ok(Buffer.byteLength(summary) < 12_288);
  });
});

test("launcher fixtures ignore inherited Git routing and configuration", async () => {
  await repository(async (root) => {
    const config = join(root, ".git", "hostile-global-config");
    writeFileSync(config, "[core]\n bare = true\n");
    const nestedEnv = { ...env, GIT_DIR: join(root, "not-a-repository"), GIT_CONFIG_GLOBAL: config,
      GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "core.bare", GIT_CONFIG_VALUE_0: "true" };
    // Start an independent test runner, not an inherited node:test worker or
    // a second owner of this process's temporary fixture root.
    delete nestedEnv.NODE_TEST_CONTEXT;
    delete nestedEnv.ENGRAM_TEST_RUN_ROOT;
    const result = spawnSync(process.execPath, ["--test", "--test-name-pattern=^clean pass", fileURLToPath(import.meta.url)], {
      env: nestedEnv,
      encoding: "utf8", windowsHide: true,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /clean pass retains logs/u);
  });
});

test("passing rows interleaved with a failure do not discard its detail", async () => {
  await repository(async (root) => {
    const output = ["thread 'fixture' panicked at test.rs:1", "ok 1 - unrelated passing-name",
      "  left: 1", " ✓ another passing-name", " right: 2", "  at test.rs:1:2"].join("\n");
    const runDir = createRun({ root, stages: [stage("interleaved",
      `console.error(${JSON.stringify(output)}); process.exitCode = 7`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 7);
    assert.equal(result.stages[0].diagnostics.text,
      "thread 'fixture' panicked at test.rs:1\n  left: 1\n right: 2\n  at test.rs:1:2\n");
    assert.doesNotMatch(await summarize(runDir), /passing-name/u);
  });
});

test("nonzero exit is preserved and later stages remain unrun", async () => {
  await repository(async (root) => {
    const marker = join(root, ".git", "should-not-run");
    const runDir = createRun({ root, stages: [
      stage("failure", "console.error('error: deliberate fixture failure'); process.exit(23)"),
      stage("later", `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`),
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.equal(result.exitCode, 23);
    assert.deepEqual(result.stages.map(({ state }) => state), ["failed", "unrun"]);
    assert.deepEqual(result.stages.map(({ cwd }) => cwd), [root, root]);
    assert.equal(result.stages[0].code, 23);
    assert.equal(existsSync(marker), false);
    assert.doesNotMatch(await summarize(runDir), /deliberate fixture failure/u);
    assert.match(await inspectFailure(runDir, "failure"), /deliberate fixture failure/u);
  });
});

test("failed exits with only passing output retain exit and log, not passing lists", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("interrupted",
      "console.log('ok 1 - passing-marker'); console.log('test pass_marker ... ok'); process.exitCode = 23")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 23);
    assert.equal(result.stages[0].diagnostics.text, "");
    const summary = await summarize(runDir);
    assert.match(summary, /FAIL .*exit=23/u);
    assert.ok(summary.includes(join(runDir, "results.json")));
    assert.doesNotMatch(summary, /passing-marker|pass_marker/u);
    assert.match(readFileSync(result.stages[0].log, "utf8"), /passing-marker/u);
  });
});

test("exclusive execution admission cannot rerun or overwrite a completed run", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "counter");
    const runDir = createRun({ root, stages: [stage("once",
      `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)] }, env);
    assert.equal((await executeRun(runDir, env)).state, "passed");
    const saved = readFileSync(join(runDir, "results.json"), "utf8");
    await assert.rejects(executeRun(runDir, env), { code: "EEXIST" });
    assert.equal(readFileSync(counter, "utf8"), "run\n");
    assert.equal(readFileSync(join(runDir, "results.json"), "utf8"), saved);
  });
});

test("unrecognized failures preserve the bounded tail and fallback explanation", async () => {
  await repository(async (root) => {
    for (const output of ["opaque unsuccessful outcome\n", `${"x".repeat(2800)}\nopaque final context\n`]) {
      const runDir = createRun({ root, stages: [stage("opaque",
        `process.stdout.write(${JSON.stringify(output)}); process.exitCode = 19`)] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.exitCode, 19);
      assert.deepEqual(result.stages[0].diagnostics, {
        text: output.slice(-2400),
        truncated: output.length > 2400,
        fallback: "no recognized diagnostic; bounded failure tail",
      });
    }
  });
});

test("detached parent emits exact recovery receipt and child completes once", async () => {
  await repository(async (root) => {
    // Node acts as a hermetic CLI, executing this fixture instead of a mailbox.
    writeFileSync(join(root, "mailbox"), `
      const fs = require('node:fs');
      const file = process.argv[process.argv.indexOf('--message-file') + 1];
      if (!fs.readFileSync(file, 'utf8').startsWith('PASS ')) process.exitCode = 9;
    `);
    const counter = join(root, ".git", "count");
    const runDir = createRun({ root, detached: true, notifyTo: "fixture-parent", stages: [stage("once",
      `require('node:assert/strict').equal(process.cwd(), ${JSON.stringify(root)}); require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)] }, env);
    assert.equal(json(join(runDir, "results.json")).stages[0].cwd, root);
    let receipt = "";
    const { child, completion } = await startDetached(runDir, env, (text) => { receipt += text; });
    child.ref(); // Test owns cleanup and waits for the actual child, no polling.
    const outcome = await completion;
    assert.equal(outcome.code, 0);
    assert.equal(outcome.error, undefined);
    assert.ok(receipt.startsWith(`STARTED ${runDir}\n`));
    assert.ok(receipt.includes(`pid=${child.pid} completion=mailbox:fixture-parent\n`));
    assert.ok(receipt.includes(`manifest=${join(runDir, "input.json")}\n`));
    // The liveness hint quotes the run directory, so a path with spaces
    // stays one argument when the command is copied.
    assert.ok(receipt.includes(`node scripts/test-launcher.mjs summary "${runDir}"`), receipt);
    const printed = /^expectedFingerprint=(.+)$/mu.exec(receipt)?.[1];
    assert.equal(printed, json(join(runDir, "input.json")).fingerprint);
    assert.equal(printed, json(join(runDir, "results.json")).before);
    assert.equal(json(join(runDir, "results.json")).state, "passed");
    assert.equal(json(join(runDir, "request.json")).detached, true, "the worker preserves the parent's mode");
    assert.equal(json(join(runDir, "results.json")).stages[0].cwd, root);
    assert.equal(json(join(runDir, "notification.json")).code, 0);
    assert.equal(readFileSync(counter, "utf8"), "run\n");
    // The detached parent prints no record, because its run has not
    // finished. The worker ends launcher.log with the record instead.
    assert.doesNotMatch(receipt, /test-launcher\/v1/u);
    const logged = readFileSync(join(runDir, "launcher.log"), "utf8");
    const record = parseRecord(logged.slice(logged.indexOf("\ntest-launcher/v1") + 1));
    assert.equal(record.run, basename(runDir));
    assert.equal(record.state, "passed");
    assert.deepEqual(record.stages.map(({ name, state }) => [name, state]), [["once", "passed"]]);
    if (process.platform !== "win32") {
      assert.equal(statSync(runDir).mode & 0o077, 0);
      for (const file of readdirSync(runDir)) assert.equal(statSync(join(runDir, file)).mode & 0o077, 0, file);
    }
  });
});

function waitForStageReady(child, broker, signal) {
  return new Promise((done, reject) => {
    const cleanup = () => {
      child.removeListener("error", fail);
      child.removeListener("close", closed);
      broker.removeListener("error", fail);
      broker.removeListener("close", brokerClosed);
      broker.removeListener("message", changed);
      signal.removeEventListener("abort", aborted);
    };
    const fail = (error) => { cleanup(); reject(error); };
    const closed = (code) => fail(new Error(`launcher ended before stage readiness (${code})`));
    const aborted = () => fail(signal.reason);
    const brokerClosed = () => fail(new Error("readiness broker ended"));
    const changed = (message) => { if (message === "connected") { cleanup(); done(); } };
    child.once("error", fail);
    child.once("close", closed);
    broker.once("error", fail);
    broker.once("close", brokerClosed);
    broker.on("message", changed);
    signal.addEventListener("abort", aborted, { once: true });
    if (signal.aborted) aborted();
    else if (child.exitCode !== null || child.signalCode !== null) closed(child.exitCode);
  });
}

for (const earlyExit of [false, true]) test(earlyExit
  ? "foreground readiness rejects a worker exiting after receipt but before ready"
  : "foreground CLI exposes its input without taking ownership of caller IPC", async (t) => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    let child, completion;
    // Local IPC, not TCP or filesystem change notifications. On POSIX both
    // processes resolve this short socket path from the same fixture cwd.
    const address = process.platform === "win32" ? `\\\\.\\pipe\\engram-${randomUUID()}` : ".git/stage.sock";
    const broker = spawn(process.execPath, ["-e", `
      const server = require('node:net').createServer(socket => {
        sockets.add(socket); socket.on('close', () => sockets.delete(socket));
        socket.on('error', () => socket.destroy());
        process.send('connected');
      });
      const sockets = new Set();
      process.on('disconnect', () => {
        for (const socket of sockets) socket.destroy();
        server.close();
      });
      server.listen(${JSON.stringify(address)}, () => process.send('listening'));
    `], { cwd: root, env, windowsHide: true, stdio: ["ignore", "ignore", "ignore", "ipc"] });
    // The broker has no captured output to drain. Observe process exit rather
    // than stdio close after disconnecting its IPC handle.
    const brokerCompletion = once(broker, "exit");
    const abort = () => {
      if (broker.connected) broker.disconnect();
      child?.kill();
    };
    t.signal.addEventListener("abort", abort, { once: true });
    try {
      const listening = await Promise.race([
        once(broker, "message", { signal: t.signal }),
        brokerCompletion.then(() => { throw new Error("broker exited before listening"); }),
      ]);
      assert.equal(listening[0], "listening");
      const source = earlyExit ? "process.exit(27)" : `
        const socket = require('node:net').connect(${JSON.stringify(address)});
        socket.on('error', error => { console.error(error); process.exitCode = 1; });`;
      child = spawn(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused", "--", process.execPath, "-e", source],
        { env, windowsHide: true, stdio: ["ignore", "pipe", "pipe", "ipc"] });
      const ipcMessages = [];
      child.on("message", (message) => ipcMessages.push(message));
      completion = once(child, "close");
      child.stderr.resume();
      const ready = waitForStageReady(child, broker, t.signal);
      let output = "";
      const receipt = new Promise((done, reject) => {
        t.signal.addEventListener("abort", () => reject(t.signal.reason), { once: true });
        child.once("error", reject);
        child.once("close", () => reject(new Error("launcher ended before its startup receipt")));
        child.stdout.on("data", (chunk) => {
          output += chunk;
          if (/^expectedFingerprint=.+\n/mu.test(output)) done(output);
        });
      });
      // Observe both promises immediately: exit/error/abort cannot strand a
      // readiness wait after the receipt has already resolved.
      const admitted = Promise.all([receipt, ready]);
      if (earlyExit) {
        await assert.rejects(admitted, /launcher ended before stage readiness \(27\)/u);
        assert.match(output, /^expectedFingerprint=.+$/mu);
        assert.equal((await completion)[0], 27);
        return;
      }
      const [early] = await admitted;
      const runDir = /^STARTED (.+)$/mu.exec(early)?.[1];
      assert.ok(runDir);
      const printed = /^expectedFingerprint=(.+)$/mu.exec(early)?.[1];
      assert.equal(printed, json(join(runDir, "input.json")).fingerprint);
      assert.ok(early.includes(`manifest=${join(runDir, "input.json")}\n`));
      assert.equal(json(join(runDir, "results.json")).state, "running");
      assert.equal(child.exitCode, null);
      assert.equal(child.connected, true, "foreground launcher must not disconnect its caller's IPC channel");
      assert.deepEqual(ipcMessages, [], "foreground launcher must not send the private worker handshake");
      broker.disconnect();
      assert.equal((await completion)[0], 0);
      assert.deepEqual(ipcMessages, []);
      assert.equal(json(join(runDir, "results.json")).state, "passed");
    } finally {
      t.signal.removeEventListener("abort", abort);
      if (broker.connected) broker.disconnect();
      if (t.signal.aborted) child?.kill();
      if (completion) await completion;
      await brokerCompletion;
    }
  });
});

test("inherited toolchain overrides are cleared for repository selection", async () => {
  await repository(async (root) => {
    writeFileSync(join(root, "rust-toolchain.toml"), '[toolchain]\nchannel = "1.90.0"\n');
    const runDir = createRun({ root, stages: [stage("environment",
      "console.log(process.env.RUSTUP_TOOLCHAIN ?? 'repository-selected')")] }, env);
    const result = await executeRun(runDir, { ...env, RUSTUP_TOOLCHAIN: "wrong-inherited-channel" });
    assert.equal(result.state, "passed");
    assert.equal(readFileSync(result.stages[0].log, "utf8"), "repository-selected\n");
    assert.deepEqual(result.clearedToolchainOverrides,
      [{ name: "RUSTUP_TOOLCHAIN", value: "wrong-inherited-channel" }]);
    assert.deepEqual(json(join(runDir, "results.json")).clearedToolchainOverrides, result.clearedToolchainOverrides);
    assert.doesNotMatch(await summarize(runDir), /inherited Rustup override/u);
  });
});

test("malformed notification targets and duplicate prerequisites refuse before creating a run", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    for (const [options, expected] of [
      [["--notify", "--detach"], /--notify requires a session id/u],
      [["--notify", ""], /--notify requires a session id/u],
      [["--notify", "   "], /--notify requires a session id/u],
      [["--notify", " --detach "], /--notify requires a session id/u],
      [["--notify", "first", "--notify", "second"], /--notify may be supplied only once/u],
      [["--require-binary-env", "bad-name"], /--require-binary-env requires an uppercase variable name/u],
      [["--require-binary-env", "EXAMPLE_BIN", "--require-binary-env", "EXAMPLE_BIN"], /duplicate binary prerequisite: EXAMPLE_BIN/u],
    ]) {
      const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused", ...options,
        "--", process.execPath, "-e", ""],
        { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(result.status, 1);
      assert.match(result.stderr, expected);
      assert.doesNotMatch(result.stdout, /STARTED|PASS/u);
      assert.equal(existsSync(join(root, ".git", "review-runs")), false);
    }
  });
});

test("entry points run their main path when invoked through a directory link", async () => {
  await repository(async (root) => {
    const real = join(root, "scripts-real");
    mkdirSync(real);
    for (const name of [...launcherFixtureFiles, "test-temp.mjs"]) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(real, name));
    }
    // Node resolves a module's own URL through links, as macOS does for
    // /var -> /private/var temp paths. A junction reproduces that aliasing on
    // Windows without extra privileges.
    const linked = join(root, "scripts");
    symlinkSync(real, linked, process.platform === "win32" ? "junction" : "dir");
    assert.notEqual(realpathSync(linked), linked, "the fixture must invoke through an alias");

    const launched = spawnSync(process.execPath, [join(linked, "test-launcher.mjs"), "focused", "--notify", "",
      "--", process.execPath, "-e", ""], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(launched.status, 1, `launcher skipped its main path: ${launched.stderr}`);
    assert.match(launched.stderr, /--notify requires a session id/u);

    const auditEnv = { ...env };
    delete auditEnv.ENGRAM_TEST_RUN_ROOT;
    const audited = spawnSync(process.execPath, [join(linked, "test-temp.mjs")],
      { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
    assert.equal(audited.status, 1, `temp audit skipped its main path: ${audited.stderr}`);
    assert.match(audited.stderr, /Usage: node scripts\/test-temp\.mjs -- PROGRAM/u);
  });
});

test("invalid foreground and detached notification setup refuses before creating a run", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    for (const [notifyTo, overrides, expected] of [
      [env.TERMAL_SESSION_ID, {}, /self-send/u],
      [` ${env.TERMAL_SESSION_ID} `, {}, /self-send/u],
      ["fixture-parent", { TERMAL_SESSION_ID: "" }, /TERMAL_SESSION_ID/u],
      ["fixture-parent", { TERMAL_CLI: "" }, /TERMAL_CLI/u],
      ["fixture-parent", { TERMAL_CLI: "relative-cli" }, /TERMAL_CLI/u],
      ["fixture-parent", { TERMAL_CLI: join(launcher, "missing.exe") }, /notification-cli-unavailable/u],
    ]) {
      for (const modeFlags of [[], ["--detach"]]) {
        const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused", ...modeFlags, "--notify", notifyTo,
          "--", process.execPath, "-e", "throw new Error('must not execute')"],
        { cwd: root, env: { ...env, ...overrides }, encoding: "utf8", windowsHide: true });
        assert.equal(result.error, undefined);
        assert.equal(result.status, 1);
        assert.match(result.stderr, expected);
        assert.doesNotMatch(result.stdout, /STARTED|End your turn/u);
        assert.equal(existsSync(join(root, ".git", "review-runs")), false);
      }
    }
  });
});

// Exercise the real child entrypoint and IPC admission, never the live mailbox.
test("detached parent rejects failed admission after child closure without a receipt", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, detached: true, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
    writeFileSync(join(runDir, "results.json"), "{");
    let receipt = "";
    await assert.rejects(startDetached(runDir, env, (text) => { receipt += text; }),
      /worker startup failed \(1\)/u);
    assert.equal(receipt, "");
    const result = json(join(runDir, "results.json"));
    assert.equal(result.state, "failed");
    assert.match(result.error, /JSON|property|position|end/iu);
    assert.ok(result.ended);
    assert.equal(existsSync(join(runDir, "unused.log")), false);
    // Rejection is observed on close, after the real child has finished its
    // terminal result and fixture notification attempt. No polling or sleeps.
    assert.equal(json(join(runDir, "notification.json")).code, 1);
    assert.match(readFileSync(join(runDir, "launcher.log"), "utf8"), /notification failed/u);
  });
});

async function worker(runDir, scriptPath = launcher) {
  const child = spawn(process.execPath, [scriptPath, "_run", runDir],
    { env, windowsHide: true, stdio: ["ignore", "pipe", "pipe", "ipc"] });
  const stdout = [], stderr = [];
  child.stdout.on("data", (chunk) => stdout.push(chunk));
  child.stderr.on("data", (chunk) => stderr.push(chunk));
  const messages = [];
  child.on("message", (message) => messages.push(message));
  const code = await new Promise((done, reject) => {
    child.once("error", reject);
    child.once("close", done);
  });
  return { code, messages, stdout: Buffer.concat(stdout).toString("utf8"),
    stderr: Buffer.concat(stderr).toString("utf8") };
}

function copyLauncher(root, transform = (source) => source) {
  const scripts = join(root, "scripts");
  mkdirSync(scripts);
  const local = join(scripts, "test-launcher.mjs");
  writeFileSync(local, transform(readFileSync(launcher, "utf8")));
  for (const name of launcherSupportFiles) copyFileSync(join(dirname(launcher), name), join(scripts, name));
  return local;
}

const workerException = "fixture private escaping worker exception";
const workerCause = "fixture private worker cause";
function injectWorkerException(source) {
  const changed = source.replace("await finish(runDir, { workerHandshake: true });",
    `throw new Error(${JSON.stringify(workerException)}, { cause: new Error(${JSON.stringify(workerCause)}) });`);
  assert.notEqual(changed, source, "the copied launcher must inject the worker exception");
  return changed;
}

test("CLI Git run-directory failure gives safe repository guidance without artifacts", (t) => {
  const root = fixtureHome("launcher-no-git-", t);
  const local = copyLauncher(root);
  const result = spawnSync(process.execPath, [local, "focused", "--", process.execPath, "-e", ""],
    { cwd: root, env, encoding: "utf8", windowsHide: true });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.equal(result.stderr, "FAIL launcher: git-run-directory-unavailable; run the launcher from a repository worktree with Git available\n");
  assert.doesNotMatch(result.stdout + result.stderr, /^[a-z][a-z-]*: (passed|failed)\b/mu);
  assert.doesNotMatch(result.stderr, /fatal:|not a git repository|ENOENT|spawn/u);
  assert.deepEqual(readdirSync(root), ["scripts"]);
});

test("worker startup errors are terminal and never acknowledge readiness", async () => {
  await repository(async (root) => {
    for (const file of ["request.json", "results.json"]) {
      const runDir = createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
      writeFileSync(join(runDir, file), "{");
      const { code, messages } = await worker(runDir);
      assert.equal(code, 1);
      assert.deepEqual(messages, []);
      const result = json(join(runDir, "results.json"));
      assert.equal(result.state, "failed");
      assert.equal(result.exitCode, 1);
      assert.ok(result.ended);
      assert.match(result.error, /JSON|property|position|end/iu);
      if (file === "results.json") {
        // Node is the fixture CLI; it refuses the mailbox args. The attempt
        // still proves notification happens after terminal results are saved.
        assert.ok(existsSync(join(runDir, "notification.json")));
        assert.match(readFileSync(join(runDir, "notification.message.txt"), "utf8"), /^FAIL/u);
      }
    }
  });
});

test("worker ready handshake follows admission and duplicate startup preserves results", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("clean")] }, env);
    const immutable = ["request.json", "completion.request.json", "input.json"].map((name) =>
      [name, readFileSync(join(runDir, name))]);
    writeFileSync(join(runDir, "launcher.log"), "admitted owner's existing log\n", { flag: "wx" });
    const first = await worker(runDir);
    assert.equal(first.code, 0);
    assert.deepEqual(first.messages, [{ ready: true }]);
    assert.equal(json(join(runDir, "results.json")).state, "passed");
    assert.doesNotMatch(first.stdout + first.stderr, /^worker-diagnostics:/mu);
    assert.equal(readdirSync(runDir).some((name) => name.startsWith("worker-error-")), false);
    for (const [name, bytes] of immutable) assert.deepEqual(readFileSync(join(runDir, name)), bytes);
    const ownerFiles = readdirSync(runDir).map((name) => [name, readFileSync(join(runDir, name))]);
    for (let attempt = 1; attempt <= 2; attempt += 1) {
      const refused = await worker(runDir);
      assert.equal(refused.code, 1);
      assert.deepEqual(refused.messages, []);
      assert.equal(refused.stdout, "");
      assert.match(refused.stderr, /FAIL launcher: execution-lock-held/u);
      assert.doesNotMatch(refused.stdout + refused.stderr, /^[a-z][a-z-]*: (passed|failed)\b/mu);
      assert.doesNotMatch(refused.stderr, /EEXIST|Error:|request-or-runtime-failed/u);
      for (const [name, bytes] of ownerFiles) assert.deepEqual(readFileSync(join(runDir, name)), bytes, name);
      const logs = readdirSync(runDir).filter((name) => name.startsWith("worker-error-"));
      assert.equal(logs.length, attempt);
      for (const name of logs) {
        assert.match(readFileSync(join(runDir, name), "utf8"), /EEXIST.*execution\.lock/u);
        if (process.platform !== "win32") assert.equal(statSync(join(runDir, name)).mode & 0o777, 0o600);
      }
      assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
    }
  }, "private-escape-injected-untrusted-worker-diagnostics-");
});

test("escaping worker exception and cause stay in artifacts without changing results or notifying", async () => {
  await repository(async (root) => {
    const local = copyLauncher(root, injectWorkerException);
    git(["add", "scripts"], root);
    const runDir = createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
    const original = readFileSync(join(runDir, "results.json"));
    const failed = await worker(runDir, local);
    assert.equal(failed.code, 1);
    assert.deepEqual(failed.messages, []);
    assert.equal(failed.stdout, "");
    assert.match(failed.stderr, /FAIL launcher: worker-runtime-failed/u);
    assert.doesNotMatch(failed.stdout + failed.stderr, /^[a-z][a-z-]*: (passed|failed)\b/mu);
    assert.doesNotMatch(failed.stderr, /fixture private|Error:|request-or-runtime-failed/u);
    const artifact = /^worker-diagnostics: (.+)$/mu.exec(failed.stderr)?.[1];
    assert.ok(artifact, failed.stderr);
    assert.equal(dirname(artifact), runDir);
    const text = readFileSync(artifact, "utf8");
    assert.ok(text.includes(workerException));
    assert.ok(text.includes(workerCause));
    assert.deepEqual(readFileSync(join(runDir, "results.json")), original);
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
  });
});

test("detached worker exception saves diagnostics before startup rejection without leaking prose", async () => {
  await repository(async (root) => {
    const local = copyLauncher(root, injectWorkerException);
    git(["add", "scripts"], root);
    const failed = spawnSync(process.execPath, [local, "focused", "--detach", "--notify", "fixture-parent",
      "--", process.execPath, "-e", ""], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(failed.status, 1);
    assert.equal(failed.stdout, "");
    assert.match(failed.stderr, /FAIL launcher: worker-startup-failed/u);
    assert.doesNotMatch(failed.stderr, /fixture private|Error:/u);
    const resultPath = /^results: (.+)$/mu.exec(failed.stderr)?.[1];
    assert.ok(resultPath, failed.stderr);
    const runDir = dirname(resultPath);
    const saved = readFileSync(join(runDir, "launcher.log"), "utf8");
    assert.doesNotMatch(failed.stdout + failed.stderr + saved, /^[a-z][a-z-]*: (passed|failed)\b/mu);
    const artifact = /^worker-diagnostics: (.+)$/mu.exec(saved)?.[1];
    assert.ok(artifact, saved);
    assert.doesNotMatch(saved, /fixture private|Error:/u);
    assert.ok(readFileSync(artifact, "utf8").includes(workerException));
    assert.equal(json(resultPath).state, "running");
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
  });
});

test("worker diagnostic write refusal reports missing artifact without masking duplicate admission", async () => {
  await repository(async (root) => {
    const local = copyLauncher(root, (source) => {
      const changed = source.replace('writeFileSync(workerLog, text, { flag: "wx", mode: 0o600 });',
        'throw Object.assign(new Error("fixture private diagnostic write refusal"), { code: "ENOSPC" });');
      assert.notEqual(changed, source, "the copied launcher must refuse the diagnostic write");
      return changed;
    });
    git(["add", "scripts"], root);
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
    writeFileSync(join(runDir, "execution.lock"), "owner's lock\n", { flag: "wx" });
    const ownerFiles = readdirSync(runDir).map((name) => [name, readFileSync(join(runDir, name))]);
    const failed = await worker(runDir, local);
    assert.equal(failed.code, 1);
    assert.deepEqual(failed.messages, []);
    assert.equal(failed.stdout, "");
    assert.match(failed.stderr, /FAIL launcher: execution-lock-held/u);
    assert.doesNotMatch(failed.stdout + failed.stderr, /^[a-z][a-z-]*: (passed|failed)\b/mu);
    assert.match(failed.stderr, /^worker-diagnostics: unavailable$/mu);
    assert.doesNotMatch(failed.stderr, /ENOSPC|fixture private|Error:|worker-runtime-failed/u);
    assert.deepEqual(readdirSync(runDir).sort(), ownerFiles.map(([name]) => name).sort());
    for (const [name, bytes] of ownerFiles) assert.deepEqual(readFileSync(join(runDir, name)), bytes);
  });
});

test("worker missing or file-valued run directory never creates a fallback artifact", async (t) => {
  const root = fixtureHome("worker-no-dir-", t);
  const file = join(root, "file");
  writeFileSync(file, "keep original file\n");
  for (const runDir of [join(root, "missing"), file]) {
    const failed = await worker(runDir);
    assert.equal(failed.code, 1);
    assert.deepEqual(failed.messages, []);
    assert.equal(failed.stdout, "");
    assert.match(failed.stderr, /FAIL launcher: execution-admission-failed/u);
    assert.doesNotMatch(failed.stdout + failed.stderr, /^[a-z][a-z-]*: (passed|failed)\b/mu);
    assert.match(failed.stderr, /^worker-diagnostics: unavailable$/mu);
    assert.doesNotMatch(failed.stderr, /ENOENT|ENOTDIR|Error:/u);
    assert.deepEqual(readdirSync(root), ["file"]);
    assert.equal(readFileSync(file, "utf8"), "keep original file\n");
  }
});

test("warnings stay in artifacts without entering completion or converting success to failure", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("warning", "console.error('warning: fixture caution')")] }, env);
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
    const runDir = createRun({ root, stages: [
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
    const runDir = createRun({ root, stages: [stage("mixed",
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
    const runDir = createRun({ root, stages: [stage("node-warnings", `console.error(${JSON.stringify(output)})`)] }, env);
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

test("bare executable names resolve through PATH and Windows shims refuse", async () => {
  await repository(async (root) => {
    const bin = join(root, ".git", "fixture bin");
    mkdirSync(bin);
    const binary = join(bin, process.platform === "win32" ? "fixture-gate.exe" : "fixture-gate");
    copyFileSync(process.execPath, binary);
    if (process.platform !== "win32") chmodSync(binary, 0o700);
    const fixtureEnv = { ...env };
    for (const key of Object.keys(fixtureEnv)) if (key.toLowerCase() === "path") delete fixtureEnv[key];
    fixtureEnv.PATH = `${join(root, "absent-path-entry")}${delimiter}${bin}`;
    const runDir = createRun({ root, stages: [{ name: "path-command", command: "fixture-gate", args: ["-e", "console.log('resolved-binary')"] }] }, env);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.stages[0].command[0], binary);
    assert.equal(readFileSync(result.stages[0].log, "utf8"), "resolved-binary\n");
    assert.notEqual(realpathSync.native(root), realpathSync.native(process.cwd()));
    const relativeEnv = { ...fixtureEnv, PATH: relative(root, bin) };
    const relativeRun = createRun({ root, stages: [{ name: "relative-path", command: "fixture-gate", args: ["-e", "console.log('relative-binary')"] }] }, env);
    const relativeResult = await executeRun(relativeRun, relativeEnv);
    assert.equal(relativeResult.state, "passed");
    assert.equal(relativeResult.stages[0].command[0], binary);
    assert.equal(readFileSync(relativeResult.stages[0].log, "utf8"), "relative-binary\n");
    if (process.platform === "win32") {
      const quotedRun = createRun({ root, stages: [{ name: "quoted-path", command: "fixture-gate", args: ["-e", "console.log('quoted-binary')"] }] }, env);
      const quotedResult = await executeRun(quotedRun, { ...fixtureEnv, PATH: `"${bin}"` });
      assert.equal(quotedResult.state, "passed");
      assert.equal(quotedResult.stages[0].command[0], binary);
      assert.equal(readFileSync(quotedResult.stages[0].log, "utf8"), "quoted-binary\n");
      for (const extension of ["cmd", "bat"]) {
        const shim = join(bin, `only-shim.${extension}`);
        writeFileSync(shim, "@echo must-not-run\r\n");
        const blocked = createRun({ root, stages: [{ name: "shim", command: shim, args: [] }] }, env);
        const refused = await executeRun(blocked, fixtureEnv);
        assert.equal(refused.state, "failed");
        assert.equal(refused.stages[0].state, "unrun");
        assert.match(refused.error, /executable not found/u);
      }
    } else {
      chmodSync(binary, 0o600);
      const blocked = createRun({ root, stages: [{ name: "permissions", command: "fixture-gate", args: [] }] }, env);
      const refused = await executeRun(blocked, fixtureEnv);
      assert.equal(refused.state, "failed");
      assert.match(refused.error, /EACCES/u);
      assert.ok(refused.error.includes(binary));
    }
  });
});

test("a Windows app execution alias that stat refuses resolves, and other stat refusals stand", () => {
  // The calls are replaced, so no real alias, file or platform is needed, and
  // the fixture directory is never created.
  const directory = join(fixtureRoot, "never-created-aliases");
  const candidate = `${resolve(process.cwd(), directory, "pwsh")}.exe`;
  const pathEnv = { PATH: directory };
  const refusal = (code) => () => { throw Object.assign(new Error(`${code}: refused, stat '${candidate}'`), { code }); };
  const file = (isFile) => () => ({ isFile: () => isFile });
  const link = (isLink) => () => ({ isSymbolicLink: () => isLink });
  const accessed = [];
  const probes = (overrides) => ({ ...fileProbes, access: (path, mode) => accessed.push([path, mode]), ...overrides });
  const find = (platform, overrides) => executable("pwsh", process.cwd(), pathEnv, { platform, probes: probes(overrides) });
  const refused = (platform, overrides, lastError) => {
    assert.throws(() => find(platform, overrides), (error) => {
      assert.match(error.message, /^executable not found: pwsh/u);
      if (lastError) assert.match(error.message, new RegExp(`last error: ${lastError}`, "u"));
      else assert.doesNotMatch(error.message, /last error/u);
      return true;
    });
  };
  // A Store-installed pwsh.exe: stat refuses with EACCES, the entry is a
  // reparse point, and an existence check passes.
  assert.equal(find("win32", { stat: refusal("EACCES"), lstat: link(true) }), candidate);
  assert.deepEqual(accessed, [[candidate, constants.F_OK]]);
  // An ordinary file still resolves as before.
  assert.equal(find("win32", { stat: file(true) }), candidate);
  // Absence, a non-file, a refused entry that is no link, and a link gone
  // between the two calls are each still refused with the existing message.
  refused("win32", { stat: refusal("ENOENT") });
  refused("win32", { stat: file(false) });
  refused("win32", { stat: refusal("EACCES"), lstat: link(false) }, "EACCES");
  refused("win32", { stat: refusal("EACCES"), lstat: refusal("ENOENT") }, "EACCES");
  // A recognized alias still has to pass the existence check.
  refused("win32", { stat: refusal("EACCES"), lstat: link(true), access: refusal("EACCES") }, "EACCES");
  // Other stat errors are not read as an alias.
  refused("win32", { stat: refusal("EPERM"), lstat: link(true) }, "EPERM");
  // Off Windows there are no app aliases, so the same answers are refused.
  refused("linux", { stat: refusal("EACCES"), lstat: link(true) }, "EACCES");
});

test("input capture failure reports once before detached admission or notification", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    // A real failing Git clean filter makes captureFingerprint refuse.
    writeFileSync(join(root, ".gitattributes"), "tracked.txt filter=refuse\n");
    writeFileSync(join(root, "tracked.txt"), "modified\n");
    git(["config", "filter.refuse.clean", `"${process.execPath.replaceAll("\\", "/")}" -e "process.exit(7)"`], root);
    git(["config", "filter.refuse.required", "true"], root);
    const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused",
      "--detach", "--notify", "fixture-parent", "--next-owner", "fixture-next", "--next-action", "Inspect the capture failure.",
      "--", process.execPath, "-e", ""],
      { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /input-capture-failed/u);
    assert.match(result.stdout, /^reason: input-capture-failed$/mu);
    assert.doesNotMatch(result.stderr, /git diff|refuse.clean/u);
    assert.doesNotMatch(result.stdout, /STARTED/u);
    const runBase = join(root, ".git", "review-runs");
    const runs = readdirSync(runBase);
    assert.equal(runs.length, 1);
    const runDir = join(runBase, runs[0]);
    assert.equal(json(join(runDir, "request.json")).detached, true, "the CLI records --detach at creation");
    assert.equal(json(join(runDir, "results.json")).state, "failed");
    assert.equal(json(join(runDir, "results.json")).reason, "input-capture-failed");
    assert.equal(json(join(runDir, "notification.target.json")).nextOwner, "fixture-next");
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false, "capture failure never sends automatically");
    const deliveries = [];
    await notifyRun(runDir, env, async (_command, args) => { deliveries.push(args); return { code: 0 }; });
    assert.deepEqual(deliveries.map((args) => args[args.indexOf("--to") + 1]), ["fixture-parent", "fixture-next"]);
    assert.match(readFileSync(join(runDir, "notification.message.txt"), "utf8"), /NEXT ACTION "Inspect the capture failure\."/u);
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
    assert.equal(existsSync(join(runDir, "notification.message.txt")), true);
    assert.equal(existsSync(join(runDir, "launcher.log")), false);
  });
});

test("Rust zero-failure totals and TAP passing names are not failure diagnostics", async () => {
  await repository(async (root) => {
    const source = [
      "test success_test ... ok",
      "Compiling warn-macros v0.1.0",
      "test result: ok. 1026 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 90.11s",
      "# Subtest: reports an error and warning correctly",
      "ok 1 - reports an error and warning correctly",
      "warning: actual compiler caution",
      "test next_success ... ok",
      "test result: ok. 50 passed; 0 failed; 0 ignored",
    ].join("\n");
    const runDir = createRun({ root, stages: [stage("rust-report", `console.log(${JSON.stringify(source)})`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.stages[0].diagnostics.text, "warning: actual compiler caution\n");
    assert.doesNotMatch(await summarize(runDir), /1026 passed|50 passed|success_test|next_success|Subtest/u);
  });
});

test("missing command preflight prevents every stage from running", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [
      stage("would-pass"),
      { name: "missing", command: join(root, "missing-executable"), args: [] },
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.deepEqual(result.stages.map(({ state }) => state), ["unrun", "unrun"]);
    assert.match(await summarize(runDir), /reason: preflight-failed/u);
  });
});

test("passing Vitest rows cannot consume diagnostics before a genuine later failure", async () => {
  await repository(async (root) => {
    const passing = Array.from({ length: 100 }, (_, index) =>
      ` \u001b[32m✓\u001b[39m fixture.test.ts > handles error and warning passing-marker-${index} 2ms`);
    const failure = [" FAIL fixture.test.ts > real failure", "Error: actual failure marker", "  at fixture.test.ts:5:7"];
    const source = [...passing, ...failure, " ✓ fixture.test.ts > another passing-marker 1ms"].join("\n");
    const runDir = createRun({ root, stages: [stage("vitest-report",
      `console.log(${JSON.stringify(source)}); process.exitCode = 1`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 1);
    assert.equal(result.stages[0].diagnostics.text, `${failure.join("\n")}\n`);
    assert.equal(result.stages[0].diagnostics.truncated, false);
    assert.doesNotMatch(await summarize(runDir), /passing-marker/u);
    assert.match(readFileSync(result.stages[0].log, "utf8"), /passing-marker-99/u);
  });
});

test("explicit binary prerequisites fail before running stages", async () => {
  await repository(async (root) => {
    const fixtureEnv = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: join(root, "missing-binary") };
    const runDir = createRun({ root, stages: [stage("unused")], requiredBinaryEnv: ["ENGRAM_LAUNCHER_FIXTURE_BIN"] }, fixtureEnv);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "failed");
    assert.equal(result.stages[0].state, "unrun");
    assert.match(await summarize(runDir), /reason: preflight-failed/u);
  });
});

test("a successful binary prerequisite is recorded before stages and CLI summary agrees", async () => {
  await repository(async (root) => {
    const fixtureEnv = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: process.execPath };
    const runDir = createRun({ root, stages: [stage("clean")],
      requiredBinaryEnv: ["ENGRAM_LAUNCHER_FIXTURE_BIN"] }, fixtureEnv);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.preflight.length, 1);
    assert.equal(result.preflight[0].name, "ENGRAM_LAUNCHER_FIXTURE_BIN");
    assert.equal(result.preflight[0].code, 0);
    assert.equal(readFileSync(result.preflight[0].log, "utf8").trim(), process.version);
    const summary = spawnSync(process.execPath, [launcher, "summary", runDir],
      { env, encoding: "utf8", windowsHide: true });
    assert.equal(summary.status, 0);
    assert.equal(summary.stdout, await summarize(runDir));
  });
});

test("summary of a silent failing preflight retains its exit and log header", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
    // Rendering fixture, not claimed execution evidence. A silent failed
    // probe needs a header even when there is no diagnostic body to render.
    const result = json(join(runDir, "results.json"));
    const log = join(runDir, "preflight-silent.log");
    writeFileSync(log, "");
    Object.assign(result, { state: "failed", exitCode: 1, ended: new Date().toISOString(),
      preflight: [{ name: "silent", code: 13, log, diagnostics: { text: "", truncated: false } }] });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const summary = await summarize(runDir);
    assert.match(summary, /^FAIL/u);
    assert.match(summary, /preflight: total=1 passed=0 failed=1/u);
    assert.match(summary, /unrun=1/u);
    assert.ok((await inspectFailure(runDir, "preflight:silent")).includes(JSON.stringify(log)));
  });
});

test("runtime spawn exceptions persist terminal failure and leave later stages unrun", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [
      { name: "invalid-argument", command: process.execPath, args: ["\0"] }, stage("later"),
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.match(result.error, /null bytes/iu);
    assert.equal(result.stages[0].error, result.error);
    assert.deepEqual(result.stages.map(({ state }) => state), ["failed", "unrun"]);
    assert.equal(json(join(runDir, "results.json")).state, "failed");
    assert.doesNotMatch(await summarize(runDir), /null bytes/iu);
    assert.ok((await inspectFailure(runDir, "invalid-argument")).includes(result.error));
  });
});

test("large diagnostic output is bounded in results while the complete log survives", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("large",
      "require('node:fs').writeSync(2, 'error: ' + 'x'.repeat(2 * 1024 * 1024) + '\\nTAIL-MARKER\\n'); process.exitCode = 4") ] }, env);
    const result = await executeRun(runDir, env);
    const entry = result.stages[0];
    assert.equal(result.exitCode, 4);
    assert.equal(entry.diagnostics.truncated, true);
    assert.ok(Buffer.byteLength(entry.diagnostics.text) < 12_288);
    assert.ok(statSync(logPath(runDir, entry)).size > 2 * 1024 * 1024);
    assert.match(readFileSync(logPath(runDir, entry), "utf8"), /TAIL-MARKER/u);
    const summary = await summarize(runDir);
    assert.ok(Buffer.byteLength(summary) < 12_288);
    assert.doesNotMatch(summary, /TAIL-MARKER|error: x/u);
    assert.match(await inspectFailure(runDir, "large"), /truncated/u);
  });
});

test("source drift before execution refuses to run the captured input", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
    writeFileSync(join(root, "tracked.txt"), "changed after capture\n");
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.equal(result.stages[0].state, "unrun");
    assert.match(result.error, /^input drift before execution; no stages run\n/u);
    assert.match(result.error, /Current Git status.*\nAM tracked\.txt/u);
  });
});

test("source drift during execution cannot be reported as a passing run", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("changes-source",
      "require('node:fs').writeFileSync('tracked.txt', 'changed during execution\\n')")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.match(result.error, /^input drift: results do not validate the current source\n/u);
    assert.match(result.error, /Current Git status.*\nAM tracked\.txt/u);
  });
});

test("an incomplete run is never a pass and cannot notify a successful completion", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("not-started")], notifyTo: "fixture-parent" }, env);
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
    assert.throws(() => createRun({ root, stages: [stage("refused")] },
      { ...env, ENGRAM_LAUNCHER_HEARTBEAT_MS: "5" }), /ENGRAM_LAUNCHER_HEARTBEAT_MS/u);
    assert.equal(existsSync(join(root, ".git", "review-runs")), false, "an invalid cadence refuses before creating a run");
    const runDir = createRun({ root, stages: [stage("never-started")] },
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
      const runDir = createRun({ root, stages: [stage("clean")] }, env);
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
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
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
    const runDir = createRun({ root, notifyTo: "fixture-parent", item: "original item",
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
    const runDir = createRun({ root, stages: [stage("active")], nextTransition: { owner: "fixture-next", action: "Recover the result." } }, env);
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
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
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
    const noResult = createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
    const early = spawnSync(process.execPath, [launcher, "notify", noResult], { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(early.status, 1);
    assert.match(early.stderr, /cannot notify before terminal results are saved/u);
  });
});

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
      assert.throws(() => createRun({ root, stages: [stage("unused")], ...invalid }, env), /requires/u);
      assert.equal(existsSync(runs), false);
    }
    const item = "  verify release  ";
    const nextTransition = { owner: "fixture-owner", action: "  inspect \"quoted\" path\\name\u0085\u2028\u2029  " };
    const runDir = createRun({ root, stages: [stage("clean")], item, nextTransition }, env);
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
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
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
    const runDir = createRun({ root, stages: [stage("first"), stage("later")], item: "release validation", nextTransition }, env);
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
    const runDir = createRun({ root, stages: [stage("broken")], notifyTo: "fixture-parent",
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
    const runDir = createRun({ root, stages: [stage("failure", "console.error('error: ' + 'x'.repeat(4000));process.exitCode=2"), stage("later")] }, env);
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
    const runDir = createRun({ root, stages: [stage("focused")] }, env);
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
    const runDir = createRun({ root, stages: [stage("count-once",
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
    const runDir = createRun({ root, notifyTo: "fixture-parent",
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
      const runDir = createRun({ root, notifyTo: "fixture-parent", nextTransition: { owner, action: "Continue." },
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
    const runDir = createRun({ root, stages: [stage("clean")], notifyTo: "fixture-parent" }, env);
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
    const runDir = createRun({ root, stages: [stage("broken")] }, env);
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
    const runDir = createRun({ root, stages: requiredStages(), notifyTo: "fixture-parent" }, env);
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
    const runDir = createRun({ root, stages: [stage("focused")] }, env);
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
    const runDir = createRun({ root, stages: [stage("focused")] }, env);
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
    const runDir = createRun({ root, stages: [stage("focused")],
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
    const runDir = createRun({ root, stages }, env);
    const result = json(join(runDir, "results.json"));
    for (const entry of result.stages) Object.assign(entry, { state: "passed", code: 0 });
    const lines = (await summarize(runDir, { result })).split("\n");
    assert.deepEqual(lines.slice(2, 37), stages.map(({ name }) => `${name}: passed exit=0 log=${join(runDir, `${name}.log`)}`));
    assert.match(lines[38], /^stages: total=35 passed=35 failed=0/u);
  });
});

test("failure completion retains numeric states and omits all warning and failure prose", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("warning"), stage("failure"), stage("later")] }, env);
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
    const runDir = createRun({ root, stages: [stage("exception"), stage("fallback"), stage("count")] }, env);
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
    const runDir = createRun({ root, stages: [stage("unfinished"), stage("later")] }, env);
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
    const runDir = createRun({ root, stages: requiredStages() }, env);
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
    const runDir = createRun({ root, stages: [stage("clean",
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
    const selfRun = createRun({ root, stages: [stage("unused")], notifyTo: "fixture-owner" }, env);
    const selfResult = await executeRun(selfRun, env);
    assert.equal(selfResult.state, "failed");
    assert.equal(selfResult.stages[0].state, "unrun");
    assert.match(selfResult.error, /self|same|sender/iu);
    const runDir = createRun({ root, stages: [stage("clean")], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    let sent = false;
    const send = async () => { sent = true; return { code: 0 }; };
    await assert.rejects(notifyRun(selfRun, env, send), /self|same|sender/iu);
    await assert.rejects(notifyRun(runDir, { ...env, TERMAL_SESSION_ID: "different-owner" }, send), /session|sender|owner/iu);
    assert.equal(sent, false);
  });
});

test("fixture removal refuses paths outside its root and deletes inside it", (t) => {
  const home = fixtureHome("engram-guard-", t);
  const target = join(fileURLToPath(new URL("..", import.meta.url)), "target");
  assert.ok(realpathSync.native(home).startsWith(realpathSync.native(target)),
    `fixtures must lie in this repository's target/: ${home}`);
  const root = join(home, "root");
  const inside = join(root, "inside");
  mkdirSync(join(inside, "nested"), { recursive: true });
  const sibling = join(home, "sibling");
  mkdirSync(sibling);
  // Shares the root's name as a string prefix but is not below it.
  const collision = join(home, "root-evil");
  mkdirSync(collision);
  for (const refused of [sibling, join(root, "..", "sibling"), collision, root, dirname(fixtureRoot)]) {
    assert.throws(() => removeFixturePath(refused, root), /Refusing to delete/u, refused);
  }
  assert.throws(() => removeFixturePath(dirname(fixtureRoot)), /Refusing to delete/u);
  assert.ok(existsSync(sibling) && existsSync(collision) && existsSync(inside));

  // A link inside the root that leads outside it is refused, and so is
  // anything reached through it.
  writeFileSync(join(sibling, "kept.txt"), "outside the root");
  const link = join(root, "link");
  symlinkSync(sibling, link, process.platform === "win32" ? "junction" : "dir");
  for (const refused of [link, join(link, "kept.txt")]) {
    assert.throws(() => removeFixturePath(refused, root), /must not be a link/u, refused);
  }
  assert.ok(existsSync(join(sibling, "kept.txt")));

  removeFixturePath(inside, root);
  assert.equal(existsSync(inside), false);
  assert.ok(existsSync(root));
  // A path that is already gone is not an error.
  removeFixturePath(inside, root);
});

test("fixture removal is anchored to this repository's target/tmp/engram", (t) => {
  // Set up the fixture first: it re-checks target/ for links, so the probe
  // below is never written through one.
  const home = fixtureHome("engram-anchor-", t);
  // A victim inside this repository but outside target/tmp/engram. A caller
  // that passes the path's own parent as its root must still be refused.
  const target = join(fileURLToPath(new URL("..", import.meta.url)), "target");
  const probe = join(target, `guard-probe-${randomUUID().slice(0, 8)}`);
  const victim = join(probe, "victim");
  const inner = join(victim, "inner");
  mkdirSync(inner, { recursive: true });
  writeFileSync(join(victim, "kept.txt"), "outside the anchor");
  try {
    for (const [root, refused] of [[probe, victim], [victim, inner]]) {
      assert.throws(() => removeFixturePath(refused, root), /Refusing to delete/u, refused);
    }
    assert.ok(existsSync(join(victim, "kept.txt")) && existsSync(inner));

    // A junction above the given root: root and path read as below the
    // anchor but really lie outside it.
    const linked = join(home, "linked");
    symlinkSync(probe, linked, process.platform === "win32" ? "junction" : "dir");
    assert.throws(() => removeFixturePath(join(linked, "victim", "inner"), join(linked, "victim")), /must not be a link/u);
    assert.ok(existsSync(inner));
    (process.platform === "win32" ? rmdirSync : unlinkSync)(linked);

    // The anchor itself is not a fixture root.
    assert.throws(() => removeFixturePath(home, dirname(fixtureRoot)), /Refusing to delete with a fixture root outside/u);
    assert.ok(existsSync(home));
  } finally {
    // One entry at a time: no recursive delete outside the guard.
    unlinkSync(join(victim, "kept.txt"));
    rmdirSync(inner);
    rmdirSync(victim);
    rmdirSync(probe);
  }
});

test("a linked target/ is refused before anything is created at its destination", async () => {
  await repository(async (root) => {
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("test-temp.mjs", import.meta.url)), join(scripts, "test-temp.mjs"));
    const elsewhere = join(root, "elsewhere");
    mkdirSync(elsewhere);
    const linkedTarget = join(root, "target");
    symlinkSync(elsewhere, linkedTarget, process.platform === "win32" ? "junction" : "dir");
    try {
      const auditEnv = { ...env };
      delete auditEnv.ENGRAM_TEST_RUN_ROOT;
      const result = spawnSync(process.execPath, [join(scripts, "test-temp.mjs"), "--", process.execPath, "-e", ""],
        { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
      assert.notEqual(result.status, 0, "a linked target/ must be refused");
      assert.match(result.stderr, /must not be a link/u);
      assert.deepEqual(readdirSync(elsewhere), [], "nothing may be created at the link's destination");
    } finally {
      (process.platform === "win32" ? rmdirSync : unlinkSync)(linkedTarget);
    }
  });
});

test("fixture creation re-checks the product chain for links on every call", async () => {
  await repository(async (root) => {
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("test-temp.mjs", import.meta.url)), join(scripts, "test-temp.mjs"));
    const victim = join(root, "victim");
    mkdirSync(victim);
    // Import first, then swap target/tmp for a link, then ask for a fixture.
    const driver = join(scripts, "driver.mjs");
    writeFileSync(driver, [
      'import { renameSync, symlinkSync } from "node:fs";',
      'import { join } from "node:path";',
      'import { fixtureHome } from "./test-temp.mjs";',
      `const root = ${JSON.stringify(root)};`,
      'renameSync(join(root, "target", "tmp"), join(root, "target", "tmp-real"));',
      `symlinkSync(${JSON.stringify(victim)}, join(root, "target", "tmp"), process.platform === "win32" ? "junction" : "dir");`,
      'fixtureHome("engram-swap-");',
    ].join("\n"));
    const link = join(root, "target", "tmp");
    try {
      const auditEnv = { ...env };
      delete auditEnv.ENGRAM_TEST_RUN_ROOT;
      const result = spawnSync(process.execPath, [driver], { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
      assert.notEqual(result.status, 0, "fixtureHome must refuse a product chain that became a link");
      assert.match(result.stderr, /must not be a link/u);
      assert.deepEqual(readdirSync(victim), [], "nothing may be created through the link");
    } finally {
      // Remove only the link itself; the fixture cleanup refuses any link.
      try { (process.platform === "win32" ? rmdirSync : unlinkSync)(link); } catch (error) { if (error.code !== "ENOENT") throw error; }
    }
  });
});

test("the run audit removes its empty run root only while no link leads to it", async () => {
  // Each case swaps one step for a link to an empty run directory outside
  // the copied helper's target/tmp/engram, then runs the end-of-run audit.
  for (const [swap, message] of [["tmp", /Test fixture product root must not be a link/u], ["run", /Test fixture root must not be a link/u]]) {
    await repository(async (root) => {
      const scripts = join(root, "scripts");
      mkdirSync(scripts);
      copyFileSync(fileURLToPath(new URL("test-temp.mjs", import.meta.url)), join(scripts, "test-temp.mjs"));
      const victim = join(root, "victim");
      const driver = join(scripts, "driver.mjs");
      writeFileSync(driver, [
        'import { mkdirSync, renameSync, symlinkSync, writeFileSync } from "node:fs";',
        'import { basename, join } from "node:path";',
        'import { assertTempClean, fixtureRoot } from "./test-temp.mjs";',
        `const root = ${JSON.stringify(root)};`,
        `const victim = ${JSON.stringify(victim)};`,
        'const victimRun = join(victim, "engram", basename(fixtureRoot));',
        "mkdirSync(victimRun, { recursive: true });",
        'writeFileSync(join(root, "victim-run.txt"), victimRun);',
        'const kind = process.platform === "win32" ? "junction" : "dir";',
        `if (${JSON.stringify(swap)} === "tmp") {`,
        '  renameSync(join(root, "target", "tmp"), join(root, "target", "tmp-real"));',
        '  symlinkSync(victim, join(root, "target", "tmp"), kind);',
        "} else {",
        '  renameSync(fixtureRoot, `${fixtureRoot}-real`);',
        "  symlinkSync(victimRun, fixtureRoot, kind);",
        "}",
        "assertTempClean([]);",
      ].join("\n"));
      const tmp = join(root, "target", "tmp");
      try {
        const auditEnv = { ...env };
        delete auditEnv.ENGRAM_TEST_RUN_ROOT;
        const result = spawnSync(process.execPath, [driver], { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
        assert.notEqual(result.status, 0, `the audit must refuse a linked ${swap} step`);
        assert.match(result.stderr, message);
        const victimRun = readFileSync(join(root, "victim-run.txt"), "utf8");
        assert.ok(existsSync(victimRun), `the empty run directory behind the link must survive: ${victimRun}`);
      } finally {
        // Remove only the links themselves; the fixture cleanup refuses any link.
        const isLink = (path) => { try { return lstatSync(path).isSymbolicLink(); } catch { return false; } };
        const candidates = isLink(tmp) ? [tmp] : readdirSync(join(tmp, "engram")).map((name) => join(tmp, "engram", name));
        for (const path of candidates.filter(isLink)) (process.platform === "win32" ? rmdirSync : unlinkSync)(path);
      }
    });
  }
});

test("the Node fixture run root leaves Windows path headroom", { skip: process.platform !== "win32" }, () => {
  // Engram stores in Node fixtures put SQLite files about 106 characters below
  // the run root; keep that under the 260-character path limit.
  assert.ok(fixtureRoot.length <= 130, `fixture run root is too long (${fixtureRoot.length}): ${fixtureRoot}`);
});

test("git in a fixture without its own repository cannot climb into this checkout", (t) => {
  const home = fixtureHome("engram-ceiling-", t);
  const result = spawnSync("git", ["rev-parse", "--show-toplevel"],
    { cwd: home, env: process.env, encoding: "utf8", windowsHide: true });
  assert.notEqual(result.status, 0, `git found a repository above the fixture: ${result.stdout}`);
  assert.match(result.stderr, /not a git repository/u);
});

// The Rust gate's Unix entry point takes its thread count and its
// file-descriptor limit from the host through scripts/test-rust-host.sh. No
// gate host is a small, refusing or misreporting one, so the tests below run
// those functions in a POSIX shell against stubbed hosts.

// A shell counts only when it runs a command successfully. On Windows the one
// installed beside Git comes first: a PATH there often holds Git's cmd
// directory without its usr/bin, and a bare name is looked up in the current
// directory before PATH.
function posixShell({ platform = process.platform, run = spawnSync, exists = existsSync } = {}) {
  const candidates = [];
  if (platform === "win32") {
    const execPath = run("git", ["--exec-path"], { encoding: "utf8" });
    if (!execPath.error && execPath.status === 0) {
      const beside = join(execPath.stdout.trim(), "..", "..", "..", "usr", "bin", "sh.exe");
      if (exists(beside)) candidates.push(beside);
    }
  }
  candidates.push("sh");
  const shell = candidates.find((candidate) => {
    const probe = run(candidate, ["-c", ":"]);
    return !probe.error && probe.status === 0;
  });
  assert.ok(shell, "these tests need a POSIX sh: the one installed with Git on Windows, or one on PATH");
  return shell;
}

function runHostFunction(harness, variables, name, { replaceEnvironment = false } = {}) {
  const result = spawnSync(posixShell(), ["-c", harness], { encoding: "utf8", env: { ...(replaceEnvironment ? {} : process.env),
    HOST_FUNCTIONS: fileURLToPath(new URL("./test-rust-host.sh", import.meta.url)), ...variables } });
  assert.equal(result.error, undefined, name);
  assert.equal(result.stderr, "", name);
  assert.equal(result.status, 0, name);
  return result.stdout.trim();
}

test("a shell is chosen only when it runs, and Git's own comes first on Windows", () => {
  // Git for Windows keeps its programs in <root>/mingw64/libexec/git-core and its shell in <root>/usr/bin.
  const execPath = "C:/Git/mingw64/libexec/git-core\n";
  const beside = join("C:/Git", "usr", "bin", "sh.exe");
  const host = ({ git = { status: 0, stdout: execPath }, installed = true, working = [] }) => ({
    run: (command, args) => {
      if (args[0] === "--exec-path") return command === "git" ? git : { status: 1, stdout: "" };
      assert.deepEqual(args, ["-c", ":"], `unexpected probe of ${command}`);
      return working.includes(command) ? { status: 0 } : { status: 1 };
    },
    exists: (path) => installed && path === beside,
  });
  assert.equal(posixShell({ platform: "win32", ...host({ working: [beside, "sh"] }) }), beside);
  assert.equal(posixShell({ platform: "win32", ...host({ working: ["sh"] }) }), "sh", "a failing Git shell is passed over");
  assert.equal(posixShell({ platform: "win32", ...host({ installed: false, working: [beside, "sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { status: 1, stdout: "" }, working: [beside, "sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { status: 1, stdout: execPath }, working: [beside, "sh"] }) }), "sh",
    "a failed Git answer is not used, whatever it printed");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { error: new Error("ENOENT") }, working: ["sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "linux", ...host({ working: [beside, "sh"] }) }), "sh", "Git's shell is a Windows fallback only");
  assert.throws(() => posixShell({ platform: "win32", ...host({ working: [] }) }), /need a POSIX sh/u, "a shell that starts and fails is not a shell");
  assert.throws(() => posixShell({ platform: "linux", run: () => ({ error: new Error("ENOENT") }), exists: () => false }), /need a POSIX sh/u);
});

// Stubs refuse any call but the ones the functions may make, and report it on
// descriptor 3, the test's stderr: the functions discard the stderr and status
// of what they call. Like real hosts, the stubs complain on stderr when they
// fail, so an empty stderr also shows that the functions keep a host's
// complaints to themselves.
test("fd soft-limit step-down asks for smaller limits largest first and never leaves a host below the previous target", () => {
  const harness = `set -eu
exec 3>&2
. "$HOST_FUNCTIONS"
requested=
sysctl() {
  if [ "$#" -ne 2 ] || [ "$1" != -n ] || [ "$2" != kern.maxfilesperproc ]; then
    echo "unexpected call: sysctl $*" >&3
    return 1
  fi
  if [ -n "$REPORTED" ]; then echo "$REPORTED"; return 0; fi
  echo "sysctl: cannot stat /proc/sys/kern/maxfilesperproc: No such file or directory" >&2
  return 1
}
ulimit() {
  if [ "$#" -ne 3 ] || [ "$1" != -S ] || [ "$2" != -n ]; then
    echo "unexpected call: ulimit $*" >&3
    exit 98
  fi
  requested="$requested $3"
  if [ "$3" -le "$ACCEPTED" ]; then current_soft_limit=$3; return 0; fi
  echo "ulimit: open files: cannot modify limit: Invalid argument" >&2
  return 1
}
current_soft_limit=$INHERITED
desired_soft_limit=$DESIRED
if raise_fd_soft_limit; then outcome=kept; else outcome=refused; fi
echo "$outcome final=$current_soft_limit requested=$requested"`;
  // [inherited limit, desired limit, maximum the host reports, largest limit the host accepts]
  const cases = [
    ["the host accepts the target", [1024, 16384, "", 1048576], "kept final=16384 requested= 16384"],
    ["a reported maximum below the target is asked for second", [256, 16384, "10240", 10240], "kept final=10240 requested= 16384 10240"],
    ["a reported maximum above the target is never requested", [256, 16384, "61440", 10000], "kept final=4096 requested= 16384 4096"],
    ["a reported maximum equal to the target is asked for once", [256, 10240, "10240", 100], "refused final=256 requested= 10240 4096"],
    ["no reported maximum falls back to the previous target", [256, 16384, "", 4096], "kept final=4096 requested= 16384 4096"],
    ["a reported maximum below the previous target is asked for after it", [256, 16384, "2048", 4096], "kept final=4096 requested= 16384 4096"],
    ["the smallest candidate is the last resort", [256, 16384, "2048", 2048], "kept final=2048 requested= 16384 4096 2048"],
    ["a reported maximum equal to the previous target is asked for once", [256, 16384, "4096", 100], "refused final=256 requested= 16384 4096"],
    ["a host that accepts nothing keeps the inherited limit and reports failure", [256, 16384, "", 100], "refused final=256 requested= 16384 4096"],
    ["a target equal to the previous one is asked for once", [256, 4096, "", 100], "refused final=256 requested= 4096"],
    ["a target below the previous one is never exceeded", [256, 1024, "", 512], "refused final=256 requested= 1024"],
    ["a target and reported maximum below the previous one are asked for once", [256, 2048, "2048", 100], "refused final=256 requested= 2048"],
    ["an inherited limit above the previous target is kept", [5000, 16384, "", 4096], "kept final=5000 requested= 16384"],
    ["a non-numeric reported maximum is left out", [256, 16384, "n/a", 100], "refused final=256 requested= 16384 4096"],
    ["a zero reported maximum is left out", [256, 16384, "0", 100], "refused final=256 requested= 16384 4096"],
  ];
  for (const [name, [inherited, desired, reported, accepted], expected] of cases) {
    assert.equal(runHostFunction(harness, { INHERITED: String(inherited), DESIRED: String(desired), REPORTED: reported,
      ACCEPTED: String(accepted) }, name), expected, name);
  }
});

test("default test threads are the available processors, at most eight, whatever OpenMP is told", () => {
  // Each source prints its value, or is absent when the value is empty. The
  // nproc stub honours both OpenMP variables as the real one does: the thread
  // count replaces the processor count and the thread limit caps it.
  const harness = `set -eu
exec 3>&2
. "$HOST_FUNCTIONS"
absent() { echo "$1: command not found" >&2; return 127; }
nproc() {
  if [ "$#" -ne 0 ]; then echo "unexpected call: nproc $*" >&3; return 1; fi
  if [ -z "$NPROC" ]; then absent nproc; return 127; fi
  count=\${OMP_NUM_THREADS:-$NPROC}
  if [ -n "\${OMP_THREAD_LIMIT:-}" ] && [ "$OMP_THREAD_LIMIT" -lt "$count" ]; then count=$OMP_THREAD_LIMIT; fi
  echo "$count"
}
getconf() {
  if [ "$#" -ne 1 ] || [ "$1" != _NPROCESSORS_ONLN ]; then echo "unexpected call: getconf $*" >&3; return 1; fi
  if [ -z "$GETCONF" ]; then absent getconf; return 127; fi
  echo "$GETCONF"
}
sysctl() {
  if [ "$#" -ne 2 ] || [ "$1" != -n ] || [ "$2" != hw.ncpu ]; then echo "unexpected call: sysctl $*" >&3; return 1; fi
  if [ -z "$SYSCTL" ]; then absent sysctl; return 127; fi
  echo "$SYSCTL"
}
default_test_threads`;
  // [nproc, getconf, sysctl, OMP_NUM_THREADS, OMP_THREAD_LIMIT]
  const cases = [
    ["a large host is capped at eight", ["24", "24", "", "", ""], "8"],
    ["exactly eight processors", ["8", "", "", "", ""], "8"],
    ["a small host uses what it has", ["4", "16", "", "", ""], "4"],
    ["affinity to one processor", ["1", "16", "", "", ""], "1"],
    ["getconf answers where nproc is absent", ["", "2", "", "", ""], "2"],
    ["sysctl answers where both are absent", ["", "", "6", "", ""], "6"],
    ["four where nothing answers", ["", "", "", "", ""], "4"],
    ["four where the answer is not a number", ["many", "16", "", "", ""], "4"],
    ["four where the answer is zero", ["0", "16", "", "", ""], "4"],
    ["an OpenMP thread count does not lower the gate's", ["24", "", "", "1", ""], "8"],
    ["an OpenMP thread limit does not lower the gate's", ["24", "", "", "", "1"], "8"],
    ["an OpenMP thread count does not raise a small host's", ["2", "", "", "16", ""], "2"],
  ];
  for (const [name, [nproc, getconf, sysctl, threadCount, threadLimit], expected] of cases) {
    // The gate host's own OpenMP settings must not decide a case.
    const { OMP_NUM_THREADS: _count, OMP_THREAD_LIMIT: _limit, ...inherited } = process.env;
    const variables = { ...inherited, NPROC: nproc, GETCONF: getconf, SYSCTL: sysctl };
    if (threadCount) variables.OMP_NUM_THREADS = threadCount;
    if (threadLimit) variables.OMP_THREAD_LIMIT = threadLimit;
    assert.equal(runHostFunction(harness, variables, name, { replaceEnvironment: true }), expected, name);
  }
});

// The record a host reads. Its grammar is a contract with that host; these
// tests hold the launcher to it, and to never giving a count that the runners'
// own summaries do not give.
const stageLine = /^test-launcher\/v1 run=(test-[0-9a-f-]+) stage=([a-z0-9][a-z0-9-]*) kind=(test|build|lint|typecheck|other) state=(passed|failed|skipped|interrupted) exit=(\d+|none)(?: executed=(?:(\d+) passed=(\d+) failed=(\d+) ignored=(\d+)|unknown why=([a-z0-9][a-z0-9-]*)) runners=(\d+) filtered=(yes|no))?$/u;
const overallLine = /^test-launcher\/v1 run=(test-[0-9a-f-]+) overall=(passed|failed|interrupted) scope=(full|focused) stages=(\d+)(?: reason=([a-z0-9][a-z0-9-]*))?$/u;
function parseRecord(text) {
  assert.ok(text.endsWith("\n"), "the record ends with a newline");
  const lines = text.slice(0, -1).split("\n");
  const overall = overallLine.exec(lines.at(-1));
  assert.ok(overall, `overall line does not parse: ${lines.at(-1)}`);
  const stages = lines.slice(0, -1).map((line) => {
    const parsed = stageLine.exec(line);
    assert.ok(parsed, `stage line does not parse: ${line}`);
    assert.ok(Buffer.byteLength(line) < 1024, `a line the host would cut: ${line}`);
    const [, run, name, kind, state, exit, executed, passed, failed, ignored, why, runners, filtered] = parsed;
    assert.equal(runners !== undefined, kind === "test", `only a test stage carries counts: ${line}`);
    return { run, name, kind, state, exit, executed: executed ?? (why ? "unknown" : undefined), passed, failed, ignored, why, runners, filtered };
  });
  const [, run, state, scope, count, reason] = overall;
  assert.equal(Number(count), stages.length);
  assert.ok(stages.every((stage) => stage.run === run), "every line carries the same run");
  assert.equal(reason === undefined, state === "passed", "a reason is given exactly when the run did not pass");
  return { run, state, scope, reason, stages };
}
// The rule the host applies to a record, as agreed with it.
const countsAsPassedTests = ({ state, stages }) => state === "passed"
  && stages.every((stage) => stage.state !== "failed" && stage.state !== "interrupted")
  && stages.filter(({ kind }) => kind === "test").every((stage) =>
    stage.state === "passed" && stage.executed !== "unknown" && stage.failed === "0")
  && stages.some((stage) => stage.kind === "test" && Number(stage.executed) >= 1);
const libtest = (passed, failed, ignored, filteredOut, verdict = failed ? "FAILED" : "ok") =>
  `running ${passed + failed + ignored} tests\ntest result: ${verdict}. ${passed} passed; ${failed} failed; ${ignored} ignored; 0 measured; ${filteredOut} filtered out; finished in 1.25s\n`;
const nodeSummary = (mark, { tests, pass, fail = 0, cancelled = 0, skipped = 0, todo = 0 }) =>
  [`tests ${tests}`, "suites 0", `pass ${pass}`, `fail ${fail}`, `cancelled ${cancelled}`, `skipped ${skipped}`,
    `todo ${todo}`, "duration_ms 12.5"].map((line) => `${mark} ${line}\n`).join("");
const printing = (name, kind, text, exit = 0) => ({ name, kind, command: process.execPath,
  args: ["-e", `process.stdout.write(${JSON.stringify(text)}); process.exitCode = ${exit}`] });
async function recordOf(root, stages, options = {}) {
  const runDir = createRun({ root, stages, ...options }, env);
  const result = await executeRun(runDir, env);
  return { runDir, result, record: parseRecord(machineRecord(result.plan, result)) };
}
async function counted(root, text, options) {
  const log = join(root, `${randomUUID()}.log`);
  writeFileSync(log, text);
  try { return await countTests(log, options); } finally { unlinkSync(log); }
}
const complete = (executed, passed, failed, ignored, runners, filteredOut) =>
  ({ executed, passed, failed, ignored, runners, filteredOut, failures: failed });
const cargoFrame = (text, marker = "     Running unittests src/lib.rs (target/debug/deps/fixture)") => `${marker}\n${text}`;

function directCargoFixture(root) {
  const bin = join(root, ".git", "cargo fixture");
  mkdirSync(bin);
  const binary = join(bin, process.platform === "win32" ? "cargo.exe" : "cargo");
  // A native process with Cargo's command shape emits a retained transcript.
  // Selection, execution, log parsing and record generation remain unmodified.
  copyFileSync(process.execPath, binary);
  if (process.platform !== "win32") chmodSync(binary, 0o700);
  for (const name of ["test", "+stable", "run", "nextest"]) writeFileSync(join(root, name),
    "process.stdout.write(process.env.FIXTURE_TRANSCRIPT); process.exitCode = Number(process.env.FIXTURE_EXIT);\n");
  const runnerEnv = { ...env };
  for (const key of Object.keys(runnerEnv)) if (key.toLowerCase() === "path") delete runnerEnv[key];
  runnerEnv.PATH = `${bin}${delimiter}${process.env.PATH ?? process.env.Path ?? ""}`;
  return { binary, async run(text, { command = "cargo", args = ["test", "--lib", "fixture"], code = 0 } = {}) {
    const runDir = createRun({ root, stages: [{ name: "rust", kind: "test", command, args }] }, runnerEnv);
    const result = await executeRun(runDir, { ...runnerEnv, FIXTURE_TRANSCRIPT: text, FIXTURE_EXIT: String(code) });
    return { result, record: parseRecord(machineRecord(result.plan, result)) };
  } };
}

test("direct Cargo execution selects outer frames for filtered counts, not generic wrappers", async () => {
  await repository(async (root) => {
    const fixture = directCargoFixture(root);
    const text = cargoFrame(libtest(17, 0, 0, 2471)
      .replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n"),
    "     Running unittests src\\lib.rs (target\\debug\\deps\\fixture.exe)");
    for (const [command, args, framed] of [
      ["cargo", ["test", "--lib", "backup::target::tests"], true],
      [fixture.binary, ["test", "--lib", "backup::target::tests"], true],
      ["cargo", ["+stable", "test", "--lib", "backup::target::tests"], true],
      ["cargo", ["test", "--workspace", "--all-targets"], true],
      ["cargo", ["run", "--", "test"], false],
      ["cargo", ["nextest", "run"], false],
      [process.execPath, ["test"], false],
    ]) {
      const { result, record } = await fixture.run(text, { command, args });
      assert.equal(result.state, "passed");
      assert.equal(record.stages[0].exit, "0");
      if (framed) {
        assert.deepEqual(result.stages[0].tests, complete(17, 17, 0, 0, 1, 2471));
        assert.deepEqual([record.stages[0].executed, record.stages[0].passed, record.stages[0].ignored,
          record.stages[0].runners, record.stages[0].filtered], ["17", "17", "0", "1", "yes"]);
      } else {
        assert.equal(record.stages[0].executed, "unknown");
        assert.equal(record.stages[0].why, "incomplete-summary");
      }
    }
  });
});

test("direct Cargo execution preserves frame refusals, doctests and failure outcomes", async () => {
  await repository(async (root) => {
    const fixture = directCargoFixture(root);
    const outer = libtest(3, 0, 0, 7);
    const frames = cargoFrame(outer) + cargoFrame(libtest(2, 0, 0, 4))
      + cargoFrame(libtest(1, 0, 0, 0).repeat(2), "   Doc-tests fixture");
    const summed = await fixture.run(frames, { args: ["test", "--workspace"] });
    assert.deepEqual(summed.result.stages[0].tests, complete(7, 7, 0, 0, 4, 11));
    // Quiet Cargo omits frame markers; a complete summary alone cannot own a frame.
    for (const option of ["-q", "--quiet"]) {
      const quiet = await fixture.run(outer, { args: ["test", option] });
      assert.deepEqual([quiet.record.stages[0].state, quiet.record.stages[0].exit,
        quiet.record.stages[0].executed, quiet.record.stages[0].why],
      ["passed", "0", "unknown", "incomplete-summary"]);
    }
    for (const [text, code, state, why, failures] of [
      [outer, 0, "passed", "incomplete-summary", 0],
      [cargoFrame("running 3 tests\n"), 0, "passed", "incomplete-summary", 0],
      [cargoFrame(outer.replace("tests\n", "tests\nrunning 3 tests\n")), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer + outer), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer.replace("3 passed", "2 passed")), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer.replace("3 passed", "9007199254740992 passed")), 0, "passed", "malformed-summary", 0],
      [cargoFrame("running 2 tests\n" + libtest(1, 0, 0, 0), "   Doc-tests fixture"), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame("running 3 tests\n" + libtest(0, 1, 0, 0) + outer.split("\n")[1] + "\n"),
        0, "failed", "ambiguous-summary", 1],
      [cargoFrame(outer), 101, "failed", undefined, 0],
    ]) {
      const { result, record } = await fixture.run(text, { code });
      assert.deepEqual([result.stages[0].state, result.stages[0].code, result.stages[0].signal], [state, code, null]);
      assert.deepEqual([record.stages[0].state, record.stages[0].exit, record.stages[0].why], [state, String(code), why]);
      assert.equal(result.stages[0].tests.failures, failures);
      if (why) assert.equal(record.stages[0].executed, "unknown");
      else assert.equal(record.stages[0].executed, "3");
    }
  });
});

test("Cargo frames preserve outer counts beside killed children without summaries", async () => {
  await repository(async (root) => {
    const outer = libtest(5, 0, 1, 0).replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n");
    for (const marker of ["     Running unittests src/lib.rs (target/debug/deps/fixture)",
      "     Running tests/contract.rs (C:\\build dir\\fixture.exe)"]) {
      const text = `${cargoFrame(outer, marker)}${cargoFrame(libtest(2, 0, 0, 17))}${cargoFrame(libtest(0, 0, 0, 4))}`;
      for (const styled of [text, `\x1b[32m${text.replaceAll("\n", "\r\n")}\x1b[0m`]) {
        assert.deepEqual(await counted(root, styled, { cargoFrames: true }), complete(7, 7, 0, 1, 3, 21));
        assert.equal((await counted(root, styled)).executed, "unknown", "Cargo-looking text alone never opts in");
      }
    }
  });
});

test("Cargo Doc-tests count the captured mixed edition-2024 rustdoc runners", async () => {
  await repository(async (root) => {
    const transcript = readFileSync(new URL("./fixtures/mixed-edition-doctests.txt", import.meta.url), "utf8");
    assert.equal(transcript.match(/^\s*Doc-tests .+$/gmu).length, 1);
    assert.equal(transcript.match(/^running 1 test$/gmu).length, 2);
    assert.equal(transcript.match(/^test result: ok\./gmu).length, 2);
    assert.deepEqual(await counted(root, transcript, { cargoFrames: true }), complete(2, 2, 0, 0, 2, 0));
    // cargo test --doc does not run unit tests; an ordinary Cargo stage does.
    const ordinary = cargoFrame(libtest(0, 0, 0, 0)) + transcript;
    assert.deepEqual(await counted(root, ordinary, { cargoFrames: true }), complete(2, 2, 0, 0, 3, 0));
  });
});

test("Cargo Doc-tests accept sequential pairs, including zero runners and frame transitions", async () => {
  await repository(async (root) => {
    const pairs = libtest(0, 0, 0, 2) + libtest(2, 0, 1, 3) + libtest(2, 0, 0, 4);
    const text = cargoFrame(pairs, "   Doc-tests fixture") + cargoFrame(libtest(1, 0, 0, 5));
    for (const styled of [text, `\x1b[32m${text.replaceAll("\n", "\r\n")}\x1b[0m`, text.trimEnd()]) {
      assert.deepEqual(await counted(root, styled, { cargoFrames: true }), complete(5, 5, 0, 1, 4, 14));
    }
    assert.deepEqual(await counted(root, cargoFrame(libtest(0, 0, 0, 0).repeat(2), "   Doc-tests fixture"),
      { cargoFrames: true }), complete(0, 0, 0, 0, 2, 0));
  });
});

test("Cargo Doc-tests invalidate the whole frame for overlapping or unfinished pairs", async () => {
  await repository(async (root) => {
    const pair = libtest(3, 0, 0, 0), summary = pair.split("\n")[1];
    const max = Number.MAX_SAFE_INTEGER;
    const cases = [
      ["", "incomplete-summary"],
      [summary + "\n", "incomplete-summary"],
      ["running 3 tests\n", "incomplete-summary"],
      [pair + "running 1 test\n", "incomplete-summary"],
      [pair + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\nrunning 1 test\n" + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\nrunning 3 tests\n" + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\n" + libtest(1, 0, 0, 0) + summary + "\n", "ambiguous-summary"],
      [pair + pair.replace("3 passed", "2 passed"), "ambiguous-summary"],
      [pair + pair.replace("0 measured", "1 measured"), "benchmarks"],
      [pair + "running invalid tests\n", "malformed-summary"],
      [pair + "running 1 test\ntest result: ok. 1 passed;", "malformed-summary"],
      [pair + pair.replace("running 3", "running 9007199254740992"), "malformed-summary"],
      [pair + pair.replace("3 passed", "9007199254740992 passed"), "malformed-summary"],
      [pair + "running " + "0".repeat(20_000) + "1 test\n", "malformed-summary"],
      [pair + "test result: " + "x".repeat(20_000) + "\n", "malformed-summary"],
      [pair + nodeSummary("#", { tests: 1, pass: 1 }), "ambiguous-summary"],
      [pair + "# tests invalid\n", "malformed-summary"],
      [pair + "# tests " + "0".repeat(20_000) + "1\n", "malformed-summary"],
      [libtest(max, 0, 0, 0) + libtest(1, 0, 0, 0), "malformed-summary"],
      [libtest(max, 0, 0, 0) + libtest(0, 1, 0, 0), "malformed-summary"],
      [libtest(0, 0, 0, max) + libtest(0, 0, 0, 1), "malformed-summary"],
    ];
    for (const [body, why] of cases) {
      for (const next of ["", cargoFrame(libtest(1, 0, 0, 0))]) {
        const separator = next && body && !body.endsWith("\n") ? "\n" : "";
        const result = await counted(root, cargoFrame(body, "   Doc-tests fixture") + separator + next, { cargoFrames: true });
        assert.equal(result.executed, "unknown", body.slice(0, 200));
        assert.equal(result.why, why, body.slice(0, 200));
        assert.equal(result.runners, next ? 1 : 0,
          "a later defect discards every earlier pair in its frame: " + body.slice(0, 200));
        for (const field of ["passed", "failed", "ignored"]) assert.equal(Object.hasOwn(result, field), false);
      }
    }
    const failing = pair + "running 3 tests\n" + libtest(0, 1, 0, 0) + summary + "\n";
    assert.deepEqual(await counted(root, cargoFrame(failing, "   Doc-tests fixture"), { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
    assert.deepEqual(await counted(root, cargoFrame(pair + nodeSummary("#", { tests: 1, pass: 0, fail: 1 }),
      "   Doc-tests fixture"), { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
  });
});

test("Cargo frames refuse incomplete or ambiguous ownership and retain readable failures", async () => {
  await repository(async (root) => {
    const outer = libtest(3, 0, 0, 0);
    const summary = outer.split("\n")[1];
    const cases = [
      ["running 3 tests\nrunning 1 test\n" + summary + "\n", "incomplete-summary"],
      [cargoFrame("running 3 tests\n"), "incomplete-summary"],
      [cargoFrame("running 3 tests\n") + cargoFrame(outer), "incomplete-summary"],
      [cargoFrame("") + cargoFrame(outer), "incomplete-summary"],
      [cargoFrame("running 3 tests\nrunning 3 tests\n" + summary + "\n"), "ambiguous-summary"],
      [cargoFrame("running 3 tests\nrunning 3 tests\n" + summary + "\n" + summary + "\n"), "ambiguous-summary"],
      [cargoFrame(outer + summary + "\n"), "ambiguous-summary"],
      [cargoFrame("running 3 tests\n" + libtest(1, 0, 0, 0) + summary + "\n"), "ambiguous-summary"],
      [cargoFrame(outer + libtest(1, 0, 0, 0)), "ambiguous-summary"],
      [cargoFrame(outer + "running 1 test\n"), "ambiguous-summary"],
      [cargoFrame(outer.replace("3 passed", "2 passed")), "ambiguous-summary"],
      [cargoFrame(outer.replace("0 measured", "1 measured")), "benchmarks"],
      [cargoFrame(outer.replace("3 passed", "9007199254740992 passed")), "malformed-summary"],
      [cargoFrame(outer.replace("running 3", "running 9007199254740992")), "malformed-summary"],
      [cargoFrame("running invalid tests\n" + summary + "\n"), "malformed-summary"],
      [cargoFrame(outer, "     Running malformed") , "malformed-summary"],
      [cargoFrame(outer, "   Doc-tests"), "malformed-summary"],
      [cargoFrame(outer, "     Running " + "x".repeat(20_000)), "malformed-summary"],
      [cargoFrame("running " + "0".repeat(20_000) + "3 tests\n" + summary + "\n"), "malformed-summary"],
      [cargoFrame(outer.replace("3 passed", "0".repeat(20_000) + "3 passed")), "malformed-summary"],
    ];
    for (const [text, why] of cases) {
      const result = await counted(root, text, { cargoFrames: true });
      assert.equal(result.executed, "unknown", text.slice(0, 200));
      assert.equal(result.why, why, text.slice(0, 200));
      assert.equal(Object.hasOwn(result, "passed"), false);
    }
    const childFailure = cargoFrame("running 3 tests\n" + libtest(0, 1, 0, 0) + summary + "\n");
    assert.deepEqual(await counted(root, childFailure, { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
    // Every field can be safe while the sum across confirmed runners is not.
    const max = Number.MAX_SAFE_INTEGER;
    for (const options of [undefined, { cargoFrames: true }]) {
      const wrap = (text) => options ? cargoFrame(text) : text;
      for (const text of [wrap(libtest(max, 0, 0, 0)) + wrap(libtest(0, 1, 0, 0)),
        wrap(libtest(max, 0, 0, 0)) + wrap(libtest(1, 0, 0, 0))]) {
        const result = await counted(root, text, options);
        assert.equal(result.executed, "unknown");
        assert.equal(result.why, "malformed-summary");
      }
    }
  });
});

test("Cargo phase execution preserves native exits, failure masking and count records", { timeout: 60_000 }, async () => {
  await repository(async (root) => {
    const fixture = rustWrapperFixture(root, process.platform === "win32" ? "win32" : "linux");
    const ordinary = requiredStages()[3];
    const outer = libtest(3, 0, 1, 0);
    const nested = cargoFrame(outer.replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n"));
    const failingChild = cargoFrame("running 4 tests\n" + libtest(0, 1, 0, 0) + outer.split("\n")[1] + "\n");
    const doctests = cargoFrame(libtest(1, 0, 0, 0).repeat(2), "   Doc-tests fixture");
    const doctestFailure = cargoFrame(libtest(1, 0, 0, 0) + libtest(0, 1, 0, 0), "   Doc-tests fixture");
    const doctestOverlap = cargoFrame("running 2 tests\n" + libtest(0, 1, 0, 0), "   Doc-tests fixture");
    for (const [text, code, state, why] of [[nested, 0, "passed", undefined],
      [cargoFrame("running 4 tests\n"), 0, "passed", "incomplete-summary"],
      [failingChild, 0, "failed", "ambiguous-summary"],
      [cargoFrame(libtest(0, 1, 0, 0)), 0, "failed", undefined],
      [nested, 101, "failed", undefined],
      [cargoFrame("running 4 tests\n"), 101, "failed", "incomplete-summary"],
      [doctests, 0, "passed", undefined], [doctests, 101, "failed", undefined],
      [doctestFailure, 0, "failed", undefined], [doctestFailure, 101, "failed", undefined],
      [doctestOverlap, 0, "failed", "ambiguous-summary"],
      [doctestOverlap, 101, "failed", "ambiguous-summary"]]) {
      writeFileSync(join(root, "scripts", "test-temp.mjs"), `process.stdout.write(${JSON.stringify(text)}); process.exitCode = ${code};`);
      const { result, record } = await recordOf(root, [{ ...ordinary, name: "owner-from-command" },
        printing("later", "test", libtest(1, 0, 0, 0))]);
      assert.deepEqual([result.stages[0].state, result.stages[0].code, result.stages[0].signal], [state, code, null]);
      assert.deepEqual([record.stages[0].state, record.stages[0].exit, record.stages[0].why], [state, String(code), why]);
      assert.deepEqual(json(join(dirname(result.stages[0].log), "results.json")).stages[0].tests, result.stages[0].tests);
      assert.equal(result.stages[1].state, state === "failed" ? "unrun" : "passed");
      if (why) {
        assert.equal(record.stages[0].executed, "unknown");
        assert.equal(record.stages[0].passed, undefined);
        assert.equal(countsAsPassedTests(record), false);
      } else assert.equal(record.stages[0].executed, text === nested ? "3"
        : text === doctests || text === doctestFailure ? "2" : "1");
    }
    // The same full-stage name and framed text through another command stay generic.
    const generic = await recordOf(root, [printing("rust", "test", nested)], { full: true });
    assert.equal(generic.record.stages[0].executed, "unknown");
    assert.equal(generic.record.stages[0].why, "incomplete-summary");
  });
});

test("every required stage has a kind, and only runner stages are tests", () => {
  for (const platform of ["win32", "linux"]) {
    assert.deepEqual(requiredStages(platform).map(({ name, kind }) => [name, kind]), [
      ["fmt", "lint"], ["check", "build"], ["clippy", "lint"], ["rust", "test"],
      ["rust-scale-claims", "test"], ["rust-scale-roots", "test"], ["rust-scale-planning", "test"], ["freeze", "test"],
      ["mcp", "test"], ["control", "test"], ["parity", "test"], ["docs", "other"],
    ]);
  }
});

test("a focused command is a test only when the launcher starts the runner itself", () => {
  for (const [command, args, kind] of [
    ["cargo", ["test", "--lib", "name"], "test"],
    ["cargo", ["+stable", "test"], "test"],
    ["C:\\tools\\cargo.exe", ["test"], "test"],
    ["/usr/local/bin/cargo", ["test"], "test"],
    ["cargo", ["clippy", "--all-targets"], "lint"],
    ["cargo", ["fmt", "--check"], "lint"],
    ["cargo", ["check"], "build"],
    ["cargo", ["build", "--release"], "build"],
    ["cargo", ["run", "--", "test"], "other"],
    ["cargo", ["nextest", "run"], "other"],
    [process.execPath, ["--test", "scripts/a.test.mjs"], "test"],
    ["node", ["--test-reporter=tap", "--test", "a.mjs"], "test"],
    ["node", ["--no-warnings", "--test-reporter=tap", "--test"], "test"],
    ["node", ["--test-reporter", "spec", "--test", "a.mjs"], "test"],
    // Each reporter prints a summary of its own.
    ["node", ["--test-reporter=spec", "--test-reporter-destination=stdout", "--test-reporter=tap", "--test-reporter-destination=stderr", "--test"], "other"],
    ["node", ["--test", "--test-reporter", "spec", "--test-reporter=tap", "a.mjs"], "other"],
    ["node", ["--test-reporter=tap", "--test", "a.mjs", "--", "--test-reporter=spec"], "test"],
    ["node", ["scripts/check-doc-links.mjs", "--test"], "other"],
    ["node", ["-e", "console.log('cargo test')"], "other"],
    // `--` ends Node's options: what follows is a script, whatever its name.
    ["node", ["--", "--test"], "other"],
    ["node", ["--import", "./setup.mjs", "run.mjs", "--test"], "other"],
    // An option the table does not know may take `--test` as its value.
    ["node", ["--disable-warning", "--test", "fake.mjs"], "other"],
    ["node", ["--title", "--test", "fake.mjs"], "other"],
    ["node", ["--unknown-option=1", "--test"], "other"],
    // A wrapper may run anything and end as it likes.
    ["npm", ["test"], "other"],
    ["npm", ["run", "test"], "other"],
    ["npx", ["vitest", "run"], "other"],
    ["sh", ["scripts/test-rust.sh"], "other"],
    ["pwsh", ["-NoProfile", "-File", "scripts/test-rust.ps1"], "other"],
    ["echo", ["cargo", "test"], "other"],
  ]) assert.equal(commandKind(command, args), kind, `${command} ${args.join(" ")}`);
});

test("counts are sums over complete runner summaries, including runners that ran nothing", async () => {
  await repository(async (root) => {
    assert.deepEqual(await counted(root, `${libtest(1057, 0, 4, 0)}noise\n${libtest(3, 0, 0, 1066)}`
      + `   Doc-tests engram\n${libtest(0, 0, 0, 0)}`), complete(1060, 1060, 0, 4, 3, 1066));
    assert.deepEqual(await counted(root, libtest(2, 1, 0, 0)), complete(3, 2, 1, 0, 1, 0));
    for (const [mark, opening] of [["ℹ", ""], ["#", "TAP version 13\n"]]) {
      assert.deepEqual(await counted(root, `${opening}✔ a test named tests 5\n${nodeSummary(mark, { tests: 7, pass: 4, fail: 1, skipped: 1, todo: 1 })}`),
        complete(5, 4, 1, 2, 1, 0));
    }
    // Coloured output and Windows line endings are the same summaries.
    assert.deepEqual(await counted(root, `\x1b[32m${libtest(2, 0, 0, 0).replaceAll("\n", "\r\n")}`.replace("running", "\x1b[0mrunning")),
      complete(2, 2, 0, 0, 1, 0));
    assert.deepEqual(await counted(root, libtest(0, 0, 3, 40)), complete(0, 0, 0, 3, 1, 40));
    // A line too long to read is skipped, not interpreted and not held in memory.
    assert.deepEqual(await counted(root, `${"x".repeat(200_000)}test result: ok. 9 passed; 0 failed\n${libtest(2, 0, 0, 0)}`),
      complete(2, 2, 0, 0, 1, 0));
  });
});

test("nothing but a runner's own summary is read: no line is taken for a test or a failure", async () => {
  await repository(async (root) => {
    // What a log may hold beside the summaries: failed tests' own lines, a
    // reporter's list of them, cargo's closing lines, a compiler's or another
    // tool's complaint. The summaries count; none of these lines does.
    for (const line of [
      "test storage::tests::breaks ... FAILED", "not ok 3 - breaks", "not ok 3 - pending # TODO",
      "✖ breaks (1.2ms)", "⚠ pending (0.4ms) # TODO", "✖ failing tests:",
      "✖ 2 problems (0 errors, 2 warnings)", "error: test failed, to rerun pass `--lib`",
      "error: 2 targets failed:", "error[E0425]: cannot find value", "warning: unused variable",
    ]) {
      assert.deepEqual(await counted(root, `${line}\n${libtest(2, 0, 0, 0)}${line}\n`), complete(2, 2, 0, 0, 1, 0), line);
      assert.deepEqual(await counted(root, `${line}\n${nodeSummary("ℹ", { tests: 3, pass: 2, todo: 1 })}${line}\n`),
        complete(2, 2, 0, 1, 1, 0), line);
      assert.deepEqual(await counted(root, `${line}\n`),
        { executed: "unknown", why: "no-summary", runners: 0, filteredOut: 0, failures: 0 }, line);
    }
  });
});

test("a missing, partial, contradictory or unsupported summary makes the count unknown and says why", async () => {
  await repository(async (root) => {
    const unknown = async (text, why, runners, because, failures = 0) => {
      assert.deepEqual(await counted(root, text), { executed: "unknown", why, runners, filteredOut: 0, failures }, because);
    };
    await unknown("", "no-summary", 0, "an empty log");
    await unknown("error[E0425]: cannot find value\nerror: could not compile `engram`\n", "no-summary", 0,
      "compilation failed before any runner");
    await unknown(`${libtest(5, 0, 0, 0)}running 3 tests\ntest a ... ok\n`, "incomplete-summary", 1,
      "a runner that started and never reported");
    await unknown(`running 4 tests\n${libtest(5, 0, 0, 0)}`, "incomplete-summary", 1,
      "a runner that announced tests and never reported");
    await unknown(`${nodeSummary("ℹ", { tests: 2, pass: 2 })}TAP version 13\n# Subtest: unfinished\n`, "incomplete-summary", 1,
      "a TAP stream that started and never summarized");
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2 }).split("\n").slice(0, 4).join("\n"), "incomplete-summary", 0,
      "a node summary cut short");
    await unknown(`${libtest(5, 0, 0, 0)}${nodeSummary("#", { tests: 2, pass: 2 }).replace("# fail 0\n", "")}`, "incomplete-summary", 1,
      "a complete runner does not stand for an incomplete one");
    await unknown("test result: ok. 2 passed; 0 failed\n", "malformed-summary", 0, "a result line cut short");
    await unknown(libtest(5, 0, 0, 0).replace("5 passed", "99999999999999999999 passed"), "malformed-summary", 0,
      "a count that is not an exact integer");
    await unknown(libtest(5, 0, 0, 0).replace("0 measured", "2 measured"), "benchmarks", 0, "benchmarks are not tests");
    await unknown(nodeSummary("ℹ", { tests: 3, pass: 2, cancelled: 1 }), "cancelled-tests", 0,
      "a cancelled test may or may not have run");
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2, cancelled: 1 }), "inconsistent-summary", 0,
      "cancelled tests are part of the total");
    await unknown(nodeSummary("ℹ", { tests: 9, pass: 2 }), "inconsistent-summary", 0, "totals that do not add up");
    for (const mark of ["ℹ", "#"]) {
      await unknown(nodeSummary(mark, { tests: 2, pass: 1, fail: 1 }).replace(`${mark} suites 0`, `${mark} suites 0.5`),
        "malformed-summary", 0, "a suite count that is not an integer", 1);
    }
    // A summary's first line that does not read, before or after a good one.
    for (const mark of ["ℹ", "#"]) {
      const good = nodeSummary(mark, { tests: 1, pass: 1 });
      for (const opener of [`${mark} tests`, `${mark} tests invalid`, `${mark} tests 1 2`, `${mark} tests ${"1".repeat(20_000)}`]) {
        await unknown(`${good}${opener}\n`, "malformed-summary", 1, `${opener.slice(0, 20)} after a summary`);
        await unknown(`${opener}\n${good}`, "malformed-summary", 1, `${opener.slice(0, 20)} before a summary`);
      }
    }
    await unknown(`${libtest(2, 0, 0, 0)}running 1 test\ntest result: ok. ${"1".repeat(20_000)} passed\n`, "malformed-summary", 1,
      "a result line too long to read");
    // Any other line may be as long as it likes.
    assert.equal((await counted(root, `${"x".repeat(20_000)}\n${libtest(2, 0, 0, 0)}tests ${"y".repeat(20_000)}\n`)).executed, 2);
    await unknown("test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n",
      "inconsistent-summary", 0, "a result that no runner announced");
    await unknown(libtest(5, 0, 0, 0).replace("running 5 tests", "running 6 tests"), "inconsistent-summary", 0,
      "a result that does not account for the tests announced");
    // The first reason met is the one given, and a later clean summary does
    // not take it back.
    await unknown(`running 3 tests\n${nodeSummary("ℹ", { tests: 9, pass: 2 })}${libtest(1, 0, 0, 0)}`, "inconsistent-summary", 1,
      "the first of several reasons");
    // Only its length is wrong with this field: read whole, it would be 2.
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2 }).replace("ℹ pass 2", `ℹ pass ${"0".repeat(20_000)}2`),
      "malformed-summary", 0, "a summary field too long to read");
    // The bound holds wherever the line falls in the 8,192-byte chunks read.
    for (const offset of [0, 1, 4_000, 8_191, 8_192, 8_193, 16_000]) {
      const padded = (zeroes) => `${"x".repeat(offset)}\n${nodeSummary("ℹ", { tests: 2, pass: 2 })
        .replace("ℹ pass 2", `ℹ pass ${"0".repeat(zeroes)}2`)}`;
      await unknown(padded(16_384 - "ℹ pass 2".length + 1), "malformed-summary", 0, `one character over the bound at offset ${offset}`);
      assert.equal((await counted(root, padded(16_384 - "ℹ pass 2".length))).executed, 2,
        `exactly at the bound at offset ${offset}`);
    }
    // The failed count of a complete summary is kept whatever became of the rest.
    await unknown(`${libtest(1, 2, 0, 0)}running 1 test\n`, "incomplete-summary", 1, "failures beside a runner that never reported", 2);
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 0, fail: 1, cancelled: 1 }), "cancelled-tests", 0,
      "failures beside cancelled tests", 1);
    await unknown(nodeSummary("#", { tests: 9, pass: 2, fail: 4 }), "inconsistent-summary", 0,
      "failures in a summary that does not add up", 4);
    await unknown(libtest(1, 3, 0, 0).replace("0 measured", "2 measured"), "benchmarks", 0, "failures beside benchmarks", 3);
    await unknown(`${nodeSummary("ℹ", { tests: 9, pass: 2 })}${libtest(2, 3, 0, 0)}`, "inconsistent-summary", 1,
      "failures after a summary that does not add up", 3);
    // Each runner keeps its failed count when that count reads, whatever
    // another field of the same summary says.
    await unknown(libtest(1, 3, 0, 0).replace("1 passed", "99999999999999999999 passed"), "malformed-summary", 0,
      "libtest failures beside a count that is not an exact integer", 3);
    // A log that cannot be read gives no count and throws nothing: the stage
    // keeps its exit.
    for (const unreadable of [join(root, "no-such.log"), root]) {
      assert.deepEqual(await countTests(unreadable), { executed: "unknown", why: "no-summary", runners: 0, filteredOut: 0, failures: 0 },
        unreadable);
    }
  });
});

test("the counts agree with what Node's own runner prints, for each reporter", async () => {
  await repository(async (root) => {
    // An independent runner, not a worker of the one running this file.
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    const run = (name, source, reporter) => {
      const file = join(root, `${name}.test.mjs`);
      writeFileSync(file, `import test from "node:test";\nimport assert from "node:assert/strict";\n${source}`);
      try {
        return spawnSync(process.execPath, ["--test", ...(reporter ? [`--test-reporter=${reporter}`] : []), file],
          { cwd: root, env: runnerEnv, encoding: "utf8", windowsHide: true });
      } finally { unlinkSync(file); }
    };
    // A failing test marked todo is todo to its runner, which exits 0. How a
    // reporter marks it differs between Node versions; its summary does not.
    const todo = `test("passes", () => {});
      test("pending", { todo: true }, () => { assert.equal(1, 2); });
      test("pending with a reason", { todo: "not written yet" }, () => { throw new Error("no"); });
      test("left out", { skip: true }, () => {});`;
    const failing = `test("passes", () => {});
      test("breaks", () => { assert.equal(1, 2); });`;
    for (const reporter of [undefined, "spec", "tap"]) {
      const passed = run("todo", todo, reporter);
      assert.equal(passed.status, 0, `${reporter}: ${passed.stderr}`);
      assert.deepEqual(await counted(root, passed.stdout), complete(1, 1, 0, 3, 1, 0), `${reporter}\n${passed.stdout}`);
      const failed = run("failing", failing, reporter);
      assert.equal(failed.status, 1, `${reporter}: ${failed.stderr}`);
      assert.deepEqual(await counted(root, failed.stdout), complete(2, 1, 1, 0, 1, 0), `${reporter}\n${failed.stdout}`);
      // Without its summary a run gives no count, and no failure is guessed.
      const cut = failed.stdout.split("\n").filter((line) => !/^(?:ℹ|#) /u.test(line)).join("\n");
      const partial = await counted(root, cut);
      assert.equal(partial.executed, "unknown", reporter);
      assert.equal(partial.failures, 0, `${reporter}\n${cut}`);
    }
    // TAP prints what a test writes behind `# `, which gives a line that
    // begins with `tests` the beginning of a summary: no count. The default
    // reporter prints the line as it is.
    const talking = `test("passes", () => { console.log("tests passed"); });`;
    for (const [reporter, expected] of [["tap", { executed: "unknown", why: "malformed-summary", runners: 1, filteredOut: 0, failures: 0 }],
      ["spec", complete(1, 1, 0, 0, 1, 0)]]) {
      const printed = run("talking", talking, reporter);
      assert.equal(printed.status, 0, `${reporter}: ${printed.stderr}`);
      assert.match(printed.stdout, reporter === "tap" ? /^# tests passed$/mu : /^tests passed$/mu, reporter);
      assert.deepEqual(await counted(root, printed.stdout), expected, `${reporter}\n${printed.stdout}`);
    }
  });
});

test("the record follows the agreed grammar for a passing run of several kinds", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("lint-first", "lint", "test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s\n"),
      printing("rust", "test", `${libtest(10, 0, 1, 0)}${libtest(2, 0, 0, 30)}`),
      printing("node-tests", "test", nodeSummary("ℹ", { tests: 4, pass: 4 })),
      printing("unclassified", undefined, ""),
    ], { full: true });
    assert.equal(result.state, "passed");
    const bare = { executed: undefined, passed: undefined, failed: undefined, ignored: undefined, why: undefined,
      runners: undefined, filtered: undefined };
    assert.deepEqual(record, { run: result.runId, state: "passed", scope: "full", reason: undefined, stages: [
      { run: result.runId, name: "lint-first", kind: "lint", state: "passed", exit: "0", ...bare },
      { run: result.runId, name: "rust", kind: "test", state: "passed", exit: "0", executed: "12",
        passed: "12", failed: "0", ignored: "1", why: undefined, runners: "2", filtered: "yes" },
      { run: result.runId, name: "node-tests", kind: "test", state: "passed", exit: "0", executed: "4",
        passed: "4", failed: "0", ignored: "0", why: undefined, runners: "1", filtered: "no" },
      { run: result.runId, name: "unclassified", kind: "other", state: "passed", exit: "0", ...bare },
    ] });
    assert.equal(countsAsPassedTests(record), true);
    // Counts are kept with the results, not only printed.
    assert.deepEqual(json(join(dirname(result.stages[1].log), "results.json")).stages[1].tests, complete(12, 12, 0, 1, 2, 30));
  });
});

test("a command that exits 0 passes, and a count that is not complete keeps the run from counting", async () => {
  await repository(async (root) => {
    // Each of these stages ends with exit 0 beside a stage with a good count.
    // The launcher calls none of them failed: it cannot read them. The host's
    // rule asks every test stage for a complete count, so none of these runs
    // counts as passed tests.
    for (const [text, why] of [
      ["finished\n", "no-summary"],
      [`${libtest(2, 0, 0, 0)}running 1 test\ntest breaks ... FAILED\n`, "incomplete-summary"],
      [`TAP version 13\nnot ok 1 - breaks\nTAP version 13\n${nodeSummary("#", { tests: 1, pass: 1 })}`, "incomplete-summary"],
      [`✖ breaks (1ms)\n${nodeSummary("ℹ", { tests: 1, pass: 1 }).split("\n").slice(0, 4).join("\n")}\n`, "incomplete-summary"],
      [nodeSummary("ℹ", { tests: 3, pass: 2, cancelled: 1 }), "cancelled-tests"],
    ]) {
      const { result, record } = await recordOf(root, [printing("unread", "test", text), printing("good", "test", libtest(3, 0, 0, 0))]);
      assert.equal(result.state, "passed", why);
      assert.equal(record.state, "passed", why);
      assert.deepEqual(record.stages.map(({ state, exit, executed, why: given, passed }) => [state, exit, executed, given, passed]),
        [["passed", "0", "unknown", why, undefined], ["passed", "0", "3", undefined, "3"]], why);
      assert.equal(countsAsPassedTests(record), false, why);
    }
    // Tests that ran and were all ignored or filtered out are counted, as none.
    const none = await recordOf(root, [printing("all-ignored", "test", libtest(0, 0, 6, 0)), printing("no-match", "test", libtest(0, 0, 0, 1070))]);
    assert.deepEqual(none.record.stages.map(({ executed, passed, runners, filtered }) => [executed, passed, runners, filtered]),
      [["0", "0", "1", "no"], ["0", "0", "1", "yes"]]);
    assert.equal(countsAsPassedTests(none.record), false, "no test was executed");
    // Another tool's line that looks like a failure fails nothing.
    const linted = await recordOf(root, [printing("with-lint", "test",
      `${nodeSummary("ℹ", { tests: 2, pass: 1, todo: 1 })}✖ failing tests:\n✖ pending (1ms) # TODO\n✖ 2 problems (0 errors, 2 warnings)\n`)]);
    assert.equal(linted.record.state, "passed");
    assert.equal(countsAsPassedTests(linted.record), true);
  });
});

test("complete summaries that count failed tests fail the stage whatever the command's exit", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("masked", "test", nodeSummary("ℹ", { tests: 1, pass: 0, fail: 1 }), 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "stage-failed");
    // The exit the process gave is kept; the state is what the runner counted.
    assert.deepEqual(record.stages.map(({ state, exit, executed, passed, failed, why }) => [state, exit, executed, passed, failed, why]),
      [["failed", "0", "1", "0", "1", undefined], ["skipped", "none", "unknown", undefined, undefined, "not-run"]]);
    assert.match(await summarize(dirname(result.stages[0].log)), /^FAIL /u);
    assert.match(await inspectFailure(dirname(result.stages[0].log), result.stages[0].name), /the runners' summaries count 1 failed tests and the command exited 0/u);
    assert.equal(countsAsPassedTests(record), false);

    // The same when another runner of the stage left no complete summary.
    const partial = await recordOf(root, [
      printing("masked", "test", `${libtest(1, 1, 0, 0)}running 1 test\n`, 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(partial.record.state, "failed");
    assert.equal(partial.record.reason, "stage-failed");
    assert.deepEqual(partial.record.stages.map(({ state, exit, executed, why, runners }) => [state, exit, executed, why, runners]),
      [["failed", "0", "unknown", "incomplete-summary", "1"], ["skipped", "none", "unknown", "not-run", "0"]]);

    // And when the summary that counts the failure counts cancelled tests too.
    const cancelled = await recordOf(root, [
      printing("masked", "test", nodeSummary("ℹ", { tests: 2, pass: 0, fail: 1, cancelled: 1 }), 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(cancelled.record.reason, "stage-failed");
    assert.deepEqual(cancelled.record.stages.map(({ state, exit, executed, why }) => [state, exit, executed, why]),
      [["failed", "0", "unknown", "cancelled-tests"], ["skipped", "none", "unknown", "not-run"]]);
    assert.equal(countsAsPassedTests(cancelled.record), false);
  });
});

test("a stage that ran and was never counted is not printed as not run", () => {
  const plan = { full: false, stages: [{ name: "ran", kind: "test" }, { name: "killed", kind: "test" }, { name: "waiting", kind: "test" }] };
  const record = parseRecord(machineRecord(plan, { runId: "test-0a1b", state: "failed", reason: "launcher-error", stages: [
    { name: "ran", state: "passed", code: 0 },
    { name: "killed", state: "failed", code: null, signal: "SIGTERM" },
    { name: "waiting", state: "unrun" },
  ] }));
  assert.deepEqual(record.stages.map(({ state, exit, executed, why }) => [state, exit, executed, why]),
    [["passed", "0", "unknown", "no-summary"], ["interrupted", "none", "unknown", "no-summary"], ["skipped", "none", "unknown", "not-run"]]);
  assert.equal(countsAsPassedTests(record), false);
});

test("filtered says whether the command line or a runner selected a part of the tests", async () => {
  for (const [command, args, selects] of [
    ["cargo", ["test"], false],
    ["cargo", ["test", "--all-features", "--", "--nocapture"], false],
    ["cargo", ["test", "--jobs", "1"], false],
    ["cargo", ["test", "-j8"], false],
    ["cargo", ["test", "-j", "8"], false],
    ["cargo", ["test", "--jobs=1", "--features", "x", "--", "--test-threads", "8"], false],
    ["cargo", ["test", "root_delta"], true],
    ["cargo", ["test", "--lib"], true],
    ["cargo", ["test", "-p", "engram"], true],
    ["cargo", ["test", "-pengram"], true],
    ["cargo", ["test", "--package=engram"], true],
    // Every target, but not the doctests.
    ["cargo", ["test", "--all-targets"], true],
    ["cargo", ["test", "--", "--ignored"], true],
    ["cargo", ["test", "--", "--skip", "slow"], true],
    ["cargo", ["clippy", "--lib"], false],
    ["node", ["--test"], false],
    ["node", ["--test", "--test-reporter", "spec"], false],
    ["node", ["--test-reporter=spec", "--test", "--test-timeout=5000"], false],
    ["node", ["--test", "scripts/parity.test.mjs"], true],
    ["node", ["--test", "--test-name-pattern=record"], true],
    ["node", ["--test-name-pattern", "record", "--test"], true],
    ["node", ["scripts/check-doc-links.mjs"], false],
    // Not a test to the launcher, so it selects none.
    ["npm", ["test", "--", "--grep", "x"], false],
  ]) assert.equal(selectsTests(command, args), selects, `${command} ${args.join(" ")}`);
  await repository(async (root) => {
    // Known from the request alone, so a stage that never ran still says it.
    const runDir = createRun({ root, stages: [
      { name: "absent", kind: "test", command: join(root, "no-such-cargo", "cargo"), args: ["test", "--lib", "name"] },
    ] }, env);
    const executed = await executeRun(runDir, env);
    const record = parseRecord(machineRecord(executed.plan, executed));
    assert.deepEqual(record.stages.map(({ state, executed: count, why, filtered }) => [state, count, why, filtered]),
      [["skipped", "unknown", "not-run", "yes"]]);
  });
  // The gate's own stages say it themselves, run or not, whatever runs them.
  for (const platform of ["win32", "linux"]) {
    const stages = requiredStages(platform);
    const record = parseRecord(machineRecord({ full: true, stages },
      { runId: "test-0a1b", state: "failed", reason: "preflight-failed", stages: stages.map(({ name }) => ({ name, state: "unrun" })) }));
    assert.deepEqual(record.stages.filter(({ kind }) => kind === "test").map(({ name, why, filtered }) => [name, why, filtered]),
      [["rust", "not-run", "no"], ["rust-scale-claims", "not-run", "yes"], ["rust-scale-roots", "not-run", "yes"],
        ["rust-scale-planning", "not-run", "yes"], ["freeze", "not-run", "yes"], ["mcp", "not-run", "yes"],
        ["control", "not-run", "yes"], ["parity", "not-run", "yes"]]);
  }
});

test("a failing stage keeps its exit and counts, and the stages after it are skipped without an exit", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("before", "build", ""),
      printing("failing", "test", libtest(3, 2, 0, 0), 101),
      printing("after", "test", libtest(1, 0, 0, 0)),
      printing("last", "other", ""),
    ]);
    assert.equal(result.state, "failed");
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "stage-failed");
    assert.deepEqual(record.stages.map(({ state, exit, executed, failed, why }) => [state, exit, executed, failed, why]), [
      ["passed", "0", undefined, undefined, undefined], ["failed", "101", "5", "2", undefined],
      ["skipped", "none", "unknown", undefined, "not-run"], ["skipped", "none", undefined, undefined, undefined],
    ]);
  });
});

test("a run whose input changed is failed in the record although every stage passed", async () => {
  await repository(async (root) => {
    const { runDir, result, record } = await recordOf(root, [{ name: "changes-source", kind: "test", command: process.execPath, args: ["-e",
      `require('node:fs').writeFileSync('tracked.txt', 'changed\\n'); process.stdout.write(${JSON.stringify(libtest(4, 0, 0, 0))})`] }]);
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "input-changed");
    assert.deepEqual(record.stages.map(({ state, executed }) => [state, executed]), [["passed", "4"]]);
    assert.equal(countsAsPassedTests(record), false);
    const summary = await summarize(runDir);
    assert.deepEqual(summary.split("\n").slice(0, 3), [
      `FAIL ${basename(runDir)} exit=${result.exitCode}`,
      `results: ${join(runDir, "results.json")}`,
      `changes-source: passed exit=0 log=${join(runDir, "changes-source.log")}`,
    ]);
    assert.match(summary, /^reason: input-changed$/mu);
    assert.equal(completionMetadata(json(join(runDir, "request.json")), result).input.comparison, "mismatch");
  });
});

test("a command that cannot be found or spawned leaves stages without an exit and says why", async () => {
  await repository(async (root) => {
    const missing = await recordOf(root, [printing("first", "test", libtest(1, 0, 0, 0)),
      { name: "absent", kind: "test", command: join(root, "no-such-program"), args: [] }]);
    assert.equal(missing.record.state, "failed");
    assert.equal(missing.record.reason, "preflight-failed");
    assert.deepEqual(missing.record.stages.map(({ state, exit, why }) => [state, exit, why]),
      [["skipped", "none", "not-run"], ["skipped", "none", "not-run"]]);

    const unspawnable = await recordOf(root, [
      { name: "invalid-argument", kind: "test", command: process.execPath, args: ["\0"] }, printing("later", "lint", "")]);
    assert.equal(unspawnable.record.state, "failed");
    assert.equal(unspawnable.record.reason, "spawn-failed");
    assert.deepEqual(unspawnable.record.stages.map(({ state, exit, why }) => [state, exit, why]),
      [["failed", "none", "not-run"], ["skipped", "none", undefined]]);

    const prerequisite = createRun({ root, stages: [printing("unused", "test", "")], requiredBinaryEnv: ["ENGRAM_LAUNCHER_ABSENT_BIN"] }, env);
    const refused = await executeRun(prerequisite, env);
    assert.equal(parseRecord(machineRecord(refused.plan, refused)).reason, "preflight-failed");

    // A toolchain the full gate cannot find is a failed prerequisite, not a stage.
    const unstartable = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: process.execPath, PATH: "", Path: "" };
    const probed = createRun({ root, stages: [printing("unused", "test", "")], full: true }, env);
    const stopped = await executeRun(probed, unstartable);
    const probedRecord = parseRecord(machineRecord(stopped.plan, stopped));
    assert.equal(probedRecord.reason, "preflight-failed");
    assert.deepEqual(probedRecord.stages.map(({ state, exit }) => [state, exit]), [["skipped", "none"]]);
  });
});

test("any other refusal and a fingerprint that cannot be checked each have their own reason", async () => {
  await repository(async (root) => {
    const duplicated = await recordOf(root, [printing("twice", "test", ""), printing("twice", "test", "")]);
    assert.equal(duplicated.record.state, "failed");
    assert.equal(duplicated.record.reason, "launcher-error");
    assert.deepEqual(duplicated.record.stages.map(({ state }) => state), ["skipped", "skipped"]);

    const misnamed = createRun({ root, stages: [printing("Upper_Case", "test", "")] }, env);
    const refused = await executeRun(misnamed, env);
    assert.match(refused.error, /lowercase letters, digits and hyphens/u);
    assert.equal(machineRecord(refused.plan, refused), "",
      "a name outside the grammar is refused before it runs and is never printed");

    const unreadable = await recordOf(root, [{ name: "breaks-git", kind: "test", command: process.execPath, args: ["-e",
      `require('node:fs').writeFileSync('.git/HEAD', 'not a ref\\n'); process.stdout.write(${JSON.stringify(libtest(2, 0, 0, 0))})`] }]);
    assert.equal(unreadable.record.state, "failed");
    assert.equal(unreadable.record.reason, "fingerprint-check-failed");
    assert.deepEqual(unreadable.record.stages.map(({ state, executed }) => [state, executed]), [["passed", "2"]]);
  });
});

test("a child that a signal ended is an interrupted stage", { skip: process.platform === "win32" }, async () => {
  await repository(async (root) => {
    const { record } = await recordOf(root, [
      { name: "killed", kind: "test", command: process.execPath, args: ["-e", "process.kill(process.pid, 'SIGKILL')"] },
      printing("later", "lint", ""),
    ]);
    assert.equal(record.state, "interrupted");
    assert.equal(record.reason, "stage-interrupted");
    assert.deepEqual(record.stages.map(({ state, exit }) => [state, exit]), [["interrupted", "none"], ["skipped", "none"]]);
  });
});

test("a stage ended by a signal is interrupted, and nothing about such a run is passed", () => {
  const request = { full: true, stages: [{ kind: "lint" }, { kind: "test" }, { kind: "test" }] };
  const record = parseRecord(machineRecord(request, { runId: "test-0a1b", state: "failed", stages: [
    { name: "fmt", state: "passed", code: 0, signal: null },
    { name: "rust", state: "failed", code: null, signal: "SIGKILL", tests: complete(7, 7, 0, 0, 1, 0) },
    { name: "mcp", state: "unrun" },
  ] }));
  assert.equal(record.state, "interrupted");
  assert.equal(record.reason, "stage-interrupted");
  assert.deepEqual(record.stages.map(({ state, exit }) => [state, exit]),
    [["passed", "0"], ["interrupted", "none"], ["skipped", "none"]]);
  assert.equal(countsAsPassedTests(record), false);
  // A reason or a why that is not a token is replaced, never printed.
  const odd = parseRecord(machineRecord({ stages: [{ kind: "test" }] },
    { runId: "test-0a1b", state: "failed", reason: "Not A Token", stages: [{ name: "only", state: "failed", code: 1,
      tests: { executed: "unknown", why: "Not A Token", runners: 0, filteredOut: 0 } }] }));
  assert.equal(odd.reason, "stage-failed");
  assert.deepEqual(odd.stages.map(({ why }) => why), ["no-summary"]);
});

test("a run in a linked worktree lives under that worktree's Git directory and prints the same record", async () => {
  await repository(async (root) => {
    git(["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "baseline"], root);
    const linked = join(root, "linked");
    git(["worktree", "add", "--quiet", linked], root);
    const runDir = createRun({ root: linked, stages: [printing("unit", "test", libtest(2, 0, 1, 0))] }, env);
    assert.match(runDir.replaceAll("\\", "/"), /\/\.git\/worktrees\/linked\/review-runs\/test-[0-9a-f-]+$/u, runDir);
    const result = await executeRun(runDir, env);
    const record = parseRecord(machineRecord(result.plan, result));
    assert.equal(record.run, basename(runDir));
    assert.equal(record.state, "passed");
    assert.deepEqual(record.stages.map(({ kind, state, executed, passed, ignored }) => [kind, state, executed, passed, ignored]),
      [["test", "passed", "2", "2", "1"]]);
    assert.ok(countsAsPassedTests(record));
  });
});

test("a stage whose log cannot be read keeps its exit, gives no count and does not fail the run", async () => {
  await repository(async (root) => {
    // The stage deletes its own log and exits 0.
    const deleting = `const fs = require('node:fs'), path = require('node:path');
      process.stdout.write(${JSON.stringify(libtest(2, 0, 0, 0))});
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) fs.rmSync(path.join(runs, run, 'gone.log'), { force: true });`;
    const runDir = createRun({ root, stages: [{ name: "gone", kind: "test", command: process.execPath, args: ["-e", deleting] },
      printing("after", "test", libtest(1, 0, 0, 0))] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(existsSync(join(runDir, "gone.log")), false, "the stage removed its log");
    const record = parseRecord(machineRecord(result.plan, result));
    assert.equal(record.state, "passed", "an unreadable log does not fail the run");
    assert.deepEqual(record.stages.map(({ name, state, exit, executed, why }) => [name, state, exit, executed, why]),
      [["gone", "passed", "0", "unknown", "no-summary"], ["after", "passed", "0", "1", undefined]]);
    assert.equal(countsAsPassedTests(record), false, "the host still asks every test stage for a count");
    assert.match(json(join(runDir, "results.json")).stages[0].diagnostics.text, /the log could not be read/u);
    for (const unreadable of [join(root, "no-such.log"), root]) {
      assert.match((await diagnostics(unreadable, true)).text, /^\[the log could not be read: [A-Z]+\]\n$/u, unreadable);
    }
  });
});

test("full Node provenance keeps the stage-kind contract", async () => {
  await repository(async (root) => {
    writeFileSync(join(root, "provenance.test.mjs"), `import test from "node:test";
test("passes", () => {});
test("skipped", { skip: true }, () => {});
`);
    git(["add", "provenance.test.mjs"], root);
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    for (const full of [false, true]) {
      const stages = [{ name: "node-tests", kind: "test", command: process.execPath,
        args: ["--test", "--test-reporter=tap", "provenance.test.mjs"] }];
      const runDir = createRun({ root, full, stages }, runnerEnv);
      const result = await executeRun(runDir, runnerEnv);
      assert.equal(result.state, "passed", JSON.stringify(result));
      assert.equal(json(join(runDir, "request.json")).full, full);
      const saved = json(join(runDir, "results.json"));
      const entry = saved.stages[0];
      assert.equal(saved.state, "passed");
      assert.equal(saved.exitCode, 0);
      assert.equal(entry.state, "passed");
      assert.equal(entry.code, 0);
      assert.equal(entry.signal, null);
      assert.equal(Object.hasOwn(entry.tests, "runner"), !full, "full-mode counts omit the runner label");
      assert.deepEqual(entry.tests, { ...complete(1, 1, 0, 1, 1, 0), ...(!full ? { runner: "node-test" } : {}) });
    }
  });
});

test("focused Node provenance preserves mode, counts and native outcomes", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    const cases = [
      ["passing", `test("passes", () => assert.equal(process.cwd(), ${JSON.stringify(root)})); test("skipped", { skip: true }, () => {});`, "tap", 0, [1, 0, 1]],
      ["failing", `test("breaks", () => { throw new Error("fixture failure"); });`, "spec", 1, [0, 1, 0]],
      ["skipped", `test("skipped", { skip: true }, () => {});`, "tap", 0, [0, 0, 1]],
      ["unknown", `test("passes", () => {});`, "dot", 0, null],
    ];
    for (const [name, source] of cases) {
      writeFileSync(join(root, `${name}.test.mjs`), `import test from "node:test";\nimport assert from "node:assert/strict";\n${source}\n`);
    }
    git(["add", "scripts", "*.test.mjs"], root);
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    const run = (...args) => spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), ...args],
      { cwd: root, env: runnerEnv, encoding: "utf8", windowsHide: true });
    for (const [name, , reporter, code, counts] of cases) {
      const launched = run("focused", "--", process.execPath, "--test", `--test-reporter=${reporter}`, `${name}.test.mjs`);
      assert.equal(launched.status, code, `${name}: ${launched.stderr}`);
      const runDir = /^STARTED (.+)$/mu.exec(launched.stdout)?.[1];
      assert.ok(runDir, launched.stdout);
      assert.equal(json(join(runDir, "request.json")).detached, false, name);
      const result = json(join(runDir, "results.json"));
      const entry = result.stages[0];
      assert.equal(entry.cwd, root, name);
      assert.equal(isAbsolute(entry.cwd), true, name);
      assert.equal(entry.code, code, name);
      assert.equal(entry.signal, null, name);
      assert.equal(result.exitCode, code, name);
      if (counts) {
        assert.equal(entry.tests.runner, "node-test", name);
        assert.deepEqual([entry.tests.passed, entry.tests.failed, entry.tests.ignored], counts, name);
        assert.equal(entry.tests.executed, counts[0] + counts[1], name);
        assert.equal(code === 0, entry.tests.failed === 0, name);
      } else {
        assert.equal(entry.tests.executed, "unknown", name);
        assert.equal(Object.hasOwn(entry.tests, "runner"), false, name);
        for (const field of ["passed", "failed", "ignored"]) assert.equal(Object.hasOwn(entry.tests, field), false, field);
      }
      const record = parseRecord(launched.stdout.slice(launched.stdout.indexOf("\ntest-launcher/v1") + 1));
      assert.equal(record.stages[0].exit, String(code), name);
      assert.equal(countsAsPassedTests(record), name === "passing", name);
    }
    const opaque = run("focused", "--", process.execPath, "-e",
      `process.stdout.write(${JSON.stringify(nodeSummary("#", { tests: 1, pass: 1 }))})`);
    assert.equal(opaque.status, 0, opaque.stderr);
    const opaqueDir = /^STARTED (.+)$/mu.exec(opaque.stdout)?.[1];
    assert.equal(Object.hasOwn(json(join(opaqueDir, "results.json")).stages[0], "tests"), false);
    // Notification does not make a foreground run detached. The fixture CLI
    // refuses the mailbox arguments; validation still finished successfully.
    const notified = run("focused", "--notify", "fixture-parent", "--", process.execPath, "--test", "passing.test.mjs");
    assert.equal(notified.status, 1);
    assert.match(notified.stderr, /notification failed; tests were NOT rerun/u);
    const notifiedDir = /^STARTED (.+)$/mu.exec(notified.stdout)?.[1];
    assert.equal(json(join(notifiedDir, "request.json")).detached, false);
    assert.equal(json(join(notifiedDir, "results.json")).stages[0].tests.runner, "node-test");
  });
});

test("detached launch refuses a foreground request without changing it", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, notifyTo: "fixture-parent", stages: [stage("unused")] }, env);
    assert.equal(json(join(runDir, "request.json")).detached, false);
    const before = directoryBytes(runDir);
    await assert.rejects(startDetached(runDir, env), /requires a run created with detached: true/u);
    assert.deepEqual(directoryBytes(runDir), before);
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
  });
});

test("the record ends the executing process's stdout, and summary and notify never print one", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    const copied = join(root, "scripts", "test-launcher.mjs");
    git(["add", "scripts"], root);
    const run = (...args) => spawnSync(process.execPath, [copied, ...args], { cwd: root, env, encoding: "utf8", windowsHide: true });
    // A warning on stderr and a bounded diagnostic excerpt on stdout precede it.
    const source = `console.error('warning: noise on stderr'); process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))})`;
    const passed = run("focused", "--", process.execPath, "--test-reporter=spec", "-e", source);
    assert.equal(passed.status, 0, passed.stderr);
    const runDir = /^STARTED (.+)$/mu.exec(passed.stdout)?.[1];
    assert.ok(runDir);
    const human = await summarize(runDir);
    assert.ok(passed.stdout.includes(human), "the human summary is unchanged and still printed");
    const tail = passed.stdout.slice(passed.stdout.indexOf(human) + human.length);
    const record = parseRecord(tail);
    assert.equal(record.run, basename(runDir));
    assert.equal(record.state, "passed");
    assert.equal(record.scope, "focused");
    // `node -e` is not the test runner, whatever it prints.
    assert.deepEqual(record.stages.map(({ name, kind, executed }) => [name, kind, executed]), [["focused", "other", undefined]]);

    // A stage can write to its run directory. What the record says of the
    // stage is what the launcher read before the stage ran.
    const rewriting = `const fs = require('node:fs'), path = require('node:path');
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) {
        const file = path.join(runs, run, 'request.json');
        const request = JSON.parse(fs.readFileSync(file, 'utf8'));
        request.full = true;
        for (const stage of request.stages) Object.assign(stage, { kind: 'test', selects: true });
        fs.writeFileSync(file, JSON.stringify(request));
      }
      process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))});`;
    const rewritten = run("focused", "--", process.execPath, "-e", rewriting);
    assert.equal(rewritten.status, 0, rewritten.stderr);
    const unchanged = parseRecord(rewritten.stdout.slice(rewritten.stdout.indexOf("\ntest-launcher/v1") + 1));
    assert.equal(unchanged.scope, "focused");
    assert.deepEqual(unchanged.stages.map(({ kind, executed, filtered }) => [kind, executed, filtered]), [["other", undefined, undefined]]);

    const failed = run("focused", "--", process.execPath, "-e", "process.exit(7)");
    assert.equal(failed.status, 7);
    const failure = parseRecord(failed.stdout.slice(failed.stdout.lastIndexOf("\ntest-launcher/v1", failed.stdout.lastIndexOf("\ntest-launcher/v1") - 1) + 1));
    assert.equal(failure.state, "failed");
    assert.deepEqual(failure.stages.map(({ state, exit }) => [state, exit]), [["failed", "7"]]);

    const replayed = run("summary", runDir);
    assert.equal(replayed.status, 0);
    assert.equal(replayed.stdout, human);
    assert.doesNotMatch(replayed.stdout, /test-launcher\/v1/u);

    // The fixture's transport is `node`, which refuses the mailbox arguments:
    // the notification is attempted and fails, and neither the executing run
    // nor a later `notify` lets that change or repeat the record.
    const notified = run("focused", "--notify", "coordinator", "--", process.execPath, "-e", "");
    assert.equal(notified.status, 1);
    assert.match(notified.stderr, /notification failed; tests were NOT rerun/u);
    const kept = parseRecord(notified.stdout.slice(notified.stdout.indexOf("\ntest-launcher/v1") + 1));
    assert.equal(kept.state, "passed", "the record covers validation, not the notification");
    const notifiedRun = /^STARTED (.+)$/mu.exec(notified.stdout)?.[1];
    assert.doesNotMatch(readFileSync(join(notifiedRun, "notification.message.txt"), "utf8"), /test-launcher\/v1/u);
    const resent = run("notify", notifiedRun);
    assert.equal(resent.status, 1);
    assert.doesNotMatch(resent.stdout, /test-launcher\/v1/u);

    // A stage that breaks request.json cannot take the record away. Last,
    // because it breaks every run's request in this fixture.
    const breaking = `const fs = require('node:fs'), path = require('node:path');
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) fs.writeFileSync(path.join(runs, run, 'request.json'), '{');
      process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))});`;
    const broken = run("focused", "--", process.execPath, "-e", breaking);
    assert.equal(broken.status, 0, broken.stderr);
    const survived = parseRecord(broken.stdout.slice(broken.stdout.lastIndexOf("\ntest-launcher/v1", broken.stdout.lastIndexOf("\ntest-launcher/v1") - 1) + 1));
    assert.equal(survived.state, "passed");
    assert.deepEqual(survived.stages.map(({ state, exit }) => [state, exit]), [["passed", "0"]]);
  });
});

// The host the recover unit tests recover from: the Windows system their
// records describe, whatever system runs them.
const fixtureHost = { platform: "win32", hostname: "fixture-host", pidNamespace: null };
// Whether this system's identity query answers for a living process.
const identifies = ["win32", "linux", "darwin"].includes(process.platform);

// Every file of a run directory, by name, as bytes.
const directoryBytes = (runDir) => Object.fromEntries(readdirSync(runDir).sort()
  .map((name) => [name, readFileSync(join(runDir, name))]));

// Rewrites a run's results.json as `change` returns it.
const rewriteResults = (runDir, change) => {
  const path = join(runDir, "results.json");
  writeFileSync(path, JSON.stringify(change(json(path)), null, 2));
};

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
    const runDir = createRun({ root, stages: [stage("clean")] }, env);
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
    const runDir = createRun({ root, stages: [stage("first"), stage("second")] }, env);
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
      const runDir = createRun({ root, stages: stages.map(({ name }) => stage(name)) }, env);
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
      const terminal = createRun({ root, stages: [stage("only")] }, env);
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
    const runDir = createRun({ root, stages: [stage("only")] }, env);
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
    const runDir = createRun({ root, stages: [stage("only")] }, env);
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
    const runDir = createRun({ root, stages: [stage("only")] }, env);
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
function parkingBroker(t, root) {
  // Both processes resolve this short socket path from the same fixture cwd.
  const address = process.platform === "win32" ? `\\\\.\\pipe\\engram-${randomUUID()}` : ".git/stage.sock";
  const broker = spawn(process.execPath, ["-e", `
    const sockets = new Set();
    const server = require('node:net').createServer(socket => {
      sockets.add(socket);
      socket.on('close', () => { sockets.delete(socket); process.send('closed'); });
      socket.on('error', () => socket.destroy());
      process.send('connected');
    });
    process.on('message', message => { if (message === 'release') for (const socket of sockets) socket.write('x'); });
    process.on('disconnect', () => { for (const socket of sockets) socket.destroy(); server.close(); });
    server.listen(${JSON.stringify(address)}, () => process.send('listening'));
  `], { cwd: root, env, windowsHide: true, stdio: ["ignore", "ignore", "ignore", "ipc"] });
  const exited = once(broker, "exit");
  const gone = exited.then(([code]) => [`broker exited (${code})`]);
  const next = () => Promise.race([once(broker, "message", { signal: t.signal }), gone]);
  const parked = `const socket = require('node:net').connect(${JSON.stringify(address)});
    socket.on('data', () => process.exit(0));
    socket.on('close', () => process.exit(0));
    socket.on('error', () => process.exit(1));`;
  const close = async () => {
    if (broker.connected) broker.send("release");
    if (broker.connected) broker.disconnect();
    await exited;
  };
  return { address, next, parked, close, release: () => broker.connected && broker.send("release") };
}

test("a launcher stopped during a Rust scale phase preserves the prior result without rerunning phases", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let child, completion;
    try {
      assert.equal((await broker.next())[0], "listening");
      const stages = requiredStages().slice(3, 7).map((entry, index) => ({ ...entry, command: process.execPath,
        args: ["-e", index === 1 ? broker.parked : "console.log('running 1 test'); console.log('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s')"] }));
      const runDir = createRun({ root, full: true, stages }, env);
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
      const runDir = createRun({ root, stages: [{ name: "parked", command: process.execPath, args: ["-e", broker.parked] }] },
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

test("stop addresses this worktree's run id, refuses unsupported runs, and retains the first intent", async () => {
  await repository(async (root) => {
    const ask = (runId, actor = "operator-a", reason = "superseded input") => requestStop(runId, { root, actor, reason });
    await assert.rejects(ask("../outside"), /run id/u);
    await assert.rejects(ask(`test-${randomUUID()}`), /does not exist/u);
    const unsupported = createRun({ root, stages: [stage("unused")] }, env);
    const before = directoryBytes(unsupported);
    await assert.rejects(ask(basename(unsupported)), /no owned Windows job/u);
    assert.deepEqual(directoryBytes(unsupported), before);
    const runDir = createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
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
      const runDir = createRun({ root, stages: [stage("never")], notifyTo: "fixture-parent" }, env);
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
    const runDir = createRun({ root, stages: [stage("never")], notifyTo: "fixture-parent" }, env);
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

test("stop intent reads retry transient errors before stopping or latching failure", () => {
  const request = { runId: `test-${randomUUID()}` };
  let calls = 0, cancellations = 0;
  const intent = { actor: "operator", reason: "superseded" };
  const transient = () => Object.assign(new Error("scanner holds request"), { code: "EPERM" });
  const control = stopController("fixture", request, { retryAttempts: 3, readIntent() {
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

  let failedCalls = 0;
  const unreadable = stopController("fixture", request, { retryAttempts: 3, readIntent() {
    failedCalls += 1;
    throw Object.assign(new Error("scanner holds request"), { code: "EBUSY" });
  } });
  try {
    assert.doesNotThrow(() => unreadable.check());
    assert.doesNotThrow(() => unreadable.check());
    assert.throws(() => unreadable.check(), (error) => error.reason === "stop-request-unreadable");
    assert.equal(failedCalls, 3);
  } finally { unreadable.close(); }
});

test("final stop check settles a transient request before terminal commitment", async () => {
  const request = { runId: `test-${randomUUID()}` };
  const intent = { actor: "operator", reason: "superseded" };
  let calls = 0;
  const control = stopController("fixture", request, { retryAttempts: 3, retryDelayMs: 0, readIntent() {
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
    const runDir = createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
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
    const runDir = createRun({ root, full: true, detached: true, stages: [stage("unused")] }, env);
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
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
    let calls = 0;
    const result = await withStopLock(runDir, () => "committed", { remove(path) {
      calls++;
      if (calls < 3) throw Object.assign(new Error("scanner holds lock"), { code: "EPERM" });
      unlinkSync(path);
    } });
    assert.equal(result, "committed");
    assert.equal(calls, 3);
    assert.equal(existsSync(join(runDir, "stop.control.lock")), false);
    const failures = [];
    assert.equal(await withStopLock(runDir, () => "still committed", {
      remove() { throw Object.assign(new Error("retained"), { code: "EBUSY" }); },
      onReleaseError(error) { failures.push(error.code); },
    }), "still committed");
    assert.deepEqual(failures, ["EBUSY"]);
  });
});

for (const code of ["EPERM", "EACCES", "EBUSY"]) test(`stop lock retries ${code} acquisition and still saves its terminal action`, async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
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

test("a leftover stop lock cannot discard a terminal result or its notification", { timeout: 20_000 }, async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, full: true, detached: true, stages: [stage("never")], notifyTo: "fixture-parent" }, env);
    const path = join(runDir, "request.json");
    writeFileSync(path, JSON.stringify({ ...json(path), stopProtocol }));
    await requestStop(basename(runDir), { root, actor: "operator", reason: "superseded" });
    writeFileSync(join(runDir, "stop.control.lock"), "left by a dead caller");
    const result = await executeRun(runDir, env);
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
    const runDir = createRun({ root, stages: [stage("rust"), stage("later")], notifyTo: "fixture-parent" }, env);
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
        const runDir = createRun({ root, full: true, detached: true, notifyTo: "fixture-parent",
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
      const runDir = createRun({ root, full: true, detached: true, stages: [stage("parent-exit")] }, env);
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
    const runDir = createRun({ root, full: true, detached: true, stages: [stage("never")] }, env);
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
  const runDir = module.createRun({ root, stages: [stage("contained", source)] }, env);
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
    const runDir = module.createRun({ root, stages: [stage("contained")] }, env);
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

if (process.platform === "win32") test("native cleanup confirmation returns an unconfirmed receipt when membership never settles", { timeout: 30_000 }, async () => {
  await repository(async (root) => {
    const local = copyLauncher(root);
    const path = join(root, "scripts", "launcher-process-job.cs");
    const source = readFileSync(path, "utf8");
    // The real job is empty; inject only the observed membership count and a
    // shorter confirmation allowance, simulating an OS cleanup that stalls.
    const changed = source.replace("const int CleanupBudgetMs = 30000;", "const int CleanupBudgetMs = 100;")
      .replace("active = accounting.ActiveProcesses;", "active = accounting.ActiveProcesses + 1;");
    assert.notEqual(changed, source);
    assert.ok(changed.includes("active = accounting.ActiveProcesses + 1;"));
    writeFileSync(path, changed);
    const module = await import(pathToFileURL(local).href);
    const runDir = module.createRun({ root, stages: [stage("contained")] }, env);
    const requestPath = join(runDir, "request.json");
    writeFileSync(requestPath, JSON.stringify({ ...json(requestPath), stopProtocol }));
    const result = await module.executeRun(runDir, env);
    assert.equal(result.reason, "process-cleanup-unconfirmed");
    const receipt = readdirSync(runDir).find((name) => /^job-.*\.json$/u.test(name));
    assert.deepEqual([json(join(runDir, receipt)).CleanupConfirmed, json(join(runDir, receipt)).ActiveProcesses], [false, 1]);
    assert.ok(result.ended);
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
      const runDir = module.createRun({ root, stages: [stage("contained")], notifyTo: "fixture-parent" }, env);
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
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
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
    const runDir = createRun({ root, stages: [stage("unused")] }, env);
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
      const runDir = createRun({ root, stages: [stage("unused")] }, env);
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

if (process.platform === "win32") test("a leftover stop lock after a clean stage preserves the passing terminal save", { timeout: 30_000 }, async () => {
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
