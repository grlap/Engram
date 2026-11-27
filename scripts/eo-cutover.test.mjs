import test from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { fixtureHome } from "./test-temp.mjs";
import { executePlan, recover } from "./eo-cutover.mjs";
import { CutoverError, fingerprint, retiredTables, retiredColumns } from "./eo-cutover-values.mjs";

const digest = text => createHash("sha256").update(text).digest("hex");
const store = (home, project) => path.join(home, "projects", digest(project.id), "engram.db");
const packetSuffixes = ["", "-wal", "-shm", "-journal"];
const encode = rows => {
  const grouped = new Map();
  for (const { table, values } of rows) { if (!grouped.has(table)) grouped.set(table, []); grouped.get(table).push(values); }
  return JSON.stringify({ engram_export: { format: "engram-json-export", tables: [...grouped].map(([name, values]) => ({ name, columns: Object.keys(values[0]), rows: values.length })), sequences: {}, left_out: [] } }) + "\n"
    + rows.map(row => JSON.stringify({ row })).join("\n") + "\n" + JSON.stringify({ end: { rows: rows.length } }) + "\n";
};
function converted(text) {
  const lines = text.trim().split("\n").map(line => JSON.parse(line));
  const header = lines[0].engram_export;
  const rows = lines.slice(1, -1).map(item => item.row).filter(row => !retiredTables.includes(row.table));
  for (const row of rows) if (row.table === "control_policy_state") for (const column of retiredColumns) delete row.values[column];
  const report = { checked_objects: 1, checked_control_records: 1, checked_work_records: 1, checked_pending_deliveries: 0, left_out: [] };
  for (const table of header.tables) {
    if (retiredTables.includes(table.name)) report.left_out.push({ name: table.name, rows: table.rows, reason: "retired turn-admission table" });
    if (table.name === "control_policy_state") for (const column of retiredColumns) report.left_out.push({ name: `${table.name}.${column}`, rows: table.rows, reason: "retired turn-admission column" });
    if (table.name === "work_schema_metadata") report.left_out.push({ name: table.name, rows: table.rows, reason: "format marker; the new store keeps its own" });
  }
  return { text: encode(rows), report };
}
async function fixture(t, count = 2) {
  const root = fixtureHome("cutover-", t);
  const home = path.join(root, "source");
  const projects = Array.from({ length: count }, (_, index) => ({ name: `Project${index}`, id: `example/project-${index}`, host_path_policy: "case_fold" }));
  const rows = [
    { table: "objects", values: { object_id: "minted-id", object_kind: "note", canonical_json: { text: '{"private":"sentinel-secret"}' } } },
    { table: "control_policy_state", values: { singleton: 1, policy_epoch: 1, required_assurance: "old", supported_effects_json: "[]", grant_ttl_seconds: 30 } },
    { table: "work_schema_metadata", values: { singleton: 1, schema_version: 1 } },
    ...retiredTables.map(table => ({ table, values: { id: "old-row" } })),
  ];
  const original = encode(rows);
  for (const project of projects) {
    await fs.mkdir(path.dirname(store(home, project)), { recursive: true });
    for (const suffix of packetSuffixes) await fs.writeFile(store(home, project) + suffix, suffix ? `original${suffix}` : original);
  }
  async function artifact(role) {
    const binary = path.join(root, `${role}.exe`);
    await fs.writeFile(binary, role);
    return { binary, profile: "release", sha256: await fingerprint(binary), source_revision: digest(role).slice(0, 40), build_fingerprint: digest(`build-${role}`), schema_reference: digest(`schema-${role}`) };
  }
  const old = await artifact("old"); const final = await artifact("final");
  const plan = { format: "engram-cutover-plan-1", home, install_path: old.binary, installed_sha256: old.sha256, old, final, projects };
  const events = []; const calls = [];
  const options = {
    workDirectory: path.join(root, "work"), emit: event => events.push(event), guard: async () => {},
    secureDirectory: async directory => fs.mkdir(directory, { recursive: true }),
    invoke: async (binary, args) => {
      calls.push(args);
      const value = flag => args[args.indexOf(flag) + 1];
      if (args.includes("readiness")) {
        const artifact = await fs.readFile(binary, "utf8") === "old" ? old : final;
        const id = (await fs.readFile(value("--project-file"), "utf8")).trim();
        return { code: 0, stdout: JSON.stringify({ ready: true, project_id: id, database: store(value("--home"), { id }), build_fingerprint: artifact.build_fingerprint,
          build: { executable_sha256: artifact.sha256, source_revision: artifact.source_revision, schema_reference: artifact.schema_reference } }) };
      }
      if (args[1] === "export") { await fs.copyFile(value("--database"), value("--out")); return { code: 0, stdout: "{}" }; }
      if (args[1] === "import") {
        const result = converted(await fs.readFile(value("--file"), "utf8"));
        await fs.writeFile(value("--out"), result.text);
        return { code: 0, stdout: JSON.stringify(result.report) };
      }
      throw new Error("unexpected fake command");
    },
  };
  const managedHome = path.join(options.workDirectory, "rehearsal", "home");
  async function unchanged() {
    for (const project of projects) for (const suffix of packetSuffixes) {
      assert.equal(await fs.readFile(store(home, project) + suffix, "utf8"), suffix ? `original${suffix}` : original);
      assert.equal(await fs.readFile(store(managedHome, project) + suffix, "utf8"), suffix ? `original${suffix}` : original);
    }
    assert.equal(await fs.readFile(path.join(options.workDirectory, "rehearsal", "bin", "engram.exe"), "utf8"), "old");
  }
  return { root, plan, options, events, calls, managedHome, original, unchanged };
}

