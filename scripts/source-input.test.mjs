import assert from "node:assert/strict";
import { existsSync, realpathSync, writeFileSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test, { after } from "node:test";
import { fixtureHome, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { createSourceFixture, sourceCases, sourceRefusalCodes } from "./source-input-fixtures.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

const repository = resolve(import.meta.dirname, "..");
const binary = join(resolve(repository, process.env.CARGO_TARGET_DIR || "target"), "debug",
  process.platform === "win32" ? "engram.exe" : "engram");
const normalized = (path) => realpathSync.native(path).replace(/^\\\\\?\\/u, "");

function run(args, env = {}) {
  const environment = { ...process.env };
  delete environment.ENGRAM_HOME;
  delete environment.ENGRAM_PROJECT_FILE;
  Object.assign(environment, env);
  const result = spawnSync(binary, ["source", ...args], {
    cwd: repository, env: environment, encoding: "utf8", timeout: 15_000, maxBuffer: 1024 * 1024 + 4096,
  });
  assert.ifError(result.error);
  return result;
}

for (const scenario of sourceCases) {
  test(`source owner conformance: ${scenario.id}`, (t) => {
    const fixture = createSourceFixture(fixtureHome("s-", t), scenario.id);
    const result = run(["--root", fixture.root, "--json"]);
    assert.equal(result.status, scenario.status === "unavailable" ? 1 : 0, result.stderr || result.stdout);
    const observation = JSON.parse(result.stdout);
    assert.deepEqual(Object.keys(observation).sort(), ["entries", "refusal", "root", "source", "status", "tree", "version"]);
    assert.equal(observation.version, 1);
    assert.equal(observation.status, scenario.status, result.stdout);
    if (scenario.id === "root-autocrlf-clean") assert.equal(result.stderr, "");
    assert.equal(observation.root.startsWith("\\\\?\\"), false, "wire root must use the plain form");
    assert.equal(normalized(observation.root), normalized(fixture.root));
    if (observation.status === "unavailable") {
      assert.equal(observation.source, null);
      assert.equal(observation.tree, null);
      assert.deepEqual(observation.entries, []);
      assert.ok(sourceRefusalCodes.includes(observation.refusal.code));
      assert.equal(observation.refusal.code, "input_unavailable");
      assert.ok(observation.refusal.message);
      if (scenario.id === "child-off-pin") assert.match(observation.refusal.message,
        /submodule 'c': HEAD differs from its pin/u);
      if (scenario.id === "child-uninitialized") assert.match(observation.refusal.message,
        /submodule 'c': uninitialized/u);
    } else {
      assert.equal(observation.source, `git-commit:${fixture.commit}`);
      assert.equal(observation.tree, fixture.tree);
      assert.equal(observation.refusal, null);
      assert.equal(observation.entries.length === 0, observation.status === "clean");
      if (scenario.id === "rename") assert.deepEqual(observation.entries,
        [{ status: "R ", path: "renamed file.txt", origin: "tracked file.txt" }]);
      if (scenario.id === "root-untracked") assert.deepEqual(observation.entries,
        [{ status: "??", path: "new file.txt" }]);
    }
  });
}

test("source refuses invalid invocation and unavailable roots without requiring a store", (t) => {
  const parent = fixtureHome("source-cli-", t);
  const fixture = createSourceFixture(parent, "clean");
  for (const args of [[], ["--root", ".", "--json"], ["--root", fixture.root],
    ["--root", fixture.root, "--json", "--timeout-ms", "8001"],
    ["--root", fixture.root, "--json", "--timeout-ms", "0"],
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

test("source shorter deadline refuses instead of emitting clean input", (t) => {
  const fixture = createSourceFixture(fixtureHome("source-deadline-", t), "clean");
  const result = run(["--root", fixture.root, "--json", "--timeout-ms", "1"]);
  assert.equal(result.status, 1, result.stdout || result.stderr);
  const observation = JSON.parse(result.stdout);
  assert.equal(observation.status, "unavailable");
  assert.equal(observation.source, null);
  assert.equal(observation.refusal.code, "deadline");
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
