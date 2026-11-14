// Process and unit discriminators for project-root identity probes: the host
// probe looks the project file up under the opposite case and writes no
// marker, and agent paths do not probe at all. Watcher journals marker
// create/remove by event index; leftover read_dir is not the proof, and the
// harness calibrates the journal with a marker of its own. Denied-create uses
// a real ACL/permission canary, and the host probe still resolves there
// because it only reads. A project file whose name has no ASCII letter makes
// the host probe fail with its WARNING, which is the control proving that
// agent paths, silent there, do not probe. Call registerRootProbeTests(test)
// from an existing Node gate so the required nine stay nine.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { watch } from "node:fs";
import { basename, isAbsolute, join, relative, resolve, sep } from "node:path";
import win32 from "node:path/win32";
import { EventEmitter } from "node:events";

import { fixtureHome, fixtureRoot, removeFixtureHomes } from "./test-temp.mjs";

import { registerJournalStartup, registerJournalEvents } from "./root-probe/tests/journal.mjs";
import { registerAclReader, registerAclRestoration } from "./root-probe/tests/acl.mjs";
import { registerOwnershipGuards, registerResourceCleanup } from "./root-probe/tests/cleanup.mjs";
import { registerRootIdentity } from "./root-probe/tests/integration.mjs";
import { createTestSupport } from "./root-probe/tests/support.mjs";

const repo = resolve(import.meta.dirname, "..");
const target = resolve(repo, process.env.CARGO_TARGET_DIR || "target");
const binary = join(
  target,
  "debug",
  process.platform === "win32" ? "engram.exe" : "engram",
);
const PROBE_MARK = "engram-path-probe";
const BARRIER_PREFIX = ".watch-barrier-";
const JOURNAL_DIAGNOSTIC_LIMIT = 16;
// A host control process with closed stdin: it starts, resolves identity and
// exits at end of input.
const CONTROL_ARGS = ["control", "--work-session", "--actor-id", "probe", "--session-id", "probe-control"];
const WARNING = "could not probe the filesystem identity";
// Ends a wait for what will never come: a deaf watcher, a stuck reader, a child
// that will not close. It fails the probe then, and says nothing about speed.
const HANG_GUARD_MS = 60000;
const RESOLVED_POLICY = /^(case_fold|case_sensitive), windows alias rules (on|off)$/u;

export function childEnv() {
  const env = { ...process.env };
  delete env.ENGRAM_HOST_PATH_POLICY;
  delete env.ENGRAM_HOME;
  delete env.ENGRAM_ACTOR_ID;
  delete env.ENGRAM_SESSION_ID;
  delete env.ENGRAM_ACTOR_CONTEXT;
  return env;
}

export function run(home, projectFile, args, extra = {}) {
  const result = spawnSync(binary, ["--home", home, "--project-file", projectFile, ...args], {
    cwd: extra.cwd ?? repo,
    env: childEnv(),
    encoding: "utf8",
    timeout: extra.timeout ?? HANG_GUARD_MS,
  });
  if (result.error) throw result.error;
  return result;
}

function workArgs(session, ...rest) {
  return ["work", "--actor-id", "probe", "--session-id", session, ...rest];
}

function addedRef(output) {
  assert.equal(output.status, 0, output.stderr);
  const ref = JSON.parse(output.stdout).work?.short_ref;
  assert.equal(typeof ref, "string", output.stdout);
  assert.match(ref, /^w-[0-9a-f]+$/u);
  return ref;
}

function assertSuccessfulPeek(result) {
  assert.equal(result.isError ?? false, false, JSON.stringify(result));
  let value = result.structuredContent;
  if (value == null && Array.isArray(result.content)) {
    const text = result.content.find((part) => part?.type === "text")?.text;
    assert.ok(text, JSON.stringify(result));
    value = JSON.parse(text);
  }
  assert.ok(value && typeof value === "object", JSON.stringify(result));
  assert.equal(value.peek?.delivery_advanced, false, JSON.stringify(value));
  assert.ok(Array.isArray(value.next), JSON.stringify(value));
  return value;
}

function assertResolvedDoctorPolicy(output) {
  assert.equal(output.status, 0, output.stderr);
  const report = JSON.parse(output.stdout);
  assert.equal(report.healthy, true, output.stdout);
  assert.match(report.host_path_policy, RESOLVED_POLICY, JSON.stringify(report.host_path_policy));
}

