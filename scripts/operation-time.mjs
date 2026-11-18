// Warnings observe an operation; they never abort it, retry it, or change its result.
import { Worker } from "node:worker_threads";

export const MINIMUM_MS = 60_000;
export const WARN_AFTER_MS = 10_000;
// libuv's host monotonic clock is shared by the launcher and its worker.
export const monotonicMs = () => Number(process.hrtime.bigint() / 1_000_000n);

export function startOperationWarning(operation, { setTimer = setTimeout, clearTimer = clearTimeout,
  afterMs = WARN_AFTER_MS, now = () => performance.now(), started = now(),
  warn = (message) => process.stderr.write(`${message}\n`) } = {}) {
  const id = ++nextId;
  let finished = false;
  const emit = () => {
    if (finished) return;
    finished = true;
    try { warn(`warning: ${operation} is still running after 10 seconds; attempt=${process.pid}/${id}; elapsed_ms=${Math.max(0, Math.floor(now() - started))}`); } catch { /* observation only */ }
  };
  if (afterMs === 0) emit();
  const timer = finished ? undefined : setTimer(emit, afterMs);
  timer?.unref?.();
  return () => { finished = true; if (timer !== undefined) clearTimer(timer); };
}

let observer;
let nextId = 0;

// A main-thread timer cannot report a blocked spawnSync. The worker writes to
// fd 2 directly: its normal stderr stream is forwarded by the blocked parent.
// It owns no child handle and has no termination or transport capability.
function syncObserver() {
  if (observer) return observer;
  observer = new Worker(`
    const { parentPort } = require("node:worker_threads");
    const { writeSync } = require("node:fs");
    const timers = new Map();
    parentPort.on("message", (message) => {
      if (message.kind === "finish") {
        clearTimeout(timers.get(message.id)); timers.delete(message.id); return;
      }
      const left = message.due - Number(process.hrtime.bigint() / 1000000n);
      const timer = setTimeout(() => {
        timers.delete(message.id);
        if (Atomics.load(new Int32Array(message.finished), 0)) return;
        try { writeSync(2, "warning: " + message.operation + " is still running after 10 seconds; attempt=" + process.pid + "/" + message.id + "; elapsed_ms=" + Math.max(0, Number(process.hrtime.bigint() / 1000000n) - message.started) + "\\n"); } catch {}
      }, Math.max(0, left));
      timers.set(message.id, timer);
    });
  `, { eval: true });
  observer.on("error", () => { observer = undefined; });
  observer.unref();
  return observer;
}

export function withSyncOperationWarning(operation, action) {
  const finished = new SharedArrayBuffer(4);
  const id = ++nextId;
  let worker;
  try {
    worker = syncObserver();
    const started = monotonicMs();
    worker.postMessage({ kind: "start", id, operation, finished, started, due: started + WARN_AFTER_MS });
  } catch { /* a missing diagnostic must not refuse an operation */ }
  try { return action(); }
  finally {
    Atomics.store(new Int32Array(finished), 0, 1);
    try { worker?.postMessage({ kind: "finish", id }); } catch { /* observation only */ }
  }
}
