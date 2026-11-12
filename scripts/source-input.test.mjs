import assert from "node:assert/strict";
import { existsSync, mkdirSync, realpathSync, writeFileSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test, { after } from "node:test";
import { fixtureHome, removeFixtureHomes, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { createSourceFixture, sourceCases, sourceRefusalCodes,
  createMachineEolFixture, machineEolCases } from "./source-input-fixtures.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

const repository = resolve(import.meta.dirname, "..");
const binary = join(resolve(repository, process.env.CARGO_TARGET_DIR || "target"), "debug",
  process.platform === "win32" ? "engram.exe" : "engram");
const normalized = (path) => realpathSync.native(path).replace(/^\\\\\?\\/u, "");

function run(args, env = {}) {
  const sandbox = fixtureHome("source-env-");
  const scratch = join(sandbox, "tmp");
  mkdirSync(scratch);
  const environment = { ...process.env };
  delete environment.ENGRAM_HOME;
  delete environment.ENGRAM_PROJECT_FILE;
  Object.assign(environment, { HOME: sandbox, USERPROFILE: sandbox, XDG_CONFIG_HOME: sandbox,
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : "/dev/null",
    TMP: scratch, TEMP: scratch, TMPDIR: scratch }, env);
  try {
    const result = spawnSync(binary, ["source", ...args], {
      cwd: repository, env: environment, encoding: "utf8", timeout: 15_000, maxBuffer: 1024 * 1024 + 4096,
    });
    assert.ifError(result.error);
    return result;
  } finally {
    removeFixtureHomes(sandbox);
  }
}

for (const scenario of sourceCases) {
  test(`source owner conformance: ${scenario.id}`, (t) => {
    if (scenario.skipOn?.includes(process.platform)) return t.skip("fixture requires Unix filename or mode semantics");
    const fixture = createSourceFixture(fixtureHome("s-", t), scenario.id);
    const result = run(["--root", fixture.root, "--json"]);
    assert.equal(result.status, scenario.status === "unavailable" ? 1 : 0, result.stderr || result.stdout);
    const observation = JSON.parse(result.stdout);
    assert.deepEqual(Object.keys(observation).sort(), ["entries", "refusal", "root", "source", "source_id", "status", "tree", "version"]);
    assert.equal(observation.version, 1);
    assert.equal(observation.status, scenario.status, result.stdout);
    if (scenario.id === "root-autocrlf-clean") assert.equal(result.stderr, "");
    assert.equal(observation.root.startsWith("\\\\?\\"), false, "wire root must use the plain form");
    assert.equal(normalized(observation.root), normalized(fixture.root));
    if (observation.status === "unavailable") {
      assert.equal(observation.source, null);
      assert.equal(observation.tree, null);
      assert.deepEqual(observation.entries, []);
      assert.equal(observation.source_id, null);
      assert.ok(sourceRefusalCodes.includes(observation.refusal.code));
      assert.equal(observation.refusal.code, "input_unavailable");
      assert.ok(observation.refusal.message);
      if (scenario.id === "child-uninitialized") assert.match(observation.refusal.message,
        /submodule 'c': uninitialized/u);
    } else {
      assert.equal(observation.source, `git-commit:${fixture.commit}`);
      assert.equal(observation.tree, fixture.tree);
      assert.equal(observation.refusal, null);
      assert.deepEqual(observation.source_id, fixture.sourceId);
      assert.equal(observation.entries.length === 0, observation.status === "clean");
      if (scenario.id === "rename") assert.deepEqual(observation.entries,
        [{ status: "R ", path: "renamed file.txt", origin: "tracked file.txt" }]);
      if (scenario.id === "root-untracked") assert.deepEqual(observation.entries,
        [{ status: "??", path: "new file.txt" }]);
    }
  });
}

for (const scenario of machineEolCases) {
  test(`source machine EOL conformance: ${scenario.id}`, (t) => {
    const fixture = createMachineEolFixture(fixtureHome("e-", t), scenario.id);
    assert.equal(fixture.ordinaryStatus, "", "the actual checkout must be clean under ordinary machine policy");
    if (["global-true", "system-true", "xdg-and-home", "xdg-only"].includes(scenario.id)) {
      assert.equal(fixture.checkoutBytes[0], "original\r\n", "checkout, not a byte rewrite, supplies CRLF");
      assert.match(fixture.isolatedStatus, /tracked file.txt/u,
        "dropping machine policy must expose the forced-rehash false-dirty regression");
    }
    if (scenario.id === "root-false-child-global-true") assert.deepEqual(fixture.checkoutBytes, ["original\n", "original\r\n"]);
    if (scenario.id === "root-global-true-child-false") assert.deepEqual(fixture.checkoutBytes, ["original\r\n", "original\n"]);
    if (scenario.id === "root-global-false-child-true") assert.deepEqual(fixture.checkoutBytes, ["original\n", "original\r\n"]);
    if (scenario.mismatch) assert.deepEqual(fixture.contextualFallback, { root: "true", child: "false" },
      "ordinary Git must prove the conditional machine fallback differs before testing the owner refusal");
    if (scenario.eol) {
      const effectiveEol = scenario.eol.toLowerCase();
      const crlf = effectiveEol === "crlf" || (effectiveEol === "native" && process.platform === "win32");
      assert.equal(fixture.checkoutBytes[0], crlf ? "original\r\n" : "original\n");
    }
    if (scenario.longPath) assert.equal(fixture.longCheckoutBytes, "long tracked original\r\n",
      "active machine fallback must govern the actual long-path child checkout");
    const result = run(["--root", fixture.root, "--json"], fixture.environment);
    assert.equal(result.status, scenario.status === "unavailable" ? 1 : 0, result.stderr || result.stdout);
    assert.equal(result.stderr, "");
    const observation = JSON.parse(result.stdout);
    assert.equal(observation.status, scenario.status, result.stdout);
    assert.deepEqual(Object.keys(observation).sort(), ["entries", "refusal", "root", "source", "source_id", "status", "tree", "version"]);
    if (scenario.status === "unavailable") {
      assert.equal(observation.source_id, null);
      assert.equal(observation.refusal.code, "input_unavailable");
      if (scenario.mismatch) assert.match(observation.refusal.message, /submodule 'c s': contextual machine line-ending fallback differs/u);
      if (scenario.insideScratch) assert.match(observation.refusal.message, /source EOL scratch is inside the inspected tree/u);
    } else {
      assert.equal(observation.source, `git-commit:${observation.source_id.commit}`);
      assert.equal(observation.tree, observation.source_id.tree);
      if (scenario.dirty) {
        const changes = scenario.dirty === "root" ? observation.source_id.changes
          : observation.source_id.submodules.find((child) => child.path === fixture.childName).changes;
        assert.equal(changes.find((change) => change.path === fixture.dirtyPath).blob, fixture.expectedDirtyBlob,
          "dirty blob hashing must preserve the same effective machine EOL policy as status");
      }
    }
    assert.deepEqual(fixture.scratchEntries(), [], "owner-private EOL scratch must be removed after success or refusal");
  });
}
test("source dirty identity compares the complete delta at one unchanged commit", (t) => {
  const fixture = createSourceFixture(fixtureHome("source-delta-", t), "root-dirty");
  const observe = () => {
    const result = run(["--root", fixture.root, "--json"]);
    assert.equal(result.status, 0, result.stdout || result.stderr);
    return JSON.parse(result.stdout);
  };
  const before = observe();
  assert.equal(before.status, "not_clean");
  assert.deepEqual(observe().source_id, before.source_id);
  writeFileSync(join(fixture.root, "tracked file.txt"), "different dirty content\n");
  const after = observe();
  assert.equal(after.source, before.source);
  assert.equal(after.tree, before.tree);
  assert.notDeepEqual(after.source_id, before.source_id);
});

test("source symlink identity hashes link text and excludes ignored target contents", { skip: process.platform === "win32" }, (t) => {
  const fixture = createSourceFixture(fixtureHome("source-link-", t), "root-symlink");
  const observe = () => {
    const result = run(["--root", fixture.root, "--json"]);
    assert.equal(result.status, 0, result.stdout || result.stderr);
    return JSON.parse(result.stdout).source_id;
  };
  const before = observe();
  assert.deepEqual(before, fixture.sourceId);
  writeFileSync(join(fixture.root, "ignored", "result.txt"), "different target bytes\n");
  assert.deepEqual(observe(), before);
});

test("source refuses invalid invocation and unavailable roots without requiring a store", (t) => {
  const parent = fixtureHome("source-cli-", t);
  const fixture = createSourceFixture(parent, "clean");
  for (const args of [[], ["--root", ".", "--json"], ["--root", fixture.root],
    ["--root", fixture.root, "--json", "--timeout-ms", "18446744073709551615"],
    ["--root", fixture.root, "--json", "--timeout-ms", "0"],
    ["--root", fixture.root, "--json", "--timeout-ms", "60001"],
    ["--root", fixture.root, "--json", "--unknown"]]) assert.equal(run(args).status, 2);
  for (const root of [parent, join(parent, "missing"), join(fixture.root, "ignored")]) {
    const result = run(["--root", root, "--json"]);
    assert.equal(result.status, 1, result.stdout);
    assert.equal(JSON.parse(result.stdout).status, "unavailable");
  }
  const missingHome = join(parent, "must-not-create-store");
  const result = run(["--root", fixture.root, "--json"], { ENGRAM_HOME: missingHome,
    GIT_DIR: join(parent, "wrong-git-dir"), GIT_WORK_TREE: parent });
  assert.equal(result.status, 0, result.stdout || result.stderr);
  assert.equal(JSON.parse(result.stdout).status, "clean");
  assert.equal(existsSync(missingHome), false);
});

test("source does not execute Git from the inspected tree", (t) => {
  const fixture = createSourceFixture(fixtureHome("source-trusted-git-", t), "clean");
  const name = process.platform === "win32" ? "git.exe" : "git";
  writeFileSync(join(fixture.root, name), "not a trusted Git executable\n", { mode: 0o755 });
  const result = run(["--root", fixture.root, "--json"], { PATH: `${fixture.root}${delimiter}${process.env.PATH}` });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.equal(JSON.parse(result.stdout).status, "not_clean");
});

test("source raises a shorter public budget to sixty seconds instead of refusing on it", (t) => {
  const fixture = createSourceFixture(fixtureHome("source-budget-", t), "clean");
  for (const budget of ["1", "60000"]) {
    const result = run(["--root", fixture.root, "--json", "--timeout-ms", budget]);
    assert.equal(result.status, 0, result.stdout || result.stderr);
    assert.equal(JSON.parse(result.stdout).status, "clean");
  }
});

test("source refuses non-UTF-8 changed paths without loss", {
  skip: process.platform === "win32" || process.platform === "darwin",
}, (t) => {
  const fixture = createSourceFixture(fixtureHome("source-non-utf-", t), "clean");
  writeFileSync(Buffer.concat([Buffer.from(`${fixture.root}/`), Buffer.from([0xff])]), "changed\n");
  const result = run(["--root", fixture.root, "--json"]);
  assert.equal(result.status, 1, result.stdout || result.stderr);
  const observation = JSON.parse(result.stdout);
  assert.equal(observation.status, "unavailable");
  assert.match(observation.refusal.message, /non-UTF-8/u);
  assert.equal(observation.source, null);
});