export class RootJournal extends EventEmitter {
  constructor(directory, {
    watchFn = watch,
    startupWrite = writeFileSync,
    startupRemove = rmSync,
    startupNow = () => performance.now(),
  } = {}) {
    super();
    this.directory = directory;
    this.events = [];
    this.errors = [];
    this.nextIndex = 0;
    this.recentCallbacks = [];
    this.callbackCount = 0;
    this.startupWrite = startupWrite;
    this.startupRemove = startupRemove;
    this.startupNow = startupNow;
    this.watchStartedAt = performance.now();
    this.watcher = watchFn(directory, { persistent: true }, (_type, name) => {
      const filename = name == null ? name : String(name);
      this.recentCallbacks.push({
        eventType: String(_type).slice(0, 64),
        filename: filename == null ? filename : filename.slice(0, 512),
        filenameType: name === null ? "null" : Buffer.isBuffer(name) ? "Buffer" : typeof name,
        filenameLength: filename == null ? 0 : filename.length,
        callback: this.callbackCount++,
        index: this.nextIndex,
        sinceWatchMs: performance.now() - this.watchStartedAt,
      });
      if (this.recentCallbacks.length > JOURNAL_DIAGNOSTIC_LIMIT) this.recentCallbacks.shift();
      if (name == null || name === "") {
        this.errors.push(
          new Error("watcher filename missing or overflow (inconclusive)"),
        );
        this.emit("event");
        return;
      }
      this.events.push({ filename: String(name), index: this.nextIndex });
      this.nextIndex += 1;
      this.emit("event");
    });
    this.watcher.on("error", (error) => {
      this.errors.push(error);
      this.emit("event");
    });
  }

  cursor() {
    return this.nextIndex;
  }

  start() {
    this.startup ??= this.startHandshake();
    return this.startup;
  }

  // fs.watch returning does not prove that Darwin's asynchronously registered
  // FSEvents stream can deliver an event yet. Establish delivery with our own
  // traffic under ONE startup deadline, before calibration or measured work.
  // This never retries a calibration, barrier, command or failed assertion.
  async startHandshake() {
    this.assertHealthy("before journal startup");
    const filename = `${BARRIER_PREFIX}startup`;
    const path = join(this.directory, filename);
    const from = this.cursor();
    const startedAt = performance.now();
    const deadline = this.startupNow() + HANG_GUARD_MS;
    let owned = false;
    let writes = 0;
    let failure;
    try {
      await new Promise((resolveStartup, reject) => {
        let settled = false;
        let timeout;
        let writer;
        const finish = (error) => {
          if (settled) return;
          settled = true;
          clearTimeout(timeout);
          clearInterval(writer);
          this.off("event", onEvent);
          this.watcher.off("close", onClose);
          if (error) reject(error);
          else resolveStartup();
        };
        const expired = () => {
          let message = "journal startup: timed out waiting for its primary watcher callback";
          try {
            message += `; journal=${JSON.stringify({
              ...this.timeoutDiagnostic(startedAt, HANG_GUARD_MS),
              startup: { filename, fromIndex: from, writes },
            })}`;
          } catch {
            // Diagnostics must not replace the original failure.
          }
          finish(new Error(message));
        };
        const onEvent = () => {
          if (this.errors.length) {
            finish(new Error(`journal startup: watcher inconclusive: ${this.errors[0]}`));
          } else if (this.startupNow() >= deadline) {
            expired();
          } else if (this.events.some((event) => event.index >= from && event.filename === filename)) {
            finish();
          }
        };
        const onClose = () => {
          const error = new Error("watcher closed during journal startup");
          this.errors.push(error);
          finish(error);
        };
        const pulse = () => {
          if (settled) return;
          if (this.startupNow() >= deadline) {
            expired();
            return;
          }
          try {
            // Acquire ownership exclusively. A collision is a failure, never
            // permission to overwrite or remove an unexpected existing file.
            this.startupWrite(path, String(writes), { flag: owned ? "r+" : "wx" });
            owned = true;
            writes += 1;
          } catch (error) {
            this.errors.push(error);
            finish(error);
          }
        };
        this.on("event", onEvent);
        this.watcher.once("close", onClose);
        timeout = setTimeout(expired, HANG_GUARD_MS);
        writer = setInterval(pulse, 25);
        pulse();
      });
      this.assertHealthy("journal startup");
    } catch (error) {
      failure = error;
    }
    const cleanupErrors = [];
    if (owned) {
      try {
        this.startupRemove(path);
      } catch (error) {
        cleanupErrors.push(error);
      }
    }
    if (failure) throwWithCleanup(failure, cleanupErrors);
    if (cleanupErrors.length) throw new AggregateError(cleanupErrors, "journal startup cleanup failed");
  }

  probeEvents(fromIndex, untilIndex) {
    return this.events.filter(
      (event) =>
        event.index >= fromIndex &&
        event.index < untilIndex &&
        event.filename.includes(PROBE_MARK),
    );
  }

