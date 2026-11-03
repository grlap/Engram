import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { chmodSync, closeSync, copyFileSync, existsSync, mkdirSync, openSync, readdirSync, readFileSync, readSync, statSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { executable, requiredStages } from "../test-launcher.mjs";
import { fixtureHome, removeFixtureHomes } from "../test-temp.mjs";

// Created once by the entry point after its environment setup.
export function createSupport({ env, entryUrl }) {
const launcher = fileURLToPath(new URL("./test-launcher.mjs", entryUrl));
const launcherSupportFiles = ["test-run-registration.mjs", "review-freeze-fingerprint.mjs", "launcher-stop.mjs", "launcher-process-job.ps1", "launcher-process-job.cs"];
const launcherFixtureFiles = ["test-launcher.mjs", ...launcherSupportFiles];
const stage = (name, source = "") => ({ name, command: process.execPath, args: ["-e", source] });
const json = (path) => JSON.parse(readFileSync(path, "utf8"));
const logPath = (runDir, entry) => isAbsolute(entry.log) ? entry.log : join(runDir, entry.log);

function git(args, cwd) {
  // Fixture Git writes must not start maintenance that outlives cleanup.
  return execFileSync("git", ["-c", "maintenance.auto=false", ...args], { cwd, encoding: "utf8" });
}

function fixtureFailureSnapshot({ runDir, binary }) {
  const bounded = (text, limit) => text.length > limit ? `[truncated]\n${text.slice(-limit)}` : text;
  const tail = (path, limit) => {
    const fd = openSync(path, "r");
    try {
      const size = statSync(path).size;
      const buffer = Buffer.alloc(Math.min(size, limit));
      const length = readSync(fd, buffer, 0, buffer.length, Math.max(0, size - buffer.length));
      return `${size > limit ? "[truncated]\n" : ""}${buffer.subarray(0, length).toString("utf8")}`;
    } finally { closeSync(fd); }
  };
  const unavailable = (error) => error.code === "ENOENT" ? "absent" : `unavailable: ${error.message}`;
  const resultsPath = join(runDir, "results.json");
  let saved, results, executableInfo, output = "absent";
  try {
    saved = JSON.parse(tail(resultsPath, 65_536));
    const outcome = ({ name, state, code, signal, error, command }) => ({ name, state,
      code: code === undefined ? "unavailable" : code, signal: signal === undefined ? "unavailable" : signal, error, command });
    results = JSON.stringify({ state: saved.state, exitCode: saved.exitCode, reason: saved.reason, error: saved.error,
      preflight: saved.preflight?.map(outcome), stages: saved.stages?.map(outcome) });
    const last = [...(saved.preflight ?? []), ...(saved.stages ?? [])].filter((entry) => entry.log).at(-1);
    if (last) {
      try { output = tail(logPath(runDir, last), 4096); } catch (error) { output = unavailable(error); }
    }
  } catch (error) { results = unavailable(error); }
  try {
    const info = statSync(binary);
    executableInfo = JSON.stringify({ path: binary, bytes: info.size, mode: (info.mode & 0o777).toString(8) });
  } catch (error) { executableInfo = `${binary}: ${unavailable(error)}`; }
  return `Fixture child snapshot (captured before cleanup; inner paths are removed):\nresults: ${resultsPath}\n`
    + `executable: ${executableInfo}\nchild results: ${bounded(results, 4096)}\nmerged stdout/stderr (last 4096 bytes):\n${output}`;
}

async function repository(callback, prefix = "engram-launcher-") {
  const root = fixtureHome(prefix);
  let lastChild;
  try {
    git(["init", "--quiet", "--template="], root);
    assert.equal(git(["config", "--bool", "--get", "maintenance.auto"], root).trim(), "false");
    git(["config", "core.autocrlf", "false"], root);
    // Match the product's existing scratch contract; a cache outside target/
    // must still appear in the fixture's ordinary Git status.
    copyFileSync(join(dirname(launcher), "..", ".gitignore"), join(root, ".gitignore"));
    writeFileSync(join(root, "tracked.txt"), "baseline\n");
    git(["add", "tracked.txt", ".gitignore"], root);
    // Track the invoked run explicitly: directory order does not identify it.
    await callback(root, (runDir, binary) => { lastChild = { runDir, binary }; });
  } catch (error) {
    if (lastChild && error && typeof error === "object") {
      try {
        const snapshot = fixtureFailureSnapshot(lastChild);
        const stack = error.stack;
        error.message += `\n${snapshot}`;
        // Node may already have materialized the assertion's original stack.
        if (typeof stack === "string") error.stack = `${stack}\n${snapshot}`;
      } catch { /* diagnostic capture must never replace the original failure */ }
    }
    throw error;
  } finally {
    removeFixtureHomes(root);
  }
}


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


function copyLauncher(root, transform = (source) => source) {
  const scripts = join(root, "scripts");
  mkdirSync(scripts);
  const local = join(scripts, "test-launcher.mjs");
  writeFileSync(local, transform(readFileSync(launcher, "utf8")));
  for (const name of launcherSupportFiles) copyFileSync(join(dirname(launcher), name), join(scripts, name));
  return local;
}


function nodeFixtureExecutable(binary) {
  if (process.platform === "darwin") {
    // Homebrew's Node loads libnode relative to its original executable.
    // A regular script keeps permission tests from chmod'ing installed Node.
    const node = "'" + process.execPath.replaceAll("'", "'\\''") + "'";
    writeFileSync(binary, `#!/bin/sh\nexec ${node} "$@"\n`);
  } else copyFileSync(process.execPath, binary);
  if (process.platform !== "win32") chmodSync(binary, 0o700);
}


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

// Every file of a run directory, by name, as bytes.
const directoryBytes = (runDir) => Object.fromEntries(readdirSync(runDir).sort()
  .map((name) => [name, readFileSync(join(runDir, name))]));

// Rewrites a run's results.json as `change` returns it.
const rewriteResults = (runDir, change) => {
  const path = join(runDir, "results.json");
  writeFileSync(path, JSON.stringify(change(json(path)), null, 2));
};


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


  return { env, entryUrl, launcher, launcherSupportFiles, launcherFixtureFiles, stage, json, logPath, git, fixtureFailureSnapshot, repository, rustPhaseArgs, rustWrapperFixture, copyLauncher, nodeFixtureExecutable, parseRecord, directoryBytes, rewriteResults, parkingBroker };
}
