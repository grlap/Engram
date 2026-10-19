import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { appendFileSync, closeSync, existsSync, linkSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const stopProtocol = "windows-job-v1";
const runToken = /^test-[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/u;
const read = (path) => JSON.parse(readFileSync(path, "utf8"));
const terminal = (result) => ["passed", "failed"].includes(result.state)
  && Boolean(result.ended) && Number.isInteger(result.exitCode);
const refused = (message) => Object.assign(new Error(message), { reason: "stop-refused", safeCompletionText: message });
const invalidStopRequest = (message) => Object.assign(new Error(message), {
  reason: "stop-request-invalid", safeCompletionText: "stop-request-invalid",
});
const unreadableStopRequest = (error) => Object.assign(new Error(`cannot read stop request: ${error.code ?? "unknown"}`), {
  reason: "stop-request-unreadable", safeCompletionText: "stop-request-unreadable", cause: error,
});
const transientReadError = (error) => ["EPERM", "EACCES", "EBUSY"].includes(error?.code);
const pause = (ms) => new Promise((done) => setTimeout(done, ms));
function text(value, name, maximum) {
  if (typeof value !== "string" || !value.trim() || Buffer.byteLength(value) > maximum || /[\x00-\x1f\x7f]/u.test(value)) {
    throw refused(`${name} must be nonblank, single-line text of at most ${maximum} bytes`);
  }
  return value;
}
export function readStopIntent(runDir, runId) {
  const path = join(runDir, "stop.request.json");
  let intent;
  try { intent = read(path); }
  catch (error) {
    if (error?.code === "ENOENT") return null;
    if (error instanceof SyntaxError) throw invalidStopRequest("malformed stop request");
    throw error;
  }
  if (intent === null || typeof intent !== "object" || Array.isArray(intent)
    || intent.runId !== runId || intent.protocol !== stopProtocol || typeof intent.id !== "string"
    || !Number.isFinite(Date.parse(intent.requestedAt))) throw invalidStopRequest("malformed stop request");
  try { text(intent.actor, "actor", 256); text(intent.reason, "reason", 2000); }
  catch { throw invalidStopRequest("malformed stop request"); }
  return intent;
}
export async function removeWithRetry(path, { remove = rmSync, attempts = 20, delayMs = 25, ...options } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    try { remove(path, options); return; }
    catch (error) {
      if (!["EPERM", "EACCES", "EBUSY"].includes(error.code) || attempt >= attempts) throw error;
      await pause(delayMs);
    }
  }
}
export async function withStopLock(runDir, action, { remove = rmSync, open = openSync,
  onReleaseError = (error) => process.stderr.write(`stop lock removal failed: ${error.code ?? "unknown"}\n`) } = {}) {
  const path = join(runDir, "stop.control.lock");
  const deadline = Date.now() + 5000;
  while (true) {
    try { closeSync(open(path, "wx", 0o600)); break; }
    catch (error) {
      if (!["EEXIST", "EPERM", "EACCES", "EBUSY"].includes(error.code)) throw error;
      if (Date.now() >= deadline) throw Object.assign(refused(`stop control is busy; retained lock: ${path}`), { stopLockTimeout: true });
      await pause(25);
    }
  }
  try { return await action(); }
  finally {
    // A transient scanner refusal must not cost a committed result. A retained
    // lock is reported, never deleted on an unsafe guess about its owner.
    try { await removeWithRetry(path, { remove }); }
    catch (error) { onReleaseError(error); }
  }
}

// Only publishes intent. A request receipt never asserts that a process ended.
// There is deliberately no PID input or signal sent by this caller.
export async function requestStop(runId, {
  root, actor, reason, now = () => new Date(), remove = rmSync, removeAttempts = 20, removeDelayMs = 25,
}) {
  if (!runToken.test(runId ?? "")) throw refused("stop requires a test run id, not a path or PID");
  text(actor, "actor", 256); text(reason, "reason", 2000);
  const git = spawnSync("git", ["rev-parse", "--path-format=absolute", "--git-path", "review-runs"],
    { cwd: root, encoding: "utf8", windowsHide: true });
  if (git.error || git.status !== 0) throw refused("cannot resolve this worktree's run directory");
  const runDir = join(git.stdout.trim(), runId);
  if (!existsSync(runDir)) throw refused(`run does not exist in this worktree: ${runId}`);
  return withStopLock(runDir, async () => {
    const request = read(join(runDir, "request.json"));
    const result = read(join(runDir, "results.json"));
    if (request.runId !== runId || result.runId !== runId || resolve(request.root) !== resolve(root)) {
      throw refused("run records do not identify this worktree and run id");
    }
    if (terminal(result)) return { requested: false, terminal: true, result, runDir };
    if (request.stopProtocol !== stopProtocol || request.full !== true || request.detached !== true) {
      throw refused("run has no owned Windows job supervisor; legacy, focused and unsupported-platform runs cannot be stopped");
    }
    const prior = readStopIntent(runDir, runId);
    if (prior) return { requested: true, duplicate: true, intent: prior, runDir };
    const intent = { protocol: stopProtocol, runId, id: randomUUID(), actor, reason, requestedAt: now().toISOString() };
    const temporary = join(runDir, `stop-${intent.id}.tmp`);
    let published = false, cleanupWarning;
    try {
      writeFileSync(temporary, `${JSON.stringify(intent)}\n`, { flag: "wx", mode: 0o600 });
      try { linkSync(temporary, join(runDir, "stop.request.json")); }
      catch (error) { throw refused(`cannot retain stop intent on this filesystem: ${error.code ?? "unknown"}`); }
      published = true;
    } finally {
      try { await removeWithRetry(temporary, { remove, attempts: removeAttempts, delayMs: removeDelayMs, force: true }); }
      catch (error) {
        if (published) cleanupWarning = `stop temporary cleanup failed after publication: ${error.code ?? "unknown"}`;
      }
    }
    return { requested: true, duplicate: false, intent, runDir, ...(cleanupWarning ? { warning: cleanupWarning } : {}) };
  }, { remove });
}

export function stopController(runDir, request, { readIntent = readStopIntent, retryAttempts = 3, retryDelayMs = 100 } = {}) {
  let intent = null, cancel = null, failure = null, readFailures = 0;
  const inspect = () => {
    if (intent || failure) return intent;
    try {
      intent = readIntent(runDir, request.runId);
      readFailures = 0;
    } catch (error) {
      readFailures += 1;
      if (transientReadError(error)) {
        if (readFailures >= retryAttempts) failure = unreadableStopRequest(error);
      } else {
        failure = error.reason ? error : unreadableStopRequest(error);
      }
    }
    if (intent || failure) cancel?.();
    return intent;
  };
  const timer = setInterval(inspect, 100);
  timer.unref();
  const throwIfStopped = () => {
    if (failure) throw failure;
    if (intent) throw Object.assign(new Error(`stopped by operator: ${intent.actor}: ${intent.reason}`), { reason: "stopped", stop: intent });
  };
  return {
    inspect,
    register(callback) { cancel = callback; inspect(); return () => { cancel = null; }; },
    check() {
      inspect();
      throwIfStopped();
    },
    async settle() {
      inspect();
      while (!intent && !failure && readFailures > 0 && readFailures < retryAttempts) {
        await pause(retryDelayMs);
        inspect();
      }
      throwIfStopped();
    },
    close() { clearInterval(timer); },
  };
}

// Walk each component before creating the next one. A pre-existing junction
// must be refused before recursive mkdir or the compiler can follow it.
function checkScratchAncestors(cwd, scratch, create) {
  const root = realpathSync(cwd);
  let current = root;
  for (const name of ["target", "tmp", "launcher-jobs", basename(scratch)]) {
    const path = join(current, name);
    let entry;
    try { entry = lstatSync(path); }
    catch (error) {
      if (!create || error.code !== "ENOENT") throw error;
      mkdirSync(path); // the existing parent was checked on the preceding step
      entry = lstatSync(path);
    }
    if (entry.isSymbolicLink() || !entry.isDirectory()) throw new Error(`compiler scratch ancestor is a link or not a directory: ${path}`);
    const actual = realpathSync(path);
    if (actual.toLowerCase() !== path.toLowerCase() || !actual.toLowerCase().startsWith(`${root.toLowerCase()}\\`)) {
      throw new Error("compiler scratch path escaped worktree");
    }
    current = path;
  }
  if (current.toLowerCase() !== scratch.toLowerCase()) throw new Error("compiler scratch root is not canonical");
}

// No raw taskkill, parent-PID snapshots or recovered IDs. The living supervisor
// owns a kernel job handle from before the first workload instruction.
export async function runJobCommand(command, args, { cwd, env, log, runDir, control, powerShell }) {
  if (process.platform !== "win32") throw refused("Windows job containment is unavailable on this platform");
  control.check();
  const receipt = join(runDir, `job-${randomUUID()}.json`);
  const scratch = resolve(cwd, "target", "tmp", "launcher-jobs", randomUUID());
  const helper = fileURLToPath(new URL("./launcher-process-job.ps1", import.meta.url));
  const source = fileURLToPath(new URL("./launcher-process-job.cs", import.meta.url));
  const spec = Buffer.from(JSON.stringify({ command, args, cwd, receipt, source,
    environment: Object.entries(env).filter(([, value]) => value !== undefined).map(([name, value]) => `${name}=${value}`) })).toString("base64");
  const helperEnv = Object.fromEntries(Object.entries(env).filter(([name]) => !["TEMP", "TMP"].includes(name.toUpperCase())));
  // Fixed code only. Paths and payload are data in the helper environment, not
  // interpolated shell text; inline commands also work under Restricted/AllSigned.
  const bootstrap = "& ([ScriptBlock]::Create([IO.File]::ReadAllText($env:ENGRAM_LAUNCHER_JOB_HELPER))) -Spec $env:ENGRAM_LAUNCHER_JOB_SPEC";
  let fd;
  let supervisorStarted = false;
  let unregister = () => {};
  try {
    checkScratchAncestors(cwd, scratch, true);
    checkScratchAncestors(cwd, scratch, false); // verify created location before helper startup
    fd = openSync(log, "wx", 0o600);
    const outcome = await new Promise((done, reject) => {
      const child = spawn(powerShell, ["-NoProfile", "-NonInteractive", "-Command", bootstrap],
        { cwd, env: { ...helperEnv, TEMP: scratch, TMP: scratch,
          ENGRAM_LAUNCHER_JOB_HELPER: helper, ENGRAM_LAUNCHER_JOB_SPEC: spec }, windowsHide: true, stdio: ["pipe", fd, fd] });
      supervisorStarted = Boolean(child.pid);
      child.on("spawn", () => { supervisorStarted = true; });
      child.on("error", reject);
      child.on("close", (code, signal) => done({ code, signal }));
      let sent = false;
      child.stdin?.on("error", () => {}); // A naturally ended supervisor can close first.
      unregister = control.register(() => {
        if (!sent) { sent = true; child.stdin?.end("stop\n"); }
      });
    });
    const proof = read(receipt);
    if (proof.Phase === "helper" && proof.CleanupConfirmed === true && proof.ActiveProcesses === 0) {
      throw Object.assign(new Error(proof.Error ?? "job helper could not start"),
        { reason: "helper-start-failed", helperStartupFailure: true });
    }
    if (proof.CleanupConfirmed !== true || proof.ActiveProcesses !== 0 || !Number.isInteger(proof.ExitCode)
      || outcome.signal || !Number.isInteger(outcome.code) || (outcome.code >>> 0) !== (proof.ExitCode >>> 0)) throw new Error(`job cleanup or exit receipt is unconfirmed: ${receipt}`);
    return { code: proof.ExitCode >>> 0, signal: null, ...(proof.Error ? { error: proof.Error } : {}),
      cleanup: { method: stopProtocol, confirmed: true, activeProcesses: 0, receipt }, stopped: proof.Stopped === true };
  } catch (error) {
    if (error.reason === "stopped") throw error;
    // Missing proof after supervisor startup is uncertainty. A missing disk
    // marker is never evidence that the supervisor did not launch a workload.
    if (supervisorStarted && !error.helperStartupFailure) throw Object.assign(error, { cleanupUnconfirmed: true });
    throw Object.assign(error, { reason: "helper-start-failed" });
  } finally {
    unregister();
    if (fd !== undefined) closeSync(fd);
    try {
      if (existsSync(scratch)) {
        checkScratchAncestors(cwd, scratch, false);
        const inspect = (path) => {
          if (lstatSync(path).isSymbolicLink()) throw new Error("compiler scratch contains a link");
          if (lstatSync(path).isDirectory()) for (const name of readdirSync(path)) inspect(join(path, name));
        };
        inspect(scratch);
        await removeWithRetry(scratch, { recursive: true, force: true });
      }
    } catch (error) {
      // The workload is already settled. Retaining compiler scratch is a
      // diagnostic, never a replacement for its outcome or cleanup proof.
      try { appendFileSync(log, `\ncompiler scratch retained: ${error.message}\n`); } catch { /* log may itself be unavailable */ }
    }
  }
}
