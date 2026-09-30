import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmdirSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { devNull } from "node:os";
import { once } from "node:events";
import { basename, delimiter, dirname, isAbsolute, join, relative } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION, WINDOWS_LIMITATION } from "./review-freeze-fingerprint.mjs";
import {
  commandKind, countTests, createRun, diagnostics, executeRun, machineRecord, notifyRun, processHost, processIdentity,
  recoverRun, renameWithRetry, requiredStages, sameCreation, selectsTests, startDetached, summarize,
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
const stage = (name, source = "") => ({ name, command: process.execPath, args: ["-e", source] });
const json = (path) => JSON.parse(readFileSync(path, "utf8"));
const logPath = (runDir, entry) => isAbsolute(entry.log) ? entry.log : join(runDir, entry.log);

async function repository(callback) {
  const root = fixtureHome("engram-launcher-");
  try {
    execFileSync("git", ["init", "--quiet", "--template="], { cwd: root });
    execFileSync("git", ["config", "core.autocrlf", "false"], { cwd: root });
    writeFileSync(join(root, "tracked.txt"), "baseline\n");
    execFileSync("git", ["add", "tracked.txt"], { cwd: root });
    await callback(root);
  } finally {
    removeFixtureHomes(root);
  }
}

test("required stages preserve all nine gates and platform-specific Rust runners", () => {
  for (const platform of ["win32", "linux"]) {
    const stages = requiredStages(platform);
    assert.equal(stages.length, 9);
    assert.equal(new Set(stages.map(({ name }) => name)).size, 9);
    assert.deepEqual(stages.slice(0, 3).map(({ command, args }) => [command, ...args]), [
      ["cargo", "fmt", "--check"],
      ["cargo", "check"],
      ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ]);
    assert.match([stages[3].command, ...stages[3].args].join(" "), platform === "win32"
      ? /pwsh.*-NoProfile.*-File scripts\/test-rust\.ps1/u
      : /scripts\/test-rust\.sh/u);
    assert.deepEqual(stages[4].args, ["--test", "scripts/review-freeze-fingerprint.test.mjs", "scripts/test-launcher.test.mjs"]);
    assert.deepEqual(stages.slice(5).map(({ args }) => args), [
      ["--test", "scripts/mcp-dogfood.test.mjs"],
      ["--test", "scripts/control-dogfood.test.mjs"],
      ["--test", "scripts/parity.test.mjs"],
      ["scripts/check-doc-links.mjs"],
    ]);
  }
});

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
    assert.equal(result.stages[0].code, 23);
    assert.equal(existsSync(marker), false);
    assert.match(await summarize(runDir), /deliberate fixture failure/u);
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
    assert.ok(summary.includes(result.stages[0].log));
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
    const runDir = createRun({ root, notifyTo: "fixture-parent", stages: [stage("once",
      `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)] }, env);
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
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
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
    assert.match(await summarize(runDir), /inherited Rustup override cleared for repository selection/u);
  });
});

test("malformed notification targets and duplicate prerequisites refuse before creating a run", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
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
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs", "test-temp.mjs"]) {
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
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    for (const [notifyTo, overrides, expected] of [
      [env.TERMAL_SESSION_ID, {}, /self-send/u],
      [` ${env.TERMAL_SESSION_ID} `, {}, /self-send/u],
      ["fixture-parent", { TERMAL_SESSION_ID: "" }, /TERMAL_SESSION_ID/u],
      ["fixture-parent", { TERMAL_CLI: "" }, /TERMAL_CLI/u],
      ["fixture-parent", { TERMAL_CLI: "relative-cli" }, /TERMAL_CLI/u],
      ["fixture-parent", { TERMAL_CLI: join(launcher, "missing.exe") }, /executable not found/u],
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
    const runDir = createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
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

async function worker(runDir) {
  const child = spawn(process.execPath, [launcher, "_run", runDir],
    { env, windowsHide: true, stdio: ["ignore", "ignore", "ignore", "ipc"] });
  const messages = [];
  child.on("message", (message) => messages.push(message));
  const code = await new Promise((done, reject) => {
    child.once("error", reject);
    child.once("close", done);
  });
  return { code, messages };
}

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
    const first = await worker(runDir);
    assert.equal(first.code, 0);
    assert.deepEqual(first.messages, [{ ready: true }]);
    const saved = readFileSync(join(runDir, "results.json"), "utf8");
    const second = await worker(runDir);
    assert.equal(second.code, 1);
    assert.deepEqual(second.messages, []);
    assert.equal(readFileSync(join(runDir, "results.json"), "utf8"), saved);
  });
});

test("warnings remain visible without converting a successful exit to failure", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("warning", "console.error('warning: fixture caution')")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.exitCode, 0);
    assert.match(await summarize(runDir), /warning: fixture caution/u);
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
    assert.match(summary, /error: essential failure detail/u);
    assert.ok(summary.indexOf("essential failure detail") < summary.indexOf("earlier caution"));
    assert.doesNotMatch(summary, /\n\n/u);
    assert.match(summary, /summary diagnostics truncated/u);
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
    assert.match(await summarize(runDir), /essential failure after warning/u);
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

test("input capture failure reports once before detached admission or notification", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    // A real failing Git clean filter makes captureFingerprint refuse.
    writeFileSync(join(root, ".gitattributes"), "tracked.txt filter=refuse\n");
    writeFileSync(join(root, "tracked.txt"), "modified\n");
    execFileSync("git", ["config", "filter.refuse.clean", `"${process.execPath.replaceAll("\\", "/")}" -e "process.exit(7)"`], { cwd: root });
    execFileSync("git", ["config", "filter.refuse.required", "true"], { cwd: root });
    const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused",
      "--detach", "--notify", "fixture-parent", "--", process.execPath, "-e", ""],
      { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /input capture failed:.*git diff/su);
    assert.doesNotMatch(result.stdout, /STARTED/u);
    const runBase = join(root, ".git", "review-runs");
    const runs = readdirSync(runBase);
    assert.equal(runs.length, 1);
    const runDir = join(runBase, runs[0]);
    assert.equal(json(join(runDir, "results.json")).state, "failed");
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false);
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
    assert.match(await summarize(runDir), /missing-executable/u);
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
    assert.match(await summarize(runDir), /ENGRAM_LAUNCHER_FIXTURE_BIN/u);
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
    assert.ok(summary.includes(`preflight silent: exit=13 log=${log}`));
    assert.match(summary, /unused: unrun/u);
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
    assert.ok((await summarize(runDir)).includes(result.error));
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
    assert.match(summary, /truncat|omitt|full log/iu);
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
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
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

test("notification publication ignores interrupted temporary bytes and freezes one complete body", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("clean")], notifyTo: "fixture-parent" }, env);
    await executeRun(runDir, env);
    const expected = await summarize(runDir);
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

test("summary renders a shared runner and stage error only once", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("broken")] }, env);
    const result = json(join(runDir, "results.json"));
    const error = "fixture shared failure detail";
    Object.assign(result, { state: "failed", ended: new Date().toISOString(), exitCode: 1, error });
    Object.assign(result.stages[0], { state: "failed", code: 1, error });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const summary = await summarize(runDir);
    assert.equal(summary.split(error).length - 1, 1);
    assert.match(summary, /broken: failed exit=1/u);
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
  return { result, record: parseRecord(machineRecord(result.plan, result)) };
}
async function counted(root, text) {
  const log = join(root, `${randomUUID()}.log`);
  writeFileSync(log, text);
  try { return await countTests(log); } finally { unlinkSync(log); }
}
const complete = (executed, passed, failed, ignored, runners, filteredOut) =>
  ({ executed, passed, failed, ignored, runners, filteredOut, failures: failed });

test("every required stage has a kind, and only runner stages are tests", () => {
  for (const platform of ["win32", "linux"]) {
    assert.deepEqual(requiredStages(platform).map(({ name, kind }) => [name, kind]), [
      ["fmt", "lint"], ["check", "build"], ["clippy", "lint"], ["rust", "test"], ["freeze", "test"],
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
    assert.match(await summarize(dirname(result.stages[0].log)), /the runners' summaries count 1 failed tests and the command exited 0/u);
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
      [["rust", "not-run", "yes"], ["freeze", "not-run", "yes"], ["mcp", "not-run", "yes"], ["control", "not-run", "yes"], ["parity", "not-run", "yes"]]);
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
    const { record } = await recordOf(root, [{ name: "changes-source", kind: "test", command: process.execPath, args: ["-e",
      `require('node:fs').writeFileSync('tracked.txt', 'changed\\n'); process.stdout.write(${JSON.stringify(libtest(4, 0, 0, 0))})`] }]);
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "input-changed");
    assert.deepEqual(record.stages.map(({ state, executed }) => [state, executed]), [["passed", "4"]]);
    assert.equal(countsAsPassedTests(record), false);
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
    execFileSync("git", ["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "baseline"],
      { cwd: root });
    const linked = join(root, "linked");
    execFileSync("git", ["worktree", "add", "--quiet", linked], { cwd: root });
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

test("the record ends the executing process's stdout, and summary and notify never print one", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of ["test-launcher.mjs", "review-freeze-fingerprint.mjs"]) {
      copyFileSync(fileURLToPath(new URL(name, import.meta.url)), join(root, "scripts", name));
    }
    const copied = join(root, "scripts", "test-launcher.mjs");
    execFileSync("git", ["add", "scripts"], { cwd: root });
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
    assert.match(await summarize(runDir), /^INTERRUPTED \(no terminal result/u, "the heartbeat alone reads as interrupted");
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
    assert.match(summary, /^INTERRUPTED \(stopped in stage second; recovered at 2026-09-30T01:00:00.000Z: .*\) test-\S+ exit=1$/mu);
    assert.match(summary, /^second: failed outcome=unknown exit=unrun\/unknown log=/mu);
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
    assert.match(await summarize(reused.runDir), /^INTERRUPTED \(stopped during startup, before any stage ran;/mu);
    const between = await recover([passed, { name: "second", state: "unrun" }]);
    assert.deepEqual([between.result.recovered.phase, between.result.recovered.stage], ["between-stages", "first"]);
    assert.match(await summarize(between.runDir), /^INTERRUPTED \(stopped between stages, after first;/mu);
    const finishing = await recover([passed]);
    assert.deepEqual([finishing.result.recovered.phase, finishing.result.recovered.stage], ["finishing", null]);

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

test("a launcher stopped during its second stage is recovered with the first stage unchanged", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let child, completion;
    try {
      assert.equal((await broker.next())[0], "listening");
      const runDir = createRun({ root, stages: [stage("first", "console.log('first stage output')"),
        { name: "second", command: process.execPath, args: ["-e", broker.parked] }] }, env);
      child = spawn(process.execPath, [launcher, "_run", runDir], { cwd: root, env, windowsHide: true, stdio: ["ignore", "ignore", "ignore"] });
      completion = once(child, "close");
      const parked = await Promise.race([broker.next(), completion.then(([code]) => [`launcher ended (${code})`])]);
      assert.equal(parked[0], "connected");
      const before = json(join(runDir, "results.json"));
      assert.deepEqual(before.stages.map(({ state }) => state), ["passed", "running"]);
      const firstLog = readFileSync(logPath(runDir, before.stages[0]));
      child.kill("SIGKILL");
      await completion;

      const recovered = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(recovered.status, 0, recovered.stderr);
      assert.match(recovered.stdout, /^INTERRUPTED \(stopped in stage second;/u);
      assert.match(recovered.stdout, /^Settled as interrupted; tests not rerun\.$/mu);
      assert.doesNotMatch(recovered.stdout, /^(?:PASS|FAIL)\b|test-launcher\/v1/mu);
      const after = json(join(runDir, "results.json"));
      assert.deepEqual([after.state, after.interrupted, after.exitCode], ["failed", true, 1]);
      assert.match(after.error, /^interrupted: /u);
      assert.deepEqual(after.stages[0], before.stages[0], "the first stage's fields are unchanged");
      assert.deepEqual(readFileSync(logPath(runDir, after.stages[0])), firstLog, "the first stage's log bytes are unchanged");
      assert.equal(after.stages[1].state, "failed");
      assert.equal(after.stages[1].outcome, "unknown");
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
      assert.match(await summarize(runDir), /^INTERRUPTED \(no terminal result/u);
      const before = directoryBytes(runDir);
      const recovered = spawnSync(process.execPath, [launcher, "recover", runDir], { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(recovered.status, 1);
      // Where start times move with the clock the living executor is unknown,
      // and recover refuses for that.
      assert.match(recovered.stderr, identifies
        ? /^REFUSED recover .*: executor \d+ is alive; a stale heartbeat does not end a run; nothing changed$/mu
        : /^REFUSED recover .*: cannot tell whether executor \d+ is alive: .*; nothing changed$/mu);
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
