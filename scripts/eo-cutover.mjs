// Offline store preparation and recoverable activation. This never restarts consumers.
import * as fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { CutoverError, fingerprint, inventory, verifyPreservation } from "./eo-cutover-values.mjs";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const helper = path.join(repository, "scripts", "eo-cutover-host.ps1");
const suffixes = ["", "-wal", "-shm", "-journal"];
const ioCodes = new Set(["ENOENT", "EEXIST", "EBUSY", "EACCES", "EPERM", "ENOSPC", "EXDEV", "EIO"]);

function fail(code, stage, project, detail) { throw new CutoverError(code, stage, project, detail); }
function samePath(a, b) { return process.platform === "win32" ? a.toLowerCase() === b.toLowerCase() : a === b; }
function within(root, value) {
  const relative = path.relative(root, value);
  return relative === "" || (!path.isAbsolute(relative) && relative !== ".." && !relative.startsWith(`..${path.sep}`));
}
async function exists(file) { try { await fs.lstat(file); return true; } catch (error) { if (error.code === "ENOENT") return false; throw error; } }
function safePath(value) {
  if (typeof value !== "string" || !path.isAbsolute(value) || /[\u0000-\u001f\u007f\u2028\u2029\u202a-\u202e\u2066-\u2069]/u.test(value)
      || path.resolve(value).split(/[\\/]/u).some(part => part.toLowerCase() === ".termal")) fail("path_refused", "preflight");
  return path.resolve(value);
}
async function noLinks(file) {
  let current = safePath(file);
  for (;;) {
    try { if ((await fs.lstat(current)).isSymbolicLink()) fail("linked_path_refused", "preflight"); }
    catch (error) { if (error.code !== "ENOENT") throw error; }
    const parent = path.dirname(current);
    if (parent === current) break;
    current = parent;
  }
}
function safeError(error) {
  if (error instanceof CutoverError) return { code: error.code, stage: error.stage, ...(error.project ? { project: error.project } : {}), ...error.detail };
  return { code: ioCodes.has(error?.code) ? error.code : "unexpected_failure", stage: "filesystem" };
}
function psQuote(value) { return `'${value.replaceAll("'", "''")}'`; }
function rollbackCommand(journal) {
  return `pwsh -NoProfile -File ${psQuote(path.join(repository, "scripts", "eo-cutover.ps1"))} -RollbackJournal ${psQuote(journal)}`;
}