  // Any root event in the window other than the journal's own barriers and
  // the project file, which every command reads (a read can raise an
  // access-time event): a write the commands made in the root, whatever its
  // name.
  foreignEvents(fromIndex, untilIndex, projectFileName) {
    return this.events.filter(
      (event) =>
        event.index >= fromIndex &&
        event.index < untilIndex &&
        !event.filename.startsWith(BARRIER_PREFIX) &&
        event.filename !== projectFileName,
    );
  }

  assertHealthy(label) {
    if (this.errors.length) {
      throw new Error(`${label}: watcher inconclusive: ${this.errors[0]}`);
    }
  }

  timeoutDiagnostic(waitStartedAt, timeoutMs, barrier) {
    const text = (value, limit = 512) => String(value).slice(0, limit);
    let realDirectory;
    try {
      realDirectory = text(realpathSync(this.directory), 2048);
    } catch (error) {
      realDirectory = `unavailable: ${text(error)}`;
    }
    const now = performance.now();
    return {
      directory: text(this.directory, 2048),
      realDirectory,
      sinceWatchMs: now - this.watchStartedAt,
      waitElapsedMs: now - waitStartedAt,
      deadlineSinceWatchMs: waitStartedAt - this.watchStartedAt + timeoutMs,
      timeoutMs,
      cursor: this.nextIndex,
      barrier: barrier && {
        filename: text(barrier.filename),
        path: text(barrier.path, 2048),
        fromIndex: barrier.fromIndex,
        writeStartedSinceWatchMs: barrier.writeStartedAt - this.watchStartedAt,
        writeFinishedSinceWatchMs: barrier.writeFinishedAt - this.watchStartedAt,
      },
      callbacks: this.recentCallbacks,
      omittedCallbacks: this.callbackCount - this.recentCallbacks.length,
      errors: this.errors.slice(-JOURNAL_DIAGNOSTIC_LIMIT).map((error) => text(error)),
      omittedErrors: Math.max(0, this.errors.length - JOURNAL_DIAGNOSTIC_LIMIT),
    };
  }

  async waitFor(predicate, timeoutMs, label, barrier) {
    if (this.errors.length) {
      throw new Error(`${label}: preexisting watcher error: ${this.errors[0]}`);
    }
    if (predicate()) return;
    const waitStartedAt = performance.now();
    await new Promise((resolvePromise, reject) => {
      let settled = false;
      const finish = (action) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.off("event", onEvent);
        action();
      };
      const onEvent = () => {
        if (this.errors.length) {
          finish(() =>
            reject(new Error(`${label}: watcher error: ${this.errors[0]}`)),
          );
          return;
        }
        if (predicate()) finish(() => resolvePromise());
      };
      const timer = setTimeout(() => {
        finish(() => {
          let message = `${label}: timed out waiting for journal event`;
          try {
            message += `; journal=${JSON.stringify(this.timeoutDiagnostic(waitStartedAt, timeoutMs, barrier))}`;
          } catch {
            // Diagnostic failure must not hide the original timeout.
          }
          reject(new Error(message));
        });
      }, timeoutMs);
      this.on("event", onEvent);
    });
  }

  async barrier(label) {
    const from = this.nextIndex;
    const filename = `${BARRIER_PREFIX}${label}`;
    const path = join(this.directory, filename);
    const writeStartedAt = performance.now();
    writeFileSync(path, label);
    const writeFinishedAt = performance.now();
    await this.waitFor(
      () =>
        this.events.some(
          (event) => event.index >= from && event.filename === filename,
        ),
      HANG_GUARD_MS,
      `barrier ${label}`,
      { filename, path, fromIndex: from, writeStartedAt, writeFinishedAt },
    );
    this.assertHealthy(`barrier ${label}`);
    return this.nextIndex;
  }

  close() {
    this.watcher.close();
  }
}

