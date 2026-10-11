// Lifecycle evidence for children owned by the control dogfood fixtures.
// Attach immediately after spawn, before readiness or any writes.
export class FixtureChild {
  constructor(child, label, stderr = () => "") {
    this.child = child;
    this.label = label;
    this.stderr = stderr;
    this.started = performance.now();
    this.events = {};
    this.latestRequest = null;
    this.pending = 0;
    this.hasClosed = false;
    this.hasExited = false;
    this.firstError = null;
    this.onFailure = () => {};
    this.closePromise = null;
    this.closed = new Promise((resolve) => { this.resolveClosed = resolve; });
    child.once("spawn", () => this.mark("spawn"));
    child.on("error", (error) => this.fail("child_error", error));
    child.stdin.on("error", (error) => this.fail("stdin_error", error));
    child.stdin.once("finish", () => this.mark("stdin_finish"));
    child.stdin.once("close", () => this.mark("stdin_close"));
    child.once("exit", (code, signal) => {
      this.hasExited = true;
      this.exitCode = code;
      this.signalCode = signal;
      this.mark("exit");
      this.onFailure(this.error(`exited code=${code} signal=${signal}`));
    });
    child.once("close", (code, signal) => {
      this.hasClosed = true;
      this.exitCode = code;
      this.signalCode = signal;
      this.mark("close");
      this.resolveClosed();
    });
  }

  mark(name) { this.events[name] ??= performance.now() - this.started; }

  fail(name, error) {
    this.mark(name);
    this.firstError ??= error;
    this.onFailure(this.error(`${name}: ${error.message}`, error));
  }

  request(operation, outcome, pending) {
    this.latestRequest = { operation, outcome, at_ms: performance.now() - this.started };
    this.pending = pending;
  }

  snapshot() {
    return {
      pid: this.child.pid ?? null,
      events: { ...this.events },
      latest_request: this.latestRequest,
      pending: this.pending,
      exit_code: this.exitCode ?? this.child.exitCode,
      signal: this.signalCode ?? this.child.signalCode,
      stdio_closed: this.hasClosed,
      error: this.firstError?.message ?? null,
      kill: this.killResult ?? null,
    };
  }

  error(reason, cause) {
    const lifecycle = this.snapshot();
    const error = new Error(`${this.label} ${reason}: ${this.stderr()}; lifecycle=${JSON.stringify(lifecycle)}`, { cause });
    error.lifecycle = lifecycle;
    return error;
  }

  killIfRunning() {
    if (!this.child.pid || this.hasExited || this.hasClosed ||
        this.child.exitCode !== null || this.child.signalCode !== null) return;
    this.mark("kill");
    try { this.killResult = { returned: this.child.kill() }; }
    catch (error) { this.killResult = { error: error.message }; }
  }

  close() {
    if (this.closePromise) return this.closePromise;
    this.mark("close_call");
    // Cache both outcomes. A late close cannot erase a timeout, and a second
    // caller never sends EOF or kills the child a second time.
    this.closePromise = new Promise((resolve, reject) => {
      const finish = () => {
        if (this.firstError) reject(this.error("shutdown failed", this.firstError));
        else resolve();
      };
      if (this.hasClosed) { finish(); return; }
      const timeout = () => {
        this.mark("timeout");
        this.killIfRunning();
        reject(this.error("shutdown timed out", this.firstError));
      };
      const timer = setTimeout(timeout, 5000);
      this.closed.then(() => {
        clearTimeout(timer);
        if (this.events.close - this.events.close_call > 5000) timeout();
        else finish();
      });
      if (!this.hasExited && this.child.exitCode === null && this.child.signalCode === null) {
        this.mark("stdin_end_call");
        try { this.child.stdin.end(); }
        catch (error) { this.fail("stdin_error", error); }
      }
    });
    return this.closePromise;
  }
}

// The body error remains the primary failure while every cleanup is attempted.
export async function finishFixture(failure, clients, remove) {
  const results = await Promise.allSettled(clients.filter(Boolean).map((client) =>
    Promise.resolve().then(() => client.close())));
  const errors = results.filter((result) => result.status === "rejected").map((result) => result.reason);
  try { remove(); } catch (error) { errors.push(error); }
  const cleanup = errors.length ? new AggregateError(errors, "Fixture shutdown failed") : null;
  if (failure) {
    if (cleanup) console.error(cleanup);
    throw failure;
  }
  if (cleanup) throw cleanup;
}