async function child(command, args) {
  return new Promise((resolve, reject) => {
    const process = spawn(command, args, { windowsHide: true, stdio: ["ignore", "pipe", "ignore"] });
    const chunks = [];
    let size = 0;
    let overflow = false;
    process.stdout.on("data", chunk => { size += chunk.length; if (size > 2 * 1024 * 1024) overflow = true; else chunks.push(chunk); });
    process.once("error", () => reject(new CutoverError("command_start_failed", "command")));
    process.once("close", code => {
      if (overflow) reject(new CutoverError("command_output_limit", "command"));
      else resolve({ code, stdout: Buffer.concat(chunks).toString("utf8") });
    });
  });
}
async function host(action, target) {
  if (process.platform !== "win32") fail("windows_required", "preflight");
  const result = await child("pwsh", ["-NoProfile", "-NonInteractive", "-File", helper, "-Action", action, "-Target", target, "-EnginePid", String(process.pid)]);
  if (result.code !== 0) fail("host_check_failed", action);
  return result.stdout.trim();
}
async function secureDirectory(directory) { await fs.mkdir(directory, { recursive: true, mode: 0o700 }); await host("SecureDirectory", directory); }
async function guard(scope, home) {
  if (await host(scope === "live" ? "Consumers" : "RehearsalConsumers", home) !== "0") fail("engram_consumer_running", "quiescence");
}
async function cli(binary, args, stage, project, invoke) {
  const result = await invoke(binary, args);
  if (result.code !== 0) fail("engram_command_failed", stage, project, { exit_code: result.code });
  try { return JSON.parse(result.stdout); } catch { fail("engram_response_invalid", stage, project); }
}
function identity(expected, actual) {
  return actual?.build_fingerprint === expected.build_fingerprint
    && actual.build?.executable_sha256 === expected.sha256
    && actual.build?.source_revision === expected.source_revision
    && actual.build?.schema_reference === expected.schema_reference;
}
function selectedStore(actual, project, database) {
  return actual?.project_id === project.id && typeof actual.database === "string" && samePath(path.resolve(actual.database), database);
}
function validateArtifact(artifact) {
  if (!artifact || artifact.profile !== "release" || !/^[a-f0-9]{40}$/u.test(artifact.source_revision)
      || ![artifact.sha256, artifact.build_fingerprint, artifact.schema_reference].every(value => /^[a-f0-9]{64}$/u.test(value))) fail("artifact_identity_required", "preflight");
  artifact.binary = safePath(artifact.binary);
}
function validatePlan(input) {
  const plan = structuredClone(input);
  if (!plan || plan.format !== "engram-cutover-plan-1" || !Array.isArray(plan.projects) || plan.projects.length === 0) fail("plan_invalid", "preflight");
  validateArtifact(plan.old);
  validateArtifact(plan.final);
  plan.home = safePath(plan.home);
  plan.install_path = safePath(plan.install_path);
  if (!/^[a-f0-9]{64}$/u.test(plan.installed_sha256)) fail("installed_identity_required", "preflight");
  const names = new Set();
  const ids = new Set();
  for (const project of plan.projects) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/u.test(project.name) || typeof project.id !== "string" || !project.id.trim()
        || project.id !== project.id.trim() || /[\u0000-\u001f\u007f]/u.test(project.id) || !["case_fold", "case_sensitive"].includes(project.host_path_policy)
        || names.has(project.name.toLowerCase()) || ids.has(project.id)) fail("project_manifest_invalid", "preflight");
    names.add(project.name.toLowerCase());
    ids.add(project.id);
  }
  return plan;
}
function storePath(home, project) {
  const directory = createHash("sha256").update(project.id).digest("hex");
  return path.join(home, "projects", directory, "engram.db");
}
async function snapshot(database) {
  const result = {};
  for (const suffix of suffixes) {
    const file = database + suffix;
    await noLinks(file);
    if (await exists(file)) {
      const stat = await fs.stat(file);
      if (!stat.isFile()) fail("store_packet_not_file", "preflight");
      result[suffix] = { bytes: stat.size, fingerprint: await fingerprint(file) };
    }
  }
  if (!result[""]) fail("store_missing", "preflight");
  return result;
}
async function sameSnapshot(database, expected) {
  return JSON.stringify(await snapshot(database)) === JSON.stringify(expected);
}
async function copyPacket(from, to, expected) {
  await fs.mkdir(path.dirname(to), { recursive: true, mode: 0o700 });
  for (const suffix of Object.keys(expected)) await fs.copyFile(from + suffix, to + suffix, fs.constants.COPYFILE_EXCL);
  if (!await sameSnapshot(from, expected) || !await sameSnapshot(to, expected)) fail("source_changed_during_capture", "capture");
}
async function census(home, projects) {
  const expected = new Set(projects.map(project => path.basename(path.dirname(storePath(home, project)))));
  const actual = new Set();
  for (const entry of await fs.readdir(path.join(home, "projects"), { withFileTypes: true })) {
    if (entry.isSymbolicLink()) fail("linked_store_directory", "preflight");
    if (entry.isDirectory() && await exists(path.join(home, "projects", entry.name, "engram.db"))) actual.add(entry.name);
  }
  if (actual.size !== expected.size || [...actual].some(id => !expected.has(id))) fail("store_census_differs", "preflight");
}
async function writeJournal(file, journal) {
  const temporary = `${file}.next`;
  const handle = await fs.open(temporary, "w", 0o600);
  try { await handle.writeFile(JSON.stringify(journal)); await handle.sync(); } finally { await handle.close(); }
  await fs.rename(temporary, file);
}
async function move(action, journal, file, hooks) {
  action.status = "intent";
  await writeJournal(file, journal);
  await hooks.beforeMove?.(action);
  await noLinks(action.from); await noLinks(action.to);
  if (action.from.startsWith(path.join(journal.work, "n") + path.sep)) {
    for (const suffix of suffixes) if (await exists(action.to + suffix)) fail("live_packet_reappeared", "activate", action.project);
  }
  if (await exists(action.to) || !await exists(action.from) || await fingerprint(action.from) !== action.fingerprint) fail("rename_input_changed", "activate");
  await fs.mkdir(path.dirname(action.to), { recursive: true, mode: 0o700 });
  await fs.rename(action.from, action.to);
  await hooks.afterMove?.(action);
  action.status = "done";
  await writeJournal(file, journal);
}

