import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { once } from "node:events";
import { basename, dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { posixStopProtocol, requestStop, runPosixCommand, stopController, waitForPosixGroupExit } from "../launcher-stop.mjs";
import { createRun, executeRun, notifyRun } from "../test-launcher.mjs";

export function registerPosixStopTests(support) {
  const { env, launcher, stage, json, logPath, repository, copyLauncher, directoryBytes, parkingBroker } = support;
test("POSIX absence observation requires ESRCH, including after Darwin EPERM", async () => {
  let clock = 0, calls = 0;
  const options = { now: () => clock, sleep: async (ms) => { clock += ms; }, timeoutMs: 50 };
  await waitForPosixGroupExit(123, { ...options, probe() {
    calls += 1;
    throw Object.assign(new Error("group probe"), { code: calls < 3 ? "EPERM" : "ESRCH" });
  } });
  assert.equal(calls, 3);
  clock = 0;
  await assert.rejects(waitForPosixGroupExit(123, { ...options,
    probe() { throw Object.assign(new Error("not absent"), { code: "EPERM" }); },
  }), /disappearance is unconfirmed/u);
  await assert.rejects(waitForPosixGroupExit(123, { ...options,
    probe() { throw Object.assign(new Error("bad probe"), { code: "EACCES" }); },
  }), { code: "EACCES" });
});

if (["darwin", "linux"].includes(process.platform)) for (const full of [false, true]) {
  test(`POSIX detached ${full ? "full" : "focused"} stop cleans its group before notification`, { timeout: 60_000 }, async (t) => {
    await repository(async (root) => {
      const local = copyLauncher(root, (source) => source.replace("if (request.full) {", "if (false) {"));
      const module = await import(pathToFileURL(local).href);
      const broker = parkingBroker(t, root);
      let completion;
      try {
        assert.equal((await broker.next())[0], "listening");
        const termFile = join(root, ".git", "term-seen");
        const resistant = `process.on('SIGTERM', () => require('node:fs').writeFileSync(${JSON.stringify(termFile)}, 'term')); ${broker.parked}`;
        const parent = `const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(resistant)}],
          { detached: false, stdio: 'ignore' }); child.on('close', () => process.exit(0));`;
        writeFileSync(join(root, "mailbox"), `const fs = require('node:fs'), path = require('node:path');
          const message = process.argv[process.argv.indexOf('--message-file') + 1];
          const result = JSON.parse(fs.readFileSync(path.join(path.dirname(message), 'results.json'), 'utf8'));
          if (result.reason !== 'stopped' || !result.interrupted || !result.processCleanup.every(x => x.confirmed && x.activeProcesses === 0)) process.exitCode = 8;
          if (!fs.readFileSync(message, 'utf8').startsWith('INTERRUPTED ')) process.exitCode = 9;`);
        const runDir = await module.createRun({ root, full, detached: true, notifyTo: "fixture-parent",
          stages: [stage("first", "console.log('before stop')"), stage("parked", parent), stage("later", "throw Error('must not run')")] }, env);
        assert.equal(json(join(runDir, "request.json")).stopProtocol, posixStopProtocol);
        const connected = broker.next();
        const launched = await module.startDetached(runDir, env, () => {});
        launched.child.ref();
        completion = launched.completion;
        assert.equal((await Promise.race([connected, completion.then((value) => [JSON.stringify(value)])]))[0], "connected");
        const before = json(join(runDir, "results.json"));
        const firstLog = readFileSync(logPath(runDir, before.stages[0]));
        const cli = spawnSync(process.execPath, [local, "stop", basename(runDir), "--actor", "fixture-owner", "--reason", "superseded input"],
          { cwd: root, env, encoding: "utf8" });
        assert.equal(cli.status, 0, cli.stderr);
        assert.match(cli.stdout, /^Stop requested;/u);
        const stopped = await requestStop(basename(runDir), { root, actor: "second", reason: "duplicate" });
        assert.equal(stopped.duplicate, true);
        assert.equal(stopped.intent.actor, "fixture-owner");
        assert.equal((await completion).code, 1);
        const result = json(join(runDir, "results.json"));
        assert.deepEqual([result.state, result.reason, result.interrupted, result.exitCode], ["failed", "stopped", true, 1]);
        assert.deepEqual(result.stages.map(({ state }) => state), ["passed", "failed", "unrun"]);
        assert.deepEqual(result.stages[0], before.stages[0]);
        assert.deepEqual(readFileSync(logPath(runDir, result.stages[0])), firstLog);
        assert.ok(existsSync(termFile), "the same-group descendant handled TERM before KILL cleanup");
        assert.ok(result.processCleanup.every(({ method, scope, confirmed, activeProcesses }) =>
          method === posixStopProtocol && scope === "process-group" && confirmed && activeProcesses === 0));
        assert.equal(json(join(runDir, "notification.json")).ok, true);
        assert.equal(broker.release() === false, false, "the outside broker survives the group stop");
        const terminalBytes = directoryBytes(runDir);
        assert.equal((await requestStop(basename(runDir), { root, actor: "late", reason: "too late" })).terminal, true);
        assert.deepEqual(directoryBytes(runDir), terminalBytes);
      } finally {
        await broker.close();
        if (completion) await completion;
      }
    });
  });
}

if (["darwin", "linux"].includes(process.platform)) test("POSIX normal command exit cleans residual group members", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    const runDir = await createRun({ root, detached: true, stages: [stage("unused")] }, env);
    const control = stopController(runDir, json(join(runDir, "request.json")));
    let pending;
    try {
      assert.equal((await broker.next())[0], "listening");
      const connected = broker.next();
      const source = `const child = require('node:child_process').spawn(process.execPath,
        ['-e', ${JSON.stringify(`${broker.parked} socket.once('connect', () => process.send('ready'));`)}], { detached: false, stdio: ['ignore','ignore','ignore','ipc'] });
        child.once('message', () => process.exit(0));`;
      pending = runPosixCommand(process.execPath, ["-e", source], { cwd: root, env,
        log: join(runDir, "normal.log"), runDir, control });
      assert.equal((await connected)[0], "connected");
      const result = await pending;
      assert.equal(result.code, 0);
      assert.equal(result.stopped, false);
      assert.equal(result.cleanup.confirmed, true);
      assert.equal(result.cleanup.scope, "process-group");
      assert.equal(broker.release() === false, false);
    } finally { await broker.close(); if (pending) await pending; control.close(); }
  });
});

