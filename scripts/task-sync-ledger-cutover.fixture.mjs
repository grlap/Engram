// Explicit two-build fixture, invoked separately from the ordinary gate.
// Both binaries are actual artifacts; no dropped-table simulation stands in
// for old-build admission. All stores belong to test-temp under target/.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, existsSync, mkdirSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { fixtureHome, tempSnapshot, assertTempClean } from "./test-temp.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

test("old and new binaries refuse across explicit ledger projection repair without writing", t => {
  for (const key of ["ENGRAM_TASK_SYNC_OLD_BINARY", "ENGRAM_TASK_SYNC_NEW_BINARY", "ENGRAM_TASK_SYNC_OLD_REVISION"]) {
    assert.ok(process.env[key], `Required actual artifact input: ${key}`);
  }
  const oldBinary = resolve(process.env.ENGRAM_TASK_SYNC_OLD_BINARY);
  const newBinary = resolve(process.env.ENGRAM_TASK_SYNC_NEW_BINARY);
  assert.notEqual(oldBinary, newBinary);
  const root = fixtureHome("ledger-cutover-", t);
  const home = join(root, "home");
  const producerHome = join(root, "producer-home");
  const dataWorktree = join(root, ".engram-data");
  const candidate = join(dataWorktree, ".engram");
  const projectFile = join(root, ".engram-project");
  writeFileSync(projectFile, "ledger-cutover-fixture\n");
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.startsWith("ENGRAM_")) delete env[key];
  // Git configuration and identity belong only to this disposable fixture.
  for (const key of Object.keys(env)) if (key.startsWith("GIT_")) delete env[key];
  Object.assign(env, {
    GIT_CEILING_DIRECTORIES: root,
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: join(root, "no-global-config"),
    GIT_AUTHOR_NAME: "Cutover fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
    GIT_COMMITTER_NAME: "Cutover fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
  });
  const hooks = join(root, "hooks");
  const template = join(root, "git-template");
  mkdirSync(hooks);
  mkdirSync(template);
  function git(cwd, args) {
    const result = spawnSync("git", ["-c", `core.hooksPath=${hooks}`, "-c", "commit.gpgSign=false", ...args], {
      cwd, env, encoding: "utf8", timeout: 30_000, maxBuffer: 4 * 1024 * 1024,
    });
    assert.ifError(result.error);
    assert.equal(result.status, 0, `git ${args.join(" ")}: ${result.stdout}\n${result.stderr}`);
    return result.stdout.trim();
  }
  git(root, ["init", "--initial-branch=code", `--template=${template}`]);
  writeFileSync(join(root, ".gitignore"), ".engram-data/\nhome/\nproducer-home/\nhooks/\ngit-template/\n");
  git(root, ["add", ".engram-project", ".gitignore"]);
  git(root, ["commit", "-m", "Create disposable cutover code checkout"]);
  const codeHead = git(root, ["rev-parse", "HEAD"]);
  const codeIndex = git(root, ["write-tree"]);
  git(root, ["worktree", "add", "--detach", dataWorktree, codeHead]);
  git(dataWorktree, ["switch", "--orphan", "engram-data"]);
  function runAt(selectedHome, binary, args, status = 0) {
    const result = spawnSync(binary, ["--home", selectedHome, "--project-file", projectFile, ...args], {
      cwd: root, env, encoding: "utf8", timeout: 30_000, maxBuffer: 4 * 1024 * 1024,
    });
    assert.ifError(result.error);
    assert.equal(result.status, status, `${args.join(" ")}: ${result.stdout}\n${result.stderr}`);
    return result.stdout;
  }
  const run = (binary, args, status = 0) => runAt(home, binary, args, status);
  const sync = ["task-sync", "--actor-id", "fixture", "--session-id", "fixture"];
  for (const selectedHome of [home, producerHome]) {
    runAt(selectedHome, oldBinary, ["init"]);
    runAt(selectedHome, oldBinary, [...sync, "enable", "--user-instruction", "Enable task sync in this owned cutover fixture", "--json"]);
  }
  runAt(producerHome, oldBinary, ["work", "--actor-id", "fixture", "--session-id", "fixture", "add", "Retain original canonical planning"]);
  runAt(producerHome, oldBinary, [...sync, "export", candidate, "--json"]);
  function folderBytes(directory = candidate) {
    return readdirSync(directory, { withFileTypes: true })
      .sort((left, right) => left.name.localeCompare(right.name))
      .map(entry => [entry.name, entry.isDirectory()
        ? folderBytes(join(directory, entry.name)) : readFileSync(join(directory, entry.name))]);
  }
  const originalExport = folderBytes();
  const importArgs = [...sync, "import", candidate, "--empty-basis", "--json"];
  const originalReceipt = JSON.parse(run(oldBinary, importArgs));
  assert.equal(originalReceipt.changed, true);
  assert.equal(originalReceipt.dry_run, false);
  assert.deepEqual(JSON.parse(run(oldBinary, importArgs)), originalReceipt);
  const originalReady = JSON.parse(run(oldBinary, ["readiness", "--json"]));
  assert.equal(originalReady.ready, true);
  assert.equal(originalReady.build.source_revision, process.env.ENGRAM_TASK_SYNC_OLD_REVISION);
  const database = originalReady.database;
  function objects() {
    const connection = new DatabaseSync(database, { readOnly: true });
    try {
      return connection.prepare("SELECT object_id,object_kind,hex(canonical_json) AS bytes,created_at FROM objects ORDER BY object_id").all();
    } finally { connection.close(); }
  }
  function operationResults() {
    const connection = new DatabaseSync(database, { readOnly: true });
    try {
      return connection.prepare("SELECT operation,idempotency_key,request_hash,hex(result_json) AS bytes FROM work_operation_results ORDER BY operation,idempotency_key").all();
    } finally { connection.close(); }
  }
  const original = objects();
  assert.ok(original.length > 0);
  assert.ok(original.some(record => record.object_kind === "task_sync_import_planning"));
  const originalResults = operationResults();
  assert.ok(originalResults.length > 0);
  const storeBytes = () => ["", "-wal", "-shm", "-journal"].map(suffix => {
    const file = database + suffix;
    return [suffix, existsSync(file) ? readFileSync(file) : null];
  });
  let bytes = storeBytes();
  const refused = JSON.parse(run(newBinary, ["readiness", "--json"], 1));
  assert.equal(refused.code, "projection_repair_required");
  assert.equal(refused.mutation_enabled, false);
  assert.deepEqual(storeBytes(), bytes);
  run(newBinary, ["work", "--actor-id", "fixture", "--session-id", "fixture", "next", "--peek"], 1);
  assert.deepEqual(storeBytes(), bytes);
  const repaired = JSON.parse(run(newBinary, ["doctor", "--repair-projections", "--json"]));
  assert.equal(repaired.healthy, true);
  assert.deepEqual(objects(), original);
  assert.deepEqual(operationResults(), originalResults);
  const ready = JSON.parse(run(newBinary, ["readiness", "--json"]));
  assert.equal(ready.ready, true);
  assert.notEqual(ready.build.source_revision, originalReady.build.source_revision);
  assert.notEqual(ready.build.schema_reference, originalReady.build.schema_reference);
  assert.deepEqual(JSON.parse(run(newBinary, importArgs)), originalReceipt);
  assert.deepEqual(objects(), original);
  assert.deepEqual(operationResults(), originalResults);
  bytes = storeBytes();
  const oldRefusal = JSON.parse(run(oldBinary, ["readiness", "--json"], 1));
  assert.equal(oldRefusal.code, "different_build_schema");
  assert.equal(oldRefusal.mutation_enabled, false);
  assert.deepEqual(storeBytes(), bytes);
  run(oldBinary, ["work", "--actor-id", "fixture", "--session-id", "fixture", "next", "--peek"], 1);
  assert.deepEqual(storeBytes(), bytes);
  assert.equal(JSON.parse(run(newBinary, ["doctor", "--repair-projections", "--json"])).healthy, true);
  assert.deepEqual(objects(), original);
  assert.deepEqual(operationResults(), originalResults);
  assert.deepEqual(folderBytes(), originalExport);
  assert.equal(git(root, ["rev-parse", "HEAD"]), codeHead);
  assert.equal(git(root, ["write-tree"]), codeIndex);
  assert.equal(git(dataWorktree, ["symbolic-ref", "HEAD"]), "refs/heads/engram-data");
  console.log(JSON.stringify({ old: originalReady.build, current: ready.build,
    canonical_records_preserved: original.length, operation_results_preserved: originalResults.length,
    receipt_replayed: true, cutover: "explicit projection repair" }));
});
