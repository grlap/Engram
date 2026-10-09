import assert from "node:assert/strict";
import { cpus } from "node:os";

// Locked rmcp 3.1.4 may drain for 5 s; allow headroom for store/runtime close.
export const CLOSE_BUDGET_MS = 10000;
const OBSERVATION_MS = 15000;
const DIAGNOSTIC_MS = 2000;
const SHUTDOWN_STAGES = new Set([
  "input_ended", "transport_close_started", "transport_close_finished",
  "trace_flush_started", "trace_flush_finished", "service_waiting_returned",
]);

/** Interval CPU counters, sampled before EOF as well as while closing.
 * These measure host activity, not the cause of a particular process delay.
 * No child process, store content or blocking shell sampler is involved.
 */
export class HostLoadSamples {
  constructor({ now = () => performance.now(), read = cpus, automatic = true } = {}) {
    this.now = now;
    this.read = read;
    this.samples = [];
    this.sample();
    if (automatic) {
      this.timer = setInterval(() => this.sample(), 1000);
      this.timer.unref();
    }
  }

  sample() {
    try {
      const processors = this.read();
      const total = processors.reduce((sum, cpu) => sum + Object.values(cpu.times).reduce((a, b) => a + b, 0), 0);
      const idle = processors.reduce((sum, cpu) => sum + cpu.times.idle, 0);
      this.samples.push({ at: this.now(), total, idle, processors: processors.length });
      if (this.samples.length > 32) this.samples.shift();
    } catch {
      this.unavailable = true;
    }
  }

  report(eofAt) {
    const intervals = [];
    for (let index = 1; index < this.samples.length; index++) {
      const before = this.samples[index - 1];
      const after = this.samples[index];
      const total = after.total - before.total;
      const idle = after.idle - before.idle;
      if (total <= 0 || idle < 0 || idle > total || after.processors !== before.processors) continue;
      intervals.push({
        from_eof_ms: before.at - eofAt,
        to_eof_ms: after.at - eofAt,
        host_cpu_busy_fraction: (total - idle) / total,
        processors: after.processors,
      });
    }
    return {
      state: intervals.length ? "measured" : "unknown",
      before_eof: intervals.some(interval => interval.to_eof_ms <= 0),
      intervals,
      sample_failed: this.unavailable ?? false,
      attribution: "host CPU intervals; correlation does not establish shutdown causation",
    };
  }

  stop() { clearInterval(this.timer); }
}

/** Times are monotonic event-callback observations, not kernel exit times. */
export class McpCloseObservation {
  constructor({ now = () => performance.now() } = {}) {
    this.now = now;
    this.milestones = [];
    this.closed = new Promise(resolve => { this.resolveClose = resolve; });
  }

  exited(code, signal) { this.exit ??= { code, signal, at: this.now() }; }
  didClose(code, signal) {
    this.close ??= { code, signal, at: this.now() };
    this.resolveClose(this.close);
  }

  /** Retain fixed stage names/times only; never copy arbitrary stderr fields. */
  shutdownMilestone(record) {
    if (record.engram_mcp_shutdown_trace !== 1 || !SHUTDOWN_STAGES.has(record.stage)
        || !Number.isFinite(record.server_elapsed_ms) || record.server_elapsed_ms < 0) return false;
    this.milestones.push({ stage: record.stage, server_elapsed_ms: record.server_elapsed_ms, received_at: this.now() });
    if (this.milestones.length > 32) this.milestones.shift();
    return true;
  }

  shutdownReport(started) {
    return {
      state: this.milestones.length ? "observed" : "unknown",
      milestones: this.milestones.map(({ received_at, ...record }) => ({ ...record, received_from_eof_ms: received_at - started })),
      missing: [...SHUTDOWN_STAGES].filter(stage => !this.milestones.some(record => record.stage === stage)),
      limitation: "best-effort trace; missing stages and uninstrumented cleanup remain unknown",
    };
  }

  async until(promise, deadline) {
    let timer;
    try {
      return await Promise.race([
        promise,
        new Promise(resolve => { timer = setTimeout(resolve, Math.max(0, deadline - this.now())); }),
      ]);
    } finally { clearTimeout(timer); }
  }

  async check({ started, child, pending = 0, stderr = "", collect = async () => ({ state: "unknown" }) }) {
    const readStderr = () => typeof stderr === "function" ? stderr() : stderr;
    const deadline = started + CLOSE_BUDGET_MS;
    let closed;
    do {
      closed = await this.until(this.closed, deadline);
    } while (!closed && this.now() < deadline);
    // Promise resolution may beat an overdue timer after an event-loop stall.
    // Its observation timestamp still must meet the original deadline.
    if (closed && closed.at <= deadline) {
      assert.equal(closed.signal, null, `MCP server terminated by ${closed.signal}`);
      assert.equal(closed.code, 0, `MCP server exited code=${closed.code}: ${readStderr()}`);
      return;
    }

    const exceededAt = this.now();
    const pendingAtDeadline = typeof pending === "function" ? pending() : pending;
    const stdinFinishedAtDeadline = child.stdin?.writableFinished;
    const stdinDestroyedAtDeadline = child.stdin?.destroyed;
    // Both observations run concurrently. A stuck diagnostic collector cannot
    // hold the close observer or extend its absolute post-deadline window.
    const diagnostic = Promise.resolve().then(collect).catch(error => ({ state: "unknown", error: String(error) }));
    const [eventual, load] = await Promise.all([
      this.until(this.closed, deadline + OBSERVATION_MS),
      this.until(diagnostic, Math.min(deadline + OBSERVATION_MS, exceededAt + DIAGNOSTIC_MS)),
    ]);
    const exit = this.exit;
    let classification = "never_closed";
    if (eventual && eventual.code === 0 && eventual.signal === null) {
      classification = exit && exit.at <= deadline ? "stdio_close_delayed" : "late_clean_exit";
    } else if (eventual || exit) {
      classification = exit?.code === 0 && exit?.signal === null ? "clean_exit_stdio_open" : "failed_exit";
    }
    const details = {
      classification,
      budget_ms: CLOSE_BUDGET_MS,
      deadline_observed_ms: exceededAt - started,
      exit_observed_ms: exit ? exit.at - started : null,
      close_observed_ms: eventual ? eventual.at - started : null,
      eventual: eventual ? { code: eventual.code, signal: eventual.signal } : null,
      host_load: load ?? { state: "unknown", reason: "collector deadline exceeded" },
      pid: child.pid,
      stdin_finished_at_deadline: stdinFinishedAtDeadline,
      stdin_destroyed_at_deadline: stdinDestroyedAtDeadline,
      pending_at_deadline: pendingAtDeadline,
      shutdown_trace: this.shutdownReport(started),
    };
    if (!eventual) {
      try { child.kill(); } catch (error) { details.kill_error = String(error); }
      await this.until(this.closed, this.now() + 1000);
    }
    // A late clean exit remains a failure, including under measured host load.
    const error = new Error(`MCP server did not close within deadline: ${JSON.stringify(details)}; stderr=${readStderr()}`);
    error.closeDiagnostic = details;
    throw error;
  }
}
