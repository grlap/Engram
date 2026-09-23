import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { devNull } from "node:os";
import { once } from "node:events";
import { basename, delimiter, isAbsolute, join, relative } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION, WINDOWS_LIMITATION } from "./review-freeze-fingerprint.mjs";
import {
  createRun, diagnostics, executeRun, notifyRun, requiredStages, startDetached, summarize,
} from "./test-launcher.mjs";
import { fixtureHome, removeFixtureHomes, tempSnapshot, assertTempClean } from "./test-temp.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));
// This test file runs in its own Node test process. Isolate both explicit Git
// children and in-process fingerprint calls from developer configuration.
for (const key of Object.keys(process.env)) if (key.toUpperCase().startsWith("GIT_")) delete process.env[key];
Object.assign(process.env, { GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : devNull,
  GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0" });
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
    const printed = /^expectedFingerprint=(.+)$/mu.exec(receipt)?.[1];
    assert.equal(printed, json(join(runDir, "input.json")).fingerprint);
    assert.equal(printed, json(join(runDir, "results.json")).before);
    assert.equal(json(join(runDir, "results.json")).state, "passed");
    assert.equal(json(join(runDir, "notification.json")).code, 0);
    assert.equal(readFileSync(counter, "utf8"), "run\n");
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

test("an incomplete run reports UNKNOWN and cannot notify a successful completion", async () => {
  await repository(async (root) => {
    const runDir = createRun({ root, stages: [stage("not-started")], notifyTo: "fixture-parent" }, env);
    assert.match(await summarize(runDir), /UNKNOWN/u);
    let sent = false;
    await assert.rejects(notifyRun(runDir, env, async () => { sent = true; return { code: 0 }; }), /terminal|running|incomplete/iu);
    assert.equal(sent, false);
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