async function releaseLock(journal) {
  const lock = path.join(journal.home, ".eo-cutover.lock");
  if (!await exists(lock)) return;
  let owner; try { owner = JSON.parse(await fs.readFile(lock, "utf8")); } catch { fail("maintenance_lock_differs", "recovery"); }
  if (owner.file !== journal.file || owner.id !== journal.id) fail("maintenance_lock_differs", "recovery");
  await fs.unlink(lock);
}
async function validateRecovery(journal) {
  if (journal.format !== "engram-cutover-journal-1" || !["live", "rehearsal"].includes(journal.scope)) fail("journal_invalid", "recovery");
  for (const root of [journal.home, journal.install, journal.backup, journal.work, journal.file]) await noLinks(root);
  if (!within(path.join(repository, "target"), journal.work) || samePath(path.join(repository, "target"), journal.work)
      || path.dirname(journal.backup) !== path.join(journal.home, "backups") || !/^eo-[a-f0-9-]+$/u.test(path.basename(journal.backup))
      || journal.file !== path.join(journal.backup, "journal.json")) fail("journal_path_invalid", "recovery");
  if (journal.scope === "live") {
    if (process.platform !== "win32" || !samePath(journal.home, path.join(os.homedir(), ".engram"))
      || !samePath(journal.install, path.join(os.homedir(), ".cargo", "bin", "engram.exe"))) fail("live_location_refused", "recovery");
  } else if (journal.home !== path.join(journal.work, "rehearsal", "home")
    || journal.install !== path.join(journal.work, "rehearsal", "bin", "engram.exe")) fail("journal_path_invalid", "recovery");
  if (!Array.isArray(journal.projects) || !journal.projects.length || !Array.isArray(journal.actions)
    || !["activating", "rolling_back", "rollback_failed", "rolled_back", "complete"].includes(journal.state)) fail("journal_invalid", "recovery");
  const expected = [];
  const names = new Set();
  const hash = value => /^[a-f0-9]{64}$/u.test(value);
  for (const project of journal.projects) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/u.test(project.name) || names.has(project.name.toLowerCase())
      || path.dirname(path.dirname(project.database)) !== path.join(journal.home, "projects")
      || !/^[a-f0-9]{64}$/u.test(path.basename(path.dirname(project.database))) || path.basename(project.database) !== "engram.db"
      || !hash(project.preparedFingerprint) || !project.old?.[""] || Object.keys(project.old).some(key => !suffixes.includes(key))) fail("journal_project_invalid", "recovery");
    names.add(project.name.toLowerCase());
    for (const [suffix, info] of Object.entries(project.old)) {
      if (!hash(info.fingerprint) || !Number.isSafeInteger(info.bytes) || info.bytes < 0) fail("journal_project_invalid", "recovery");
      expected.push([project.database + suffix, path.join(journal.backup, "stores", project.name, `engram.db${suffix}`), info.fingerprint]);
    }
    const directory = path.basename(path.dirname(project.database));
    expected.push([path.join(journal.work, "n", "projects", directory, "engram.db"), project.database, project.preparedFingerprint]);
  }
  expected.push([journal.install, path.join(journal.backup, "binary", "engram.exe"), journal.installedFingerprint]);
  expected.push([path.join(journal.work, "final", "engram.exe"), journal.install, journal.finalFingerprint]);
  if (expected.length !== journal.actions.length) fail("journal_action_invalid", "recovery");
  for (const [index, action] of journal.actions.entries()) {
    const project = journal.projects.find(item => action.from === item.database || action.to === item.database
      || suffixes.slice(1).some(suffix => action.from === item.database + suffix));
    if (JSON.stringify([action.from, action.to, action.fingerprint]) !== JSON.stringify(expected[index])
      || action.project !== project?.name || !hash(action.fingerprint)
      || !["planned", "intent", "done", "reversed"].includes(action.status)) fail("journal_action_invalid", "recovery");
    await noLinks(action.from); await noLinks(action.to);
  }
  for (const action of journal.coordination ?? []) {
    const project = journal.projects.find(item => suffixes.slice(1).some(suffix => action.from === item.database + suffix
      && action.to === path.join(journal.backup, "new-coordination", item.name, `engram.db${suffix}`)));
    if (!project || !hash(action.fingerprint) || !["intent", "done"].includes(action.status)) fail("journal_action_invalid", "recovery");
    await noLinks(action.from); await noLinks(action.to);
  }
}

