import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { powerShellEnvironment } from "../launcher-stop.mjs";
import { createRun, diagnostics, executeRun, startDetached, summarize, systemProbes } from "../test-launcher.mjs";
import { fixtureHome, removeFixturePath } from "../test-temp.mjs";

export function registerLifecycleTests(support) {
  const { env, entryUrl, launcher, launcherFixtureFiles, stage, json, logPath, git, repository, copyLauncher, parseRecord } = support;
test("detached parent emits exact recovery receipt and child completes once", async () => {
  await repository(async (root) => {
    // Node acts as a hermetic CLI, executing this fixture instead of a mailbox.
    writeFileSync(join(root, "mailbox"), `
      const fs = require('node:fs');
      const file = process.argv[process.argv.indexOf('--message-file') + 1];
      if (!fs.readFileSync(file, 'utf8').startsWith('PASS ')) process.exitCode = 9;
    `);
    const counter = join(root, ".git", "count");
    const runDir = await createRun({ root, detached: true, notifyTo: "fixture-parent", stages: [stage("once",
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
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
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
    const runDir = await createRun({ root, stages: [stage("environment",
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
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
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
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(real, name));
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
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
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
    const runDir = await createRun({ root, detached: true, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
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

test("Windows PowerShell cache stays pinned through identity, ordinary and contained stages",
  { skip: process.platform !== "win32", timeout: 120_000 }, async () => {
    await repository(async (root) => {
      const local = copyLauncher(root);
      const helper = join(root, "scripts", "launcher-process-job.ps1");
      const source = readFileSync(helper, "utf8");
      writeFileSync(helper, source.replace("$ErrorActionPreference = 'Stop'",
        "$ErrorActionPreference = 'Stop'\n[Console]::WriteLine('fixture-helper-cache:' + $env:PSModuleAnalysisCachePath)"));
      git(["add", "scripts"], root);
      const baseline = git(["status", "--porcelain", "--untracked-files=all"], root);
      const module = await import(pathToFileURL(local).href);
      const realQuery = module.systemProbes.powershell;
      let identityEnv;
      module.systemProbes.powershell = (script, childEnv, context) => {
        identityEnv = childEnv;
        return realQuery(script, childEnv, context);
      };
      const powerShell = join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
      // Neither spelling is allowed to leak into the child. This checks the
      // cache-pinning contract, not an attribution of the historical writer.
      const inherited = { ...env, PSMODULEANALYSISCACHEPATH: "Microsoft/Windows/PowerShell/ModuleAnalysisCache",
        psmoduleanalysiscachepath: "relative-cache" };
      const original = { ...inherited };
      const script = `$ErrorActionPreference = 'Stop'
Import-Module Microsoft.PowerShell.Management -Force
$cache = $env:PSModuleAnalysisCachePath
if (![IO.Path]::IsPathRooted($cache)) { throw 'cache is not pinned' }
[Console]::WriteLine('fixture-stage-cache:' + (@{ cache = $cache; cwd = [IO.Directory]::GetCurrentDirectory() } | ConvertTo-Json -Compress))`;
      for (const contained of [false, true]) {
        // The cache fixture uses the different-session notification route.
        const runDir = await module.createRun({ root, full: contained, detached: contained, notifyTo: "fixture-parent",
          stages: [{ name: "powershell", command: powerShell, args: ["-NoProfile", "-NonInteractive", "-Command", script] }] }, inherited);
        const result = await module.executeRun(runDir, inherited);
        assert.equal(result.state, "passed", result.error);
        assert.equal(result.stages[0].code, 0);
        assert.equal(result.before, json(join(runDir, "request.json")).expectedFingerprint);
        assert.equal(result.after, result.before);
        const log = readFileSync(logPath(runDir, result.stages[0]), "utf8");
        const line = log.split(/\r?\n/u).find((value) => value.startsWith("fixture-stage-cache:"));
        assert.ok(line, "a real PowerShell workload executed");
        const report = JSON.parse(line.slice("fixture-stage-cache:".length));
        assert.equal(isAbsolute(report.cache), true);
        assert.equal(relative(realpathSync(root), report.cache).split(/[\\/]/u)[0], "target");
        assert.ok(report.cache.includes(basename(runDir)), "each run owns its cache path");
        assert.equal(realpathSync(report.cwd), realpathSync(root));
        assert.equal(identityEnv.PSModuleAnalysisCachePath, report.cache, "the first identity query uses the same environment");
        assert.equal(Object.keys(identityEnv).filter((name) => name.toUpperCase() === "PSMODULEANALYSISCACHEPATH").length, 1);
        // PowerShell does not promise a cache write for this invocation. The
        // contract asserted here is pinning; inspect any real write after exit.
        if (existsSync(report.cache)) assert.equal(statSync(report.cache).isFile(), true);
        if (contained) assert.ok(log.includes(`fixture-helper-cache:${report.cache}`), "the full-only supervisor is pinned too");
        assert.equal(git(["ls-files", "--others", "--exclude-standard"], root), "");
        assert.equal(git(["status", "--porcelain", "--untracked-files=all"], root), baseline);
      }
      assert.deepEqual(inherited, original, "no caller or global environment mutation");
    });
  });

test("Windows PowerShell cache refuses linked scratch before writing outside its root",
  { skip: process.platform !== "win32" }, (t) => {
    const root = fixtureHome("cache-root-", t), outside = fixtureHome("cache-outside-", t);
    mkdirSync(join(root, "target", "tmp"), { recursive: true });
    const link = join(root, "target", "tmp", "launcher-powershell");
    symlinkSync(outside, link, "junction");
    try {
      assert.throws(() => powerShellEnvironment(root, "fixture-cache", env), /link|escaped/u);
      assert.deepEqual(readdirSync(outside), []);
    } finally { unlinkSync(link); }
  });

test("Windows PowerShell cache accepts scratch created concurrently",
  { skip: process.platform !== "win32" }, async () => {
    await repository(async (root) => {
      copyLauncher(root);
      const support = join(root, "scripts", "launcher-stop.mjs");
      const source = readFileSync(support, "utf8");
      // Deterministically model another creator winning after the failed
      // lstat: a real directory exists when mkdir reports EEXIST.
      const changed = source.replace("mkdirSync(path);",
        "mkdirSync(path); fixtureCreated.push(path); throw Object.assign(new Error('fixture concurrent creation'), { code: 'EEXIST' });");
      assert.notEqual(changed, source, "the copied helper must inject the mkdir race");
      writeFileSync(support, `${changed}\nexport const fixtureCreated = [];\n`);
      const module = await import(pathToFileURL(support).href);
      const childEnv = module.powerShellEnvironment(root, "racing-cache", env);
      const scratch = join(realpathSync(root), "target", "tmp", "launcher-powershell", "racing-cache");
      assert.equal(childEnv.PSModuleAnalysisCachePath, join(scratch, "ModuleAnalysisCache"));
      assert.deepEqual(module.fixtureCreated, [join(realpathSync(root), "target"), join(realpathSync(root), "target", "tmp"),
        dirname(scratch), scratch]);
      assert.equal(lstatSync(scratch).isDirectory(), true);
      assert.deepEqual(module.powerShellEnvironment(root, "racing-cache", env), childEnv);
    });
  });

test("Windows cache startup refusals retain saved results and unrun stages",
  { skip: process.platform !== "win32" }, async () => {
    for (const contained of [false, true]) for (const fault of ["target", "target/tmp", "cache-directory", "cache-file"]) {
      await repository(async (root) => {
        const outside = fixtureHome("cache-startup-outside-");
        const runDir = await createRun({ root, full: contained, detached: contained,
          stages: [stage("first", "require('node:fs').writeFileSync('should-not-run', '')"), stage("second")] }, env);
        const initial = json(join(runDir, "results.json"));
        const cache = join(root, "target", "tmp", "launcher-powershell", basename(runDir), "ModuleAnalysisCache");
        const link = fault === "cache-file" ? null : join(root, ...(fault === "cache-directory"
          ? ["target", "tmp", "launcher-powershell"] : fault.split("/")));
        try {
          if (link) {
            mkdirSync(dirname(link), { recursive: true });
            symlinkSync(outside, link, "junction");
          } else mkdirSync(cache, { recursive: true });
          let ready = false;
          const result = await executeRun(runDir, env, () => { ready = true; });
          assert.equal(result.state, "failed");
          assert.equal(result.exitCode, 1);
          assert.equal(result.reason, contained ? "helper-start-failed" : "launcher-error");
          assert.match(result.error, /scratch ancestor is a link|cache must be a regular file/u);
          assert.deepEqual(result.stages, initial.stages);
          assert.equal(result.started, initial.started);
          assert.deepEqual(result.registration, initial.registration);
          assert.deepEqual(result.heartbeat, initial.heartbeat);
          assert.deepEqual(json(join(runDir, "results.json")), JSON.parse(JSON.stringify(result)));
          assert.equal(result.processCleanup, undefined, "no supervisor was started");
          assert.equal(ready, false);
          assert.equal(existsSync(join(root, "should-not-run")), false);
          assert.deepEqual(readdirSync(outside), []);
        } finally {
          // Unlink only this known junction before fixture cleanup recurses.
          if (link && existsSync(link) && lstatSync(link).isSymbolicLink()) unlinkSync(link);
          await removeFixturePath(outside);
        }
      });
    }
  });

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
      const runDir = await createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
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
    const runDir = await createRun({ root, stages: [stage("clean")] }, env);
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
    const runDir = await createRun({ root, stages: [stage("unused")], notifyTo: "fixture-parent" }, env);
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
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
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

}
