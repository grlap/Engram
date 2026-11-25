import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { appendFileSync, closeSync, existsSync, linkSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { asOneOperation, insideOperation, namedIfSlow, nextAttempt, operationMinimumMs, warnAfterMs, warningSink, warnWhileBlocking, warnWhileRunning } from "./operation-warning.mjs";

export const stopProtocol = "windows-job-v1";
export const posixStopProtocol = "posix-group-v1";
const runToken = /^test-[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/u;
// A run record or receipt read that returns after ten seconds is named as it returns.
const read = (path) => namedIfSlow("run record read", () => JSON.parse(readFileSync(path, "utf8")));
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
export function readStopIntent(runDir, runId, protocol = stopProtocol) {
  const path = join(runDir, "stop.request.json");
  let intent;
  // Read without its own warning: the stop controller's read episode and the
  // stop request publication each name this read once.
  try { intent = JSON.parse(readFileSync(path, "utf8")); }
  catch (error) {
    if (error?.code === "ENOENT") return null;
    if (error instanceof SyntaxError) throw invalidStopRequest("malformed stop request");
    throw error;
  }
  if (intent === null || typeof intent !== "object" || Array.isArray(intent)
    || intent.runId !== runId || intent.protocol !== protocol || typeof intent.id !== "string"
    || !Number.isFinite(Date.parse(intent.requestedAt))) throw invalidStopRequest("malformed stop request");
  try { text(intent.actor, "actor", 256); text(intent.reason, "reason", 2000); }
  catch { throw invalidStopRequest("malformed stop request"); }
  return intent;
}
// A removal another program refuses for a moment, such as a scanner holding
// the file, is retried for the shared monotonic minute, named once after ten
// seconds. `attempts` only caps the tries a caller allows.
export async function removeWithRetry(path, { remove = rmSync, windowMs = operationMinimumMs, attempts = Infinity,
  delayMs = 25, now = () => performance.now(), operation = "file removal", warnAfter = warnAfterMs, ...options } = {}) {
  const started = now();
  const settled = warnWhileRunning(operation, { after: warnAfter, now });
  try {
    for (let attempt = 1; ; attempt += 1) {
      try { remove(path, options); return; }
      catch (error) {
        if (!["EPERM", "EACCES", "EBUSY"].includes(error.code) || attempt >= attempts
          || now() - started >= windowMs) throw error;
        await pause(delayMs);
      }
    }
  } finally { settled(); }
}
export async function withStopLock(runDir, action, { remove = rmSync, open = openSync, releaseWindowMs = operationMinimumMs,
  onReleaseError = (error) => process.stderr.write(`stop lock removal failed: ${error.code ?? "unknown"}\n`) } = {}) {
  const path = join(runDir, "stop.control.lock");
  // Product behaviour: how long a stop or a terminal save waits on this lock.
  // The monotonic clock, so a wall-clock step cannot end the wait early.
  const deadline = performance.now() + operationMinimumMs;
  const settled = warnWhileRunning("stop control lock wait");
  try {
    while (true) {
      try { closeSync(open(path, "wx", 0o600)); break; }
      catch (error) {
        if (!["EEXIST", "EPERM", "EACCES", "EBUSY"].includes(error.code)) throw error;
        if (performance.now() >= deadline) throw Object.assign(refused(`stop control is busy; retained lock: ${path}`), { stopLockTimeout: true });
        await pause(25);
      }
    }
  } finally { settled(); }
  try { return await action(); }
  finally {
    // A transient scanner refusal must not cost a committed result. A retained
    // lock is reported, never deleted on an unsafe guess about its owner.
    try { await removeWithRetry(path, { remove, windowMs: releaseWindowMs, operation: "stop control lock release" }); }
    catch (error) { onReleaseError(error); }
  }
}

// Only publishes intent. A request receipt never asserts that a process ended.
// There is deliberately no PID input or signal sent by this caller.
export async function requestStop(runId, {
  root, actor, reason, now = () => new Date(), remove = rmSync, removeAttempts = Infinity, removeDelayMs = 25,
}) {
  if (!runToken.test(runId ?? "")) throw refused("stop requires a test run id, not a path or PID");
  text(actor, "actor", 256); text(reason, "reason", 2000);
  // A Git subprocess, and the probe of the directory it names, can be slow on
  // a busy host; the lookup is named once after ten seconds.
  const { git, runDir, exists } = warnWhileBlocking("run directory lookup", () => {
    const git = spawnSync("git", ["rev-parse", "--path-format=absolute", "--git-path", "review-runs"],
      { cwd: root, encoding: "utf8", windowsHide: true });
    if (git.error || git.status !== 0) return { git };
    const runDir = join(git.stdout.trim(), runId);
    return { git, runDir, exists: existsSync(runDir) };
  });
  if (git.error || git.status !== 0) throw refused("cannot resolve this worktree's run directory");
  if (!exists) throw refused(`run does not exist in this worktree: ${runId}`);
  return withStopLock(runDir, async () => {
    // Reading the run's records and publishing the intent is one operation,
    // named once if it passes ten seconds; the temporary cleanup after it
    // names its own wait.
    let outcome, temporary, published = false, cleanupWarning;
    try {
      outcome = await asOneOperation("stop request publication", () => {
        const request = read(join(runDir, "request.json"));
        const result = read(join(runDir, "results.json"));
        if (request.runId !== runId || result.runId !== runId || resolve(request.root) !== resolve(root)) {
          throw refused("run records do not identify this worktree and run id");
        }
        if (terminal(result)) return { settled: { requested: false, terminal: true, result, runDir } };
        const windows = request.stopProtocol === stopProtocol && request.full === true;
        const posix = request.stopProtocol === posixStopProtocol;
        if ((!windows && !posix) || request.detached !== true) {
          throw refused("run has no owned Windows job or POSIX process-group supervisor; legacy and unsupported runs cannot be stopped");
        }
        const prior = readStopIntent(runDir, runId, request.stopProtocol);
        if (prior) return { settled: { requested: true, duplicate: true, intent: prior, runDir } };
        const intent = { protocol: request.stopProtocol, runId, id: randomUUID(), actor, reason, requestedAt: now().toISOString() };
        temporary = join(runDir, `stop-${intent.id}.tmp`);
        writeFileSync(temporary, `${JSON.stringify(intent)}\n`, { flag: "wx", mode: 0o600 });
        try { linkSync(temporary, join(runDir, "stop.request.json")); }
        catch (error) { throw refused(`cannot retain stop intent on this filesystem: ${error.code ?? "unknown"}`); }
        published = true;
        return { intent };
      });
    } finally {
      if (temporary) {
        try { await removeWithRetry(temporary, { remove, attempts: removeAttempts, delayMs: removeDelayMs, force: true,
          operation: "stop temporary cleanup" }); }
        catch (error) {
          if (published) cleanupWarning = `stop temporary cleanup failed after publication: ${error.code ?? "unknown"}`;
        }
      }
    }
    if (outcome.settled) return outcome.settled;
    return { requested: true, duplicate: false, intent: outcome.intent, runDir, ...(cleanupWarning ? { warning: cleanupWarning } : {}) };
  }, { remove });
}

// A stop intent that cannot be read for a moment, as when a scanner holds the
// file, is retried: only one unreadable for the shared minute ends the run,
// with one warning after ten seconds. A readable intent acts at once.
export function stopController(runDir, request, { readIntent = readStopIntent, retryWindowMs = operationMinimumMs,
  retryDelayMs = 100, now = () => performance.now(), warn = (text) => { try { warningSink.write(text); } catch { /* never fails the run */ } } } = {}) {
  let intent = null, cancel = null, failure = null, failingSince = null, warned = false;
  const inspect = () => {
    if (intent || failure) return intent;
    // Checked whenever an episode's time is read, on a refusal, on the read
    // that ends it and at the end of the window, so one that lasted ten
    // seconds or more is named once.
    const warnOnce = (failing) => {
      if (warned || failing < warnAfterMs) return;
      warned = true;
      warn(`warning: stop request read is still running after 10 seconds; attempt=${nextAttempt()}; elapsed_ms=${Math.round(failing)}\n`);
    };
    // An episode starts with its first read, so one slow read counts too.
    const began = now();
    try {
      intent = readIntent(runDir, request.runId, request.stopProtocol ?? stopProtocol);
      // A read that succeeds ends the episode; a later one starts afresh.
      warnOnce(now() - (failingSince ?? began));
      failingSince = null;
      warned = false;
    } catch (error) {
      if (transientReadError(error)) {
        failingSince ??= began;
        const failing = now() - failingSince;
        warnOnce(failing);
        if (failing >= retryWindowMs) failure = unreadableStopRequest(error);
      } else {
        // A permanent error, alone or after transient refusals, ends the episode.
        warnOnce(now() - (failingSince ?? began));
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
      while (!intent && !failure && failingSince !== null) {
        await pause(retryDelayMs);
        inspect();
      }
      throwIfStopped();
    },
    close() { clearInterval(timer); },
  };
}

// Called only after the live child handle has reported helper closure. Never
// send a terminating signal to this numeric group after its leader has exited.
// Confirmation waits the shared minute, naming itself once after ten seconds.
export async function waitForPosixGroupExit(pgid, { probe = (pid) => process.kill(-pid, 0),
  timeoutMs = operationMinimumMs, now = () => performance.now(), sleep = pause } = {}) {
  const started = now(), deadline = started + timeoutMs;
  let warned = false;
  const warnOnce = () => {
    const waited = now() - started;
    // Inside an operation that is named already, such as a preflight command,
    // that operation's warning covers this wait.
    if (warned || waited < warnAfterMs || insideOperation()) return;
    warned = true;
    try { warningSink.write(`warning: process group cleanup confirmation is still running after 10 seconds; attempt=${nextAttempt()}; elapsed_ms=${Math.round(waited)}\n`); }
    catch { /* observation never fails the confirmation */ }
  };
  while (true) {
    try { probe(pgid); }
    catch (error) {
      if (error.code === "ESRCH") { warnOnce(); return; }
      if (error.code !== "EPERM") { warnOnce(); throw error; }
    }
    warnOnce();
    if (now() >= deadline) throw new Error("owned POSIX process group disappearance is unconfirmed");
    await sleep(Math.min(25, Math.max(0, deadline - now())));
  }
}

export async function runPosixCommand(command, args, { cwd, env, log, runDir, control }) {
  if (!["darwin", "linux"].includes(process.platform)) throw refused("POSIX process groups are unavailable on this platform");
  control.check();
  const receipt = join(runDir, `group-${randomUUID()}.json`);
  const helper = fileURLToPath(new URL("./launcher-process-group.mjs", import.meta.url));
  const fd = namedIfSlow("command log creation", () => openSync(log, "wx", 0o600));
  let unregister = () => {}, child, admitted = false, stopped = false, deadline, closing = () => {};
  // From the spawn to the helper's ready message: a cold start past ten
  // seconds is named. Settled on readiness, error or exit.
  const starting = warnWhileRunning("POSIX helper startup");
  try {
    const outcome = await new Promise((done, reject) => {
      // Inside an operation the parent already names, such as a preflight probe,
      // the helper names nothing itself.
      const spec = Buffer.from(JSON.stringify({ command, args, cwd, receipt, ownedByParent: insideOperation() })).toString("base64");
      child = spawn(process.execPath, [helper, spec], { cwd, env, detached: true,
        stdio: ["ignore", fd, fd, "ipc"] });
      const send = (message) => {
        if (child.connected) child.send(message, () => {});
      };
      // The stop arrives from a poll outside this call's operation, so whether
      // an enclosing operation already names the wait is taken here.
      const owned = insideOperation();
      unregister = control.register(() => {
        if (stopped) return;
        stopped = true;
        send({ kind: "stop" });
        // The shared minute for the helper's own cleanup, plus its one-second
        // TERM-to-KILL step.
        closing = owned ? () => false : warnWhileRunning("POSIX helper closure after stop");
        deadline = setTimeout(() => {
          closing();
          child.unref();
          if (child.connected) child.disconnect();
          reject(Object.assign(new Error("POSIX helper closure is unconfirmed"), { cleanupUnconfirmed: true }));
        }, operationMinimumMs + 1_000);
      });
      child.once("message", (message) => {
        starting();
        if (message?.ready !== true) { reject(new Error("invalid POSIX helper startup response")); return; }
        if (stopped) send({ kind: "stop" });
        else { admitted = true; send({ kind: "start" }); }
      });
      child.once("error", (error) => { starting(); reject(error); });
      // Reap the helper independently of IPC EOF: escaped descendants must
      // not keep a completed command waiting for inherited descriptors.
      child.once("exit", (code, signal) => {
        starting();
        clearTimeout(deadline);
        closing();
        child.channel?.unref();
        if (child.connected) child.disconnect();
        done({ code, signal });
      });
    });
    // Closure/reaping precedes every group probe. Signal zero only observes;
    // EPERM must eventually become ESRCH, never merely be treated as success.
    await waitForPosixGroupExit(child.pid);
    const proof = read(receipt);
    if (proof.protocol !== posixStopProtocol || proof.pid !== child.pid || proof.cleanupStarted !== true
      || (proof.terminationError && proof.terminationError !== "EPERM")
      || (!proof.terminationError && outcome.signal !== "SIGKILL")
      || !(Number.isInteger(proof.code) || proof.code === null)) throw new Error("POSIX helper outcome or cleanup receipt is unconfirmed");
    const unsolicited = proof.stopped === true && !stopped;
    return { code: unsolicited ? 1 : proof.code, signal: proof.signal,
      ...(unsolicited ? { error: "POSIX helper stopped without a controller request" }
        : proof.error ? { error: proof.error } : {}),
      cleanup: { method: posixStopProtocol, scope: "process-group", confirmed: true, activeProcesses: 0, receipt },
      stopped: proof.stopped === true };
  } catch (error) {
    // Closing our private channel asks the living helper to clean its own
    // group. No recovered numeric PID is ever used as a fallback killer.
    if (child) { child.unref(); if (child.connected) child.disconnect(); }
    if (admitted || error.cleanupUnconfirmed) throw Object.assign(error, { cleanupUnconfirmed: true });
    throw Object.assign(error, { reason: "helper-start-failed" });
  } finally { starting(); clearTimeout(deadline); closing(); unregister(); closeSync(fd); }
}

// Walk each component before creating the next one. A pre-existing junction
// must be refused before recursive mkdir or the compiler can follow it.
// Its callers name the preparation or inspection it belongs to.
function checkScratchAncestors(cwd, scratch, create, directory = "launcher-jobs") {
  const root = realpathSync(cwd);
  let current = root;
  for (const name of ["target", "tmp", directory, basename(scratch)]) {
    const path = join(current, name);
    let entry;
    try { entry = lstatSync(path); }
    catch (error) {
      if (!create || error.code !== "ENOENT") throw error;
      // Another launcher or Cargo may have created this component since lstat.
      // Inspect that entry below just as strictly as one we created ourselves.
      try { mkdirSync(path); }
      catch (creationError) { if (creationError.code !== "EEXIST") throw creationError; }
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

// PowerShell can write its module cache before the workload starts. Pin it
// before spawning either the identity query or the job supervisor, without
// changing the caller's AppData, profile, or process environment.
export function powerShellEnvironment(cwd, key, env) {
  const childEnv = { ...env };
  if (process.platform !== "win32") return childEnv;
  if (!/^[a-z0-9][a-z0-9-]*$/u.test(key)) throw new Error("invalid PowerShell scratch key");
  // One preparation: resolving the root, creating and checking the scratch
  // path and checking the cache, named once if together they pass ten seconds.
  const cache = namedIfSlow("PowerShell scratch preparation", () => {
    const scratch = join(realpathSync(cwd), "target", "tmp", "launcher-powershell", key);
    checkScratchAncestors(cwd, scratch, true, "launcher-powershell");
    checkScratchAncestors(cwd, scratch, false, "launcher-powershell");
    const cache = join(scratch, "ModuleAnalysisCache");
    try {
      const entry = lstatSync(cache);
      if (entry.isSymbolicLink() || !entry.isFile()) throw new Error("PowerShell cache must be a regular file");
    } catch (error) { if (error.code !== "ENOENT") throw error; }
    return cache;
  });
  for (const name of Object.keys(childEnv)) if (name.toUpperCase() === "PSMODULEANALYSISCACHEPATH") delete childEnv[name];
  childEnv.PSModuleAnalysisCachePath = cache;
  return childEnv;
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
  // Fixed code only. Paths and payload are data in the helper environment, not
  // interpolated shell text; inline commands also work under Restricted/AllSigned.
  const bootstrap = "& ([ScriptBlock]::Create([IO.File]::ReadAllText($env:ENGRAM_LAUNCHER_JOB_HELPER))) -Spec $env:ENGRAM_LAUNCHER_JOB_SPEC";
  let fd;
  let supervisorStarted = false;
  let unregister = () => {};
  try {
    const childEnv = powerShellEnvironment(cwd, basename(runDir), env);
    const spec = Buffer.from(JSON.stringify({ command, args, cwd, receipt, source, ownedByParent: insideOperation(),
      environment: Object.entries(childEnv).filter(([, value]) => value !== undefined).map(([name, value]) => `${name}=${value}`) })).toString("base64");
    const helperEnv = Object.fromEntries(Object.entries(childEnv).filter(([name]) => !["TEMP", "TMP"].includes(name.toUpperCase())));
    // Creating the scratch path and verifying it before helper startup is one
    // preparation, named once if it passes ten seconds.
    namedIfSlow("compiler scratch preparation", () => {
      checkScratchAncestors(cwd, scratch, true);
      checkScratchAncestors(cwd, scratch, false);
    });
    fd = namedIfSlow("command log creation", () => openSync(log, "wx", 0o600));
    const outcome = await new Promise((done, reject) => {
      // Creating the supervisor process is synchronous and can block on a cold load.
      // The helper times its own startup from this spawn on the Windows
      // performance counter, which Node's high-resolution time also reads.
      const child = namedIfSlow("command start", () => spawn(powerShell, ["-NoProfile", "-NonInteractive", "-Command", bootstrap],
        { cwd, env: { ...helperEnv, TEMP: scratch, TMP: scratch,
          ENGRAM_LAUNCHER_JOB_HELPER: helper, ENGRAM_LAUNCHER_JOB_SPEC: spec,
          ENGRAM_LAUNCHER_JOB_STARTED_NS: process.hrtime.bigint().toString() }, windowsHide: true, stdio: ["pipe", fd, fd] }));
      // After a stop request, the wait for the supervisor to clean up and close
      // is named once after ten seconds, as the POSIX path names its own.
      let closing = () => {};
      supervisorStarted = Boolean(child.pid);
      child.on("spawn", () => { supervisorStarted = true; });
      child.on("error", reject);
      child.on("close", (code, signal) => { closing(); done({ code, signal }); });
      let sent = false;
      child.stdin?.on("error", () => {}); // A naturally ended supervisor can close first.
      // The stop arrives from a poll outside this call's operation, so whether
      // an enclosing operation already names the wait is taken here.
      const owned = insideOperation();
      unregister = control.register(() => {
        if (sent) return;
        sent = true;
        closing = owned ? () => false : warnWhileRunning("job helper closure after stop");
        child.stdin?.end("stop\n");
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
      const present = namedIfSlow("compiler scratch inspection", () => {
        if (!existsSync(scratch)) return false;
        checkScratchAncestors(cwd, scratch, false);
        const inspect = (path) => {
          if (lstatSync(path).isSymbolicLink()) throw new Error("compiler scratch contains a link");
          if (lstatSync(path).isDirectory()) for (const name of readdirSync(path)) inspect(join(path, name));
        };
        inspect(scratch);
        return true;
      });
      if (present) {
        await removeWithRetry(scratch, { recursive: true, force: true, operation: "compiler scratch cleanup" });
      }
    } catch (error) {
      // The workload is already settled. Retaining compiler scratch is a
      // diagnostic, never a replacement for its outcome or cleanup proof.
      try { namedIfSlow("command log append", () => appendFileSync(log, `\ncompiler scratch retained: ${error.message}\n`)); } catch { /* log may itself be unavailable */ }
    }
  }
}