export async function recover(journalFile, options = {}) {
  const emit = options.emit ?? (value => console.log(JSON.stringify(value)));
  let journal;
  try { journal = JSON.parse(await fs.readFile(safePath(journalFile), "utf8")); } catch { fail("journal_unreadable", "recovery"); }
  if (journal?.file !== path.resolve(journalFile)) fail("journal_path_invalid", "recovery");
  await validateRecovery(journal);
  await (options.guard ?? guard)(journal.scope, journal.scope === "rehearsal" ? journal.work : journal.home);
  if (journal.state === "rolled_back") { await releaseLock(journal); return { state: "rolled_back" }; }
  if (journal.state === "complete" && !options.rollback) { await releaseLock(journal); return { state: "complete" }; }
  // Check every current managed main file before changing any of them.
  for (const project of journal.projects) {
    if (await exists(project.database)) {
      const hash = await fingerprint(project.database);
      if (![project.old[""].fingerprint, project.preparedFingerprint].includes(hash)) fail("managed_content_changed", "recovery", project.name);
      if (hash === project.preparedFingerprint) {
        for (const suffix of ["-wal", "-journal"]) if (await exists(project.database + suffix) && (await fs.stat(project.database + suffix)).size !== 0) fail("post_cutover_writes_observed", "recovery", project.name);
      }
    }
  }
  if (await exists(journal.install) && ![journal.installedFingerprint, journal.finalFingerprint].includes(await fingerprint(journal.install))) fail("installed_content_changed", "recovery");
  journal.coordination ??= [];
  const activeCoordination = new Set();
  for (const project of journal.projects) {
    const newMove = journal.actions.find(action => action.to === project.database);
    // The new main has been activated only if its staging path is absent.
    if (!["intent", "done"].includes(newMove.status) || await exists(newMove.from) || !await exists(project.database)) continue;
    for (const suffix of suffixes.slice(1)) activeCoordination.add(project.database + suffix);
    for (const suffix of suffixes.slice(1)) {
      const from = project.database + suffix;
      if (!await exists(from) || journal.coordination.some(action => action.from === from)) continue;
      journal.coordination.push({ from, to: path.join(journal.backup, "new-coordination", project.name, `engram.db${suffix}`),
        fingerprint: await fingerprint(from), status: "intent" });
    }
  }
  // Simulate every rollback move before performing the first one. This also
  // handles a crash after rename but before the completion journal write.
  const virtual = new Map();
  async function current(file) {
    if (!virtual.has(file)) virtual.set(file, await exists(file) ? await fingerprint(file) : null);
    return virtual.get(file);
  }
  for (const action of journal.coordination) {
    const from = await current(action.from); const to = await current(action.to);
    if (!activeCoordination.has(action.from)) {
      if (to !== action.fingerprint) fail("recovery_content_differs", "recovery");
      continue;
    }
    if (from === action.fingerprint && to === null) { virtual.set(action.from, null); virtual.set(action.to, action.fingerprint); }
    else if (from !== null || to !== action.fingerprint) fail("recovery_content_differs", "recovery");
  }
  for (const action of [...journal.actions].reverse()) {
    if (["planned", "reversed"].includes(action.status)) continue;
    const from = await current(action.from); const to = await current(action.to);
    if (from === action.fingerprint && to === null) continue;
    if (from !== null || to !== action.fingerprint) fail("recovery_content_differs", "recovery");
    virtual.set(action.to, null); virtual.set(action.from, action.fingerprint);
  }
  for (const project of journal.projects) for (const suffix of suffixes) {
    if (await current(project.database + suffix) !== (project.old[suffix]?.fingerprint ?? null)) fail("recovery_packet_differs", "recovery", project.name);
  }
  if (await current(journal.install) !== journal.installedFingerprint) fail("recovery_binary_differs", "recovery");
  journal.state = "rolling_back";
  await writeJournal(journal.file, journal);
  try {
    for (const action of journal.coordination) {
      if (!activeCoordination.has(action.from)) continue;
      if (await exists(action.from)) {
        if (await exists(action.to) || await fingerprint(action.from) !== action.fingerprint) fail("recovery_content_differs", "recovery");
        await fs.mkdir(path.dirname(action.to), { recursive: true, mode: 0o700 });
        await fs.rename(action.from, action.to);
      }
      action.status = "done"; await writeJournal(journal.file, journal);
    }
    for (const action of [...journal.actions].reverse()) {
      if (action.status === "planned" || action.status === "reversed") continue;
      const from = await exists(action.from);
      const to = await exists(action.to);
      if (from && !to && await fingerprint(action.from) === action.fingerprint) {
        action.status = "reversed";
        await writeJournal(journal.file, journal);
        continue;
      }
      if (from || !to || await fingerprint(action.to) !== action.fingerprint) fail("recovery_content_differs", "recovery");
      await fs.mkdir(path.dirname(action.from), { recursive: true, mode: 0o700 });
      await fs.rename(action.to, action.from);
      await options.afterReverse?.(action);
      action.status = "reversed";
      await writeJournal(journal.file, journal);
    }
    journal.state = "rolled_back";
    await writeJournal(journal.file, journal);
    await releaseLock(journal);
    emit({ stage: "rollback", state: "rolled_back" });
    return { state: "rolled_back" };
  } catch (error) {
    journal.state = "rollback_failed";
    await writeJournal(journal.file, journal);
    emit({ ...safeError(error), state: "rollback_failed" });
    throw error;
  }
}

