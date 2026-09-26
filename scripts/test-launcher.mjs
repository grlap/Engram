#!/usr/bin/env node
// One execution, durable logs, one completion. Notification retries never run tests.
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { constants, accessSync, closeSync, createReadStream, existsSync, linkSync, mkdirSync,
  openSync, readFileSync, realpathSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { captureFingerprint, fingerprintLimitations } from "./review-freeze-fingerprint.mjs";

const script = fileURLToPath(import.meta.url);
const repository = resolve(dirname(script), "..");
const diagnosticLimit = 2400;
// A run without terminal results is alive while its heartbeat is fresh: the
// launcher refreshes `heartbeat.at` every `heartbeat.everyMs`, and a heartbeat
// older than this many intervals means the launcher has most likely stopped,
// whatever process now holds its pid. The one-second floor normally keeps that
// window wider than the launcher's own synchronous steps; an unusually slow
// input capture can briefly read as interrupted until the next beat.
export const HEARTBEAT_STALE_INTERVALS = 3;
const validHeartbeatEveryMs = (value) => Number.isInteger(value) && value >= 1_000 && value <= 600_000;
function heartbeatEveryMs(env) {
  const raw = env.ENGRAM_LAUNCHER_HEARTBEAT_MS;
  if (raw === undefined || raw === "") return 10_000;
  const value = Number(raw);
  if (!validHeartbeatEveryMs(value)) {
    throw new Error("ENGRAM_LAUNCHER_HEARTBEAT_MS must be an integer from 1000 to 600000");
  }
  return value;
}
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const transientRenameCodes = new Set(["EPERM", "EACCES", "EBUSY"]);
// Windows can refuse a rename for a moment while another process, such as an
// indexer or antivirus scanner, holds the target open. Retry that I/O briefly
// so a transient refusal cannot cost a run its terminal result; this never
// reruns a test.
export function renameWithRetry(from, to, { rename = renameSync, attempts = 20, delayMs = 25 } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    try { rename(from, to); return; }
    catch (error) {
      if (!transientRenameCodes.has(error.code) || attempt >= attempts) throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delayMs);
    }
  }
}
function save(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx", mode: 0o600 });
    renameWithRetry(temporary, path);
  } finally { rmSync(temporary, { force: true }); }
}

export function requiredStages(platform = process.platform) {
  return [
    ["fmt", "cargo", ["fmt", "--check"]],
    ["check", "cargo", ["check"]],
    ["clippy", "cargo", ["clippy", "--all-targets", "--all-features", "--", "-D", "warnings"]],
    ["rust", platform === "win32" ? "pwsh" : "sh", platform === "win32"
      ? ["-NoProfile", "-File", "scripts/test-rust.ps1"] : ["scripts/test-rust.sh"]],
    ["freeze", process.execPath, ["--test", "scripts/review-freeze-fingerprint.test.mjs", "scripts/test-launcher.test.mjs"]],
    ["mcp", process.execPath, ["--test", "scripts/mcp-dogfood.test.mjs"]],
    ["control", process.execPath, ["--test", "scripts/control-dogfood.test.mjs"]],
    ["parity", process.execPath, ["--test", "scripts/parity.test.mjs"]],
    ["docs", process.execPath, ["scripts/check-doc-links.mjs"]],
  ].map(([name, command, args]) => ({ name, command, args }));
}

function executable(command, cwd, env) {
  let lastError;
  const path = Object.entries(env).find(([key]) => key.toLowerCase() === "path")?.[1] ?? "";
  const bases = /[\\/]/u.test(command) || isAbsolute(command)
    ? [resolve(cwd, command)] : path.split(delimiter).map((part) => {
      if (process.platform === "win32" && part.startsWith('"') && part.endsWith('"')) part = part.slice(1, -1);
      return resolve(cwd, part, command);
    });
  // .cmd/.bat require a shell: callers must name that shell explicitly.
  for (const base of bases) {
    for (const candidate of process.platform === "win32" ? [base, `${base}.exe`] : [base]) {
      try {
        if (process.platform === "win32" && !/\.exe$/iu.test(candidate)) continue;
        if (!statSync(candidate).isFile()) continue;
        accessSync(candidate, process.platform === "win32" ? constants.F_OK : constants.X_OK);
        return candidate;
      } catch (error) {
        if (error.code !== "ENOENT") lastError = error;
      }
    }
  }
  throw new Error(`executable not found: ${command}${lastError ? `; last error: ${lastError.message}` : ""}; check PATH or supply the shell explicitly`);
}

