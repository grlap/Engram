import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";
import { captureCommitInputStatus } from "../review-freeze-fingerprint.mjs";
import { commitInputEligible, createRun, executeRun, machineRecord, startDetached, summarize } from "../test-launcher.mjs";
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

  test("committed-input eligibility requires its after fingerprint even when status is clean", () => {
    const expected = createHash("sha256").update("admitted fixture input").digest("hex");
    const changed = createHash("sha256").update("changed fixture input").digest("hex");
    const result = { state: "passed", input: { outcome: "clean" }, expectedFingerprint: expected, after: expected };
    assert.equal(commitInputEligible(result), true);
    assert.equal(commitInputEligible({ ...result, after: changed }), false);
    assert.equal(commitInputEligible({ ...result, after: undefined }), false);
    assert.equal(commitInputEligible({ ...result, state: "failed" }), false);
  });

  test("committed-input debris summary survives a damaged snapshot and malformed path metadata", async () => {
    await fixture(async ({ root, marking }) => {
      const runDir = await createRun({ root, input: "commit", stages: [marking("require('node:fs').writeFileSync('new.txt', 'debris');")] }, env);
      const result = await executeRun(runDir, env);
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
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.stages[0].code, 0);
      assert.equal(result.input.outcome, outcome);
      assert.equal(result.input.eligible, false);
      assert.equal(result.input.after.clean, false);
      assert.equal(result.exitCode, 1);
      const reason = outcome === "debris_pending_disposition" ? "new-untracked-only" : "input-changed";
      assert.equal(result.reason, reason);
      assert.match(machineRecord(json(join(runDir, "request.json")), result), new RegExp(`overall=failed .*reason=${reason}`, "u"));
      assert.match(await summarize(runDir), /FAIL .*exit=1/u);
      assert.match(await summarize(runDir), new RegExp(`reason: ${reason}`, "u"));
      if (outcome === "debris_pending_disposition") {
        assert.match(await summarize(runDir), /input-change: new-untracked-only;.*total=1;.*input.after.paths/u);
      }
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
      assert.match(result.error, /matching clean commit/u);
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

  for (const flag of ["--assume-unchanged", "--skip-worktree"]) test(`committed-input refuses ${flag}`, async () => {
    await fixture(async ({ root, marker, marking }) => {
      git(["update-index", flag, "tracked.txt"], root);
      writeFileSync(join(root, "tracked.txt"), "hidden edit");
      const runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      const result = json(join(runDir, "results.json"));
      assert.equal(result.state, "failed");
      assert.equal(result.input.skippedPaths[0].path, "tracked.txt");
      assert.equal(existsSync(marker), false);
    });
  });

  test("committed-input names configured filters and unsupported Gitlinks", async () => {
    await fixture(async ({ root, marker, marking }) => {
      git(["config", "filter.fixture.clean", "unused-filter"], root);
      let runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      assert.match(json(join(runDir, "results.json")).error, /configured Git filters/u);
      git(["config", "--remove-section", "filter.fixture"], root);
      const head = captureCommitInputStatus(root, env).commit;
      git(["update-index", "--add", "--cacheinfo", `160000,${head},child`], root);
      runDir = await createRun({ root, input: "commit", stages: [marking()] }, env);
      assert.match(json(join(runDir, "results.json")).error, /unsupported Gitlinks/u);
      assert.deepEqual(json(join(runDir, "results.json")).input.gitlinks, ["child"]);
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

  test("committed-input detached worker executes once and retains its commit record", { timeout: 60_000 }, async () => {
    await fixture(async ({ root, marker, marking }) => {
      const runDir = await createRun({ root, input: "commit", detached: true, notifyTo: "fixture-parent", stages: [marking()] }, env);
      const request = json(join(runDir, "request.json"));
      const { child, completion } = await startDetached(runDir, env, () => {});
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

  for (const moved of [false, true]) test(`committed-input detached worker refuses ${moved ? "a moved commit" : "new tracked dirt"} before stages`, { timeout: 60_000 }, async () => {
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

  test("committed-input detached CLI names a dirty-input refusal after its worker exits", { timeout: 60_000 }, async () => {
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
      assert.equal(result.state, "passed", result.error);
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
      assert.equal(result.state, "passed");
      assert.equal(result.stages[0].code, 0);
      assert.equal(result.input.eligible, false);
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
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.input.outcome, "input_changed");
      assert.equal(result.input.eligible, false);
      assert.equal(result.after, undefined);
      assert.equal(result.exitCode, 1);
      assert.equal(result.reason, "input-unverified");
      if (kind === "filter") assert.notEqual(result.input.after.filters.length, 0);
      else assert.deepEqual(result.input.after.gitlinks, ["child"]);
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