if (["darwin", "linux"].includes(process.platform)) for (const errorCode of ["EPERM", "EACCES"]) {
  test(`POSIX terminating ${errorCode} needs helper exit and group absence`, { timeout: 60_000 }, async () => {
    await repository(async (root) => {
      copyLauncher(root);
      const helper = join(root, "scripts", "launcher-process-group.mjs");
      writeFileSync(helper, readFileSync(helper, "utf8").replace("let child,", `
        const realKill = process.kill.bind(process);
        process.kill = (pid, signal) => { if (signal === 'SIGKILL') throw Object.assign(new Error('fixture signal error'), { code: ${JSON.stringify(errorCode)} }); return realKill(pid, signal); };
        let child,`));
      const module = await import(pathToFileURL(join(root, "scripts", "launcher-stop.mjs")).href);
      const runDir = await createRun({ root, detached: true, stages: [stage("unused")] }, env);
      const control = stopController(runDir, json(join(runDir, "request.json")));
      try {
        const pending = module.runPosixCommand(process.execPath, ["-e", ""], { cwd: root, env,
          log: join(runDir, "signal.log"), runDir, control });
        if (errorCode === "EPERM") {
          const result = await pending;
          assert.equal(result.code, 0);
          assert.equal(result.cleanup.confirmed, true);
          assert.equal(json(result.cleanup.receipt).terminationError, "EPERM", "the injected signal refusal occurred");
        } else await assert.rejects(pending, (error) => error.cleanupUnconfirmed === true);
      } finally { control.close(); }
    });
  });
}

