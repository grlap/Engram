import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { captureCommitInputStatus, classifySourceIdChange, sourceBinaryPath, sourceIdDifference, sourceIdsEqual } from "../review-freeze-fingerprint.mjs";
import { createSourceFixture, sourceCases } from "../source-input-fixtures.mjs";
import { fixtureHome } from "../test-temp.mjs";
import { commitInputEligible, createRun, executeRun, machineRecord, selectDefaultInput, startDetached, summarize } from "../test-launcher.mjs";
import { registerTestRun } from "../test-run-registration.mjs";

export function registerCommitInputTests(support) {
  const { env, git, repository, stage, json, copyLauncher } = support;
  const commit = (root) => git(["-c", "user.name=Engram Test", "-c", "user.email=test@example.invalid",
    "commit", "--quiet", "--no-gpg-sign", "--allow-empty", "-m", "fixture"], root);
  const fixture = (callback) => repository(async (root) => {
    writeFileSync(join(root, "mailbox"), "process.exitCode = 0;\n");
    git(["add", "--", "mailbox"], root);
    commit(root);
    const admin = git(["rev-parse", "--absolute-git-dir"], root).trim();
    const marker = join(admin, "stage-ran");
    const marking = (extra = "") => stage("fixture", `require('node:fs').appendFileSync(${JSON.stringify(marker)}, 'x');${extra}`);
    await callback({ root, admin, marker, marking });
  });

  for (const scenario of sourceCases) test(`launcher owner conformance: ${scenario.id}`, async (t) => {
    if (scenario.skipOn?.includes(process.platform)) return t.skip("fixture requires Unix filename or mode semantics");
    const input = createSourceFixture(fixtureHome("lc-", t), scenario.id);
    const root = input.root;
    const marker = join(git(["rev-parse", "--absolute-git-dir"], root).trim(), "stage-ran");
    const stages = [stage("fixture", `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran');`)];
    if (scenario.status === "unavailable") {
      assert.throws(() => captureCommitInputStatus(root, env), /committed input unavailable/u);
      assert.throws(() => selectDefaultInput(root, env), /committed input unavailable/u);
      let runDir;
      await assert.rejects(createRun({ root, input: "commit", stages }, env), (error) => {
        runDir = error.runDir;
        return error.reason === "input-capture-failed";
      });
      assert.equal(json(join(runDir, "results.json")).stages[0].state, "unrun");
      assert.equal(existsSync(join(runDir, "input.json")), false);
    } else {
      const captured = captureCommitInputStatus(root, env);
      assert.equal(captured.status, scenario.status);
      assert.equal(captured.commit, input.commit);
      assert.equal(captured.tree, input.tree);
      assert.deepEqual(captured.sourceId, input.sourceId);
      assert.equal(selectDefaultInput(root, env), scenario.status === "clean" ? "commit" : undefined);
      const runDir = await createRun({ root, input: "commit", stages }, env);
      const result = await executeRun(runDir, env);
      if (scenario.status === "clean") {
        assert.equal(result.state, "passed", result.error);
        assert.equal(result.input.eligible, true);
        assert.equal(readFileSync(marker, "utf8"), "ran");
      } else {
        assert.equal(result.state, "failed");
        assert.equal(result.reason, "input-not-clean");
        assert.equal(result.stages[0].state, "unrun");
      }
    }
    if (scenario.status !== "clean") assert.equal(existsSync(marker), false);
  });

  test("source adapter validates bounded JSON and exit pairing without a fallback predicate", async () => {
    await fixture(async ({ root }) => {
      const measured = captureCommitInputStatus(root, env);
      const observation = { version: 1, status: "clean", root: measured.root,
        source: `git-commit:${measured.commit}`, tree: measured.tree, entries: [], refusal: null, source_id: measured.sourceId };
      const response = (value = observation, status = 0) => ({ status, signal: null,
        stdout: Buffer.from(JSON.stringify(value) + "\n"), stderr: Buffer.alloc(0) });
      const capture = (result) => captureCommitInputStatus(root, env, { run: (binary, args, options) => {
        assert.equal(binary, env.ENGRAM_SOURCE_BINARY);
        assert.deepEqual(args, ["source", "--root", resolve(root), "--json"]);
        assert.equal(options.timeout, 10_000);
        assert.equal(options.maxBuffer, 1024 * 1024 + 1);
        assert.equal(options.windowsHide, true);
        assert.deepEqual(options.stdio, ["ignore", "pipe", "pipe"]);
        return result;
      } });
      assert.deepEqual(capture(response()), measured);
      const refused = { ...observation, status: "unavailable", source: null, tree: null,
        refusal: { code: "deadline", message: "reader deadline" }, source_id: null };
      assert.throws(() => capture(response(refused, 1)), (error) => error.refusal?.code === "deadline");
      for (const result of [
        response({ ...observation, version: 2 }), response({ ...observation, extra: true }),
        response({ ...observation, source: [observation.source] }), response({ ...observation, tree: null }),
        response({ ...observation, root: join(root, "missing") }), response({ ...observation, entries: [{}] }),
        response({ ...observation, status: "not_clean" }), response({ ...observation, status: "future_status" }),
        response({ ...refused, refusal: { code: "future_code", message: "unknown" } }, 1),
        response({ ...refused, source: observation.source }, 1), response({ ...refused, entries: [{}] }, 1),
        response({ ...refused, refusal: { code: "deadline", message: "x".repeat(4097) } }, 1),
        response(observation, 1), response(refused, 0), response(observation, 2),
        { ...response(), stdout: Buffer.from("{truncated") },
        { ...response(), stdout: Buffer.from([0xff]) },
        { ...response(), stdout: Buffer.alloc(1024 * 1024 + 2, 32) },
        { ...response(), stderr: Buffer.from("warning") },
        { ...response(), signal: "SIGTERM" },
        { ...response(), error: Object.assign(new Error("owner timed out"), { code: "ETIMEDOUT" }) },
      ]) assert.throws(() => capture(result), /committed input unavailable/u);
      const renamed = { ...observation, status: "not_clean", entries: [{ status: "R ", path: "new", origin: "old" }] };
      assert.deepEqual(capture(response(renamed)).entries, renamed.entries);
      assert.throws(() => capture(response({ ...renamed, entries: [{ status: "R ", path: "new" }] })), /malformed changed-path/u);
      const change = { path: "file", status: " M", origin: null, blob: measured.commit, mode: "100644" };
      const identified = { ...measured.sourceId, changes: [change] };
      assert.ok(sourceIdsEqual(identified, { changes: [{ mode: change.mode, blob: change.blob, origin: null,
        status: change.status, path: change.path }], submodules: [], tree: identified.tree, commit: identified.commit }));
      assert.equal(sourceIdsEqual(identified, measured.sourceId), false);
      const added = { ...change, path: "new.txt", status: "??" };
      assert.deepEqual(classifySourceIdChange(identified, { ...identified, changes: [change, added] }),
        { outcome: "new-untracked-only", newPaths: ["new.txt"] });
      const child = { path: "child", commit: measured.commit, changes: [change] };
      const withChild = { ...identified, submodules: [child] };
      assert.deepEqual(classifySourceIdChange(withChild, { ...withChild,
        submodules: [{ ...child, changes: [change, added] }] }),
      { outcome: "new-untracked-only", newPaths: ["child/new.txt"] });
      for (const changed of [
        { ...identified, commit: measured.tree },
        { ...identified, changes: [added] },
        { ...identified, changes: [{ ...change, blob: measured.tree }, added] },
        { ...identified, changes: [change, { ...added, status: " A" }] },
        { ...identified, changes: [{ ...added, path: change.path }, change] },
      ]) assert.equal(classifySourceIdChange(identified, changed).outcome, "input-changed");
      assert.equal(classifySourceIdChange(withChild, { ...withChild,
        submodules: [{ ...child, commit: measured.tree }] }).outcome, "input-changed");
      const detail = sourceIdDifference(identified, { ...identified, changes: [{ ...change, blob: measured.tree }] });
      assert.match(detail, /file.*\[ M\]/u);
      assert.ok(detail.includes(change.blob) && detail.includes(measured.tree));
      assert.ok(Buffer.byteLength(sourceIdDifference(measured.sourceId, { ...measured.sourceId,
        changes: [{ ...added, path: "long".repeat(2000) }] })) < 4300);
      for (const source_id of [null, { ...measured.sourceId, extra: true },
        { ...measured.sourceId, tree: "invalid" }, { ...measured.sourceId, changes: [{ ...change, origin: undefined }] },
        { ...measured.sourceId, changes: [{ ...change, blob: null }] },
        { ...measured.sourceId, changes: [{ ...change, path: "../outside" }] },
        { ...measured.sourceId, changes: [{ ...change, mode: 100644 }] },
        { ...measured.sourceId, changes: [change, change] },
        { ...measured.sourceId, changes: [{ ...change, path: "z" }, { ...change, path: "a" }] },
        { ...measured.sourceId, submodules: [{ path: "child", commit: measured.commit, changes: [], extra: true }] },
      ]) assert.throws(() => capture(response({ ...observation, source_id })), /source_id/u);
      assert.throws(() => capture(response({ ...refused, source_id: measured.sourceId }, 1)), /unavailable envelope/u);
    });
  });

  test("source adapter chooses one installed path or an absolute override and refuses missing owners", async () => {
    await fixture(async ({ root, marker, marking }) => {
      const { ENGRAM_SOURCE_BINARY: _override, ...installedEnv } = env;
      const cargoHome = join(root, "missing-cargo-home");
      const missing = { ...installedEnv, CARGO_HOME: cargoHome };
      const expected = join(cargoHome, "bin", process.platform === "win32" ? "engram.exe" : "engram");
      assert.equal(sourceBinaryPath(missing), expected);
      assert.equal(sourceBinaryPath({ ...missing, ENGRAM_SOURCE_BINARY: env.ENGRAM_SOURCE_BINARY }), env.ENGRAM_SOURCE_BINARY);
      assert.throws(() => sourceBinaryPath({ ...missing, ENGRAM_SOURCE_BINARY: "relative-owner" }), /absolute/u);
      assert.throws(() => sourceBinaryPath({ ...missing, CARGO_HOME: "" }), /CARGO_HOME must be absolute/u);
      assert.throws(() => selectDefaultInput(root, missing), /ENGRAM_SOURCE_BINARY/u);
      let runDir;
      await assert.rejects(createRun({ root, selectInput: true, stages: [marking()] }, missing), (error) => {
        runDir = error.runDir;
        return error.reason === "input-capture-failed";
      });
      assert.equal(json(join(runDir, "request.json")).sourceBinary, expected);
      assert.equal(json(join(runDir, "results.json")).stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
    });
  });

  test("worker refuses a changed recorded owner before executing a stage", async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const path = join(runDir, "request.json");
      writeFileSync(path, JSON.stringify({ ...json(path), sourceBinary: join(root, "different-owner") }));
      const result = await executeRun(runDir, env);
      assert.equal(result.reason, "input-capture-failed");
      assert.match(result.error, /source owner or identity differs from the admitted snapshot/u);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
    });
  });

  for (const snapshot of ["missing owner", "malformed", "missing"]) test(`worker refuses a ${snapshot} completion owner snapshot before executing a stage`, async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const path = join(runDir, "completion.request.json");
      if (snapshot === "missing owner") {
        const admitted = json(path);
        delete admitted.sourceBinary;
        writeFileSync(path, JSON.stringify(admitted));
      } else if (snapshot === "malformed") writeFileSync(path, "{truncated");
      else unlinkSync(path);
      const result = await executeRun(runDir, env);
      assert.equal(result.reason, "input-capture-failed");
      assert.match(result.error, snapshot === "missing owner"
        ? /source owner or identity differs from the admitted snapshot/u
        : /admitted source snapshot is missing or unreadable/u);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
    });
  });

  test("worker refuses an unavailable recorded owner before executing a stage", async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const request = json(join(runDir, "request.json"));
      assert.equal(json(join(runDir, "completion.request.json")).sourceBinary, request.sourceBinary);
      // The executable stays fixed; its observation becomes unavailable only
      // after launch admission, before the worker's own capture.
      git(["config", "filter.refuse.clean", "must-not-execute"], root);
      const result = await executeRun(runDir, env);
      assert.equal(result.reason, "input-capture-failed");
      assert.match(result.error, /committed input unavailable/u);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
    });
  });

  test("implicit input selects commit only while clean, and never downgrades an unavailable capture", async () => {
    await fixture(async ({ root }) => {
      assert.equal(selectDefaultInput(root, env), "commit");
      writeFileSync(join(root, "tracked.txt"), "changed");
      assert.equal(selectDefaultInput(root, env), undefined);
      git(["restore", "tracked.txt"], root);
      assert.equal(selectDefaultInput(root, env), "commit");
      git(["update-index", "--skip-worktree", "tracked.txt"], root);
      assert.equal(selectDefaultInput(root, env), "commit", "the owner's documented hidden-index limitation is shared");
      assert.throws(() => selectDefaultInput(join(root, "absent"), env));
    });
  });

  test("default selection rejects a nested directory and retains capture refusal without executing", async () => {
    await fixture(async ({ root, marking, marker }) => {
      const nested = join(root, "nested");
      mkdirSync(nested);
      assert.throws(() => selectDefaultInput(nested, env), /committed input unavailable/u);
      let nestedRun;
      await assert.rejects(() => createRun({ root: nested, input: "commit", stages: [marking()] }, env), (error) => {
        assert.equal(error.reason, "input-capture-failed");
        nestedRun = error.runDir;
        return true;
      });
      assert.equal(json(join(nestedRun, "results.json")).stages[0].state, "unrun");
      git(["config", "filter.refuse.clean", "must-not-execute"], root);
      let runDir;
      await assert.rejects(() => createRun({ root, selectInput: true, stages: [marking()] }, env), (error) => {
        assert.equal(error.reason, "input-capture-failed");
        runDir = error.runDir;
        return true;
      });
      // createRun keeps its existing saved failure and recovery path. An
      // unavailable selection never becomes an executing plain request.
      const result = json(join(runDir, "results.json"));
      assert.equal(result.state, "failed");
      assert.equal(result.reason, "input-capture-failed");
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
      assert.equal(existsSync(join(runDir, "input.json")), false);
    });
  });

  test("implicit selection executes a clean commit and an unchanged dirty plain input", async () => {
    await fixture(async ({ root, marking }) => {
      let runDir = await createRun({ root, selectInput: true, stages: [marking()] }, env);
      assert.equal(json(join(runDir, "request.json")).input, "commit");
      let result = await executeRun(runDir, env);
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.input.kind, "commit");
      assert.equal(result.input.eligible, true);
      writeFileSync(join(root, "tracked.txt"), "uncommitted correction\n");
      runDir = await createRun({ root, selectInput: true, stages: [marking()] }, env);
      assert.equal(Object.hasOwn(json(join(runDir, "request.json")), "input"), false);
      result = await executeRun(runDir, env);
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.input, undefined);
      assert.equal(result.before, result.after);
      assert.deepEqual(result.sourceIdBefore, result.sourceIdAfter);
    });
  });

  test("launcher compares dirty child contents even when its old fingerprint stays equal", async (t) => {
    const fixture = createSourceFixture(fixtureHome("source-child-drift-", t), "child-dirty");
    const command = stage("edit-child", `require('node:fs').writeFileSync(${JSON.stringify(join(fixture.child, "tracked file.txt"))}, 'different child content\\n');`);
    const runDir = await createRun({ root: fixture.root, stages: [command] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.stages[0].state, "passed", result.error);
    assert.equal(result.before, result.after, "legacy dirty-submodule marker cannot identify child bytes");
    assert.equal(result.sourceIdBefore.commit, result.sourceIdAfter.commit);
    assert.notDeepEqual(result.sourceIdBefore, result.sourceIdAfter);
    assert.equal(result.state, "failed");
    assert.equal(result.reason, "input-changed");
    assert.equal(result.exitCode, 1);
  });

  test("committed-input eligibility requires its after fingerprint even when status is clean", () => {
    const expected = createHash("sha256").update("admitted fixture input").digest("hex");
    const changed = createHash("sha256").update("changed fixture input").digest("hex");
    const sourceId = { commit: expected.slice(0, 40), tree: changed.slice(0, 40), submodules: [], changes: [] };
    const result = { state: "passed", input: { outcome: "clean" }, expectedFingerprint: expected, after: expected,
      sourceIdBefore: sourceId, sourceIdAfter: sourceId };
    assert.equal(commitInputEligible(result), true);
    assert.equal(commitInputEligible({ ...result, after: changed }), false);
    assert.equal(commitInputEligible({ ...result, after: undefined }), false);
    assert.equal(commitInputEligible({ ...result, state: "failed" }), false);
    assert.equal(commitInputEligible({ ...result, sourceIdAfter: { ...sourceId, commit: changed.slice(0, 40) } }), false);
  });

  test("historical committed-input debris summary survives a damaged snapshot and malformed path metadata", async () => {
    await fixture(async ({ root, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking("require('node:fs').writeFileSync('new.txt', 'debris');")] }, env);
      const result = await executeRun(runDir, env);
      // Retained results from the previous launcher remain readable as stored.
      Object.assign(result, { state: "passed", reason: "new-untracked-only" });
      delete result.sourceIdBefore;
      delete result.sourceIdAfter;
      writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      assert.equal(result.input.outcome, "debris_pending_disposition");
      writeFileSync(join(runDir, "completion.request.json"), "{damaged");
      assert.match(await summarize(runDir), /FAIL .*exit=1/u);
      assert.match(await summarize(runDir), /input-change: new-untracked-only;.*total=1;.*input.after.paths/u);
      for (const after of [undefined, { paths: [] }, { paths: [{ status: " M", path: "tracked.txt" }] }]) {
        writeFileSync(join(runDir, "results.json"), JSON.stringify({ ...result, input: { ...result.input, after } }));
        const summary = await summarize(runDir);
        assert.match(summary, /FAIL .*exit=1/u);
        assert.match(summary, /input-change: details unavailable/u);
      }
    });
  });

  test("committed-input records a clean commit and tree through execution and its frozen completion", async () => {
    await fixture(async ({ root, marker, marking }) => {
      const status = captureCommitInputStatus(root, env);
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const request = json(join(runDir, "request.json"));
      assert.equal(request.input, "commit");
      assert.equal(request.commit, status.commit);
      assert.equal(request.tree, status.tree);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "passed", result.error);
      assert.equal(readFileSync(marker, "utf8"), "x");
      assert.deepEqual(result.input, { kind: "commit", commit: status.commit, tree: status.tree, cleanBefore: true,
        after: { commit: status.commit, tree: status.tree, clean: true, paths: [] }, outcome: "clean", eligible: true });
      const snapshot = json(join(runDir, "completion.request.json"));
      assert.equal(snapshot.commit, status.commit);
      assert.equal(snapshot.tree, status.tree);
      assert.equal(snapshot.input, "commit");
      writeFileSync(join(runDir, "request.json"), JSON.stringify({ ...request, input: undefined, commit: "rewritten" }));
      assert.match(await summarize(runDir), /committed-input:.*outcome="clean" eligible=true/u);
      await assert.rejects(executeRun(runDir, env), /EEXIST/u);
      assert.equal(readFileSync(marker, "utf8"), "x");
    });
  });

  for (const [name, dirty] of [
    ["tracked edit", (root) => writeFileSync(join(root, "tracked.txt"), "changed")],
    ["staged edit", (root) => { writeFileSync(join(root, "tracked.txt"), "changed"); git(["add", "tracked.txt"], root); }],
    ["untracked path", (root) => writeFileSync(join(root, "new.txt"), "new")],
  ]) test(`committed-input refuses ${name} before any stage`, async () => {
    await fixture(async ({ root, marker, marking }) => {
      dirty(root);
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed");
      assert.equal(result.reason, "input-not-clean");
      assert.equal(result.input.cleanBefore, false);
      assert.notEqual(result.input.dirtyPaths.length, 0);
      assert.equal(existsSync(marker), false);
    });
  });

  test("committed-input respects repository ignores and local excludes", async () => {
    await fixture(async ({ root, admin, marking }) => {
      mkdirSync(join(root, "target"));
      writeFileSync(join(root, "target", "ignored-output"), "ignored");
      mkdirSync(join(admin, "info"), { recursive: true });
      writeFileSync(join(admin, "info", "exclude"), "excluded.txt\n");
      writeFileSync(join(root, "excluded.txt"), "excluded");
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.input.eligible, true);
    });
  });

  for (const [name, code, outcome] of [
    ["tracked drift", "require('node:fs').writeFileSync('tracked.txt', 'changed');", "input_changed"],
    ["new debris", "require('node:fs').writeFileSync('new.txt', 'new');", "debris_pending_disposition"],
  ]) test(`committed-input preserves passing stages while recording ${name}`, async () => {
    await fixture(async ({ root, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking(code)] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed");
      assert.equal(result.stages[0].state, "passed");
      assert.equal(result.stages[0].code, 0);
      assert.equal(result.input.outcome, outcome);
      assert.equal(result.input.eligible, false);
      assert.equal(result.input.after.clean, false);
      assert.equal(result.exitCode, 1);
      const reason = name === "new debris" ? "new-untracked-only" : "input-changed";
      assert.equal(result.reason, reason);
      assert.match(machineRecord(json(join(runDir, "request.json")), result), new RegExp(`overall=failed .*reason=${reason}`, "u"));
      assert.match(await summarize(runDir), /FAIL .*exit=1/u);
      assert.match(await summarize(runDir), new RegExp(`reason: ${reason}`, "u"));
      if (name === "new debris") {
        assert.match(await summarize(runDir), /input-change: new-untracked-only;.*total=1;.*input\.after\.paths/u);
        assert.deepEqual(result.input.after.paths, [{ status: "??", path: "new.txt" }]);
      }
      assert.notDeepEqual(result.sourceIdBefore, result.sourceIdAfter);
    });
  });

  test("committed-input never makes a failed stage eligible", async () => {
    await fixture(async ({ root }) => {
      const runDir = await createRun({ root, input: "commit", stages: [stage("fail", "process.exit(7)")] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed");
      assert.equal(result.exitCode, 7);
      assert.equal(result.input.outcome, "clean");
      assert.equal(result.input.eligible, false);
    });
  });

  test("committed-input refuses HEAD movement during capture and before worker admission", async () => {
    await fixture(async ({ root, marker, marking }) => {
      let calls = 0;
      const commitInputStatus = () => {
        const status = captureCommitInputStatus(root, env);
        if (++calls === 1) commit(root);
        return status;
      };
      await assert.rejects(createRun({ root, input: "commit", stages: [marking()], commitInputStatus }, env), /input capture failed/u);
      assert.equal(existsSync(marker), false);
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      commit(root);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed");
      assert.match(result.error, /source_id changed before execution/u);
      assert.equal(existsSync(marker), false);
    });
  });

  test("committed-input detects dirt during capture even if it is reverted before admission", async () => {
    await fixture(async ({ root, marker, marking }) => {
      const original = readFileSync(join(root, "tracked.txt"));
      let calls = 0;
      const commitInputStatus = () => {
        const status = captureCommitInputStatus(root, env);
        writeFileSync(join(root, "tracked.txt"), ++calls === 1 ? "changed" : original);
        return status;
      };
      await assert.rejects(createRun({ root, input: "commit", stages: [marking()], commitInputStatus }, env), /input capture failed/u);
      assert.equal(existsSync(marker), false);
      assert.equal(captureCommitInputStatus(root, env).entries.length, 0);
    });
  });

  for (const flag of ["--assume-unchanged", "--skip-worktree"]) test(`committed-input shares the owner's ${flag} limitation`, async () => {
    await fixture(async ({ root, marker, marking }) => {
      git(["update-index", flag, "tracked.txt"], root);
      writeFileSync(join(root, "tracked.txt"), "hidden edit");
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.input.eligible, true);
      assert.equal(existsSync(marker), true);
    });
  });

  test("committed-input retains the owner's configured-filter refusal before stages", async () => {
    await fixture(async ({ root, marker, marking }) => {
      git(["config", "filter.fixture.clean", "unused-filter"], root);
      let runDir;
      await assert.rejects(createRun({ root, input: "commit", stages: [marking()] }, env), (error) => {
        runDir = error.runDir;
        return error.reason === "input-capture-failed";
      });
      assert.match(json(join(runDir, "results.json")).error, /input_unavailable:.*filter/iu);
      assert.equal(existsSync(marker), false);
    });
  });

  test("committed-input parses rename paths without splitting spaces", async () => {
    await fixture(async ({ root }) => {
      git(["mv", "tracked.txt", "renamed file.txt"], root);
      const status = captureCommitInputStatus(root, env);
      assert.deepEqual(status.entries, [{ status: "R ", path: "renamed file.txt", origin: "tracked.txt" }]);
    });
  });

  test("committed-input detached worker executes once and retains its commit record", { timeout: 120_000 }, async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", detached: true, notifyTo: "fixture-parent", stages: [marking()] }, env);
      const request = json(join(runDir, "request.json"));
      assert.equal(request.sourceBinary, env.ENGRAM_SOURCE_BINARY);
      assert.equal(json(join(runDir, "completion.request.json")).sourceBinary, request.sourceBinary);
      const { child, completion } = await startDetached(runDir,
        { ...env, ENGRAM_SOURCE_BINARY: join(root, "must-not-switch-owner") }, () => {});
      child.ref();
      const outcome = await completion;
      assert.equal(outcome.code, 0, readFileSync(join(runDir, "launcher.log"), "utf8"));
      const result = json(join(runDir, "results.json"));
      assert.equal(result.input.commit, request.commit);
      assert.equal(result.input.tree, request.tree);
      assert.equal(result.input.eligible, true);
      assert.equal(readFileSync(marker, "utf8"), "x");
      await assert.rejects(executeRun(runDir, env), /EEXIST/u);
      assert.equal(readFileSync(marker, "utf8"), "x");
    });
  });

  test("committed-input CLI accepts the explicit spelling and refuses malformed selections", async () => {
    await fixture(async ({ root }) => {
      const local = copyLauncher(root);
      git(["add", "--", "scripts"], root);
      commit(root);
      const launched = spawnSync(process.execPath, [local, "focused", "--input", "commit", "--", process.execPath, "-e", ""],
        { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(launched.status, 0, launched.stderr);
      assert.match(launched.stdout, /committed-input:.*outcome="clean" eligible=true/u);
      const directory = /^results: (.+)[\\/]results.json$/mu.exec(launched.stdout)?.[1];
      assert.ok(directory, launched.stdout);
      assert.equal(json(join(directory, "request.json")).input, "commit");
      for (const options of [["--input"], ["--input", "staged"], ["--input", "commit", "--input", "commit"]]) {
        const refused = spawnSync(process.execPath, [local, "focused", ...options, "--", process.execPath, "-e", ""],
          { cwd: root, env, encoding: "utf8", windowsHide: true });
        assert.equal(refused.status, 1, refused.stderr);
        assert.match(refused.stderr, /invalid launcher option/u);
      }
    });
  });

  for (const moved of [false, true]) test(`committed-input detached worker refuses ${moved ? "a moved commit" : "new tracked dirt"} before stages`, { timeout: 120_000 }, async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", detached: true, notifyTo: "fixture-parent", stages: [marking()] }, env);
      const request = json(join(runDir, "request.json"));
      if (moved) commit(root);
      else writeFileSync(join(root, "tracked.txt"), "changed after launch capture");
      const { child, completion } = await startDetached(runDir, env, () => {});
      child.ref();
      const outcome = await completion;
      const result = json(join(runDir, "results.json"));
      assert.equal(outcome.code, 1, readFileSync(join(runDir, "launcher.log"), "utf8"));
      assert.equal(result.reason, "input-changed");
      assert.equal(result.input.commit, request.commit);
      assert.equal(result.input.eligible, false);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
      assert.equal(json(join(runDir, "notification.json")).ok, true);
      assert.match(readFileSync(join(runDir, "notification.message.txt"), "utf8"), /reason: input-changed/u);
      await assert.rejects(executeRun(runDir, env), /EEXIST/u);
    });
  });

  test("committed-input detached CLI names a dirty-input refusal after its worker exits", { timeout: 120_000 }, async () => {
    await fixture(async ({ root }) => {
      const local = copyLauncher(root);
      git(["add", "--", "scripts"], root);
      commit(root);
      writeFileSync(join(root, "tracked.txt"), "dirty before launch");
      const launched = spawnSync(process.execPath, [local, "focused", "--detach", "--notify", "fixture-parent", "--input", "commit", "--", process.execPath, "-e", "throw new Error('must not execute')"],
        { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(launched.status, 1, launched.stderr);
      assert.match(launched.stderr, /FAIL launcher: input-not-clean/u);
      assert.doesNotMatch(launched.stderr, /worker-startup-failed/u);
      const directory = /^results: (.+)[\\/]results.json$/mu.exec(launched.stderr)?.[1];
      assert.ok(directory, launched.stderr);
      assert.equal(existsSync(join(directory, "execution.lock")), true, "an actual detached worker admitted the refusal");
      const result = json(join(directory, "results.json"));
      assert.equal(result.reason, "input-not-clean");
      assert.equal(result.input.cleanBefore, false);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(json(join(directory, "notification.json")).ok, true);
      assert.match(readFileSync(join(directory, "notification.message.txt"), "utf8"), /reason: input-not-clean/u);
    });
  });

  test("committed-input records a commit moved by a stage as ineligible", async () => {
    await fixture(async ({ root, marking }) => {
      const advance = `require('node:child_process').execFileSync('git', ${JSON.stringify(["-c", "user.name=Engram Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "--no-gpg-sign", "-m", "stage commit"])});`;
      const runDir = await createRun({ root, input: "commit", stages: [marking(advance)] }, env);
      const before = json(join(runDir, "request.json")).commit;
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed", result.error);
      assert.equal(result.stages[0].state, "passed");
      assert.notDeepEqual(result.sourceIdAfter, result.sourceIdBefore);
      assert.notEqual(result.input.after.commit, before);
      assert.equal(result.input.outcome, "input_changed");
      assert.equal(result.input.eligible, false);
      assert.equal(result.exitCode, 1);
      assert.match(machineRecord(json(join(runDir, "request.json")), result), /overall=failed .*reason=input-changed/u);
      assert.match(await summarize(runDir), /FAIL .*exit=1/u);
    });
  });

  test("committed-input CLI isolates initial run-directory discovery from inherited Git redirection", async () => {
    await fixture(async ({ root, admin }) => {
      const local = copyLauncher(root);
      git(["add", "--", "scripts"], root);
      commit(root);
      const redirected = { ...env, GIT_DIR: join(root, "missing.git"), GIT_WORK_TREE: join(root, "wrong-tree") };
      const launched = spawnSync(process.execPath, [local, "focused", "--input", "commit", "--", process.execPath, "-e", ""],
        { cwd: root, env: redirected, encoding: "utf8", windowsHide: true });
      assert.equal(launched.status, 0, launched.stderr);
      const directory = /^results: (.+)[\\/]results.json$/mu.exec(launched.stdout)?.[1];
      assert.ok(directory, launched.stdout);
      assert.equal(directory.startsWith(join(admin, "review-runs")), true, directory);
      assert.equal(json(join(directory, "request.json")).root, root);
      assert.equal(json(join(directory, "results.json")).input.eligible, true);
    });
  });

  for (const [name, code, reason] of [
    ["tracked drift", "require('node:fs').writeFileSync('tracked.txt', 'changed');", "input-changed"],
    ["new debris", "require('node:fs').writeFileSync('new.txt', 'new');", "new-untracked-only"],
    ["moved commit", `require('node:child_process').execFileSync('git', ${JSON.stringify(["-c", "user.name=Engram Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "--no-gpg-sign", "-m", "stage commit"])});`, "input-changed"],
  ]) test(`committed-input CLI fails the process and machine record for ${name}`, async () => {
    await fixture(async ({ root }) => {
      const local = copyLauncher(root);
      git(["add", "--", "scripts"], root);
      commit(root);
      const launched = spawnSync(process.execPath, [local, "focused", "--input", "commit", "--", process.execPath, "-e", code],
        { cwd: root, env, encoding: "utf8", windowsHide: true });
      assert.equal(launched.status, 1, launched.stderr);
      assert.match(launched.stdout, new RegExp(`overall=failed .*reason=${reason}`, "u"));
      assert.match(launched.stdout, /FAIL .*exit=1/u);
      assert.match(launched.stdout, new RegExp(`reason: ${reason}`, "u"));
      const directory = /^results: (.+)[\\/]results.json$/mu.exec(launched.stdout)?.[1];
      assert.ok(directory, launched.stdout);
      const result = json(join(directory, "results.json"));
      assert.equal(result.state, "failed");
      assert.equal(result.stages[0].state, "passed");
      assert.equal(result.stages[0].code, 0);
      assert.equal(result.input.eligible, false);
      if (name === "new debris") {
        assert.equal(result.input.outcome, "debris_pending_disposition");
        assert.deepEqual(result.input.after.paths, [{ status: "??", path: "new.txt" }]);
        assert.match(launched.stdout, /input-change: new-untracked-only;.*total=1;.*input\.after\.paths/u);
        assert.notDeepEqual(result.sourceIdBefore, result.sourceIdAfter);
      }
    });
  });

  for (const kind of ["filter", "gitlink"]) test(`committed-input refuses after-run ${kind} without unsafe capture`, async () => {
    await fixture(async ({ root, admin }) => {
      const filterMarker = join(admin, "filter-ran");
      const head = captureCommitInputStatus(root, env).commit;
      const filterScript = join(admin, "filter.cjs");
      writeFileSync(filterScript, `require('node:fs').writeFileSync(${JSON.stringify(filterMarker)}, 'ran');`);
      const filter = `"${process.execPath}" "${filterScript}"`;
      const args = kind === "filter" ? ["config", "filter.fixture.clean", filter]
        : ["update-index", "--add", "--cacheinfo", `160000,${head},child`];
      // A real filter attribute makes an unsafe post-run fingerprint execute
      // the configured command; the safe branch must leave this marker absent.
      writeFileSync(join(root, ".gitattributes"), "tracked.txt filter=fixture\n");
      git(["add", "--", ".gitattributes"], root);
      commit(root);
      const source = `require('node:child_process').execFileSync('git', ${JSON.stringify(args)});require('node:fs').writeFileSync('tracked.txt', 'changed');`;
      const runDir = await createRun({ root, input: "commit", stages: [stage("configure", source)] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.state, "failed");
      assert.equal(result.stages[0].state, "passed");
      assert.equal(result.input.outcome, "input_changed");
      assert.equal(result.input.eligible, false);
      assert.equal(result.after, undefined);
      assert.equal(result.exitCode, 1);
      assert.equal(result.reason, "input-unverified");
      assert.equal(result.input.after.refusal.code, "input_unavailable");
      assert.ok(result.input.after.refusal.message);
      assert.equal(existsSync(filterMarker), false);
      assert.match(await summarize(runDir), /reason: input-unverified/u);
    });
  });

  test("committed-input preserves the legacy registration identity and binds explicit selection", async () => {
    await fixture(async ({ root, marking }) => {
      let request, runDir;
      const options = { fetch: async (_url, init) => {
        if (init.method === "POST") {
          runDir = JSON.parse(init.body).runDirectory;
          request = json(join(runDir, "request.json"));
          return new Response(JSON.stringify({ contract: "termal-durable-test-run-registration", version: 1, runId: request.runId, registered: true }));
        }
        return new Response(JSON.stringify({ source: "index", run: { runId: request.runId, ownerSessionId: request.owner,
          initiatingSessionId: request.owner, registrationState: "registered", trackingDurability: "durable",
          trackingProvenances: ["launcherRegistered"], creatorPid: request.creatorPid, startedAt: request.started,
          detached: request.detached, runDir, worktree: request.root } }));
      } };
      const hostedEnv = { ...env, TERMAL_BASE_URL: "http://127.0.0.1:1" };
      await createRun({ root, stages: [marking()] }, hostedEnv, options);
      const legacyFields = ["runId", "root", "stages", "notifyTo", "full", "scope", "detached", "creatorPid", "owner", "started",
        "requiredBinaryEnv", "prerequisiteFiles", "needsCargo", "liveEngram"];
      const ordered = (value) => Array.isArray(value) ? value.map(ordered) : value && typeof value === "object"
        ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, ordered(value[key])])) : value;
      const legacy = createHash("sha256").update(JSON.stringify(ordered(Object.fromEntries(legacyFields.map((key) => [key, request[key] ?? null]))))).digest("hex");
      const saved = json(join(runDir, "request.json"));
      assert.equal(saved.registration.requestIdentity, legacy);
      assert.equal(Object.hasOwn(saved, "input"), false);
      assert.equal(Object.hasOwn(json(join(runDir, "completion.request.json")), "input"), false);
      const withInput = { ...saved, input: "commit" };
      const registered = await registerTestRun(withInput, runDir, hostedEnv, options);
      assert.notEqual(registered.requestIdentity, legacy);
      assert.equal(registered.state, "registered");
      let calls = 0;
      const mutating = { fetch: async (...args) => { const response = await options.fetch(...args); if (++calls === 1) delete withInput.input; return response; } };
      const refused = await registerTestRun(withInput, runDir, hostedEnv, mutating);
      assert.equal(refused.state, "unconfirmed");
      assert.equal(refused.reason, "request-changed");
    });
  });
}
