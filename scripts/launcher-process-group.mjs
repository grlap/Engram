// A living session/group leader owns every terminating signal. The worker
// sends intent over IPC and, after this helper exits, only observes absence.
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

const spec = JSON.parse(Buffer.from(process.argv[2], "base64").toString("utf8"));
let child, started = false, cleaning = false, stopping = false, completed = false;
let outcome = { code: null, signal: null };
let timer;

function receipt(extra = {}) {
  writeFileSync(spec.receipt, `${JSON.stringify({ protocol: "posix-group-v1", pid: process.pid,
    cleanupStarted: true, stopped: stopping, ...outcome, ...extra })}\n`, { flag: "wx", mode: 0o600 });
}

function finish() {
  clearTimeout(timer);
  // A complete receipt records intent/outcome, not proof that cleanup worked.
  // Even if publication fails, clean our own group; the worker reports the
  // missing proof rather than inventing an outcome.
  try { receipt(); } catch { /* worker requires the complete receipt */ }
  try { process.kill(-process.pid, "SIGKILL"); }
  catch (error) {
    // No retry or escalation after a refused signal. In particular Darwin
    // EPERM needs exit/reap plus group-absence confirmation by the worker.
    try { writeFileSync(spec.receipt, `${JSON.stringify({ protocol: "posix-group-v1", pid: process.pid,
      cleanupStarted: true, stopped: stopping, ...outcome, terminationError: error.code ?? "unknown" })}\n`,
    { flag: "w", mode: 0o600 }); } catch { /* missing proof remains uncertainty */ }
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
    process.exit(1); // never send a further signal after this error
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
    child = spawn(spec.command, spec.args, { cwd: spec.cwd, env: process.env,
      // Never pass our private IPC descriptor through a workload exec,
      // including on platforms that leave extra inherited fds open.
      detached: false, stdio: ["ignore", 1, 2, "ignore"] });
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