export function createRun({ root = repository, stages, notifyTo, requiredBinaryEnv = [], full = false }, env = process.env) {
  const everyMs = heartbeatEveryMs(env);
  const git = spawnSync("git", ["rev-parse", "--path-format=absolute", "--git-path", "review-runs"],
    { cwd: root, encoding: "utf8", windowsHide: true });
  if (git.error || git.status !== 0) throw new Error(`cannot locate Git run directory: ${git.error?.message ?? git.stderr}`);
  const runId = `test-${randomUUID()}`;
  const runDir = join(git.stdout.trim(), runId);
  mkdirSync(runDir, { recursive: true, mode: 0o700 });
  const request = { runId, root: resolve(root), stages, notifyTo, requiredBinaryEnv, full,
    owner: env.TERMAL_SESSION_ID ?? null, started: new Date().toISOString() };
  save(join(runDir, "request.json"), request);
  // A worker that never starts leaves this first heartbeat to go stale.
  save(join(runDir, "results.json"), { runId, state: "running", started: request.started,
    heartbeat: { at: request.started, everyMs }, limitations: fingerprintLimitations(),
    stages: (stages ?? []).map(({ name }) => ({ name, state: "unrun" })) });
  try {
    const input = captureFingerprint(root);
    save(join(runDir, "input.json"), input);
    request.expectedFingerprint = input.fingerprint;
    save(join(runDir, "request.json"), request);
    // The capture blocks the event loop; beat once it returns.
    const created = readJson(join(runDir, "results.json"));
    created.heartbeat.at = new Date().toISOString();
    save(join(runDir, "results.json"), created);
  } catch (error) {
    const result = readJson(join(runDir, "results.json"));
    Object.assign(result, { state: "failed", exitCode: 1, ended: new Date().toISOString(), error: error.message });
    save(join(runDir, "results.json"), result);
    throw new Error(`input capture failed: ${error.message}; results: ${join(runDir, "results.json")}`, { cause: error });
  }
  return runDir;
}

export async function runCommand(command, args, { cwd, env, log }) {
  const fd = openSync(log, "wx", 0o600);
  try {
    return await new Promise((done) => {
      let error;
      const child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ["ignore", fd, fd] });
      child.on("error", (value) => { error = value.message; });
      child.on("close", (code, signal) => done({ code, signal, ...(error ? { error } : {}) }));
    });
  } finally { closeSync(fd); }
}

