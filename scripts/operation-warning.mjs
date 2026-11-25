import { AsyncLocalStorage } from "node:async_hooks";
import { Worker } from "node:worker_threads";

// Shared operation timing for launcher helpers, the same rule Engram follows:
// a limit that ends an operation is at least a minute, and an operation still
// running after ten seconds says so once on stderr. The warning never cancels.
export const operationMinimumMs = 60_000;
export const warnAfterMs = 10_000;
// At process end a queued warning is delivered best effort: the end waits for
// it at most this long, and never longer.
export const exitFlushMs = 1_000;

// Each warning names its attempt as Engram's own do, process and sequence,
// so two lines with the same operation and time still tell apart.
let attempts = 0;
export const nextAttempt = () => `${process.pid}/${attempts++}`;
const prefix = (operation, attempt) => `warning: ${operation} is still running after 10 seconds; attempt=${attempt}; elapsed_ms=`;
const line = (operation, elapsed, attempt = nextAttempt()) => `${prefix(operation, attempt)}${Math.round(elapsed)}\n`;
// One clock for every thread: the process's time origin plus its own offset.
const clock = () => performance.timeOrigin + performance.now();

// Where warnings go: a writer thread of the process's own, started on the
// first line, writes each line to stderr in the order it was handed over.
// The calling thread only hands the line over and goes on, so neither a full
// pipe nor a synchronous loop on that thread holds the other up, and a pipe
// whose reader has gone reports only to the writer, never as a stream error.
// The writer shares nothing the work uses: no lock and none of the shared
// file-system workers. Each line it writes is reported back, so a flush counts
// a handed-over line as not yet out until then, and while any line is out the
// writer keeps the process alive, as a pending write would. Each operation
// warns at most once, so the waiting lines never outnumber the operations.
// When no writer thread can start, warnings are not written. Tests may
// replace `write`.
// Watcher threads still writing a line of their own (see warnWhileBlocking),
// and callers waiting for every line to be out.
let watchersWriting = 0;
// Lines handed to the writer thread and not yet written.
let handedOver = 0;
const idleWaiters = [];
const idle = () => watchersWriting === 0 && handedOver === 0;
const wakeIfIdle = () => { if (idle()) for (const wake of idleWaiters.splice(0)) wake(); };
// A warning thread runs its own small script, so it takes none of this
// process's options: one such as a module input type would change how that
// script loads.
const writerThread = { eval: true, execArgv: [], stdout: false, stderr: false };
// How a warning thread writes one line: the whole of it, on its own thread.
// Where stderr is a pipe in non-blocking mode, as Node leaves it on Linux
// once the process's stderr stream exists, a full pipe takes part of a line
// or none: the thread waits a moment and writes the rest, as a blocking write
// would, until an ended entry point's bound has passed: the bytes the system
// took are out, and the rest of the line is dropped with the lines still
// waiting. Any other error ends the line; observation never fails the
// operation.
const writeWholeSource = `const writeWhole = (text, dropping) => {
  const bytes = Buffer.from(text);
  const pause = new Int32Array(new SharedArrayBuffer(4));
  let offset = 0;
  while (offset < bytes.length) {
    try { offset += writeSync(2, bytes, offset); }
    catch (error) {
      if (error?.code !== "EAGAIN" || Atomics.load(dropping, 0) !== 0) return;
      Atomics.wait(pause, 0, 0, 10);
    }
  }
};`;
// Set once an ended entry point's bound has passed: lines still waiting are
// then dropped rather than written (see endWithinWarningBound).
const dropping = new Int32Array(new SharedArrayBuffer(4));
let lineWriter;
const lineWriterSource = `const { writeSync } = require("node:fs");
const { parentPort, workerData } = require("node:worker_threads");
${writeWholeSource}
const dropping = new Int32Array(workerData.dropping);
parentPort.on("message", (text) => {
  if (Atomics.load(dropping, 0) === 0) {
    try { writeWhole(text, dropping); } catch { /* observation never fails the operation */ }
  }
  parentPort.postMessage(0);
});`;
export const warningSink = {
  write(text) {
    try {
      if (!lineWriter) {
        lineWriter = new Worker(lineWriterSource, { ...writerThread, workerData: { dropping: dropping.buffer } });
        const written = () => {
          handedOver -= 1;
          if (handedOver === 0) lineWriter?.unref();
          wakeIfIdle();
        };
        const gone = () => { lineWriter = undefined; handedOver = 0; wakeIfIdle(); };
        lineWriter.on("message", written).on("error", gone).on("exit", gone);
      }
      handedOver += 1;
      lineWriter.ref();
      lineWriter.postMessage(text);
    } catch { /* observation never fails the operation */ }
  },
  // For a process about to end: settles once every warning is out, or after
  // `ms`, whichever comes first, so the end is never held longer for one.
  flush(ms = exitFlushMs) {
    if (idle()) return Promise.resolve();
    return new Promise((done) => {
      const timer = setTimeout(done, ms);
      idleWaiters.push(() => { clearTimeout(timer); done(); });
    });
  },
};