export async function waitForChildClose(
  closed,
  kill,
  { gracefulMs = HANG_GUARD_MS, forcedMs = HANG_GUARD_MS } = {},
) {
  const timed = async (milliseconds) => {
    let timer;
    try {
      return await Promise.race([
        closed,
        new Promise((resolvePromise) => {
          timer = setTimeout(() => resolvePromise(null), milliseconds);
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  };
  const first = await timed(gracefulMs);
  if (first) return first;
  kill();
  const second = await timed(forcedMs);
  if (!second) {
    throw new Error("MCP did not exit after forced close");
  }
  return second;
}

function writeProject(projectDir, id, name = ".engram-project") {
  mkdirSync(projectDir, { recursive: true });
  writeFileSync(join(projectDir, name), id);
}

export function throwWithCleanup(primary, cleanupErrors) {
  if (!cleanupErrors.length) throw primary;
  throw new AggregateError([primary, ...cleanupErrors], primary.message);
}

export async function cleanupDeniedResources({
  shutdown,
  restore,
  closeReader,
  deleteRoot,
  deleteHome,
}) {
  const errors = [];
  const steps = [];
  let shutdownOk = !shutdown;
  if (shutdown) {
    try {
      await shutdown();
      shutdownOk = true;
      steps.push("shutdown");
    } catch (error) {
      errors.push(error);
      steps.push("shutdown-failed");
    }
  }
  let restored = false;
  try {
    await restore();
    restored = true;
    steps.push("restore");
  } catch (error) {
    errors.push(error);
    steps.push("restore-failed");
  }
  if (closeReader) {
    try {
      await closeReader();
      steps.push("reader-close");
    } catch (error) {
      shutdownOk = false;
      errors.push(error);
      steps.push("reader-close-failed");
    }
  }
  if (shutdownOk && restored) {
    try {
      await deleteRoot();
      steps.push("delete-root");
    } catch (error) {
      errors.push(error);
      steps.push("delete-root-failed");
    }
  } else {
    steps.push("no-delete-root");
  }
  if (deleteHome) {
    if (shutdownOk) {
      try {
        await deleteHome();
        steps.push("delete-home");
      } catch (error) {
        errors.push(error);
        steps.push("delete-home-failed");
      }
    } else {
      steps.push("no-delete-home");
    }
  }
  return { restored, shutdownOk, errors, steps };
}

function removeOwned(...homes) {
  removeFixtureHomes(...homes);
  const left = homes.filter((home) => existsSync(home));
  if (left.length) {
    throw new Error(`fixture still present after remove: ${left.join(", ")}`);
  }
}

// Lexical child-of-root only. Does not prove registered ownership or symlink safety.
export function isExactOwnedFixturePath(directory, root, pathApi = { resolve, relative, isAbsolute, sep }) {
  const resolved = pathApi.resolve(directory);
  const resolvedRoot = pathApi.resolve(root);
  const rel = pathApi.relative(resolvedRoot, resolved);
  if (!rel || pathApi.isAbsolute(rel) || rel.split(pathApi.sep).some((part) => part === "..")) {
    return false;
  }
  return true;
}

export function assertExactOwnedFixture(directory) {
  if (!isExactOwnedFixturePath(directory, fixtureRoot)) {
    throw new Error(`refusing ACL on non-owned fixture path: ${resolve(directory)}`);
  }
}

function pwsh(command, extraEnv) {
  const result = spawnSync("pwsh", ["-NoProfile", "-Command", command], {
    encoding: "utf8",
    timeout: HANG_GUARD_MS,
    env: { ...childEnv(), ...extraEnv },
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`pwsh ACL failed (${result.status}): ${result.stdout} ${result.stderr}`);
  }
  return result.stdout.trim();
}

// One process per fixture; readiness does not spend the ACL operation's allowance.
// Each observed phase gets the hang guard, with no retries.
const ACL_READER_COMMAND = `
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$ErrorActionPreference = 'Stop'
[Console]::WriteLine('ready')
while ($null -ne ($request = [Console]::ReadLine())) {
  if ($request -eq 'close') { break }
  if ($request -ne 'read') { throw 'invalid ACL reader request' }
  $sddl = (Get-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH).GetSecurityDescriptorSddlForm('Access')
  [Console]::WriteLine((ConvertTo-Json -InputObject $sddl -Compress))
}`;

export class WindowsAclReader {
  constructor(directory, { spawnImpl = spawn, setTimer = setTimeout, clearTimer = clearTimeout } = {}) {
    assertExactOwnedFixture(directory);
    this.setTimer = setTimer;
    this.clearTimer = clearTimer;
    this.buffer = "";
    this.stderr = "";
    this.child = spawnImpl("pwsh", ["-NoProfile", "-NonInteractive", "-Command", ACL_READER_COMMAND], {
      env: { ...childEnv(), ROOT_PROBE_ACL_PATH: directory }, stdio: ["pipe", "pipe", "pipe"],
    });
    this.closed = new Promise((resolveClosed) => { this.resolveClosed = resolveClosed; });
    this.onError = (cause) => this.fail(cause);
    this.onData = (chunk) => {
      this.buffer += chunk.toString("utf8");
      if (this.buffer.length > 65536) return this.fail(new Error("ACL reader output exceeded its bound"));
      let end;
      while ((end = this.buffer.indexOf("\n")) >= 0) {
        const line = this.buffer.slice(0, end).trimEnd();
        this.buffer = this.buffer.slice(end + 1);
        try {
          if (!this.pending) throw new Error("unsolicited ACL reader response");
          const value = this.pending.phase === "startup" ? line : JSON.parse(line);
          if (this.pending.phase === "startup" ? value !== "ready" : typeof value !== "string" || !value.startsWith("D:")) {
            throw new Error("invalid ACL reader response");
          }
          this.settle(null, value);
        } catch (cause) { this.fail(cause); }
      }
    };
    this.onStderr = (chunk) => { this.stderr = (this.stderr + chunk.toString("utf8")).slice(-8192); };
    this.onClose = (code, signal) => {
      this.result = { code, signal };
      if (this.pending || !this.closing) this.fail(new Error(`ACL reader exited (${code}, ${signal}): ${this.stderr}`));
      this.resolveClosed(this.result);
      this.detach();
    };
    this.child.on("error", this.onError);
    this.child.on("close", this.onClose);
    this.child.stdin.on("error", this.onError);
    this.child.stdout.setEncoding("utf8").on("data", this.onData);
    this.child.stderr.setEncoding("utf8").on("data", this.onStderr);
    this.ready = this.expect("startup", "capture");
    this.ready.catch(() => {}); // It can fail before the caller awaits read().
  }

  error(phase, operation, cause) {
    return Object.assign(new Error(`ACL reader ${operation} ${phase} failed: ${cause.message}`, { cause }), {
      code: cause.code === "ETIMEDOUT" ? "ROOT_PROBE_ACL_TIMEOUT" : "ROOT_PROBE_ACL_FAILED", phase, operation,
    });
  }

  expect(phase, operation) {
    if (this.failure) return Promise.reject(this.error(phase, operation, this.failure));
    if (this.pending) return Promise.reject(this.error(phase, operation, new Error("overlapping ACL reader requests")));
    return new Promise((resolveResponse, reject) => {
      this.pending = { phase, operation, resolveResponse, reject };
      this.pending.timer = this.setTimer(() => this.fail(Object.assign(new Error("observation deadline elapsed"), {
        code: "ETIMEDOUT",
      })), HANG_GUARD_MS);
    });
  }

  settle(error, value) {
    const pending = this.pending;
    this.pending = null;
    if (!pending) return;
    this.clearTimer(pending.timer);
    if (error) pending.reject(error);
    else pending.resolveResponse(value);
  }

  fail(cause) {
    this.failure ??= this.error(this.pending?.phase ?? "idle", this.pending?.operation ?? "observation", cause);
    this.settle(this.failure);
  }

  async read(operation) {
    try {
      await this.ready;
      const response = this.expect("read", operation);
      if (!this.failure) this.child.stdin.write("read\n");
      return await response;
    } catch (error) {
      try { await this.close(true); }
      catch (cleanupError) { throwWithCleanup(error, [cleanupError]); }
      throw error;
    }
  }

  async waitClosed(allowanceMs) {
    let timer;
    try {
      return await Promise.race([this.closed, new Promise((resolveWait) => {
        timer = this.setTimer(() => resolveWait(null), allowanceMs);
      })]);
    } finally { this.clearTimer(timer); }
  }

  close(force = false) {
    this.closing ??= (async () => {
      if (!this.result) {
        if (force) this.child.kill();
        else this.child.stdin.end("close\n");
        // A forced close only waits a moment before killing again; running out
        // of it fails nothing. What fails is closure never being confirmed, which
        // keeps the fixtures: that proves the reader ended, not how fast.
        if (!await this.waitClosed(force ? 1000 : HANG_GUARD_MS)) {
          const deadline = this.error("shutdown", "closure", Object.assign(new Error("shutdown deadline elapsed"), { code: "ETIMEDOUT" }));
          this.child.kill();
          if (!await this.waitClosed(HANG_GUARD_MS)) throw this.error("shutdown", "closure", new Error("reader closure unconfirmed after kill"));
          if (!force) throw deadline;
        }
      }
      if (!force && (this.result.code !== 0 || this.result.signal)) {
        throw this.error("shutdown", "closure", new Error(`reader exited (${this.result.code}, ${this.result.signal}): ${this.stderr}`));
      }
      if (!force && this.failure) throw this.error("shutdown", "closure", this.failure);
    })();
    return this.closing;
  }

  detach() {
    this.child.off("error", this.onError);
    this.child.off("close", this.onClose);
    this.child.stdin.off("error", this.onError);
    this.child.stdout.off("data", this.onData);
    this.child.stderr.off("data", this.onStderr);
  }
}

function windowsSetDacl(directory, dacl) {
  assertExactOwnedFixture(directory);
  pwsh(
    "$acl = Get-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH; $acl.SetSecurityDescriptorSddlForm($env:ROOT_PROBE_ACL_SDDL, [System.Security.AccessControl.AccessControlSections]::Access); Set-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH -AclObject $acl",
    { ROOT_PROBE_ACL_PATH: directory, ROOT_PROBE_ACL_SDDL: dacl },
  );
}

function windowsAddDenyCreate(directory) {
  assertExactOwnedFixture(directory);
  pwsh(
    "$acl = Get-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH; $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User; if (-not $sid) { throw 'current Windows identity has no User SID' }; $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'CreateFiles,AppendData', 'None', 'None', 'Deny'); $acl.AddAccessRule($rule) | Out-Null; Set-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH -AclObject $acl",
    { ROOT_PROBE_ACL_PATH: directory },
  );
}

export function setupOwnedStore(prefix, deps = {}) {
  const create = deps.fixtureHome ?? fixtureHome;
  const write = deps.writeProject ?? writeProject;
  const init = deps.init ?? ((home, projectFile) => run(home, projectFile, ["init"]));
  const remove = deps.remove ?? removeOwned;
  const projectFileName = deps.projectFileName ?? ".engram-project";
  const created = [];
  try {
    const home = create(`${prefix}home-`);
    created.push(home);
    const projectDir = create(`${prefix}root-`);
    created.push(projectDir);
    const projectId = `${prefix}project`;
    write(projectDir, projectId, projectFileName);
    const projectFile = join(projectDir, projectFileName);
    const initialized = init(home, projectFile);
    assert.equal(initialized.status, 0, initialized.stderr);
    return { home, projectDir, projectFile, projectId, created };
  } catch (error) {
    const cleanup = [];
    for (const path of created) {
      try {
        remove(path);
      } catch (cleanupError) {
        cleanup.push(cleanupError);
      }
    }
    throwWithCleanup(error, cleanup);
  }
}

export async function withOwnedStore(t, prefix, body, deps = {}) {
  const setup = deps.setupOwnedStore ?? setupOwnedStore;
  const remove = deps.remove ?? removeOwned;
  const fixtures = setup(prefix);
  const session = createSession();
  const restore = deps.restore ?? (async () => {});
  let finished = false;

  const runCleanup = () =>
    cleanupDeniedResources({
      shutdown: () => session.shutdown(),
      restore,
      closeReader: deps.closeReader,
      deleteRoot: () => remove(fixtures.projectDir),
      deleteHome: () => remove(fixtures.home),
    });

  t.after(async () => {
    if (finished) return;
    const { errors } = await runCleanup();
    if (errors.length) {
      console.error(new AggregateError(errors, "owned-store after-hook cleanup failed"));
    }
  });

  let result;
  let bodyError;
  try {
    result = await body({ ...fixtures, session });
  } catch (error) {
    bodyError = error;
  }
  const { errors } = await runCleanup();
  finished = true;
  if (bodyError) throwWithCleanup(bodyError, errors);
  if (errors.length) throw new AggregateError(errors, errors[0].message);
  return result;
}

async function withJournal(projectDir, body, { createJournal = (directory) => new RootJournal(directory) } = {}) {
  const journal = createJournal(projectDir);
  try {
    await journal.start();
    const result = await body(journal);
    journal.assertHealthy("final journal");
    return result;
  } finally {
    journal.close();
  }
}

// Proves the journal sees a marker-named create and remove in this root, so a
// later window with no probe events is evidence rather than a deaf watcher.
async function calibrateJournal(journal, projectDir) {
  journal.assertHealthy("before calibration");
  await journal.barrier("calibration-ready");
  const started = journal.cursor();
  const marker = join(projectDir, `.${PROBE_MARK}-calibration.Probe`);
  writeFileSync(marker, "calibration");
  await journal.waitFor(
    () => journal.probeEvents(started, journal.cursor()).length > 0,
    HANG_GUARD_MS,
    "calibration marker",
  );
  rmSync(marker);
  const until = await journal.barrier("calibration");
  assert.ok(
    journal.probeEvents(started, until).length > 0,
    `calibration produced no probe-marker events; journal=${JSON.stringify(journal.events)}`,
  );
}

async function assertNoProbe(journal, home, projectFile, args, label) {
  journal.assertHealthy(`before ${label}`);
  const started = journal.cursor();
  const output = run(home, projectFile, args);
  const until = await journal.barrier(label);
  assert.equal(output.status, 0, `${args.join(" ")}\n${output.stderr}`);
  assert.equal(
    journal.probeEvents(started, until).length,
    0,
    `probe events during ${args.join(" ")}: ${JSON.stringify(journal.events)}`,
  );
  assert.deepEqual(
    journal.foreignEvents(started, until, basename(projectFile)),
    [],
    `root writes during ${args.join(" ")}: ${JSON.stringify(journal.events)}`,
  );
  return output;
}

// Set-Acl marks every DACL it writes as auto-inherited (the SDDL "AI" control
// flag). A directory whose parents lack that flag, as under this checkout's
// target/, reads back with it after a restore. The flag records how
// inheritance propagates, not who has access, so a restore is compared on
// everything else: every ACE and the other control flags.
export function daclWithoutAutoInherited(sddl) {
  return sddl.replace(/^D:((?:P|AR|AI)*)/u, (_, flags) =>
    `D:${(flags.match(/P|AR|AI/gu) ?? []).filter((flag) => flag !== "AI").join("")}`);
}

export class DeniedCreate {
  constructor(directory, options = {}) {
    this.directory = directory;
    this.windows = process.platform === "win32";
    this.applied = false;
    this.projectId = options.projectId;
    this.originalDacl = undefined;
    this.originalMode = undefined;
    this.chmod = options.chmodSync ?? chmodSync;
    this.stat = options.statSync ?? statSync;
    this.writeFile = options.writeFileSync ?? writeFileSync;
    this.readFile = options.readFileSync ?? readFileSync;
    this.denyImpl = options.deny;
    this.restoreAcl = options.restoreAcl;
    this.reader = options.reader;
    this.getDacl = options.getDacl ?? ((_, operation) => {
      this.reader ??= new WindowsAclReader(this.directory);
      return this.reader.read(operation);
    });
    this.setDacl = options.setDacl ?? windowsSetDacl;
    this.addDeny = options.addDeny ?? windowsAddDenyCreate;
  }

  async apply() {
    if (!this.denyImpl) await this.#capture();
    this.applied = true;
    this.#deny();
    const canary = join(this.directory, "canary-denied-create");
    try {
      this.writeFile(canary, "no");
    } catch (error) {
      if (error.code !== "EACCES" && error.code !== "EPERM") {
        throw error;
      }
      const contents = this.readFile(join(this.directory, ".engram-project"), "utf8");
      assert.equal(contents, this.projectId);
      return;
    }
    let restoreError;
    try {
      await this.restore();
    } catch (error) {
      restoreError = error;
    }
    const failure = new Error("canary write succeeded; create denial is not in force");
    if (restoreError) {
      throw new AggregateError([failure, restoreError], failure.message);
    }
    throw failure;
  }

  async restore() {
    if (!this.applied) return;
    await this.#restoreAcl();
    this.writeFile(join(this.directory, ".postrestore-write"), "ok");
    this.applied = false;
  }

  closeReader() {
    return this.reader?.close();
  }

  async #capture() {
    if (this.windows) {
      this.originalDacl = await this.getDacl(this.directory, "capture");
      assert.ok(this.originalDacl, "original DACL SDDL was empty");
      return;
    }
    this.originalMode = this.stat(this.directory).mode;
  }

  #deny() {
    if (this.denyImpl) {
      this.denyImpl();
      return;
    }
    if (this.windows) {
      this.addDeny(this.directory);
      return;
    }
    this.chmod(this.directory, 0o555);
  }

  async #restoreAcl() {
    if (this.restoreAcl) {
      this.restoreAcl();
      return;
    }
    if (this.windows) {
      assert.ok(this.originalDacl, "original DACL was not captured");
      this.setDacl(this.directory, this.originalDacl);
      assert.equal(
        daclWithoutAutoInherited(await this.getDacl(this.directory, "restore-verification")),
        daclWithoutAutoInherited(this.originalDacl),
        "restored DACL does not equal the captured DACL",
      );
      return;
    }
    this.chmod(this.directory, this.originalMode & 0o7777);
  }
}

export class McpSession {
  constructor(home, projectFile) {
    this.stderr = "";
    this.buffer = "";
    this.nextId = 1;
    this.pending = new Map();
    this.closing = null;
    this.child = spawn(
      binary,
      [
        "--home",
        home,
        "--project-file",
        projectFile,
        "mcp",
        "--actor-id",
        "probe-mcp",
        "--session-id",
        "probe-mcp-session",
      ],
      { cwd: repo, env: childEnv(), stdio: ["pipe", "pipe", "pipe"] },
    );
    this.child.on("error", (error) => this.#failAll(error));
    this.child.stdin.on("error", (error) => this.#failAll(error));
    this.child.stderr.setEncoding("utf8").on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.child.stdout.setEncoding("utf8").on("data", (chunk) => this.#receive(chunk));
    this.closed = new Promise((resolvePromise) => {
      this.child.once("close", (code, signal) => resolvePromise({ code, signal }));
    });
    this.child.once("exit", (code, signal) => {
      if (this.pending.size) {
        this.#failAll(
          new Error(`MCP exited prematurely code=${code} signal=${signal}: ${this.stderr}`),
        );
      }
    });
  }

  #failAll(error) {
    for (const [id, pending] of this.pending) {
      this.pending.delete(id);
      pending.reject(error);
    }
  }

  #receive(chunk) {
    this.buffer += chunk.toString("utf8");
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (line === "") continue;
      const message = JSON.parse(line);
      if (message.id === undefined) continue;
      const pending = this.pending.get(String(message.id));
      if (!pending) continue;
      this.pending.delete(String(message.id));
      if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
      else pending.resolve(message.result);
    }
  }

  request(method, params) {
    const id = this.nextId++;
    const message = { jsonrpc: "2.0", id, method };
    if (params !== undefined) message.params = params;
    return new Promise((resolvePromise, reject) => {
      let settled = false;
      const fail = (error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.pending.delete(String(id));
        reject(error);
      };
      const timer = setTimeout(() => {
        fail(new Error(`MCP timeout ${method}: ${this.stderr}`));
      }, HANG_GUARD_MS);
      this.pending.set(String(id), {
        resolve: (value) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          this.pending.delete(String(id));
          resolvePromise(value);
        },
        reject: fail,
      });
      try {
        if (this.child.exitCode !== null || this.child.signalCode) {
          fail(
            new Error(
              `MCP not running exit=${this.child.exitCode} signal=${this.child.signalCode}: ${this.stderr}`,
            ),
          );
          return;
        }
        this.child.stdin.write(`${JSON.stringify(message)}\n`);
      } catch (error) {
        fail(error);
      }
    });
  }

  async handshakeAndPeek() {
    await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "root-probe", version: "1" },
    });
    try {
      this.child.stdin.write(
        `${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`,
      );
    } catch (error) {
      throw new Error(`MCP initialized notify failed: ${error.message}; ${this.stderr}`);
    }
    return assertSuccessfulPeek(
      await this.request("tools/call", {
        name: "next",
        arguments: { peek: true },
      }),
    );
  }

  async close() {
    if (this.closing) return this.closing;
    this.closing = this.#close();
    return this.closing;
  }

  async #close() {
    this.#failAll(new Error("MCP session closing"));
    if (!this.child.stdin.destroyed) {
      try {
        this.child.stdin.end();
      } catch {
        // Child may already have closed stdin.
      }
    }
    return waitForChildClose(this.closed, () => this.child.kill());
  }
}