if (["darwin", "linux"].includes(process.platform)) test("POSIX private-channel EOF cleans the helper's owned group", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let helper, completion;
    const wait = async (pending, signal = t.signal) => {
      signal.throwIfAborted();
      let abort;
      try {
        return await Promise.race([pending, new Promise((_, reject) => {
          abort = () => reject(signal.reason);
          signal.addEventListener("abort", abort, { once: true });
        })]);
      } finally { if (abort) signal.removeEventListener("abort", abort); }
    };
    try {
      assert.equal((await broker.next())[0], "listening");
      const receipt = join(root, ".git", "eof-receipt.json");
      const spec = Buffer.from(JSON.stringify({ command: process.execPath, args: ["-e", broker.parked], cwd: root, receipt })).toString("base64");
      const connected = broker.next();
      helper = spawn(process.execPath, [fileURLToPath(new URL("../launcher-process-group.mjs", import.meta.url)), spec],
        { cwd: root, env, detached: true, stdio: ["ignore", "ignore", "ignore", "ipc"] });
      // Local IPC disconnect need not produce ChildProcess's stdio-close
      // event. Exit observes the owned helper's actual termination/reaping.
      completion = once(helper, "exit");
      assert.deepEqual((await once(helper, "message", { signal: t.signal }))[0], { ready: true });
      helper.send({ kind: "start" });
      assert.equal((await connected)[0], "connected");
      helper.disconnect();
      await wait(completion);
      await waitForPosixGroupExit(helper.pid);
      assert.equal(json(receipt).stopped, true);
      assert.equal(broker.release() === false, false);
    } finally {
      if (helper?.connected) helper.disconnect();
      await broker.close(); // release the parked fixture even if an assertion failed
      if (completion) {
        try { await wait(completion, AbortSignal.timeout(5000)); }
        catch (error) {
          // Only this test's still-owned child handle, never a recovered PID
          // or process group. Failure remains a failure after fixture cleanup.
          if (helper.exitCode === null && helper.signalCode === null) helper.kill("SIGKILL");
          helper.unref();
          throw error;
        }
      }
    }
  });
});

if (["darwin", "linux"].includes(process.platform)) test("POSIX absent helper receipt never becomes confirmed cleanup", { timeout: 60_000 }, async () => {
  await repository(async (root) => {
    copyLauncher(root);
    const helper = join(root, "scripts", "launcher-process-group.mjs");
    writeFileSync(helper, readFileSync(helper, "utf8").replaceAll("writeFileSync(spec.receipt,", "writeFileSync(spec.receipt + '.missing',"));
    const module = await import(pathToFileURL(join(root, "scripts", "launcher-stop.mjs")).href);
    const runDir = await createRun({ root, detached: true, stages: [stage("unused")] }, env);
    const control = stopController(runDir, json(join(runDir, "request.json")));
    try {
      await assert.rejects(module.runPosixCommand(process.execPath, ["-e", ""], { cwd: root, env,
        log: join(runDir, "missing.log"), runDir, control }), (error) => error.cleanupUnconfirmed === true);
    } finally { control.close(); }
  });
});

if (["darwin", "linux"].includes(process.platform)) test("POSIX stop retained before admission starts no command", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, full: true, detached: true, stages: [stage("never", "throw Error('must not run')")] }, env);
    await requestStop(basename(runDir), { root, actor: "before-owner", reason: "superseded before admission" });
    const result = await executeRun(runDir, env);
    assert.deepEqual([result.state, result.reason, result.interrupted], ["failed", "stopped", true]);
    assert.equal(result.stop.actor, "before-owner");
    assert.ok(result.stages.every(({ state }) => state === "unrun"));
    assert.equal(result.processCleanup, undefined);
  });
});