test("six-store preparation, activation and idempotent rollback preserve every original packet", async t => {
  const f = await fixture(t, 6);
  const result = await executePlan(f.plan, f.options);
  assert.equal(result.ready, 6);
  assert.equal(result.total, 6);
  assert.equal(f.events.filter(event => event.stage === "prepared").length, 6);
  assert.equal(f.events.filter(event => event.stage === "old_planning_count" && event.count === 0).length, 6);
  assert.ok(f.events.some(event => event.stage === "rollback_command" && event.command.includes("-RollbackJournal")));
  assert.equal(JSON.stringify(f.events).includes("sentinel-secret"), false);
  assert.equal(await fs.readFile(result.install, "utf8"), "final");
  // Readiness can create empty coordination files; preserve them aside during recovery.
  await fs.writeFile(store(result.home, f.plan.projects[0]) + "-wal", "");
  await fs.writeFile(store(result.home, f.plan.projects[0]) + "-shm", "new coordination");
  const recovery = { rollback: true, emit: f.options.emit, guard: f.options.guard };
  assert.equal((await recover(result.journal, recovery)).state, "rolled_back");
  assert.equal((await recover(result.journal, recovery)).state, "rolled_back");
  await f.unchanged();
});

test("failure immediately before or after each activation rename restores all stores and the binary", async t => {
  // Two stores with a main, WAL, SHM and journal: ten store moves plus two binary moves.
  for (const point of ["beforeMove", "afterMove"]) for (let boundary = 0; boundary < 12; boundary++) {
    const f = await fixture(t);
    let index = 0;
    f.options[point] = async () => { if (index++ === boundary) throw new CutoverError("injected_failure", "activate"); };
    await assert.rejects(executePlan(f.plan, f.options), { code: "injected_failure" });
    assert.ok(f.events.some(event => event.state === "rolled_back"), `${point} ${boundary}`);
    await f.unchanged();
  }
});

test("all imports settle before a failed preparation returns; managed files stay unchanged", async t => {
  const f = await fixture(t);
  const invoke = f.options.invoke;
  let active = 0; let settled = 0;
  f.options.invoke = async (binary, args) => {
    if (args[1] !== "import") return invoke(binary, args);
    active++;
    await new Promise(resolve => setTimeout(resolve, 10));
    active--; settled++;
    return { code: 7, stdout: "sentinel-secret" };
  };
  await assert.rejects(executePlan(f.plan, f.options), { code: "engram_command_failed", stage: "import" });
  assert.equal(active, 0); assert.equal(settled, 2);
  await f.unchanged();
  assert.equal(JSON.stringify(f.events).includes("sentinel-secret"), false);
});