export function createSession() {
  const clients = [];
  let confirmed = false;
  return {
    attach(client) {
      clients.push(client);
      confirmed = false;
      return client;
    },
    get shutdownConfirmed() {
      return confirmed && clients.length === 0;
    },
    async shutdown() {
      if (confirmed && clients.length === 0) return;
      const errors = [];
      let index = 0;
      while (index < clients.length) {
        try {
          await Promise.resolve().then(() => clients[index].close());
          clients.splice(index, 1);
        } catch (error) {
          errors.push(error);
          index += 1;
        }
      }
      if (errors.length) {
        confirmed = false;
        throw errors.length === 1
          ? errors[0]
          : new AggregateError(errors, errors[0].message);
      }
      confirmed = true;
    },
  };
}

export async function withDeniedRoot(t, prefix, body, deps = {}) {
  const Denied = deps.DeniedCreate ?? DeniedCreate;
  let denial;
  return withOwnedStore(
    t,
    prefix,
    async (owned) => {
      denial = new Denied(owned.projectDir, { projectId: owned.projectId });
      await denial.apply();
      return body({ ...owned, denial });
    },
    {
      ...deps,
      restore: () => denial?.restore(),
      closeReader: () => denial?.closeReader(),
    },
  );
}

export async function withWritableRoot(t, prefix, body, deps = {}) {
  return withOwnedStore(t, prefix, body, deps);
}

