import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { spawnSync } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { delimiter, join } from "node:path";
import test from "node:test";
import { createRun, executeRun, startDetached, summarize } from "../test-launcher.mjs";
import { registerTestRun } from "../test-run-registration.mjs";

export function registerRequestMessageTests(support) {
  const { env, git, repository, stage, json, copyLauncher, nodeFixtureExecutable, withPrependedPath } = support;
  const native = () => `mailbox-message-${randomUUID()}`;
  const relay = () => `relay-message-${Buffer.from(JSON.stringify({ version: 1, home: {
    host: { hostId: randomUUID(), hostGeneration: 1 }, storeId: randomUUID() }, id: native() })).toString("base64url")}`;
  const ack = (request) => ({ contract: "termal-durable-test-run-registration", version: 1,
    runId: request.runId, registered: true });
  const view = (request, directory, source, echo) => ({ source, run: {
    runId: request.runId, ownerSessionId: request.owner, initiatingSessionId: request.owner,
    registrationState: "registered", trackingDurability: "durable", trackingProvenances: ["launcherRegistered"],
    creatorPid: request.creatorPid, startedAt: request.started, detached: request.detached,
    runDir: directory, worktree: request.root,
    ...(echo === "missing" ? {} : { requestMessageId: echo === "match" ? request.requestMessageId : echo }),
  } });
  const fixture = (callback) => repository(async (root) => {
    // Full-mode probes run fixture Node executables, never a Rust toolchain.
    const bin = join(root, "target", "bin");
    mkdirSync(bin, { recursive: true });
    nodeFixtureExecutable(join(bin, process.platform === "win32" ? "cargo.exe" : "cargo"));
    for (const file of ["fmt", "clippy", "mailbox"]) writeFileSync(join(root, file), "process.exitCode = 0;\n");
    git(["add", "--", "fmt", "clippy", "mailbox"], root);
    const marker = join(root, ".git", "selected-stage");
    const marking = stage("once", `require('node:fs').appendFileSync(${JSON.stringify(marker)}, 'x')`);
    const fixtureEnv = withPrependedPath(env, bin);
    await callback({ root, marker, marking, fixtureEnv });
  });
  const host = async (fixtureEnv, callback) => {
    const records = new Map(), errors = [];
    const state = { echo: "match", source: "index", gets: 0 };
    const server = createServer(async (incoming, outgoing) => {
      try {
        if (incoming.method === "POST") {
          const chunks = [];
          for await (const chunk of incoming) chunks.push(chunk);
          const body = JSON.parse(Buffer.concat(chunks));
          const request = json(join(body.runDirectory, "request.json"));
          assert.deepEqual(body, { runId: request.runId, runDirectory: body.runDirectory,
            ...(request.requestMessageId === undefined ? {} : { requestMessageId: request.requestMessageId }) });
          records.set(request.runId, { request, directory: body.runDirectory });
          outgoing.end(JSON.stringify(ack(request)));
        } else {
          state.gets++;
          const record = records.get(incoming.url.split("/").at(-1));
          assert.ok(record);
          outgoing.end(JSON.stringify(view(record.request, record.directory, state.source, state.echo)));
        }
      } catch (error) { errors.push(error); outgoing.statusCode = 500; outgoing.end("{}"); }
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    try {
      await callback({ state, hostEnv: { ...fixtureEnv, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` } });
      assert.deepEqual(errors, []);
    } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
  };

  test("request-message fixture prepends one PATH while preserving native Path entries", () => {
    const directory = join("fixture", "bin"), inherited = join("inherited", "bin");
    for (const key of ["Path", "PATH", "path"]) {
      const original = { [key]: inherited, OTHER: "retained" };
      const prepared = withPrependedPath(original, directory);
      assert.deepEqual(prepared, { OTHER: "retained", PATH: `${directory}${delimiter}${inherited}` });
      assert.deepEqual(original, { [key]: inherited, OTHER: "retained" });
    }
  });

  for (const source of ["index", "registration"]) test(`request-message selection confirms ${source} before foreground execution`, async () => {
    await fixture(async ({ root, marker, marking, fixtureEnv }) => host(fixtureEnv, async ({ state, hostEnv }) => {
      state.source = source;
      const selected = native();
      const runDir = await createRun({ root, full: true, requestMessageId: selected, stages: [marking] }, hostEnv);
      assert.equal(json(join(runDir, "completion.request.json")).requestMessageId, selected);
      assert.equal(state.gets, 1);
      const result = await executeRun(runDir, hostEnv);
      assert.equal(result.state, "passed", result.error);
      assert.equal(result.exitCode, 0);
      assert.equal(state.gets, 2, "the foreground executor rechecks the exact existing GET");
      assert.equal(result.preflight.length, 3);
      assert.equal(readFileSync(marker, "utf8"), "x");
      assert.equal(result.requestMessageId, selected);
    }));
  });

  for (const echo of ["missing", "different"]) test(`request-message selection refuses ${echo} launch echo despite a valid POST acknowledgement`, async () => {
    await fixture(async ({ root, marker, marking, fixtureEnv }) => host(fixtureEnv, async ({ state, hostEnv }) => {
      state.echo = echo;
      const runDir = await createRun({ root, full: true, requestMessageId: native(), stages: [marking] }, hostEnv);
      const result = await executeRun(runDir, hostEnv);
      assert.equal(result.state, "failed");
      assert.equal(result.reason, "request-selection-unconfirmed");
      assert.equal(result.registration.reason, "query-identity-mismatch");
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(join(runDir, "input.json")), false);
      assert.equal(existsSync(marker), false);
      assert.match(await summarize(runDir), /reason: request-selection-unconfirmed/u);
    }));
  });

  for (const echo of ["missing", "different"]) test(`request-message selection refuses ${echo} worker echo before any probe or stage`, async () => {
    await fixture(async ({ root, marker, marking, fixtureEnv }) => host(fixtureEnv, async ({ state, hostEnv }) => {
      const runDir = await createRun({ root, full: true, requestMessageId: native(), stages: [marking] }, hostEnv);
      state.echo = echo;
      const result = await executeRun(runDir, hostEnv);
      assert.equal(result.state, "failed");
      assert.equal(result.reason, "request-selection-unconfirmed");
      assert.equal(result.registration.reason, "query-identity-mismatch");
      assert.equal(result.preflight.length, 0);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
      assert.equal(state.gets, 2);
    }));
  });

  for (const change of ["replace", "remove", "remove-request-and-result", "add", "missing-snapshot", "damaged-snapshot"]) test(`request-message selection refuses ${change} at worker admission`, async () => {
    await fixture(async ({ root, marker, marking, fixtureEnv }) => host(fixtureEnv, async ({ state, hostEnv }) => {
      const runDir = await createRun({ root, full: true, stages: [marking],
        ...(change === "add" ? {} : { requestMessageId: native() }) }, hostEnv);
      const request = json(join(runDir, "request.json"));
      if (change === "remove" || change === "remove-request-and-result") delete request.requestMessageId;
      else if (change === "replace" || change === "add") request.requestMessageId = native();
      if (change === "remove-request-and-result") {
        const result = json(join(runDir, "results.json"));
        delete result.requestMessageId;
        writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
      }
      if (change === "missing-snapshot") unlinkSync(join(runDir, "completion.request.json"));
      else if (change === "damaged-snapshot") writeFileSync(join(runDir, "completion.request.json"), "{damaged");
      writeFileSync(join(runDir, "request.json"), JSON.stringify(request));
      const result = await executeRun(runDir, hostEnv);
      assert.equal(result.state, "failed");
      assert.equal(result.reason, "request-selection-changed");
      assert.equal(result.preflight.length, 0);
      assert.equal(result.stages[0].state, "unrun");
      assert.equal(existsSync(marker), false);
      assert.equal(state.gets, 1);
    }));
  });

  for (const echo of ["match", "missing", "different"]) test(`request-message actual detached worker handles ${echo} confirmation without fallback`, { timeout: 120_000 }, async () => {
    await fixture(async ({ root, marker, marking, fixtureEnv }) => host(fixtureEnv, async ({ state, hostEnv }) => {
      const runDir = await createRun({ root, full: true, detached: true, notifyTo: "fixture-parent",
        requestMessageId: relay(), stages: [marking] }, hostEnv);
      state.echo = echo;
      if (echo === "match") {
        const { child, completion } = await startDetached(runDir, hostEnv, () => {});
        child.ref();
        assert.equal((await completion).code, 0, readFileSync(join(runDir, "launcher.log"), "utf8"));
        assert.equal(readFileSync(marker, "utf8"), "x");
      } else {
        await assert.rejects(startDetached(runDir, hostEnv, () => {}), (error) => error.safeCompletionText === "request-selection-unconfirmed");
        assert.equal(existsSync(marker), false);
        assert.equal(json(join(runDir, "results.json")).stages[0].state, "unrun");
      }
      assert.equal(state.gets, 2);
      assert.equal(existsSync(join(runDir, "execution.lock")), true);
      assert.equal(json(join(runDir, "notification.json")).ok, true);
      await assert.rejects(executeRun(runDir, hostEnv), /EEXIST/u);
    }));
  });

  test("request-message identity is conditional and detects mutation during registration", async () => {
    await fixture(async ({ root, marking, fixtureEnv }) => host(fixtureEnv, async ({ hostEnv }) => {
      const runDir = await createRun({ root, full: true, stages: [marking] }, hostEnv);
      const request = json(join(runDir, "request.json"));
      const fields = ["runId", "root", "stages", "notifyTo", "full", "scope", "detached", "creatorPid", "owner", "started",
        "requiredBinaryEnv", "prerequisiteFiles", "needsCargo", "liveEngram"];
      const ordered = (value) => Array.isArray(value) ? value.map(ordered) : value && typeof value === "object"
        ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, ordered(value[key])])) : value;
      const legacy = createHash("sha256").update(JSON.stringify(ordered(Object.fromEntries(fields.map((key) => [key, request[key] ?? null]))))).digest("hex");
      assert.equal(request.registration.requestIdentity, legacy);
      assert.equal(Object.hasOwn(request, "requestMessageId"), false);
      assert.equal(Object.hasOwn(json(join(runDir, "completion.request.json")), "requestMessageId"), false);
      request.requestMessageId = native();
      const fetch = async (_url, options) => new Response(JSON.stringify(options.method === "POST"
        ? ack(request) : view(request, runDir, "index", "match")));
      const selected = await registerTestRun(request, runDir, hostEnv, { fetch });
      assert.equal(selected.state, "registered");
      assert.notEqual(selected.requestIdentity, legacy);
      let calls = 0;
      const mutating = async (...args) => {
        const response = await fetch(...args);
        if (++calls === 1) delete request.requestMessageId;
        return response;
      };
      const changed = await registerTestRun(request, runDir, hostEnv, { fetch: mutating });
      assert.equal(changed.state, "unconfirmed");
      assert.equal(changed.reason, "request-changed");
    }));
  });

  test("request-message CLI accepts native and relay spellings but refuses standalone selection", async () => {
    await fixture(async ({ root, marker }) => {
      const local = copyLauncher(root);
      const standalone = Object.fromEntries(Object.entries(env).filter(([key]) => !key.startsWith("TERMAL_")));
      for (const selected of [native(), relay()]) {
        const result = spawnSync(process.execPath, [local, "full", "--request-message-id", selected],
          { cwd: root, env: standalone, encoding: "utf8", windowsHide: true });
        assert.equal(result.status, 1, result.stderr);
        assert.match(result.stdout, /reason: request-selection-unconfirmed/u);
        const directory = /^results: (.+)[\\/]results.json$/mu.exec(result.stdout)?.[1];
        assert.ok(directory, result.stdout);
        assert.equal(json(join(directory, "request.json")).requestMessageId, selected);
        assert.equal(json(join(directory, "results.json")).registration.reason, "standalone");
        assert.equal(json(join(directory, "results.json")).stages.every((entry) => entry.state === "unrun"), true);
      }
      assert.equal(existsSync(marker), false);
    });
  });

  test("request-message rejects focused, malformed, missing and duplicate selectors before allocating a run", async () => {
    await fixture(async ({ root, marking, fixtureEnv }) => {
      await assert.rejects(createRun({ root, requestMessageId: native(), stages: [marking] }, fixtureEnv), /request-selection-full-only/u);
      for (const selected of ["", "bare-uuid", "mailbox-123", "relay-message-", "relay-message-bad=", "relay-message-" + "a".repeat(1025), null,
        "MAILBOX-MESSAGE-" + randomUUID(), "mailbox-message-ABCDEFAB-CDEF-ABCD-EFAB-CDEFABCDEFAB", "Relay-Message-a",
        "mailbox-meſſage-" + randomUUID(), "relay-message-ſ", "relay-message-K"]) {
        await assert.rejects(createRun({ root, full: true, requestMessageId: selected, stages: [marking] }, fixtureEnv), /invalid-request-message-id/u);
      }
      const local = copyLauncher(root), selected = native();
      for (const args of [["full", "--request-message-id"], ["full", "--request-message-id", "bad"],
        ["full", "--request-message-id", selected, "--request-message-id", selected],
        ["focused", "--request-message-id", selected, "--", process.execPath, "-e", ""]]) {
        const result = spawnSync(process.execPath, [local, ...args], { cwd: root, env, encoding: "utf8", windowsHide: true });
        assert.equal(result.status, 1, result.stderr);
        assert.doesNotMatch(result.stdout, /^results:/mu);
      }
    });
  });
}
