import assert from "node:assert/strict";
import test from "node:test";
import { startOperationWarning, withSyncOperationWarning } from "./operation-time.mjs";

function clock() {
  let now = 0;
  const timers = new Set();
  const warnings = [];
  return {
    warnings,
    options: { now: () => now, setTimer(action, delay) { const timer = { action, due: now + delay }; timers.add(timer); return timer; },
      clearTimer(timer) { timers.delete(timer); }, warn(message) { warnings.push(message); } },
    advance(to) { now = to; for (const timer of [...timers]) if (timer.due <= now) {
      timers.delete(timer); timer.action();
    } },
  };
}

test("a warning reports a live operation once at ten seconds without settling it", () => {
  const time = clock();
  const finish = startOperationWarning("test registration", time.options);
  time.advance(9999);
  assert.deepEqual(time.warnings, []);
  time.advance(10000);
  assert.equal(time.warnings.length, 1);
  assert.match(time.warnings[0], /^warning: test registration is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=10000$/u);
  time.advance(59000);
  assert.equal(time.warnings.length, 1);
  // The caller still owns completion; observation did not abort or resolve it.
  finish();
  time.advance(60000);
  assert.equal(time.warnings.length, 1);
});

test("completion, including a thrown synchronous result, suppresses later warnings", () => {
  const time = clock();
  const finish = startOperationWarning("test call", time.options);
  finish();
  time.advance(60000);
  assert.deepEqual(time.warnings, []);
  const failure = new Error("original outcome");
  assert.throws(() => withSyncOperationWarning("test query", () => { throw failure; }), error => error === failure);
  assert.equal(withSyncOperationWarning("test query", () => 42), 42);
});
