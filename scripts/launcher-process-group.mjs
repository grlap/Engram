// A living session/group leader owns every terminating signal. The worker
// sends intent over IPC and, after this helper exits, only observes absence.
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { exitFlushMs, namedIfSlow, warningSink, warnWhileBlocking } from "./operation-warning.mjs";

// Warnings go to the stage log off the thread doing the work. Before this
// helper ends its group or itself, a warning still on its way out gets at
// most exitFlushMs, best effort; the end is never held longer for it.

const spec = JSON.parse(Buffer.from(process.argv[2], "base64").toString("utf8"));
// Inside an operation the parent already names, such as a preflight probe,
// this helper names nothing itself.
if (spec.ownedByParent) warningSink.write = () => {};
let child, started = false, cleaning = false, stopping = false, completed = false;
let outcome = { code: null, signal: null };
let timer;

// The receipt is one small local write, and this helper ends its own group
// right after it. A receipt write still running after ten seconds is named
// by a watcher thread while it runs, or as it returns.
function writeReceipt(fields, flag) {
  const write = () => writeFileSync(spec.receipt, `${JSON.stringify({ protocol: "posix-group-v1", pid: process.pid,
    cleanupStarted: true, stopped: stopping, ...outcome, ...fields })}\n`, { flag, mode: 0o600 });
  return spec.ownedByParent ? write() : warnWhileBlocking("process group receipt write", write);
}
function receipt(extra = {}) { writeReceipt(extra, "wx"); }

// Waits until the warnings are out or the end's one bound has passed.
const warningsOutBy = (end) => warningSink.flush(Math.max(0, end - performance.now()));

async function finish() {
  clearTimeout(timer);
  // A complete receipt records intent/outcome, not proof that cleanup worked.
  // Even if publication fails, clean our own group; the worker reports the
  // missing proof rather than inventing an outcome.
  try { receipt(); } catch { /* worker requires the complete receipt */ }
  const end = performance.now() + exitFlushMs;
  await warningsOutBy(end);
  try { process.kill(-process.pid, "SIGKILL"); }
  catch (error) {
    // No retry or escalation after a refused signal. In particular Darwin
    // EPERM needs exit/reap plus group-absence confirmation by the worker.
    try { writeReceipt({ terminationError: error.code ?? "unknown" }, "w"); } catch { /* missing proof remains uncertainty */ }
    await warningsOutBy(end);
    process.exit(1);
  }
}

function stop() {
  if (cleaning) return;
  cleaning = true;
  stopping = true;
  if (!child || completed) { finish(); return; }
  try { process.kill(-process.pid, "SIGTERM"); }
  catch (error) {
    try { receipt({ terminationError: error.code ?? "unknown" }); } catch { /* worker requires proof */ }
    // Never send a further signal after this error.
    warningsOutBy(performance.now() + exitFlushMs).then(() => process.exit(1));
    return;
  }
  timer = setTimeout(finish, 1000);
}

// Install all loss/stop handlers before admitting the first workload.
process.on("SIGTERM", stop);
process.on("disconnect", stop);
process.on("message", (message) => {
  if (message?.kind === "stop") { stop(); return; }
  if (message?.kind !== "start" || started) return;
  started = true;
  if (cleaning) { finish(); return; }
  try {
    // Creating the workload process is synchronous and can block on a cold load.
    child = namedIfSlow("command start", () => spawn(spec.command, spec.args, { cwd: spec.cwd, env: process.env,
      // Never pass our private IPC descriptor through a workload exec,
      // including on platforms that leave extra inherited fds open.
      detached: false, stdio: ["ignore", 1, 2, "ignore"] }));
    child.once("error", (error) => { outcome.error = error.message; });
    child.once("close", (code, signal) => {
      completed = true;
      outcome = { ...outcome, code, signal };
      if (!cleaning) { cleaning = true; finish(); }
      // Cancellation retains its grace for TERM-resistant descendants.
    });
  } catch (error) {
    outcome.error = error.message;
    cleaning = true;
    finish();
  }
});
if (!process.connected) stop();
else process.send({ ready: true }, (error) => { if (error) stop(); });
