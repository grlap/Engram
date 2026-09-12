// Process and unit discriminators for unused project-root identity probes.
// Watcher journals marker create/remove by event index; leftover read_dir is
// not the proof. Denied-create uses a real ACL/permission canary plus host
// WARNING control. Call registerRootProbeTests(test) from an existing Node
// gate so the required nine stay nine.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { watch } from "node:fs";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import win32 from "node:path/win32";
import { EventEmitter } from "node:events";

import { fixtureHome, fixtureRoot, removeFixtureHomes } from "./test-temp.mjs";

const repo = resolve(import.meta.dirname, "..");
const target = resolve(repo, process.env.CARGO_TARGET_DIR || "target");
const binary = join(
  target,
  "debug",
  process.platform === "win32" ? "engram.exe" : "engram",
);
const PROBE_MARK = "engram-path-probe";
const WARNING = "could not probe the filesystem identity";
const ACL_TIMEOUT_MS = 15000;
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
    timeout: extra.timeout ?? 30000,
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
  constructor(directory, { watchFn = watch } = {}) {
    super();
    this.directory = directory;
    this.events = [];
    this.errors = [];
    this.nextIndex = 0;
    this.watcher = watchFn(directory, { persistent: true }, (_type, name) => {
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

  probeEvents(fromIndex, untilIndex) {
    return this.events.filter(
      (event) =>
        event.index >= fromIndex &&
        event.index < untilIndex &&
        event.filename.includes(PROBE_MARK),
    );
  }

  assertHealthy(label) {
    if (this.errors.length) {
      throw new Error(`${label}: watcher inconclusive: ${this.errors[0]}`);
    }
  }

  async waitFor(predicate, timeoutMs, label) {
    if (this.errors.length) {
      throw new Error(`${label}: preexisting watcher error: ${this.errors[0]}`);
    }
    if (predicate()) return;
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
        finish(() =>
          reject(new Error(`${label}: timed out waiting for journal event`)),
        );
      }, timeoutMs);
      this.on("event", onEvent);
    });
  }

  async barrier(label) {
    const from = this.nextIndex;
    const filename = `.watch-barrier-${label}`;
    writeFileSync(join(this.directory, filename), label);
    await this.waitFor(
      () =>
        this.events.some(
          (event) => event.index >= from && event.filename === filename,
        ),
      5000,
      `barrier ${label}`,
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
  { gracefulMs = 10000, forcedMs = 1000 } = {},
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

function writeProject(projectDir, id) {
  mkdirSync(projectDir, { recursive: true });
  writeFileSync(join(projectDir, ".engram-project"), id);
}

export function throwWithCleanup(primary, cleanupErrors) {
  if (!cleanupErrors.length) throw primary;
  throw new AggregateError([primary, ...cleanupErrors], primary.message);
}

export async function cleanupDeniedResources({
  shutdown,
  restore,
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
    timeout: ACL_TIMEOUT_MS,
    env: { ...childEnv(), ...extraEnv },
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`pwsh ACL failed (${result.status}): ${result.stdout} ${result.stderr}`);
  }
  return result.stdout.trim();
}

function windowsDacl(directory) {
  assertExactOwnedFixture(directory);
  return pwsh(
    "(Get-Acl -LiteralPath $env:ROOT_PROBE_ACL_PATH).GetSecurityDescriptorSddlForm('Access')",
    { ROOT_PROBE_ACL_PATH: directory },
  );
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
  const created = [];
  try {
    const home = create(`${prefix}home-`);
    created.push(home);
    const projectDir = create(`${prefix}root-`);
    created.push(projectDir);
    const projectId = `${prefix}project`;
    write(projectDir, projectId);
    const projectFile = join(projectDir, ".engram-project");
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

async function withJournal(projectDir, body) {
  const journal = new RootJournal(projectDir);
  try {
    const result = await body(journal);
    journal.assertHealthy("final journal");
    return result;
  } finally {
    journal.close();
  }
}

async function calibratedWindow(journal, home, projectFile, hostArgs, label) {
  journal.assertHealthy(`before ${label}`);
  const started = journal.cursor();
  const host = run(home, projectFile, hostArgs);
  const until = await journal.barrier(`${label}-after-host`);
  const probes = journal.probeEvents(started, until);
  assert.ok(
    probes.length > 0,
    `${label} calibration produced no probe-marker events; journal=${JSON.stringify(journal.events)} stderr=${host.stderr}`,
  );
  return host;
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
  return output;
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
    this.getDacl = options.getDacl ?? windowsDacl;
    this.setDacl = options.setDacl ?? windowsSetDacl;
    this.addDeny = options.addDeny ?? windowsAddDenyCreate;
  }

  apply() {
    if (!this.denyImpl) this.#capture();
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
      this.restore();
    } catch (error) {
      restoreError = error;
    }
    const failure = new Error("canary write succeeded; create denial is not in force");
    if (restoreError) {
      throw new AggregateError([failure, restoreError], failure.message);
    }
    throw failure;
  }

  restore() {
    if (!this.applied) return;
    this.#restoreAcl();
    this.writeFile(join(this.directory, ".postrestore-write"), "ok");
    this.applied = false;
  }

  #capture() {
    if (this.windows) {
      this.originalDacl = this.getDacl(this.directory);
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

  #restoreAcl() {
    if (this.restoreAcl) {
      this.restoreAcl();
      return;
    }
    if (this.windows) {
      assert.ok(this.originalDacl, "original DACL was not captured");
      this.setDacl(this.directory, this.originalDacl);
      assert.equal(
        this.getDacl(this.directory),
        this.originalDacl,
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
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
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
      }, 15000);
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
      denial.apply();
      return body({ ...owned, denial });
    },
    {
      ...deps,
      restore: () => denial?.restore(),
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

function silentWatch() {
  const watcher = new EventEmitter();
  watcher.close = () => {};
  return watcher;
}

export function registerRootProbeTests(test) {
  test("owned-fixture guard accepts a child and refuses same-root or sibling", (t) => {
    const child = fixtureHome("rpj-guard-", t);
    assert.doesNotThrow(() => assertExactOwnedFixture(child));
    assert.throws(() => assertExactOwnedFixture(fixtureRoot), /non-owned fixture path/u);
    const sibling = resolve(fixtureRoot, "..", "sibling-not-owned");
    assert.throws(() => assertExactOwnedFixture(sibling), /non-owned fixture path/u);
  });

  test("owned-fixture guard refuses Windows different-drive and UNC relatives", () => {
    const root = "C:\\fixture";
    assert.equal(isExactOwnedFixturePath("C:\\fixture\\child", root, win32), true);
    assert.equal(isExactOwnedFixturePath("C:\\fixture", root, win32), false);
    assert.equal(isExactOwnedFixturePath("C:\\other", root, win32), false);
    assert.equal(isExactOwnedFixturePath("D:\\outside", root, win32), false);
    assert.equal(isExactOwnedFixturePath("\\\\server\\share\\outside", root, win32), false);
  });

  test("root journal waitFor times out and cleans listeners", async (t) => {
    const dir = fixtureHome("rpj-timeout-", t);
    const journal = new RootJournal(dir, { watchFn: () => silentWatch() });
    t.after(() => journal.close());
    await assert.rejects(
      () => journal.waitFor(() => false, 40, "never"),
      /timed out waiting for journal event/u,
    );
    assert.equal(journal.listenerCount("event"), 0);
  });

  test("root journal rejects preexisting errors and null filenames", async (t) => {
    const dir = fixtureHome("rpj-null-", t);
    const preexisting = new RootJournal(dir, {
      watchFn: () => {
        const watcher = silentWatch();
        queueMicrotask(() => watcher.emit("error", new Error("overflow")));
        return watcher;
      },
    });
    t.after(() => preexisting.close());
    await new Promise((resolvePromise) => setImmediate(resolvePromise));
    await assert.rejects(
      () => preexisting.waitFor(() => true, 40, "pre"),
      /preexisting watcher error: Error: overflow/u,
    );

    const nullName = new RootJournal(dir, {
      watchFn: (_directory, _options, callback) => {
        const watcher = silentWatch();
        queueMicrotask(() => callback("rename", null));
        return watcher;
      },
    });
    t.after(() => nullName.close());
    await assert.rejects(
      () => nullName.waitFor(() => false, 200, "null-name"),
      /filename missing or overflow/u,
    );
  });

  test("denied-create canary success and restore failure are not swallowed", (t) => {
    const dir = fixtureHome("rpj-canary-", t);
    writeProject(dir, "canary-project");
    const succeeded = new DeniedCreate(dir, {
      projectId: "canary-project",
      deny: () => {},
      restoreAcl: () => {},
      writeFileSync: () => {},
      readFileSync: () => "canary-project",
    });
    assert.throws(() => succeeded.apply(), /canary write succeeded/u);

    const restoreFailed = new DeniedCreate(dir, {
      projectId: "canary-project",
      deny: () => {},
      restoreAcl: () => {
        throw new Error("restore exploded");
      },
      writeFileSync: () => {},
      readFileSync: () => "canary-project",
    });
    assert.throws(() => restoreFailed.apply(), (error) => {
      assert.ok(error instanceof AggregateError);
      assert.match(error.errors[0].message, /canary write succeeded/u);
      assert.match(error.errors[1].message, /restore exploded/u);
      return true;
    });
  });

  test("denied setup failure owns temps from the first create", () => {
    const created = [];
    const removed = [];
    assert.throws(
      () =>
        setupOwnedStore("rpj-setup-", {
          fixtureHome(prefix) {
            created.push(prefix);
            return `/tmp/${prefix}${created.length}`;
          },
          writeProject() {},
          init() {
            throw new Error("init failed");
          },
          remove(path) {
            removed.push(path);
          },
        }),
      /init failed/u,
    );
    assert.deepEqual(created, ["rpj-setup-home-", "rpj-setup-root-"]);
    assert.deepEqual(removed, ["/tmp/rpj-setup-home-1", "/tmp/rpj-setup-root-2"]);
  });

  test("denied cleanup shuts down before restore and skips root delete after restore failure", async () => {
    const failed = await cleanupDeniedResources({
      shutdown: async () => {},
      restore: async () => {
        throw new Error("restore failed");
      },
      deleteRoot: async () => {
        throw new Error("root should not be deleted");
      },
      deleteHome: async () => {},
    });
    assert.equal(failed.restored, false);
    assert.deepEqual(failed.steps, [
      "shutdown",
      "restore-failed",
      "no-delete-root",
      "delete-home",
    ]);
    assert.equal(failed.errors.length, 1);
    assert.match(failed.errors[0].message, /restore failed/u);

    const ok = await cleanupDeniedResources({
      shutdown: async () => {},
      restore: async () => {},
      deleteRoot: async () => {},
      deleteHome: async () => {},
    });
    assert.equal(ok.restored, true);
    assert.deepEqual(ok.steps, ["shutdown", "restore", "delete-root", "delete-home"]);
  });

  test("withMcp keeps the body error when shutdown also fails", async () => {
    const bodyError = new Error("body A");
    const shutdownError = new Error("shutdown B");
    await assert.rejects(
      () =>
        withMcp(
          {
            attach(client) {
              return client;
            },
            async shutdown() {
              throw shutdownError;
            },
          },
          "home",
          "project",
          async () => {
            throw bodyError;
          },
          { open: () => ({ close: async () => {} }) },
        ),
      (error) => {
        assert.ok(error instanceof AggregateError);
        assert.equal(error.errors[0], bodyError);
        assert.equal(error.errors[1], shutdownError);
        assert.equal(error.message, bodyError.message);
        return true;
      },
    );
  });

  test("failed session close stays failed on retry and skips fixture delete", async () => {
    const journal = [];
    const session = createSession();
    session.attach({
      async close() {
        journal.push("close");
        throw new Error("close failed");
      },
    });
    await assert.rejects(() => session.shutdown(), /close failed/u);
    await assert.rejects(() => session.shutdown(), /close failed/u);
    assert.deepEqual(journal, ["close", "close"]);
    assert.equal(session.shutdownConfirmed, false);

    const cleanup = await cleanupDeniedResources({
      shutdown: () => session.shutdown(),
      restore: async () => {
        journal.push("restore");
      },
      deleteRoot: async () => {
        journal.push("delete-root");
      },
      deleteHome: async () => {
        journal.push("delete-home");
      },
    });
    assert.equal(cleanup.shutdownOk, false);
    assert.deepEqual(cleanup.steps, [
      "shutdown-failed",
      "restore",
      "no-delete-root",
      "no-delete-home",
    ]);
    assert.deepEqual(journal, ["close", "close", "close", "restore"]);
    assert.match(cleanup.errors[0].message, /close failed/u);
  });

  test("denied apply owes restore before mutation; wrapper restores injected deny throw", async (t) => {
    const journal = [];
    const state = { current: "captured" };
    await assert.rejects(
      () =>
        withDeniedRoot(
          t,
          "rpj-deny-",
          async () => {
            throw new Error("body should not run");
          },
          {
            setupOwnedStore: () => ({
              home: "H",
              projectDir: "R",
              projectFile: "R/.engram-project",
              projectId: "rpj-deny-project",
            }),
            DeniedCreate: class InjectedDeny extends DeniedCreate {
              constructor(directory, options) {
                super(directory, {
                  ...options,
                  deny() {
                    journal.push("deny");
                    state.current = "mutated";
                    throw new Error("deny failed after mutation");
                  },
                  restoreAcl() {
                    journal.push("restore");
                    state.current = "captured";
                  },
                  writeFileSync() {},
                  readFileSync: () => options.projectId,
                });
              }
            },
            remove(path) {
              journal.push(`delete:${path}`);
            },
          },
        ),
      (error) => {
        assert.match(error.message, /deny failed after mutation/u);
        assert.equal(error instanceof AggregateError, false);
        return true;
      },
    );
    assert.equal(state.current, "captured");
    assert.deepEqual(journal, ["deny", "restore", "delete:R", "delete:H"]);
  });

  test("writable wrapper keeps both fixtures when MCP close fails", async (t) => {
    const journal = [];
    await assert.rejects(
      () =>
        withWritableRoot(
          t,
          "rpw-close-",
          async ({ session }) => {
            await withMcp(session, "H", "project", async () => {}, {
              open: () => ({
                async close() {
                  journal.push("close");
                  throw new Error("close failed");
                },
              }),
            });
          },
          {
            setupOwnedStore: () => ({
              home: "H",
              projectDir: "R",
              projectFile: "R/.engram-project",
              projectId: "rpw-close-project",
            }),
            remove(path) {
              journal.push(`delete:${path}`);
            },
          },
        ),
      (error) => {
        assert.match(error.message, /close failed/u);
        return true;
      },
    );
    assert.ok(journal.includes("close"), journal.join(","));
    assert.equal(journal.includes("delete:H"), false, journal.join(","));
    assert.equal(journal.includes("delete:R"), false, journal.join(","));
  });

  test("MCP graceful close avoids forced kill", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    let killed = 0;
    const closed = new Promise((resolvePromise) => {
      setTimeout(() => resolvePromise({ code: 0, signal: null }), 25);
    });
    const waiting = waitForChildClose(closed, () => {
      killed += 1;
    }, { gracefulMs: 10000, forcedMs: 1000 });
    t.mock.timers.tick(25);
    await waiting;
    assert.equal(killed, 0);
  });

  test("writable host probe events calibrate the journal; agent and MCP emit none", async (t) => {
    await withWritableRoot(t, "rpw-", async ({ home, projectDir, projectFile, session: mcpSession }) => {
    await withJournal(projectDir, async (journal) => {
      const doctor = await calibratedWindow(
        journal,
        home,
        projectFile,
        ["doctor", "--json"],
        "doctor",
      );
      assertResolvedDoctorPolicy(doctor);

      const session = "probe-session";
      for (const [label, args] of [
        ["next", workArgs(session, "next")],
        ["peek", workArgs(session, "next", "--peek")],
        ["ls", workArgs(session, "ls")],
        ["memories", workArgs(session, "memories")],
        ["core-next-focus", workArgs(session, "core", "next", "--sections", "focus")],
      ]) {
        await assertNoProbe(journal, home, projectFile, args, label);
      }

      const added = await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "add", "Probe mutation", "--json"),
        "add",
      );
      const ref = addedRef(added);
      const shown = await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "show", ref),
        "show",
      );
      assert.match(shown.stdout, new RegExp(ref));
      await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "core", "focus", ref),
        "core-focus",
      );
      await assertNoProbe(
        journal,
        home,
        projectFile,
        ["graph", "--actor-id", "probe", "--session-id", session, "save", "--stdout"],
        "graph",
      );
      await assertNoProbe(
        journal,
        home,
        projectFile,
        ["backup", "--out", join(home, "probe-backup.db")],
        "backup",
      );

      const mcpStarted = journal.cursor();
      await withMcp(mcpSession, home, projectFile, async (mcp) => {
        await mcp.handshakeAndPeek();
        assert.doesNotMatch(mcp.stderr, new RegExp(WARNING));
      });
      const mcpUntil = await journal.barrier("mcp");
      assert.equal(
        journal.probeEvents(mcpStarted, mcpUntil).length,
        0,
        `probe events during MCP: ${JSON.stringify(journal.events)}`,
      );
    });
    });
  });

  test("denied-create root keeps host WARNING and agent/MCP succeed without it", async (t) => {
    await withDeniedRoot(t, "rpd-", async ({ home, projectFile, session }) => {
      const doctor = run(home, projectFile, ["doctor", "--json"]);
      assert.match(
        doctor.stderr,
        new RegExp(WARNING),
        `host doctor must demonstrate resolver warning: ${doctor.stderr}`,
      );

      const actor = "probe-denied";
      for (const args of [
        workArgs(actor, "next"),
        workArgs(actor, "next", "--peek"),
        workArgs(actor, "ls"),
        workArgs(actor, "memories"),
        workArgs(actor, "core", "next", "--sections", "focus"),
      ]) {
        const output = run(home, projectFile, args);
        assert.equal(output.status, 0, `${args.join(" ")}\n${output.stderr}`);
        assert.doesNotMatch(output.stderr, new RegExp(WARNING), output.stderr);
        assert.equal(output.stderr.includes("ENGRAM_HOST_PATH_POLICY"), false);
      }

      const added = run(home, projectFile, workArgs(actor, "add", "Denied mutation", "--json"));
      const ref = addedRef(added);
      assert.doesNotMatch(added.stderr, new RegExp(WARNING), added.stderr);
      const shown = run(home, projectFile, workArgs(actor, "show", ref));
      assert.equal(shown.status, 0, shown.stderr);
      assert.match(shown.stdout, new RegExp(ref));
      assert.doesNotMatch(shown.stderr, new RegExp(WARNING), shown.stderr);
      const focused = run(home, projectFile, workArgs(actor, "core", "focus", ref));
      assert.equal(focused.status, 0, focused.stderr);
      assert.doesNotMatch(focused.stderr, new RegExp(WARNING), focused.stderr);

      await withMcp(session, home, projectFile, async (mcp) => {
        await mcp.handshakeAndPeek();
        assert.doesNotMatch(mcp.stderr, new RegExp(WARNING), mcp.stderr);
      });
    });
  });
}