if (["darwin", "linux"].includes(process.platform)) test("POSIX normal exit isolates IPC and does not wait for an escaped fixture", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    // This broker and the detached witness are deliberately outside the
    // helper's group. Fixture cleanup owns them, never the stop adapter.
    const broker = parkingBroker(t, root);
    const runDir = await createRun({ root, detached: true, stages: [stage("unused")] }, env);
    const control = stopController(runDir, json(join(runDir, "request.json")));
    const log = join(runDir, "escaped.log");
    let pending;
    try {
      assert.equal((await broker.next())[0], "listening");
      const connected = broker.next();
      const escaped = `${broker.parked}
        socket.once('connect', () => { require('node:fs').writeSync(4, 'ready'); require('node:fs').closeSync(4); });`;
      // Deliberate fd inheritance exercises the Linux risk. Darwin can
      // expose a Node-owned fd 3 that posix_spawn refuses with EBADF.
      const inherited = process.platform === "linux" ? 3 : "ignore";
      const source = `const fs = require('node:fs');
        if (fs.fstatSync(3).isSocket()) throw Error('workload inherited private IPC');
        console.log('private IPC isolated');
        const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(escaped)}],
          { detached: true, stdio: ['ignore','ignore','ignore',${JSON.stringify(inherited)},'pipe'] });
        let ready = false;
        child.once('error', error => { console.error('escaped fixture spawn failed:', error.code); process.exit(1); });
        child.once('exit', code => { if (!ready) { console.error('escaped fixture ended before ready:', code); process.exit(1); } });
        child.stdio[4].once('data', () => { ready = true; process.exit(0); });`;
      pending = runPosixCommand(process.execPath, ["-e", source], { cwd: root, env,
        log, runDir, control });
      assert.equal((await Promise.race([connected, pending.then((result) => ["ended", result.code])]))[0], "connected");
      const result = await pending;
      assert.equal(result.code, 0);
      assert.equal(result.cleanup.confirmed, true);
      assert.match(readFileSync(join(runDir, "escaped.log"), "utf8"), /private IPC isolated/u);
      const disconnected = broker.next();
      assert.notEqual(broker.release(), false, "outside fixture still lives after helper cleanup");
      assert.equal((await disconnected)[0], "closed");
    } catch (error) {
      if (existsSync(log)) {
        const diagnostic = `\nEscaped fixture output before cleanup:\n${readFileSync(log, "utf8").slice(-4096)}`;
        error.message += diagnostic;
        if (typeof error.stack === "string") error.stack += diagnostic;
      }
      throw error;
    } finally { await broker.close(); if (pending) await pending; control.close(); }
  });
});

if (["darwin", "linux"].includes(process.platform)) test("POSIX unsolicited helper stop cannot pass a TERM-clean workload", { timeout: 60_000 }, async (t) => {
  await repository(async (root) => {
    const broker = parkingBroker(t, root);
    let pending;
    try {
      assert.equal((await broker.next())[0], "listening");
      const source = `process.on('SIGTERM', () => process.exit(0)); ${broker.parked}
        socket.once('connect', () => process.kill(process.ppid, 'SIGTERM'));`;
      const runDir = await createRun({ root, detached: true, stages: [stage("interrupted", source), stage("later")] }, env);
      pending = executeRun(runDir, env);
      const result = await pending;
      assert.deepEqual([result.state, result.exitCode], ["failed", 1]);
      assert.equal(result.interrupted, undefined, "no operator stop intent was supplied");
      assert.equal(result.stop, undefined);
      assert.equal(result.stages[1].state, "unrun");
      assert.match(result.stages[0].error, /without a controller request/u);
      const proof = json(result.processCleanup[0].receipt);
      assert.deepEqual([proof.stopped, proof.code], [true, 0], "a clean root exit cannot disguise helper cancellation");
      assert.equal(result.processCleanup[0].confirmed, true);
    } finally { await broker.close(); if (pending) await pending; }
  });
});

if (["darwin", "linux"].includes(process.platform)) test("POSIX stop after helper startup prevents workload admission", { timeout: 60_000 }, async () => {
  await repository(async (root) => {
    const local = copyLauncher(root);
    const path = join(root, "scripts", "launcher-stop.mjs");
    const original = readFileSync(path, "utf8");
    const changed = original.replace('child.once("message", (message) => {', `child.once("message", async (message) => {
      await requestStop(basename(runDir), { root: cwd, actor: 'startup-owner', reason: 'stop before start' });
      try { control.check(); } catch (error) { if (error.reason !== 'stopped') throw error; }`);
    assert.notEqual(changed, original);
    writeFileSync(path, changed);
    const module = await import(pathToFileURL(local).href);
    const marker = join(root, ".git", "workload-ran");
    const runDir = await module.createRun({ root, detached: true,
      stages: [stage("never", `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`)] }, env);
    const result = await module.executeRun(runDir, env);
    assert.deepEqual([result.state, result.reason, result.interrupted, result.stop.actor], ["failed", "stopped", true, "startup-owner"]);
    assert.equal(existsSync(marker), false);
    assert.equal(result.processCleanup[0].confirmed, true);
    const proof = json(result.processCleanup[0].receipt);
    assert.deepEqual([proof.stopped, proof.code, proof.signal], [true, null, null], "helper stopped before spawning a workload");
  });
});


}