test("complete-tree planning is counted before any import and untrusted project labels stay private", async t => {
  const f = await fixture(t);
  const file = store(f.plan.home, f.plan.projects[0]);
  const rows = f.original.trim().split("\n").slice(1, -1).map(line => JSON.parse(line).row);
  rows[0].values.object_kind = "task_sync_import_planning";
  rows[0].values.canonical_json = { json: { project_id: "sentinel-secret", tree: {} } };
  await fs.writeFile(file, encode(rows));
  await assert.rejects(executePlan(f.plan, f.options), { code: "old_complete_planning_refused" });
  assert.equal(f.calls.filter(args => args[1] === "import").length, 0);
  assert.equal(JSON.stringify(f.events).includes("sentinel-secret"), false);
  assert.ok(f.events.some(event => event.stage === "old_planning_project_count" && event.affected_project === "unlisted" && event.count === 1));
});

test("final readiness failure rolls back; changed prepared bytes cannot be blessed by a later snapshot", async t => {
  for (const fault of ["ready", "changed"]) {
    const f = await fixture(t);
    const invoke = f.options.invoke;
    f.options.invoke = async (binary, args) => {
      const result = await invoke(binary, args);
      if (fault === "ready" && binary.includes(`${path.sep}rehearsal${path.sep}`)) return { code: 9, stdout: "private failure" };
      if (fault === "changed" && args[1] === "export" && binary.includes(`${path.sep}final${path.sep}`)) await fs.appendFile(args[args.indexOf("--database") + 1], "changed");
      return result;
    };
    await assert.rejects(executePlan(f.plan, f.options), { code: fault === "ready" ? "engram_command_failed" : "prepared_content_changed" });
    await f.unchanged();
  }
});

test("rollback refuses consumer, changed managed content, changed backup and nonempty new WAL without partial restoration", async t => {
  for (const fault of ["consumer", "main", "backup", "wal"]) {
    const f = await fixture(t);
    const result = await executePlan(f.plan, f.options);
    const journal = JSON.parse(await fs.readFile(result.journal, "utf8"));
    const database = store(result.home, f.plan.projects[0]);
    if (fault === "main") await fs.appendFile(database, "new work");
    if (fault === "backup") await fs.appendFile(journal.actions[0].to, "changed backup");
    if (fault === "wal") await fs.writeFile(database + "-wal", "new work");
    const current = await fingerprint(database);
    await assert.rejects(recover(result.journal, { rollback: true, emit: f.options.emit,
      guard: async () => { if (fault === "consumer") throw new CutoverError("engram_consumer_running", "quiescence"); } }));
    assert.equal(await fingerprint(database), current);
    assert.equal(await fs.readFile(result.install, "utf8"), "final");
  }
});

test("journal pathname and action graph are bound; recovery never follows a substituted path", async t => {
  const f = await fixture(t);
  const result = await executePlan(f.plan, f.options);
  const journal = JSON.parse(await fs.readFile(result.journal, "utf8"));
  const copy = path.join(f.root, "copy.json");
  await fs.writeFile(copy, JSON.stringify(journal));
  await assert.rejects(recover(copy, { guard: f.options.guard, rollback: true }), { code: "journal_path_invalid" });
  journal.actions[0].to = path.join(f.root, "unrelated");
  await fs.writeFile(result.journal, JSON.stringify(journal));
  await assert.rejects(recover(result.journal, { guard: f.options.guard, rollback: true }), { code: "journal_action_invalid" });
  assert.equal(await fs.readFile(result.install, "utf8"), "final");
});