// For an entry point whose own work has ended: the process ends as usual once
// its writes are out, and warnings still waiting their turn hold it at most
// `ms`; past that bound they are dropped. The one write already in flight
// completes as the system completes it: Node ends no process before a thread
// of its own returns from a write it has issued.
export function endWithinWarningBound(ms = exitFlushMs) {
  setTimeout(() => { Atomics.store(dropping, 0, 1); }, ms).unref();
}

// The operation that owns the current call's one warning, when an enclosing
// asOneOperation started it: guards inside it stand down, so one operation is
// named once. Outside any such operation every guard is its own.
const enclosing = new AsyncLocalStorage();

// Whether an enclosing operation already names the current call: a helper
// process it starts can then stay silent rather than name it twice.
export const insideOperation = () => enclosing.getStore() !== undefined;

// Runs `run`, synchronous or asynchronous, as one operation with one warning,
// which covers every step inside it.
export async function asOneOperation(operation, run, options) {
  const settled = warnWhileRunning(operation, options);
  try { return await enclosing.run(operation, run); }
  finally { settled(); }
}

// Starts the ten-second warning for `operation`, a fixed name and never caller
// text. An operation that continues an earlier phase passes the time that
// phase already took as `before`. Call the returned function once the
// operation settles; it says whether the warning was written. A synchronous
// call can hold the timer back past its time, so settling writes the one
// warning itself when it is due and the timer has not.
export function warnWhileRunning(operation, { now = () => performance.now(), after = warnAfterMs, before = 0 } = {}) {
  if (enclosing.getStore() !== undefined) return () => false;
  const started = now();
  let fired = false;
  const fire = () => {
    if (fired) return;
    fired = true;
    try { warningSink.write(line(operation, before + now() - started)); } catch { /* observation never fails the operation */ }
  };
  const timer = setTimeout(fire, Math.max(0, after - before));
  timer.unref?.();
  // The first settling is final: a later call, such as a cleanup path's,
  // only reports it and never warns for time spent after the operation ended.
  let settled = false;
  return () => {
    if (!settled) {
      settled = true;
      clearTimeout(timer);
      if (before + now() - started >= after) fire();
    }
    return fired;
  };
}

// For a short synchronous step, such as one file call: it holds this thread,
// so it is named once as it returns when it took ten seconds or more. Its
// result and its error pass through unchanged.
export function namedIfSlow(operation, step, options) {
  const settled = warnWhileRunning(operation, options);
  try { return step(); }
  finally { settled(); }
}

// A watcher thread keeps its own timers while the caller's thread is blocked.
// It is due at the caller's start plus the warning time, whenever it boots.
// The shared flag says who writes the one line: open, then claimed by the
// watcher and written, or taken by the caller once its call has returned.
const OPEN = 0, WATCHER_WRITING = 1, CALLER_TOOK = 2, WATCHER_WROTE = 3;
const watcherSource = `const { writeSync } = require("node:fs");
const { workerData } = require("node:worker_threads");
${writeWholeSource}
const clock = () => performance.timeOrigin + performance.now();
const flag = new Int32Array(workerData.flag);
const dropping = new Int32Array(workerData.dropping);
setTimeout(() => {
  if (Atomics.compareExchange(flag, 0, ${OPEN}, ${WATCHER_WRITING}) !== ${OPEN}) return;
  try { writeWhole(workerData.prefix + Math.round(clock() - workerData.started) + String.fromCharCode(10), dropping); }
  catch { /* observation never fails the operation */ }
  Atomics.store(flag, 0, ${WATCHER_WROTE});
  Atomics.notify(flag, 0);
}, Math.max(0, workerData.started + workerData.after - clock()));`;

// For a synchronous call such as spawnSync, which blocks this thread's
// timers: the one warning is written while the call is still running, or,
// when the watcher could not yet, queued as soon as the call returns. A
// caller whose process ends right after the call awaits warningSink.flush()
// first, which also waits for a watcher still writing its line.
export function warnWhileBlocking(operation, run, { after = warnAfterMs } = {}) {
  if (enclosing.getStore() !== undefined) return run();
  const started = clock();
  const attempt = nextAttempt();
  const flag = new Int32Array(new SharedArrayBuffer(4));
  let watcher;
  try {
    watcher = new Worker(watcherSource, { ...writerThread,
      workerData: { after, started, prefix: prefix(operation, attempt), flag: flag.buffer, dropping: dropping.buffer } });
    watcher.on("error", () => {});
    watcher.unref();
  } catch { watcher = undefined; /* observation never fails the operation */ }
  try { return run(); }
  finally {
    const elapsed = clock() - started;
    const prior = Atomics.compareExchange(flag, 0, OPEN, CALLER_TOOK);
    if (prior === OPEN && elapsed >= after) {
      try { warningSink.write(line(operation, elapsed, attempt)); } catch { /* observation never fails the operation */ }
    }
    if (prior === WATCHER_WRITING) {
      // A watcher in the middle of its line finishes it and ends by itself;
      // until it says so, a flush counts its line as not yet out.
      try {
        const waiting = Atomics.waitAsync(flag, 0, WATCHER_WRITING);
        if (waiting.async) {
          watchersWriting += 1;
          waiting.value.then(() => { watchersWriting -= 1; wakeIfIdle(); });
        }
      } catch { /* observation never fails the operation */ }
    } else watcher?.terminate().catch(() => {});
  }
}