export async function executePlan(input, options) {
  const started = performance.now();
  const plan = validatePlan(input);
  const scope = options.apply ? "live" : "rehearsal";
  const emit = options.emit ?? (value => console.log(JSON.stringify(value)));
  const invoke = options.invoke ?? child;
  const secure = options.secureDirectory ?? secureDirectory;
  const checkConsumers = options.guard ?? guard;
  const work = safePath(options.workDirectory);
  if (!within(path.join(repository, "target"), work) || samePath(path.join(repository, "target"), work)) fail("work_directory_refused", "preflight");
  if (scope === "live") {
    if (process.platform !== "win32" || !samePath(plan.home, path.join(os.homedir(), ".engram")) || !samePath(plan.install_path, path.join(os.homedir(), ".cargo", "bin", "engram.exe"))) fail("live_location_refused", "preflight");
  } else if (!within(path.join(repository, "target"), plan.home)) fail("rehearsal_source_refused", "preflight");
  for (const file of [work, plan.home, plan.install_path, plan.old.binary, plan.final.binary]) await noLinks(file);
  if (await exists(work)) fail("work_directory_exists", "preflight");
  await checkConsumers(scope, scope === "live" ? plan.home : work);
  await fs.mkdir(path.dirname(work), { recursive: true, mode: 0o700 });
  await fs.mkdir(work, { mode: 0o700 });
  await secure(work);
  let home = plan.home;
  let install = plan.install_path;
  if (scope === "rehearsal") {
    await census(plan.home, plan.projects);
    home = path.join(work, "rehearsal", "home");
    install = path.join(work, "rehearsal", "bin", "engram.exe");
    for (const project of plan.projects) {
      const from = storePath(plan.home, project);
      await noLinks(from);
      await copyPacket(from, storePath(home, project), await snapshot(from));
    }
    await fs.mkdir(path.dirname(install), { recursive: true, mode: 0o700 });
    await fs.copyFile(plan.old.binary, install, fs.constants.COPYFILE_EXCL);
  }
  await census(home, plan.projects);
  if (await exists(path.join(home, ".eo-cutover.lock"))) fail("maintenance_already_pending", "preflight");
  const installedFingerprint = await fingerprint(install);
  if (installedFingerprint !== (scope === "live" ? plan.installed_sha256 : plan.old.sha256)) fail("installed_identity_differs", "preflight");
  const binaries = {};
  for (const [role, artifact] of [["old", plan.old], ["final", plan.final]]) {
    if (await fingerprint(artifact.binary) !== artifact.sha256) fail("artifact_bytes_differ", "preflight");
    const binary = path.join(work, role, "engram.exe");
    await fs.mkdir(path.dirname(binary), { recursive: true, mode: 0o700 });
    await fs.copyFile(artifact.binary, binary, fs.constants.COPYFILE_EXCL);
    if (await fingerprint(binary) !== artifact.sha256) fail("artifact_copy_differs", "preflight");
    binaries[role] = binary;
  }
  const capturedHome = path.join(work, "o");
  const preparedHome = path.join(work, "n");
  const projects = [];
  for (const project of plan.projects) {
    const database = storePath(home, project);
    await noLinks(database);
    const old = await snapshot(database);
    const captured = storePath(capturedHome, project);
    const prepared = storePath(preparedHome, project);
    if (!options.invoke && process.platform === "win32" && path.dirname(prepared).length + 65 >= 260) fail("shorter_work_directory_required", "preflight");
    await copyPacket(database, captured, old);
    await fs.mkdir(path.dirname(prepared), { recursive: true, mode: 0o700 });
    const marker = path.join(work, "p", project.name, ".engram-project");
    await fs.mkdir(path.dirname(marker), { recursive: true, mode: 0o700 });
    await fs.writeFile(marker, `${project.id}\n`, { flag: "wx", mode: 0o600 });
    projects.push({ ...project, database, captured, prepared, marker, old });
  }
  const context = (project, selectedHome) => ["--home", selectedHome, "--project-file", project.marker, "--host-path-policy", project.host_path_policy];
  const exported = await Promise.allSettled(projects.map(async project => {
    const tick = performance.now();
    const ready = await cli(binaries.old, [...context(project, capturedHome), "readiness", "--json"], "old_readiness", project.name, invoke);
    if (!ready.ready || !identity(plan.old, ready) || !selectedStore(ready, project, project.captured)) fail("old_readiness_identity_differs", "old_readiness", project.name);
    project.export = path.join(work, `${project.name}.old.jsonl`);
    await cli(binaries.old, ["migration", "export", "--database", project.captured, "--out", project.export], "export", project.name, invoke);
    project.before = await inventory(project.export);
    const count = [...project.before.formerPlanning.values()].reduce((sum, value) => sum + value, 0);
    emit({ stage: "old_planning_count", project: project.name, count });
    for (const [id, value] of project.before.formerPlanning) emit({ stage: "old_planning_project_count", project: project.name,
      affected_project: plan.projects.find(item => item.id === id)?.name ?? "unlisted", count: value });
    if (count) fail("old_complete_planning_refused", "preflight", project.name, { count });
    project.export_ms = Math.round(performance.now() - tick);
  }));
  const exportFailure = exported.find(result => result.status === "rejected");
  if (exportFailure) throw exportFailure.reason;
  const imported = await Promise.allSettled(projects.map(async project => {
    const tick = performance.now();
    project.importReport = await cli(binaries.final, ["migration", "import", "--file", project.export, "--out", project.prepared], "import", project.name, invoke);
    for (const field of ["checked_objects", "checked_control_records", "checked_work_records", "checked_pending_deliveries"]) {
      if (!Number.isSafeInteger(project.importReport?.[field]) || project.importReport[field] < 0) fail("import_validation_report_missing", "import", project.name);
    }
    const importedPacket = await snapshot(project.prepared);
    if ((importedPacket["-wal"]?.bytes ?? 0) || (importedPacket["-journal"]?.bytes ?? 0)) fail("prepared_writes_pending", "import", project.name);
    const nextExport = path.join(work, `${project.name}.new.jsonl`);
    await cli(binaries.final, ["migration", "export", "--database", project.prepared, "--out", nextExport], "new_export", project.name, invoke);
    project.comparison = verifyPreservation(project.before, await inventory(nextExport), project.importReport);
    const ready = await cli(binaries.final, [...context(project, preparedHome), "readiness", "--json"], "prepared_readiness", project.name, invoke);
    if (!ready.ready || !identity(plan.final, ready) || !selectedStore(ready, project, project.prepared)) fail("prepared_readiness_identity_differs", "prepared_readiness", project.name);
    const packet = await snapshot(project.prepared);
    if (packet[""].fingerprint !== importedPacket[""].fingerprint) fail("prepared_content_changed", "compare", project.name);
    if ((packet["-wal"]?.bytes ?? 0) || (packet["-journal"]?.bytes ?? 0)) fail("prepared_writes_pending", "prepared_readiness", project.name);
    project.preparedFingerprint = packet[""].fingerprint;
    project.import_ms = Math.round(performance.now() - tick);
    emit({ stage: "prepared", project: project.name, ready: true, ...project.comparison, export_ms: project.export_ms, import_ms: project.import_ms });
  }));
  const importFailure = imported.find(result => result.status === "rejected");
  if (importFailure) throw importFailure.reason;
  await checkConsumers(scope, scope === "rehearsal" ? work : home);
  for (const project of projects) {
    if (!await sameSnapshot(project.database, project.old)) fail("source_changed_after_capture", "activate", project.name);
    if (await fingerprint(project.prepared) !== project.preparedFingerprint) fail("prepared_content_changed", "activate", project.name);
  }
  if (await fingerprint(install) !== installedFingerprint || await fingerprint(binaries.final) !== plan.final.sha256) fail("binary_changed_after_prepare", "activate");
  const backup = path.join(home, "backups", `eo-${randomUUID()}`);
  await noLinks(backup);
  await secure(backup);
  const device = (await fs.stat(home)).dev;
  for (const directory of [work, path.dirname(install), backup]) if ((await fs.stat(directory)).dev !== device) fail("volume_differs", "preflight");
  const journalFile = path.join(backup, "journal.json");
  const journal = { format: "engram-cutover-journal-1", id: randomUUID(), state: "activating", scope, file: journalFile,
    work, home, install, backup, installedFingerprint, finalFingerprint: plan.final.sha256,
    projects: projects.map(({ name, database, old, preparedFingerprint }) => ({ name, database, old, preparedFingerprint })), actions: [] };
  for (const project of projects) {
    for (const [suffix, info] of Object.entries(project.old)) journal.actions.push({ from: project.database + suffix, to: path.join(backup, "stores", project.name, `engram.db${suffix}`), fingerprint: info.fingerprint, project: project.name, status: "planned" });
    journal.actions.push({ from: project.prepared, to: project.database, fingerprint: project.preparedFingerprint, project: project.name, status: "planned" });
  }
  journal.actions.push({ from: install, to: path.join(backup, "binary", "engram.exe"), fingerprint: installedFingerprint, status: "planned" });
  journal.actions.push({ from: binaries.final, to: install, fingerprint: plan.final.sha256, status: "planned" });
  // Publish a readable journal before taking the maintenance lock. No managed
  // file moves until both are durable, so a failed lock leaves stores intact.
  await writeJournal(journalFile, journal);
  emit({ stage: "recovery_command", command: rollbackCommand(journalFile) });
  await fs.link(journalFile, path.join(home, ".eo-cutover.lock"));
  try {
    for (const action of journal.actions) {
      const project = projects.find(item => item.database === action.to);
      if (project) for (const suffix of suffixes) if (await exists(project.database + suffix)) fail("live_packet_reappeared", "activate", project.name);
      await checkConsumers(scope, scope === "rehearsal" ? work : home);
      try { await move(action, journal, journalFile, options); }
      catch (error) {
        if (error instanceof CutoverError) { error.project ??= action.project; throw error; }
        fail(ioCodes.has(error?.code) ? error.code : "rename_failed", "activate", action.project, { role: action.project ? "store" : "binary" });
      }
    }
    for (const project of projects) {
      const ready = await cli(install, [...context(project, home), "readiness", "--json"], "installed_readiness", project.name, invoke);
      if (!ready.ready || !identity(plan.final, ready) || !selectedStore(ready, project, project.database)) fail("installed_readiness_identity_differs", "installed_readiness", project.name);
      const actual = await snapshot(project.database);
      if (actual[""].fingerprint !== project.preparedFingerprint || (actual["-wal"]?.bytes ?? 0) || (actual["-journal"]?.bytes ?? 0)) fail("installed_store_changed", "installed_readiness", project.name);
    }
    await checkConsumers(scope, scope === "rehearsal" ? work : home);
    if (await fingerprint(install) !== plan.final.sha256) fail("installed_binary_changed", "installed_readiness");
    journal.state = "complete";
    await writeJournal(journalFile, journal);
    try { await releaseLock(journal); } catch { emit({ stage: "complete", code: "maintenance_lock_retained" }); }
    const result = { stage: "complete", scope, ready: projects.length, total: projects.length, wall_ms: Math.round(performance.now() - started),
      old_build: plan.old.build_fingerprint, final_build: plan.final.build_fingerprint, final_source: plan.final.source_revision, final_schema: plan.final.schema_reference };
    emit(result);
    emit({ stage: "rollback_command", command: rollbackCommand(journalFile) });
    return { ...result, journal: journalFile, home, install };
  } catch (error) {
    emit(safeError(error));
    try { await recover(journalFile, { guard: checkConsumers, emit, rollback: true }); }
    catch (recoveryError) { emit({ ...safeError(recoveryError), state: "manual_recovery_required" }); }
    emit({ stage: "rollback_command", command: rollbackCommand(journalFile) });
    throw error;
  }
}