test("recovery resumes after an interrupted reverse rename and ignores a torn next journal", async t => {
  const f = await fixture(t);
  const result = await executePlan(f.plan, f.options);
  await fs.writeFile(store(result.home, f.plan.projects[0]) + "-shm", "new coordination");
  let restoredOldSidecar = false;
  await assert.rejects(recover(result.journal, { rollback: true, emit: f.options.emit, guard: f.options.guard,
    afterReverse: async action => {
      if (action.from.endsWith("-shm")) { restoredOldSidecar = true; throw new CutoverError("interrupted_reverse", "recovery"); }
    } }), { code: "interrupted_reverse" });
  assert.equal(restoredOldSidecar, true);
  await fs.writeFile(result.journal + ".next", "torn JSON");
  assert.equal((await recover(result.journal, { rollback: true, emit: f.options.emit, guard: f.options.guard })).state, "rolled_back");
  await f.unchanged();
});

test("wrong selected project and source census refuse preparation", async t => {
  for (const fault of ["project", "census"]) {
    const f = await fixture(t);
    if (fault === "census") {
      await fs.mkdir(path.join(f.plan.home, "projects", digest("unlisted")), { recursive: true });
      await fs.writeFile(path.join(f.plan.home, "projects", digest("unlisted"), "engram.db"), "unlisted");
    } else {
      const invoke = f.options.invoke;
      f.options.invoke = async (binary, args) => {
        const result = await invoke(binary, args);
        if (args.includes("readiness")) { const body = JSON.parse(result.stdout); body.project_id = "another"; result.stdout = JSON.stringify(body); }
        return result;
      };
    }
    await assert.rejects(executePlan(f.plan, f.options), { code: fault === "project" ? "old_readiness_identity_differs" : "store_census_differs" });
    assert.equal(f.calls.filter(args => args[1] === "import").length, 0);
  }
});

test("backup and sidecar links are refused before following them", async t => {
  const f = await fixture(t);
  const outside = path.join(f.root, "unrelated"); await fs.mkdir(outside);
  let injected = false;
  f.options.guard = async (_scope, selected) => {
    if (selected !== f.options.workDirectory || injected || !await fs.stat(f.managedHome).then(() => true, () => false)) return;
    injected = true;
    await fs.symlink(outside, path.join(f.managedHome, "backups"), process.platform === "win32" ? "junction" : "dir");
  };
  await assert.rejects(executePlan(f.plan, f.options), { code: "linked_path_refused" });
  assert.deepEqual(await fs.readdir(outside), []);
  // Remove only the link, never recurse through it; fixture cleanup then stays confined.
  await fs.unlink(path.join(f.managedHome, "backups"));
  await f.unchanged();
});

test("host helper protects private scratch with an explicit ACL", { skip: process.platform !== "win32" }, async t => {
  const directory = fixtureHome("cutover-acl-", t);
  const script = path.resolve("scripts/eo-cutover-host.ps1");
  const result = await new Promise((resolve, reject) => {
    const child = spawn("pwsh", ["-NoProfile", "-NonInteractive", "-File", script, "-Action", "SecureDirectory", "-Target", directory], { windowsHide: true, stdio: "ignore" });
    child.once("error", reject); child.once("close", resolve);
  });
  assert.equal(result, 0);
});

async function withRehearsalEngine(directory, work, plan, inspect) {
  const engine = path.join(directory, "eo-cutover.mjs");
  await fs.writeFile(engine, "process.on('message', () => process.disconnect()); process.send('ready');\n");
  const child = spawn(process.execPath, [engine, "--plan", plan, "--work-directory", work],
    { windowsHide: true, stdio: ["ignore", "ignore", "ignore", "ipc"] });
  const ended = new Promise((resolve, reject) => { child.once("error", reject); child.once("close", resolve); });
  try {
    await Promise.race([new Promise(resolve => child.once("message", resolve)), ended.then(() => {
      throw new Error("rehearsal fixture engine exited before reporting ready");
    })]);
    return await inspect();
  } finally {
    if (child.connected) child.send("finish");
    assert.equal(await ended, 0);
  }
}