// Inspect logs once AFTER close, never tail them. Bound memory even for a single
// enormous line; keep the full bytes on disk. Filtering never determines success.
export async function diagnostics(log, failed) {
  let selected = "", errors = "", tail = "", pending = "", matchingBytes = 0, longLine = false;
  let continuation = false;
  let context = 0, errorContext = false;
  const accept = (line) => {
    const clean = line.replace(/\x1b\[[0-9;]*[A-Za-z]/gu, "");
    if (/^\s*(?:ok\s+\d+\b|# Subtest:|test\s+.*\.\.\.\s+ok\b|test result: ok\.|[✔✓])/u.test(clean)) {
      // Concurrent runners can interleave passing rows with failure details.
      // Suppress that row without terminating the bounded diagnostic context.
      return;
    }
    tail = `${tail}${clean}\n`.slice(-diagnosticLimit);
    const nodeWarning = /^\s*(?:#\s*)?\(node:\d+\)\s+(?:\[[^\]\r\n]+\]\s+)?[A-Za-z]*Warning:/u.test(clean);
    const warning = nodeWarning || /\bwarning(?:\b|\[)/iu.test(clean);
    const failure = !nodeWarning && !/^\s*(?:#\s*)?warning(?:\b|\[)/iu.test(clean)
      && /\berror(?:\b|\[)|^\s*(?:test .*\.\.\. FAILED|test result: FAILED|FAIL\s|not ok\b|✖|failures:|thread .*panicked|Caused by:)/iu.test(clean);
    if (warning || failure) { context = 5; errorContext = failure; }
    if (context > 0) {
      const text = `${clean}\n`;
      matchingBytes += text.length;
      if (failed && errorContext) errors += text.slice(0, Math.max(0, diagnosticLimit - errors.length));
      else selected += text.slice(0, Math.max(0, diagnosticLimit - selected.length));
      context -= 1;
    }
  };
  for await (const chunk of createReadStream(log, { encoding: "utf8", highWaterMark: 8192 })) {
    pending += chunk;
    let end;
    while ((end = pending.indexOf("\n")) >= 0) {
      if (!continuation) accept(pending.slice(0, end));
      continuation = false;
      pending = pending.slice(end + 1);
    }
    if (pending.length > 16384) {
      // Retain one bounded prefix per logical line. Never reinterpret a later
      // fragment as a passing-test header or a new failure/context line.
      if (!continuation) accept(pending.slice(0, 16384));
      pending = ""; continuation = true; longLine = true;
    }
  }
  if (pending && !continuation) accept(pending);
  selected = (errors + selected).slice(0, diagnosticLimit);
  const fallback = failed && !selected;
  return { text: selected || (fallback ? tail : ""),
    truncated: longLine || matchingBytes > diagnosticLimit || (fallback && statSync(log).size > diagnosticLimit),
    ...(fallback ? { fallback: "no recognized diagnostic; bounded failure tail" } : {}) };
}

function inputDrift(message, root) {
  const status = spawnSync("git", ["status", "--short"],
    { cwd: root, encoding: "utf8", windowsHide: true });
  const detail = status.error || status.status !== 0
    ? `status unavailable: ${status.error?.message ?? status.stderr}`
    : status.stdout || "(clean; inspect HEAD/index and the saved input)";
  return new Error(`${message}\nCurrent Git status (investigation context, not a since-capture diff):\n`
    + detail.slice(0, diagnosticLimit).trimEnd()
    + (detail.length > diagnosticLimit ? "\n[status truncated]" : ""));
}

function validateNotification(request, env) {
  if (!request.notifyTo) throw new Error("no notification target recorded");
  if (!request.owner || env.TERMAL_SESSION_ID !== request.owner) throw new Error("TERMAL_SESSION_ID must match the sender recorded at run creation (asserted context, not authentication)");
  if (request.notifyTo === request.owner) throw new Error("self-send is not supported; use a genuine worker and coordinator, or foreground host completion wait");
  if (!env.TERMAL_CLI || !isAbsolute(env.TERMAL_CLI)) throw new Error("TERMAL_CLI must name the inherited absolute executable");
  return executable(env.TERMAL_CLI, request.root, env);
}

export async function executeRun(runDir, env = process.env, ready = () => {}) {
  // Exclusive admission: an interrupted run is not a retryable test command.
  // A losing invocation must not overwrite the admitted owner's results.
  closeSync(openSync(join(runDir, "execution.lock"), "wx", 0o600));
  let request, heartbeat;
  let result = { runId: basename(runDir), started: null, stages: [], limitations: fingerprintLimitations() };
  const resultPath = join(runDir, "results.json");
  // Let Rustup read the repository's toolchain file, not an inherited override.
  const childEnv = { ...env };
  const clearedToolchainOverrides = [];
  for (const key of Object.keys(childEnv)) if (key.toUpperCase() === "RUSTUP_TOOLCHAIN") {
    clearedToolchainOverrides.push({ name: key, value: childEnv[key] });
    delete childEnv[key];
  }
  const probe = async (name, command, args) => {
    const log = join(runDir, `preflight-${name}.log`);
    const outcome = await runCommand(command, args, { cwd: request.root, env: childEnv, log });
    result.preflight.push({ name, ...outcome, log,
      diagnostics: await diagnostics(log, outcome.code !== 0 || Boolean(outcome.error)) });
    save(resultPath, result);
    if (outcome.code !== 0 || outcome.error) throw new Error(`${name} preflight failed (${outcome.error ?? outcome.code}); see ${log}`);
  };
  try {
    request = readJson(join(runDir, "request.json"));
    result.started = request.started;
    result.stages = (request.stages ?? []).map(({ name }) => ({ name, state: "unrun" }));
    const saved = readJson(resultPath);
    if (!saved || !Array.isArray(saved.stages)) throw new Error("invalid initial results: stages missing");
    result = saved;
    if (result.state === "failed") return result;
    // The cadence createRun recorded, held to the same bounds as the setting;
    // anything else falls back to this worker's own setting.
    const recorded = result.heartbeat?.everyMs;
    const everyMs = validHeartbeatEveryMs(recorded) ? recorded : heartbeatEveryMs(env);
    Object.assign(result, { pid: process.pid, owner: request.owner, preflight: [], clearedToolchainOverrides,
      heartbeat: { at: new Date().toISOString(), everyMs } });
    save(resultPath, result);
    // Beat on a timer, not only at stage boundaries, so a long stage stays
    // visibly alive. A reader holding results.json open can make one save fail
    // on Windows; the next beat retries, and a missed beat never fails the run.
    heartbeat = setInterval(() => {
      result.heartbeat.at = new Date().toISOString();
      try { save(resultPath, result); } catch { /* retried on the next beat */ }
    }, everyMs);
    heartbeat.unref();
    await ready();
    if (!Array.isArray(request.stages) || request.stages.length === 0) throw new Error("at least one stage is required");
    const names = new Set();
    for (const stage of request.stages) {
      if (!/^[a-zA-Z0-9_-]+$/u.test(stage.name) || names.has(stage.name)) throw new Error("stage names must be unique safe filenames");
      names.add(stage.name);
      if (typeof stage.command !== "string" || !Array.isArray(stage.args) || !stage.args.every((arg) => typeof arg === "string")) throw new Error("stage must contain a command and string arguments");
      stage.command = executable(stage.command, request.root, childEnv);
    }
    if (request.notifyTo) validateNotification(request, env);
    for (const name of request.requiredBinaryEnv) {
      if (!/^[A-Z][A-Z0-9_]*$/u.test(name)) throw new Error("invalid binary environment variable name");
      if (!env[name] || !isAbsolute(env[name])) throw new Error(`required binary environment variable ${name} must be an absolute executable path`);
      let binary;
      try { binary = executable(env[name], request.root, childEnv); }
      catch (error) { throw new Error(`${name}: ${error.message}`); }
      await probe(name, binary, ["--version"]);
    }
    if (request.full) {
      // Probe toolchain components before the expensive suite, preserving diagnostics.
      for (const [index, args] of [["version", ["--version"]], ["fmt", ["fmt", "--version"]], ["clippy", ["clippy", "--version"]]]) {
        await probe(`cargo-${index}`, executable("cargo", request.root, childEnv), args);
      }
    }
    const before = captureFingerprint(request.root);
    if (before.fingerprint !== request.expectedFingerprint) throw inputDrift("input drift before execution; no stages run", request.root);
    result.expectedFingerprint = request.expectedFingerprint;
    result.before = before.fingerprint;
    // The capture blocked the timer; beat with this save.
    result.heartbeat.at = new Date().toISOString();
    save(resultPath, result);
    for (let index = 0; index < request.stages.length; index += 1) {
      const stage = request.stages[index];
      const entry = result.stages[index];
      Object.assign(entry, { state: "running", started: new Date().toISOString(), command: [stage.command, ...stage.args], log: join(runDir, `${stage.name}.log`) });
      save(resultPath, result);
      const outcome = await runCommand(stage.command, stage.args, { cwd: request.root, env: childEnv, log: entry.log });
      Object.assign(entry, outcome, { ended: new Date().toISOString(), state: outcome.code === 0 && !outcome.error ? "passed" : "failed" });
      entry.diagnostics = await diagnostics(entry.log, entry.state === "failed");
      save(resultPath, result);
      if (entry.state === "failed") break;
    }
    result.after = captureFingerprint(request.root).fingerprint;
    if (result.after !== result.expectedFingerprint) throw inputDrift("input drift: results do not validate the current source", request.root);
    result.state = result.stages.every((stage) => stage.state === "passed") ? "passed" : "failed";
    result.exitCode = result.stages.find((stage) => stage.state === "failed")?.code || (result.state === "passed" ? 0 : 1);
  } catch (error) {
    result.state = "failed"; result.exitCode = 1; result.error = error.message;
    for (const stage of result.stages) if (stage.state === "running") { stage.state = "failed"; stage.error = error.message; }
  }
  clearInterval(heartbeat);
  result.ended = new Date().toISOString();
  save(resultPath, result);
  return result;
}

// How a reader tells a run's state from results.json alone. The pid is never
// consulted: after a kill it may already belong to an unrelated process.
export function runLiveness(result, now = Date.now()) {
  if (["passed", "failed"].includes(result.state) && result.ended && Number.isInteger(result.exitCode)) {
    return result.state;
  }
  const at = Date.parse(result.heartbeat?.at ?? "");
  const everyMs = result.heartbeat?.everyMs;
  if (!Number.isFinite(at) || !Number.isInteger(everyMs) || everyMs <= 0) return "unknown";
  return now - at > HEARTBEAT_STALE_INTERVALS * everyMs ? "interrupted" : "running";
}

export async function summarize(runDir, { now = Date.now() } = {}) {
  const result = readJson(join(runDir, "results.json"));
  const liveness = runLiveness(result, now);
  const terminal = liveness === "passed" || liveness === "failed";
  const status = {
    passed: "PASS",
    failed: "FAIL",
    running: `RUNNING (no terminal result yet; heartbeat ${result.heartbeat?.at})`,
    interrupted: `INTERRUPTED (no terminal result and no heartbeat since ${result.heartbeat?.at}, more than ${HEARTBEAT_STALE_INTERVALS} × ${result.heartbeat?.everyMs} ms ago; the launcher has most likely stopped)`,
    unknown: "UNKNOWN (no terminal result and no heartbeat; running or interrupted)",
  }[liveness];
  const lines = [`${status} ${result.runId} exit=${terminal ? result.exitCode : "unknown"}`,
    `results: ${join(runDir, "results.json")}`];
  if (result.error) lines.push(`runner: ${result.error.slice(0, diagnosticLimit)}`);
  if (result.clearedToolchainOverrides?.length) lines.push("toolchain: inherited Rustup override cleared for repository selection; original value in results.json");
  let remaining = 6000;
  // Failure excerpts get first use of the shared budget; earlier successful
  // commands may emit warnings but must not crowd out the actual failure.
  const entries = [
    ...(result.preflight ?? []).map((probe) => ({
      ...probe, failed: probe.code !== 0 || Boolean(probe.error),
      header: `preflight ${probe.name}: exit=${probe.code} log=${probe.log}`,
    })),
    ...result.stages.map((stage) => ({ ...stage, failed: stage.state === "failed",
      header: `${stage.name}: ${stage.state} exit=${stage.code ?? "unrun/unknown"}${stage.log ? ` log=${stage.log}` : ""}`,
    })),
  ].sort((a, b) => Number(b.failed) - Number(a.failed));
  for (const stage of entries) {
    lines.push(stage.header);
    if (stage.error && stage.error !== result.error) lines.push(stage.error.slice(0, diagnosticLimit));
    if (stage.diagnostics?.text) {
      const text = stage.diagnostics.text.trimEnd();
      if (remaining > 0) lines.push(text.slice(0, remaining));
      if (text.length > remaining) lines.push("[summary diagnostics truncated; full output in log]");
      remaining = Math.max(0, remaining - text.length);
    }
    if (stage.diagnostics?.truncated) lines.push("[diagnostics truncated; full output in log]");
  }
  if (result.limitations) lines.push(...[result.limitations].flat());
  return `${lines.join("\n")}\n`;
}

export async function notifyRun(runDir, env = process.env, send = runCommand) {
  const request = readJson(join(runDir, "request.json"));
  const result = readJson(join(runDir, "results.json"));
  if (!["passed", "failed"].includes(result.state) || !result.ended) throw new Error("cannot notify before terminal results are saved");
  const cli = validateNotification(request, env);
  const messageFile = join(runDir, "notification.message.txt");
  if (!existsSync(messageFile)) {
    const temporary = `${messageFile}.${randomUUID()}.tmp`;
    try {
      writeFileSync(temporary, await summarize(runDir), { flag: "wx", mode: 0o600 });
      // Publish complete bytes without replacing a body another sender froze.
      // An interrupted write leaves only a temporary file, never a retry body.
      try { linkSync(temporary, messageFile); }
      catch (error) { if (error.code !== "EEXIST") throw error; }
    } finally { rmSync(temporary, { force: true }); }
  }
  const args = ["mailbox", "send", "--to", request.notifyTo, "--message-file", messageFile,
    "--idempotency-key", `engram-tests:${request.runId}`, "--json"];
  const log = join(runDir, `notification-${randomUUID()}.log`);
  const outcome = await send(cli, args, { cwd: request.root, env, log });
  const receipt = { ...outcome, log, attempted: new Date().toISOString() };
  save(join(runDir, "notification.json"), receipt);
  if (outcome.code !== 0 || outcome.error) throw new Error(`notification failed; tests were NOT rerun. Retry: node scripts/test-launcher.mjs notify "${runDir}"; log=${log}`);
  return receipt;
}

function startupReceipt(runDir, pid, completion) {
  const request = readJson(join(runDir, "request.json"));
  return `STARTED ${runDir}\npid=${pid} completion=${completion}\nmanifest=${join(runDir, "input.json")}\nexpectedFingerprint=${request.expectedFingerprint}\n`;
}

async function finish(runDir, { foregroundReceipt = false, workerHandshake = false } = {}) {
  const result = await executeRun(runDir, process.env, async () => {
    if (foregroundReceipt) process.stdout.write(startupReceipt(runDir, process.pid, "host-process-wait"));
    if (workerHandshake && process.send) {
      await new Promise((done, reject) => process.send({ ready: true }, (error) => error ? reject(error) : done()));
      process.disconnect();
    }
  });
  process.stdout.write(await summarize(runDir));
  process.exitCode = result.exitCode;
  if (readJson(join(runDir, "request.json")).notifyTo) {
    try { await notifyRun(runDir); }
    catch (error) { process.stderr.write(`${error.message}\n`); process.exitCode ||= 1; }
  }
}

export async function startDetached(runDir, env = process.env, write = (text) => process.stdout.write(text)) {
  const request = readJson(join(runDir, "request.json"));
  validateNotification(request, env);
  // No inherited terminal handles: process lifetime is independent of the turn.
  const fd = openSync(join(runDir, "launcher.log"), "wx", 0o600);
  try {
    const child = spawn(process.execPath, [script, "_run", runDir],
      { detached: true, windowsHide: true, stdio: ["ignore", fd, fd, "ipc"], env });
    // The completion handle also lets fixtures await child exit before cleanup.
    const completion = new Promise((done) => {
      let error;
      child.once("error", (value) => { error = value.message; });
      child.once("close", (code, signal) => done({ code, signal, error }));
    });
    // One startup handshake, not test polling. Early admission/read errors are
    // returned to the caller before it is told to yield; EEXIST preserves owner.
    await new Promise((done, reject) => {
      child.once("error", reject);
      const failed = (code) => reject(new Error(`worker startup failed (${code}); see ${join(runDir, "launcher.log")}`));
      // Wait for channel closure, not just process exit: a readiness message
      // can still be pending when the process has already ended.
      child.once("close", failed);
      child.once("message", (message) => {
        if (message.ready !== true) { reject(new Error("invalid worker startup response")); return; }
        child.removeListener("close", failed);
        done();
      });
    });
    child.unref();
    write(startupReceipt(runDir, child.pid, `mailbox:${request.notifyTo}`)
      + "End your turn; do not poll. Missing terminal results mean running/interrupted, not PASS; "
      + `\`node scripts/test-launcher.mjs summary "${runDir}"\` tells which from the heartbeat.\n`);
    return { child, completion };
  } finally { closeSync(fd); }
}

async function main(args) {
  const mode = args.shift();
  if (["summary", "notify", "_run"].includes(mode)) {
    if (args.length !== 1) throw new Error(`${mode} requires one run directory`);
    const runDir = resolve(args[0]);
    if (mode === "summary") { process.stdout.write(await summarize(runDir)); return; }
    if (mode === "notify") { await notifyRun(runDir); console.log(`Notification sent; tests not rerun. ${runDir}`); return; }
    await finish(runDir, { workerHandshake: true }); return;
  }
  if (!["full", "focused"].includes(mode)) throw new Error("usage: test-launcher.mjs full|focused [--notify SESSION] [--detach] [--require-binary-env NAME] [-- COMMAND ARGS...] | summary|notify RUN_DIR");
  let notifyTo, detach = false;
  const requiredBinaryEnv = [];
  while (args.length && args[0] !== "--") {
    const flag = args.shift();
    if (flag === "--detach") detach = true;
    else if (flag === "--notify") {
      if (notifyTo !== undefined) throw new Error("--notify may be supplied only once");
      if (!args[0]?.trim() || args[0].trim().startsWith("-")) throw new Error("--notify requires a session id (nonblank, not a flag)");
      notifyTo = args.shift().trim();
    }
    else if (flag === "--require-binary-env") {
      if (!/^[A-Z][A-Z0-9_]*$/u.test(args[0] ?? "")) throw new Error("--require-binary-env requires an uppercase variable name");
      const name = args.shift();
      if (requiredBinaryEnv.includes(name)) throw new Error(`duplicate binary prerequisite: ${name}`);
      requiredBinaryEnv.push(name);
    }
    else throw new Error(`invalid option: ${flag}`);
  }
  if (args[0] === "--") args.shift();
  if ((mode === "full" && args.length) || (mode === "focused" && !args.length)) throw new Error("full takes no command; focused requires -- COMMAND ARGS");
  if (detach && !notifyTo) throw new Error("detached mode requires a different coordinator --notify SESSION; otherwise use foreground host completion wait");
  if (notifyTo) validateNotification({ notifyTo, owner: process.env.TERMAL_SESSION_ID, root: repository }, process.env);
  const stages = mode === "full" ? requiredStages() : [{ name: "focused", command: args[0], args: args.slice(1) }];
  const runDir = createRun({ stages, notifyTo, requiredBinaryEnv, full: mode === "full" });
  if (!detach) { await finish(runDir, { foregroundReceipt: true }); return; }
  await startDetached(runDir);
}

// Node resolves this module's own path through links (macOS temp paths run
// through /var -> /private/var), so resolve the invoked path the same way.
if (process.argv[1] && realpathSync(process.argv[1]) === script) {
  main(process.argv.slice(2)).catch((error) => { console.error(`FAIL launcher: ${error.message}`); process.exitCode = 1; });
}