export async function withMcp(session, home, projectFile, body, deps = {}) {
  const open = deps.open ?? ((h, p) => new McpSession(h, p));
  const mcp = session ? session.attach(open(home, projectFile)) : open(home, projectFile);
  let result;
  let bodyError;
  try {
    result = await body(mcp);
  } catch (error) {
    bodyError = error;
  }
  try {
    if (session) await session.shutdown();
    else await mcp.close();
  } catch (shutdownError) {
    if (bodyError) throwWithCleanup(bodyError, [shutdownError]);
    throw shutdownError;
  }
  if (bodyError) throw bodyError;
  return result;
}

export function registerRootProbeTests(test) {
  const support = {
    RootJournal,
    withJournal,
    DeniedCreate,
    cleanupDeniedResources,
    writeProject,
    daclWithoutAutoInherited,
    assertExactOwnedFixture,
    isExactOwnedFixturePath,
    setupOwnedStore,
    withMcp,
    createSession,
    withDeniedRoot,
    withWritableRoot,
    waitForChildClose,
    withOwnedStore,
    calibrateJournal,
    assertNoProbe,
    assertResolvedDoctorPolicy,
    WARNING,
    RESOLVED_POLICY,
    CONTROL_ARGS,
    workArgs,
    addedRef,
    run,
    BARRIER_PREFIX,
    HANG_GUARD_MS,
    ...createTestSupport({ RootJournal, WindowsAclReader }),
  };
  registerJournalStartup(test, support);
  registerAclReader(test, support);
  registerOwnershipGuards(test, support);
  registerJournalEvents(test, support);
  registerAclRestoration(test, support);
  registerResourceCleanup(test, support);
  registerRootIdentity(test, support);
}