test("rehearsal census recognizes an orphaned engine only in its own target", { skip: process.platform !== "win32" }, async t => {
  const directory = fixtureHome("cutover-orphan-", t);
  const target = path.join(directory, "rehearsal work");
  const sibling = `${target}-other`;
  await withRehearsalEngine(directory, sibling, path.join(target, "sibling plan.json"), async () => {
    const census = selected => new Promise((resolve, reject) => {
      const check = spawn("pwsh", ["-NoProfile", "-NonInteractive", "-File", path.resolve("scripts/eo-cutover-host.ps1"),
        "-Action", "RehearsalConsumers", "-Target", selected, "-EnginePid", String(process.pid)], { windowsHide: true, stdio: ["ignore", "pipe", "ignore"] });
      let text = ""; check.stdout.setEncoding("utf8").on("data", value => { text += value; });
      check.once("error", reject); check.once("close", code => resolve({ code, count: Number(text.trim()) }));
    });
    assert.deepEqual(await census(target), { code: 0, count: 0 });
    assert.deepEqual(await census(sibling), { code: 0, count: 1 });
  });
});

test("Windows census uses readable rehearsal scope and keeps live checks conservative", { skip: process.platform !== "win32" }, async t => {
  const root = fixtureHome("cutover-census-", t);
  const fixtureScript = path.join(root, "census.ps1");
  const hostScript = path.resolve("scripts/eo-cutover-host.ps1");
  const target = path.join(root, "rehearsal home");
  await fs.writeFile(fixtureScript, `
param([string]$HostScript, [string]$Target, [string]$ProcessCase, [string]$Action)
function Get-CimInstance {
    switch ($ProcessCase) {
        'equals' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe --home="' + $Target + '" work next' }
            [pscustomobject]@{ ProcessId = 43; Name = 'engram.exe'; CommandLine = 'engram.exe "--home=' + $Target + '-sibling" work next' }
        }
        'unreadable-node' {
            [pscustomobject]@{ ProcessId = 42; Name = 'node.exe'; ExecutablePath = 'C:\\node\\node.exe'; CommandLine = $null }
        }
        'unreadable-engram' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; ExecutablePath = $null; CommandLine = $null }
        }
        'home-from-env' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe mcp' }
        }
        'relative-home' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe --home local mcp' }
        }
        'incomplete-quote' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe --home "' + $Target }
        }
        'nonbreaking-space' {
            $parent = Split-Path -Parent $Target
            $leaf = Split-Path -Leaf $Target
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe --home="' + $parent + '"\\' + $leaf + ' work next' }
        }
        'unrelated' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe --home "' + $Target + '-sibling" mcp' }
            [pscustomobject]@{ ProcessId = 43; Name = 'node.exe'; CommandLine = 'node.exe unrelated.mjs "' + $Target + '"' }
        }
        'journal' {
            [pscustomobject]@{ ProcessId = 42; Name = 'node.exe'; CommandLine = 'node.exe .\\eo-cutover.mjs --rollback "' + (Join-Path $Target 'journal.json') + '"' }
        }
        'migration' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; CommandLine = 'engram.exe migration --warn-slow export --database "' + (Join-Path $Target 'engram.db') + '" --out elsewhere.json' }
        }
        'local-executable' {
            [pscustomobject]@{ ProcessId = 42; Name = 'engram.exe'; ExecutablePath = (Join-Path $Target 'engram.exe'); CommandLine = 'engram.exe mcp' }
        }
        'census-unavailable' {
            throw 'private fixture details'
        }
    }
}
& $HostScript -Action $Action -Target $Target
exit $LASTEXITCODE
`);
  const census = (processCase, action = "RehearsalConsumers", selected = target) => new Promise((resolve, reject) => {
    const child = spawn("pwsh", ["-NoProfile", "-NonInteractive", "-File", fixtureScript, hostScript, selected, processCase, action],
      { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    child.stdout.setEncoding("utf8").on("data", value => { stdout += value; });
    child.stderr.setEncoding("utf8").on("data", value => { stderr += value; });
    child.once("error", reject);
    child.once("close", code => resolve({ code, stdout: stdout.trim(), stderr: stderr.trim() }));
  });
  const count = stdout => ({ code: 0, stdout, stderr: "" });
  const refused = { code: 1, stdout: "", stderr: '{"code":"host_inspection_failed"}' };
  for (const processCase of ["equals", "journal", "migration", "incomplete-quote"]) {
    assert.deepEqual(await census(processCase), count("1"), processCase);
  }
  for (const processCase of ["unreadable-node", "unreadable-engram", "home-from-env", "relative-home", "local-executable", "unrelated"]) {
    assert.deepEqual(await census(processCase), count("0"), processCase);
  }
  assert.deepEqual(await census("nonbreaking-space", "RehearsalConsumers", path.join(root, "case\u00a0copy")), count("1"));
  assert.deepEqual(await census("census-unavailable"), { ...refused, stdout: "census_unavailable" });
  assert.deepEqual(await census("unrelated", "RehearsalConsumers", "relative"), { ...refused, stdout: "scope_invalid" });
  assert.deepEqual(await census("unrelated", "Consumers"), count("1"));
  assert.deepEqual(await census("journal", "Consumers"), count("1"));
  assert.deepEqual(await census("unreadable-node", "Consumers"), refused);
});

test("public PowerShell entry invokes the engine and keeps malformed plan content private", { skip: process.platform !== "win32" }, async t => {
  const root = fixtureHome("cutover-entry-", t);
  const plan = path.join(root, "plan.json");
  const work = path.join(root, "work");
  await fs.writeFile(plan, JSON.stringify({ format: "invalid", home: path.join(root, "home"), private: "sentinel-secret" }));
  const result = await new Promise((resolve, reject) => {
    const entry = spawn("pwsh", ["-NoProfile", "-NonInteractive", "-File", path.resolve("scripts/eo-cutover.ps1"),
      "-Plan", plan, "-WorkDirectory", work], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let text = "";
    entry.stdout.setEncoding("utf8").on("data", value => { text += value; }); entry.stderr.setEncoding("utf8").on("data", value => { text += value; });
    entry.once("error", reject); entry.once("close", code => resolve({ code, text }));
  });
  assert.equal(result.code, 1);
  assert.equal(result.text.includes("sentinel-secret"), false);
  assert.deepEqual(JSON.parse(result.text.trim()), { code: "plan_invalid", stage: "preflight" });
  await assert.rejects(fs.stat(work), { code: "ENOENT" });
});

test("public PowerShell rollback restores complete packets while a sibling rehearsal is active", { skip: process.platform !== "win32" }, async t => {
  const f = await fixture(t);
  const result = await executePlan(f.plan, f.options);
  const journal = JSON.parse(await fs.readFile(result.journal, "utf8"));
  assert.ok(Object.hasOwn(journal.projects[0].old, ""));
  const sibling = `${f.options.workDirectory}-sibling`;
  const siblingPlan = path.join(f.options.workDirectory, "sibling plan.json");
  const rollback = await withRehearsalEngine(f.root, sibling, siblingPlan, () => new Promise((resolve, reject) => {
    const entry = spawn("pwsh", ["-NoProfile", "-NonInteractive", "-File", path.resolve("scripts/eo-cutover.ps1"),
      "-RollbackJournal", result.journal], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let text = "";
    entry.stdout.setEncoding("utf8").on("data", value => { text += value; }); entry.stderr.setEncoding("utf8").on("data", value => { text += value; });
    entry.once("error", reject); entry.once("close", code => resolve({ code, text }));
  }));
  const diagnostics = [];
  const safeToken = value => typeof value === "string" && /^[A-Za-z0-9_]{1,64}$/u.test(value);
  for (const line of rollback.text.slice(-2048).split(/\r?\n/u)) {
    let event;
    try { event = JSON.parse(line); } catch { continue; }
    if (safeToken(event?.code) && safeToken(event?.stage)) {
      const reason = safeToken(event.reason) ? ": " + event.reason : "";
      diagnostics.push(`${event.code} (${event.stage})${reason}`);
      if (diagnostics.length === 3) break;
    }
  }
  assert.equal(rollback.code, 0, diagnostics.join(", ") || "rollback emitted no structured failure code and stage");
  assert.deepEqual(JSON.parse(rollback.text.trim()), { stage: "rollback", state: "rolled_back" });
  await f.unchanged();
});