async function main(args) {
  if (process.env.ENGRAM_CUTOVER_WRAPPER !== "1") fail("use_powershell_entry", "preflight");
  const flags = new Map();
  for (let i = 0; i < args.length; i++) {
    if (flags.has(args[i])) fail("arguments_invalid", "preflight");
    if (args[i] === "--apply") flags.set(args[i], true);
    else if (["--plan", "--work-directory", "--recover", "--rollback"].includes(args[i]) && args[i + 1]) flags.set(args[i], args[++i]);
    else fail("arguments_invalid", "preflight");
  }
  if (flags.has("--recover") || flags.has("--rollback")) {
    if (flags.size !== 1) fail("arguments_invalid", "preflight");
    return recover(flags.get("--recover") ?? flags.get("--rollback"), { rollback: flags.has("--rollback") });
  }
  if (!flags.has("--plan") || !flags.has("--work-directory")) fail("arguments_invalid", "preflight");
  let plan;
  try { plan = JSON.parse(await fs.readFile(safePath(flags.get("--plan")), "utf8")); } catch { fail("plan_unreadable", "preflight"); }
  return executePlan(plan, { apply: flags.has("--apply"), workDirectory: flags.get("--work-directory") });
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch(error => { console.error(JSON.stringify(safeError(error))); process.exitCode = 1; });
}
