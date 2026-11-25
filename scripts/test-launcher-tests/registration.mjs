import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { join } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";
import { hostTransport, registerTestRun, confirmDetachedRegistration, registrationLine } from "../test-run-registration.mjs";
import { asOneOperation, namedIfSlow, warningSink, warnWhileRunning } from "../operation-warning.mjs";
import { createRun, executeRun, notifyRun, runStage, summarize } from "../test-launcher.mjs";

export function registerRegistrationTests(support) {
  const { env, stage, json, git, repository } = support;
const registrationAck = (request) => ({ contract: "termal-durable-test-run-registration", version: 1,
  runId: request.runId, registered: true });
const registrationQuery = (request, runDir, source = "index") => ({ source, run: {
  runId: request.runId, ownerSessionId: request.owner, initiatingSessionId: request.owner,
  registrationState: "registered", trackingDurability: "durable", trackingProvenances: ["launcherRegistered"],
  creatorPid: request.creatorPid, startedAt: request.started, detached: request.detached,
  runDir, worktree: request.root,
} });
const response = (body, status = 200) => new Response(JSON.stringify(body), { status });
const registrationEnv = { ...env, TERMAL_BASE_URL: "http://127.0.0.1:1" };

test("registration confirms the canonical paths of a mixed-case checkout", async () => {
  await repository(async (root) => {
    const checkout = join(root, "UpperCaseCheckout");
    mkdirSync(checkout);
    git(["init", "--quiet", "--template="], checkout);
    git(["config", "core.autocrlf", "false"], checkout);
    writeFileSync(join(checkout, "tracked.txt"), "baseline\n");
    git(["add", "tracked.txt"], checkout);
    git(["-c", "user.name=Engram Test", "-c", "user.email=test@example.invalid",
      "commit", "--quiet", "--no-gpg-sign", "-m", "fixture baseline"], checkout);
    const canonicalRoot = realpathSync.native(checkout);
    assert.ok(canonicalRoot.endsWith("UpperCaseCheckout"));
    let request, directory;
    const fetch = async (_url, options) => {
      if (options.method === "POST") {
        directory = JSON.parse(options.body).runDirectory;
        request = json(join(directory, "request.json"));
        return response(registrationAck(request));
      }
      const query = registrationQuery(request, realpathSync.native(directory));
      query.run.worktree = canonicalRoot;
      return response(query);
    };
    const runDir = await createRun({ root: checkout, detached: true, stages: [stage("once")] },
      registrationEnv, { fetch });
    request = json(join(runDir, "request.json"));
    assert.equal(request.registration.state, "registered");
    assert.equal(request.registration.nativeWaitReady, true);
    const confirmed = await confirmDetachedRegistration(request, runDir, registrationEnv, { fetch });
    assert.equal(confirmed.state, "registered");
    assert.equal(confirmed.nativeWaitReady, true);
  });
});

test("registration distinguishes real case-sensitive roots and run directories", async (t) => {
  await repository(async (root) => {
    const parent = join(root, "case-sensitive");
    mkdirSync(parent);
    if (process.platform === "win32") {
      const fsutil = (action) => spawnSync("fsutil.exe", ["file", action, parent,
        ...(action === "setCaseSensitiveInfo" ? ["enable"] : [])], {
        encoding: "utf8", windowsHide: true, timeout: 60_000, maxBuffer: 4_096,
      });
      for (const action of ["setCaseSensitiveInfo", "queryCaseSensitiveInfo"]) {
        const result = fsutil(action);
        // A hang fails: only fsutil being unavailable may drop this coverage.
        assert.notEqual(result.error?.code, "ETIMEDOUT", `fsutil ${action} hung`);
        if (result.error || result.status !== 0) {
          const detail = String(result.error?.message ?? [result.stdout, result.stderr].filter(Boolean).join(" "))
            .trim().slice(0, 512);
          t.skip(`case-sensitive Windows directory unavailable: ${action}: ${detail || result.status}`);
          return;
        }
      }
    }
    const worktree = join(parent, "RegistrationRoot"), otherWorktree = join(parent, "registrationroot");
    const directory = join(parent, "RunDirectory"), otherDirectory = join(parent, "rundirectory");
    mkdirSync(worktree);
    try { mkdirSync(otherWorktree); }
    catch (error) {
      if (error.code !== "EEXIST") throw error;
      t.skip(`case-sensitive filesystem unavailable on ${process.platform}: case-only sibling already exists`);
      return;
    }
    mkdirSync(directory);
    mkdirSync(otherDirectory);
    for (const [path, marker] of [[worktree, "worktree"], [otherWorktree, "other-worktree"],
      [directory, "run"], [otherDirectory, "other-run"]]) {
      writeFileSync(join(path, "marker"), marker);
      assert.equal(readFileSync(join(path, "marker"), "utf8"), marker);
    }
    assert.deepEqual(readdirSync(parent).sort(), ["RegistrationRoot", "RunDirectory", "registrationroot", "rundirectory"]);
    assert.notEqual(realpathSync.native(worktree), realpathSync.native(otherWorktree));
    assert.notEqual(realpathSync.native(directory), realpathSync.native(otherDirectory));
    assert.equal(readFileSync(join(worktree, "marker"), "utf8"), "worktree");
    assert.equal(readFileSync(join(directory, "marker"), "utf8"), "run");
    const request = { runId: randomUUID(), root: worktree, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    const fetch = (change = {}) => async (_url, options) => {
      const query = registrationQuery(request, realpathSync.native(directory));
      query.run.worktree = realpathSync.native(worktree);
      Object.assign(query.run, change);
      return response(options.method === "POST" ? registrationAck(request) : query);
    };
    const wrongRoot = await registerTestRun(request, directory, registrationEnv,
      { fetch: fetch({ worktree: realpathSync.native(otherWorktree) }) });
    assert.equal(wrongRoot.state, "unconfirmed");
    assert.equal(wrongRoot.reason, "query-identity-mismatch");
    assert.equal(wrongRoot.nativeWaitReady, false);
    request.registration = await registerTestRun(request, directory, registrationEnv, { fetch: fetch() });
    assert.equal(request.registration.state, "registered");
    const wrongDirectory = await confirmDetachedRegistration(request, directory, registrationEnv,
      { fetch: fetch({ runDir: realpathSync.native(otherDirectory) }) });
    assert.equal(wrongDirectory.state, "unconfirmed");
    assert.equal(wrongDirectory.reason, "query-identity-mismatch");
    assert.equal(wrongDirectory.nativeWaitReady, false);
  });
});

test("registration covers all launch modes before source identity capture through the supported HTTP contract", async () => {
  await repository(async (root, observe) => {
    const requests = new Map(), calls = [], errors = [];
    const server = createServer(async (incoming, outgoing) => {
      try {
        calls.push([incoming.method, incoming.url]);
        let body;
        if (incoming.method === "POST") {
          const chunks = [];
          for await (const chunk of incoming) chunks.push(chunk);
          body = JSON.parse(Buffer.concat(chunks));
          assert.deepEqual(Object.keys(body).sort(), ["runDirectory", "runId"]);
          const request = json(join(body.runDirectory, "request.json"));
          assert.equal(request.runId, body.runId);
          assert.equal(request.creatorPid, process.pid);
          assert.equal(request.owner, env.TERMAL_SESSION_ID);
          assert.equal(existsSync(join(body.runDirectory, "input.json")), false);
          assert.deepEqual(request.sourceId, undefined);
          // The capture must include a source change made during registration.
          writeFileSync(join(root, "tracked.txt"), `${request.runId}\n`);
          requests.set(request.runId, { request, directory: body.runDirectory });
          outgoing.end(JSON.stringify(registrationAck(request)));
        } else {
          const found = requests.get(incoming.url.split("/").at(-1));
          assert.ok(found);
          outgoing.end(JSON.stringify(registrationQuery(found.request, found.directory)));
        }
      } catch (error) { errors.push(error); outgoing.statusCode = 500; outgoing.end("{}"); }
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const hostEnv = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
    try {
      for (const full of [false, true]) for (const detached of [false, true]) {
        const runDir = await createRun({ root, full, detached, notifyTo: "fixture-parent",
          stages: [stage("once", "console.log('ok 1 - registration fixture')")] }, hostEnv);
        const request = json(join(runDir, "request.json"));
        assert.equal(request.registration.state, "registered");
        assert.equal(request.registration.nativeWaitReady, true);
        assert.equal(json(join(runDir, "results.json")).registration.state, "registered");
        assert.deepEqual(json(join(runDir, "input.json")).sourceId, request.sourceId);
        assert.equal(calls.at(-2)[1], `/api/sessions/${env.TERMAL_SESSION_ID}/test-runs`);
        assert.equal(calls.at(-1)[1], `/api/test-runs/${request.runId}`);
        if (!full) {
          const before = calls.length;
          observe(runDir, env.ENGRAM_SOURCE_BINARY);
          const result = await executeRun(runDir, hostEnv);
          assert.equal(result.state, "passed");
          assert.deepEqual(result.sourceIdBefore, request.sourceId);
          assert.deepEqual(result.sourceIdAfter, result.sourceIdBefore);
          assert.equal(calls.length, before + (detached ? 1 : 0));
          assert.match(await summarize(runDir), /registration: registered; native-wait=ready; registration succeeded; host check credit decided at delivery/u);
        }
      }
      assert.deepEqual(errors, []);
    } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
  });
});

test("registration fallback preserves stage execution, terminal results and notification retry intent", async () => {
  await repository(async (root) => {
    const attempts = [];
    const runDir = await createRun({ root, detached: true, notifyTo: "fixture-parent",
      stages: [stage("once", "require('node:fs').appendFileSync('.git/registration-count', 'once')")] },
    registrationEnv, { fetch: async (_url, options) => {
      attempts.push(options.body);
      throw Object.assign(new Error("lost response"), { cause: { code: "EPIPE" } });
    } });
    assert.equal(attempts.length, 2);
    assert.equal(attempts[0], attempts[1]);
    const result = await executeRun(runDir, registrationEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.registration.state, "unconfirmed");
    assert.equal(readFileSync(join(root, ".git", "registration-count"), "utf8"), "once");
    const summary = await summarize(runDir);
    assert.match(summary, /registration: unconfirmed; native-wait=unavailable; use --notify/u);
    const sends = [];
    const send = async (_command, args) => {
      sends.push({ key: args[args.indexOf("--idempotency-key") + 1],
        message: readFileSync(args[args.indexOf("--message-file") + 1], "utf8") });
      return { code: sends.length === 1 ? 1 : 0 };
    };
    await assert.rejects(notifyRun(runDir, registrationEnv, send));
    await notifyRun(runDir, registrationEnv, send);
    assert.equal(sends.length, 2);
    assert.deepEqual(sends[0], sends[1]);
    assert.equal(sends[0].message, summary);
    assert.equal(readFileSync(join(root, ".git", "registration-count"), "utf8"), "once");
  });
});

test("registration refuses malformed or unavailable context locally without a host call", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner" };
    const forbidden = { fetch: () => assert.fail("no host call for unavailable context") };
    assert.equal((await registerTestRun(request, root, {}, forbidden)).state, "untracked");
    for (const context of [{ TERMAL_SESSION_ID: "fixture-owner" }, { ...registrationEnv, TERMAL_BASE_URL: "file:///x" },
      { ...registrationEnv, TERMAL_BASE_URL: "http://user:secret@localhost" },
      { ...registrationEnv, TERMAL_SESSION_ID: "wrong-owner" }]) {
      assert.equal((await registerTestRun(request, root, context, forbidden)).state, "unconfirmed");
    }
  });
});

test("registration never replays a definitive redirect or a refused connection and preserves fallback execution", async () => {
  await repository(async (root) => {
    const calls = [];
    const server = createServer((request, reply) => {
      calls.push([request.method, request.url]);
      reply.writeHead(307, { location: "/unexpected-target" });
      reply.end();
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const hostEnv = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
    try {
      const runDir = await createRun({ root, stages: [stage("once",
        "require('node:fs').appendFileSync('.git/redirect-count', 'once')")] }, hostEnv);
      const request = json(join(runDir, "request.json"));
      assert.deepEqual(calls, [["POST", `/api/sessions/${env.TERMAL_SESSION_ID}/test-runs`]]);
      assert.equal(request.registration.state, "unconfirmed");
      assert.equal(request.registration.reason, "registration-refused");
      assert.equal((await executeRun(runDir, hostEnv)).state, "passed");
      assert.equal(readFileSync(join(root, ".git", "redirect-count"), "utf8"), "once");
      let refusedCalls = 0;
      const result = await registerTestRun(request, runDir, hostEnv, { fetch: async () => {
        refusedCalls++;
        throw Object.assign(new Error("connection refused"), { cause: { code: "ECONNREFUSED" } });
      } });
      assert.equal(refusedCalls, 1);
      assert.equal(result.state, "unconfirmed");
    } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
  });
});

test("registration and detached confirmation keep short-cadence heartbeats fresh", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setInterval"], now: Date.now() });
  await repository(async (root) => {
    const hostEnv = { ...registrationEnv, ENGRAM_LAUNCHER_HEARTBEAT_MS: "1000" };
    let request, directory;
    const fetch = async (_url, options) => {
      if (options.method === "POST") {
        directory = JSON.parse(options.body).runDirectory;
        request = json(join(directory, "request.json"));
        const before = Date.parse(json(join(directory, "results.json")).heartbeat.at);
        t.mock.timers.tick(4000);
        assert.equal(Date.parse(json(join(directory, "results.json")).heartbeat.at), before + 4000);
        assert.match(await summarize(directory), /^RUNNING /u);
        return response(registrationAck(request));
      }
      return response(registrationQuery(request, directory));
    };
    const runDir = await createRun({ root, detached: true, notifyTo: "fixture-parent", stages: [stage("once")] }, hostEnv, { fetch });
    t.mock.method(hostTransport, "send", async () => {
      const before = Date.parse(json(join(runDir, "results.json")).heartbeat.at);
      t.mock.timers.tick(4000);
      assert.equal(Date.parse(json(join(runDir, "results.json")).heartbeat.at), before + 4000);
      assert.match(await summarize(runDir), /^RUNNING /u);
      return response(registrationQuery(request, runDir));
    });
    const result = await executeRun(runDir, hostEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.registration.state, "registered");
    const terminal = readFileSync(join(runDir, "results.json"));
    t.mock.timers.tick(4000);
    assert.deepEqual(readFileSync(join(runDir, "results.json")), terminal, "the timer stops before terminal completion");
  });
});

test("registration distinguishes durable registration from indexing and keeps ambiguous fallback sticky", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    const fetch = async (_url, options) => response(options.method === "POST"
      ? registrationAck(request) : registrationQuery(request, root, "registration"));
    request.registration = await registerTestRun(request, root, registrationEnv, { fetch });
    assert.equal(request.registration.state, "registered-but-not-indexed");
    assert.equal(request.registration.nativeWaitReady, false);
    assert.equal(registrationLine(request.registration), "registration: registered-but-not-indexed; native-wait=unavailable; use --notify; registration succeeded; host check credit decided at delivery");
    let savedDirectory;
    const runDir = await createRun({ root, stages: [stage("once")] }, registrationEnv,
      { fetch: async (_url, options) => {
        if (options.method === "POST") savedDirectory = JSON.parse(options.body).runDirectory;
        const saved = json(join(savedDirectory, "request.json"));
        return response(options.method === "POST" ? registrationAck(saved) : registrationQuery(saved, savedDirectory, "registration"));
      } });
    assert.match(await summarize(runDir), /registration: registered-but-not-indexed; native-wait=unavailable; use --notify; registration succeeded; host check credit decided at delivery/u);
    const indexed = async () => response(registrationQuery(request, root));
    assert.equal((await confirmDetachedRegistration(request, root, registrationEnv, { fetch: indexed })).nativeWaitReady, false);
    request.registration = { state: "unconfirmed", nativeWaitReady: false };
    assert.deepEqual(await confirmDetachedRegistration(request, root, registrationEnv,
      { fetch: () => assert.fail("ambiguous registration cannot be promoted by a late query") }), request.registration);
  });
});

test("registration wording preserves untracked and ordinary unconfirmed messages", () => {
  for (const state of ["untracked", "unconfirmed"]) {
    assert.equal(registrationLine({ state }), `registration: ${state}; native-wait=unavailable; use --notify; not host check credit`);
  }
});

test("registration preserves bounded POST refusal diagnostics in metadata and summary", async () => {
  await repository(async (root) => {
    let calls = 0;
    const runDir = await createRun({ root, stages: [stage("once")] }, registrationEnv,
      { fetch: async (_url, options) => {
        calls++;
        assert.equal(options.method, "POST");
        return response({ error: "Registration does not match the current work root", secret: "never-print-this-secret" }, 409);
      } });
    const registration = json(join(runDir, "request.json")).registration;
    assert.equal(calls, 1);
    assert.equal(registration.state, "unconfirmed");
    assert.equal(registration.reason, "registration-refused");
    assert.equal(registration.nativeWaitReady, false);
    assert.equal(registration.httpStatus, 409);
    assert.equal(registration.hostMessage, "Registration does not match the current work root");
    assert.deepEqual(json(join(runDir, "results.json")).registration, registration);
    assert.match(await summarize(runDir), /POST HTTP 409; host message="Registration does not match the current work root"/u);
    assert.doesNotMatch(JSON.stringify(registration) + await summarize(runDir), /never-print-this-secret/u);
    assert.equal((await executeRun(runDir, registrationEnv)).state, "passed");
    assert.deepEqual(json(join(runDir, "results.json")).registration, registration);
  });
});

test("registration refusal bodies are bounded and never dumped as diagnostics", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner" };
    for (const reply of [() => new Response(null, { status: 403 }),
      () => new Response("private raw body", { status: 403 }),
      () => response({ error: { secret: "private" }, message: "private" }, 403),
      () => response({ error: "x".repeat(65_537) }, 403)]) {
      let calls = 0;
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => { calls++; return reply(); } });
      assert.deepEqual(result, { state: "unconfirmed", reason: "registration-refused", nativeWaitReady: false,
        httpStatus: 403, hostMessage: null });
      assert.equal(calls, 1);
      assert.match(registrationLine(result), /POST HTTP 403; host message unavailable$/u);
      assert.doesNotMatch(registrationLine(result), /private/u);
    }
    const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => response({
      error: `\u001b[31mfirst\nsecond\u2028${"z".repeat(500)}`, secret: "never-print-this-secret",
      registrationState: "durabilityPending", retryAfterMs: 1,
    }, 503) });
    assert.equal(result.reason, "registration-refused");
    assert.equal(result.httpStatus, 503);
    assert.equal(result.hostMessage.length, 384);
    assert.match(result.hostMessage, /^\[31mfirst second /u);
    assert.doesNotMatch(result.hostMessage, /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u);
    assert.doesNotMatch(registrationLine(result), /never-print-this-secret/u);
    const persisted = { ...result, hostMessage: `\u001b${"y".repeat(800)}` };
    assert.ok(registrationLine(persisted).length < 600);
    assert.doesNotMatch(registrationLine(persisted), /\u001b/u);
    assert.doesNotMatch(registrationLine({ ...result, httpStatus: "503\nprivate" }), /POST HTTP/u);
  });
});

test("registration retains refusal status when its body exhausts the shared budget", async () => {
  await repository(async (root) => {
    let signal, calls = 0;
    const result = await registerTestRun({ runId: randomUUID(), root, owner: "fixture-owner" }, root, registrationEnv,
      { budgetMs: 10, fetch: async (_url, options) => {
        calls++;
        signal = options.signal;
        return new Response(new ReadableStream({ start() {} }), { status: 503 });
      } });
    assert.equal(calls, 1);
    assert.equal(signal.aborted, true);
    assert.deepEqual(result, { state: "unconfirmed", reason: "registration-timeout", nativeWaitReady: false,
      httpStatus: 503, hostMessage: null });
    assert.match(registrationLine(result), /POST HTTP 503; host message unavailable$/u);
  });
});

// A host that answers only after ten seconds is still waited for, and the wait
// says so once on stderr. The test owns the timers; no real ten seconds pass.
test("a registration still unanswered after ten seconds warns once and still registers", async (t) => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const written = [];
    t.mock.method(warningSink, "write", (text) => { written.push(String(text)); });
    let answer;
    const answered = new Promise((done) => { answer = done; });
    const pending = registerTestRun(request, root, registrationEnv, { fetch: async (_url, options) => {
      if (options.method !== "POST") return response(registrationQuery(request, root));
      await answered;
      return response(registrationAck(request));
    } });
    t.mock.timers.tick(10_001);
    answer();
    const result = await pending;
    t.mock.timers.reset();
    assert.equal(result.state, "registered");
    assert.deepEqual(written.filter((line) => line.startsWith("warning: ")).map((line) => line.split(";")[0]),
      ["warning: TermAl test-run registration is still running after 10 seconds"]);
  });
});

// Registration and a detached worker's confirmation share one budget and one
// warning. A registration that itself warned leaves its slow confirmation
// silent; after eight seconds spent registering, the confirmation's third
// second is the handshake's eleventh and warns. The test owns the timers.
test("a detached confirmation continues the registration's one warning", async (t) => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    const answer = (options) => response(options.method === "POST" ? registrationAck(request) : registrationQuery(request, root));
    const written = [];
    t.mock.method(warningSink, "write", (text) => { written.push(text); });
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const held = () => { let release; const until = new Promise((done) => { release = done; }); return { until, release }; };

    // A slow registration warns once; its slow confirmation adds nothing.
    const posting = held();
    const registering = registerTestRun(request, root, registrationEnv, { fetch: async (_url, options) => {
      if (options.method === "POST") await posting.until;
      return answer(options);
    } });
    t.mock.timers.tick(10_001);
    posting.release();
    const warnedRegistration = await registering;
    assert.equal(warnedRegistration.state, "registered");
    const querying = held();
    const confirmingWarned = confirmDetachedRegistration({ ...request, registration: warnedRegistration }, root, registrationEnv, {
      fetch: async () => { await querying.until; return answer({}); } });
    t.mock.timers.tick(20_000);
    querying.release();
    assert.equal((await confirmingWarned).state, "registered");
    assert.equal(written.length, 1, written.join(""));

    // A fast registration that spent eight seconds: the confirmation warns.
    written.length = 0;
    const quick = await registerTestRun(request, root, registrationEnv, { fetch: async (_url, options) => answer(options) });
    assert.equal(written.length, 0);
    const late = held();
    const confirming = confirmDetachedRegistration({ ...request, registration: { ...quick, remainingMs: 52_000 } }, root,
      registrationEnv, { fetch: async () => { await late.until; return answer({}); } });
    t.mock.timers.tick(2_001);
    late.release();
    assert.equal((await confirming).state, "registered");
    assert.equal(written.length, 1, written.join(""));
    t.mock.timers.reset();
  });
});

// A synchronous call can hold the timer back past its time: settling then
// writes the one warning, and an operation that settles in time writes none.
test("an operation settling past ten seconds before its timer ran is named as it settles", (t) => {
  const written = [];
  t.mock.method(warningSink, "write", (text) => { written.push(text); });
  let clock = 0;
  const late = warnWhileRunning("fixture operation", { now: () => clock });
  clock = 11_000;
  assert.equal(late(), true);
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: fixture operation is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=11000\n$/u);
  written.length = 0;
  clock = 0;
  const quick = warnWhileRunning("fixture operation", { now: () => clock });
  clock = 9_000;
  assert.equal(quick(), false);
  assert.equal(written.length, 0);
  // Its first settling is final: settling again later, past ten seconds,
  // writes nothing for time spent after the operation ended.
  clock = 30_000;
  assert.equal(quick(), false);
  assert.equal(written.length, 0);
});

// A synchronous step such as a file call is named once as it returns when it
// took ten seconds or more, also when it throws; a quick one is never named.
test("a slow synchronous step is named once as it returns, and its result is unchanged", (t) => {
  const written = [];
  t.mock.method(warningSink, "write", (text) => { written.push(text); });
  let clock = 0;
  const slow = () => { clock += 11_000; return "read"; };
  assert.equal(namedIfSlow("fixture read", slow, { now: () => clock }), "read");
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: fixture read is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=11000\n$/u);
  written.length = 0;
  const failing = () => { clock += 12_000; throw new Error("refused"); };
  assert.throws(() => namedIfSlow("fixture write", failing, { now: () => clock }), /refused/u);
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: fixture write is still running after 10 seconds;.*elapsed_ms=12000\n$/u);
  written.length = 0;
  assert.equal(namedIfSlow("fixture read", () => { clock += 9_000; return 1; }, { now: () => clock }), 1);
  assert.equal(written.length, 0);
});

// One operation is named once: a slow step inside it is covered by the
// operation's own warning and does not name itself as well. Outside any
// operation the same step names itself.
test("a slow step inside one operation is named once, by the operation", async (t) => {
  const written = [];
  t.mock.method(warningSink, "write", (text) => { written.push(text); });
  let clock = 0;
  const now = () => clock;
  const step = () => namedIfSlow("fixture step", () => { clock += 11_000; return "read"; }, { now });
  assert.equal(await asOneOperation("fixture operation", step, { now }), "read");
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: fixture operation is still running after 10 seconds;.*elapsed_ms=11000\n$/u);
  written.length = 0;
  assert.equal(step(), "read");
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: fixture step is still running after 10 seconds;/u);
});

// A stage still running after ten seconds is named once, as a stage, and is
// never stopped for its time: its outcome and its failure pass through, and a
// quick stage says nothing.
test("a long stage is named once and runs to its own end", async (t) => {
  const written = [];
  t.mock.method(warningSink, "write", (text) => { written.push(text); });
  let clock = 0;
  const now = () => clock;
  const long = async () => { clock += 45_000; return { code: 0 }; };
  assert.deepEqual(await runStage(long, { now }), { code: 0 });
  assert.equal(written.length, 1, written.join(""));
  assert.match(written[0], /^warning: stage run is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=45000\n$/u);
  written.length = 0;
  await assert.rejects(runStage(() => { clock += 12_000; throw new Error("stage failed"); }, { now }), /stage failed/u);
  assert.equal(written.length, 1);
  assert.match(written[0], /^warning: stage run is still running after 10 seconds;/u);
  written.length = 0;
  assert.deepEqual(await runStage(() => { clock += 2_000; return { code: 1 }; }, { now }), { code: 1 });
  assert.deepEqual(written, []);
  // While the stage is still running, its warning is already out; its end
  // adds none and changes nothing about its outcome.
  let release;
  const running = runStage(() => new Promise((done) => { release = done; }), { after: 20 });
  await new Promise((done) => setTimeout(done, 200));
  assert.equal(written.length, 1, "the warning comes while the stage runs");
  assert.match(written[0], /^warning: stage run is still running after 10 seconds;/u);
  release({ code: 0 });
  assert.deepEqual(await running, { code: 0 });
  assert.equal(written.length, 1);
});

// A warning whose stderr reader has gone is contained: the launcher goes on
// rather than dying on a stream error while its operation still settles.
test("a warning to a stderr whose reader has gone never ends the process", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warnWhileRunning } = await import(${JSON.stringify(helper)});
    warnWhileRunning("fixture operation", { after: 100 });
    setTimeout(() => process.stdout.write("survived"), 600);`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  child.stderr.destroy();
  let stdout = "";
  child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
  const [code] = await once(child, "close");
  assert.equal(code, 0);
  assert.equal(stdout, "survived");
});

// A warning due while stderr is full, its reader paused, still leaves the
// launcher's own thread free: its later timers run. The child cannot exit
// while its own stderr write waits, so the test ends it once it has spoken.
test("a warning to a full stderr leaves the operation's thread running", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink, warnWhileRunning } = await import(${JSON.stringify(helper)});
    warningSink.write("x".repeat(1 << 22));
    warnWhileRunning("fixture operation", { after: 100 });
    setTimeout(() => process.stdout.write("survived"), 800);`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  // Listen for the exit from the start, so an early one fails the test.
  const exited = once(child, "exit");
  child.stderr.pause();
  try {
    const outcome = await new Promise((done) => {
      const guard = setTimeout(() => done("timers stopped behind the warning"), 30_000);
      exited.then(([code]) => { clearTimeout(guard); done(`the child exited early with ${code}`); });
      child.stdout.setEncoding("utf8").on("data", (chunk) => {
        if (chunk.includes("survived")) { clearTimeout(guard); done("survived"); }
      });
    });
    assert.equal(outcome, "survived");
  } finally {
    child.kill();
    await exited;
  }
});

// Warnings due while stderr is full never take the file-system workers the
// launcher's own work needs: after several of them, a file read still ends,
// even when the process has only one such worker.
test("warnings to a full stderr leave the launcher's file reads running", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { readFile } = await import("node:fs/promises");
    const { warningSink, warnWhileRunning } = await import(${JSON.stringify(helper)});
    warningSink.write("x".repeat(1 << 22));
    for (let index = 0; index < 6; index += 1) warnWhileRunning("fixture operation " + index, { after: 10 });
    setTimeout(async () => {
      await readFile(${JSON.stringify(fileURLToPath(new URL("../operation-warning.mjs", import.meta.url)))});
      process.stdout.write("read");
    }, 300);`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, UV_THREADPOOL_SIZE: "1" } });
  const exited = once(child, "exit");
  child.stderr.pause();
  try {
    const outcome = await new Promise((done) => {
      const guard = setTimeout(() => done("the read waited behind the warnings"), 30_000);
      exited.then(([code]) => { clearTimeout(guard); done(`the child exited early with ${code}`); });
      child.stdout.setEncoding("utf8").on("data", (chunk) => {
        if (chunk.includes("read")) { clearTimeout(guard); done("read"); }
      });
    });
    assert.equal(outcome, "read");
  } finally {
    child.kill();
    await exited;
  }
});

// Warnings that fall due together all reach a stderr that is being read, in
// the order they fell due, however many operations warn at once (a cutover
// runs one per project): one write at a time never costs a line.
test("warnings due together all reach a working stderr in order", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink } = await import(${JSON.stringify(helper)});
    for (let index = 0; index < 40; index += 1) warningSink.write("warning: " + index + "\\n");`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
  const [code] = await once(child, "close");
  assert.equal(code, 0);
  assert.equal(stderr, Array.from({ length: 40 }, (_, index) => `warning: ${index}\n`).join(""));
});

// A synchronous call that ends just past its warning time is still named once,
// even when the watcher had not yet come up to say so itself.
test("a blocking call that ends just past its warning time is still named once", () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warnWhileBlocking } = await import(${JSON.stringify(helper)});
    const { spawnSync } = await import("node:child_process");
    const blocked = warnWhileBlocking("fixture probe",
      () => spawnSync(process.execPath, ["-e", "setTimeout(() => {}, 300)"]), { after: 1 });
    setTimeout(() => process.stdout.write("returned " + blocked.status), 500);`;
  const child = spawnSync(process.execPath, ["--input-type=module", "-e", source],
    { encoding: "utf8", windowsHide: true, timeout: 60_000 });
  assert.equal(child.status, 0, child.stderr);
  assert.equal(child.stdout, "returned 0");
  const warnings = child.stderr.split(/\r?\n/u).filter(Boolean);
  assert.equal(warnings.length, 1, child.stderr);
  assert.match(warnings[0], /^warning: fixture probe is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=\d+$/u);
});

// A caller that ends its process right after a slow blocking call, as the
// POSIX group helper does after its receipt, still leaves the one warning
// when it flushes first: whether the watcher or the caller names the call,
// the line is out before the end.
test("a process that flushes before it ends right after a slow blocking call still names it once", () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warnWhileBlocking, warningSink } = await import(${JSON.stringify(helper)});
    // Shorter than the watcher thread takes to start, so the caller names it.
    warnWhileBlocking("fixture receipt", () => { const end = Date.now() + 5; while (Date.now() < end); }, { after: 1 });
    await warningSink.flush();
    process.exit(0);`;
  for (let round = 0; round < 5; round += 1) {
    const child = spawnSync(process.execPath, ["--input-type=module", "-e", source],
      { encoding: "utf8", windowsHide: true, timeout: 60_000 });
    assert.equal(child.status, 0, child.stderr);
    const warnings = child.stderr.split(/\r?\n/u).filter(Boolean);
    assert.equal(warnings.length, 1, child.stderr);
    assert.match(warnings[0], /^warning: fixture receipt is still running after 10 seconds;/u);
  }
});

// A process about to end waits for its warnings only up to the flush bound:
// with a stderr nobody reads, the flush still settles and the end goes on.
test("a flush before the end settles at its bound when stderr does not drain", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink } = await import(${JSON.stringify(helper)});
    warningSink.write("x".repeat(1 << 22));
    await warningSink.flush(100);
    process.stdout.write("ended");`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const exited = once(child, "exit");
  child.stderr.pause();
  try {
    const outcome = await new Promise((done) => {
      const guard = setTimeout(() => done("the end waited for stderr"), 30_000);
      exited.then(([code]) => { clearTimeout(guard); done(`the child exited early with ${code}`); });
      child.stdout.setEncoding("utf8").on("data", (chunk) => {
        if (chunk.includes("ended")) { clearTimeout(guard); done("ended"); }
      });
    });
    assert.equal(outcome, "ended");
  } finally {
    child.kill();
    await exited;
  }
});

// A line written from inside a synchronous loop, such as a save's rename
// retries, never holds that loop: with a stderr nobody reads, the caller goes
// on at once, and a flush before the end still settles at its bound.
test("a line written from a synchronous loop never waits for a stderr that does not drain", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink } = await import(${JSON.stringify(helper)});
    const { writeSync } = await import("node:fs");
    warningSink.write("x".repeat(1 << 22));
    writeSync(1, "went on;");
    await warningSink.flush(100);
    writeSync(1, "ended");`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const exited = once(child, "exit");
  child.stderr.pause();
  try {
    let stdout = "";
    const outcome = await new Promise((done) => {
      const guard = setTimeout(() => done(`the caller waited for stderr: ${stdout}`), 30_000);
      exited.then(([code]) => { clearTimeout(guard); done(`the child exited early with ${code}`); });
      child.stdout.setEncoding("utf8").on("data", (chunk) => {
        stdout += chunk;
        if (stdout.includes("ended")) { clearTimeout(guard); done(stdout); }
      });
    });
    assert.equal(outcome, "went on;ended");
  } finally {
    child.kill();
    await exited;
  }
});

// A line written from a synchronous loop reaches a working stderr whole, and
// a flush before the end waits for it.
test("a line written from a synchronous loop reaches a working stderr before the end", () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink } = await import(${JSON.stringify(helper)});
    warningSink.write("warning: from a loop\\n");
    const end = Date.now() + 50; while (Date.now() < end);
    await warningSink.flush();
    process.exit(0);`;
  const child = spawnSync(process.execPath, ["--input-type=module", "-e", source],
    { encoding: "utf8", windowsHide: true, timeout: 60_000 });
  assert.equal(child.status, 0, child.stderr);
  assert.equal(child.stderr, "warning: from a loop\n");
});

// An entry point whose work has ended is not held by warnings still waiting
// behind a slow write: past the bound they are dropped, and the process ends
// with its own exit code and output. What the system already took of the
// write in flight is out; where stderr is a non-blocking pipe, the rest of
// that line goes with the waiting ones. A first short line, which the pipe
// takes at once, shows the writer is running before the bound passes.
test("an ended entry point drops warnings still waiting past the bound", async () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warningSink, endWithinWarningBound } = await import(${JSON.stringify(helper)});
    process.stdout.write("result");
    warningSink.write("warning: first\\n");
    await warningSink.flush(30_000);
    warningSink.write("x".repeat(1 << 22));
    for (let index = 0; index < 3; index += 1) warningSink.write("warning: waiting " + index + "\\n");
    process.exitCode = 3;
    endWithinWarningBound(100);`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", source],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "", stderr = "";
  child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
  // Paused before the listener, so stderr is not read until it is released.
  child.stderr.setEncoding("utf8").pause();
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const exited = once(child, "exit");
  const stderrEnded = once(child.stderr, "end");
  const stdoutEnded = once(child.stdout, "end");
  // Where stderr is a non-blocking pipe, as it is outside Windows once a
  // writer thread exists, the end waits for no reader at all: stderr is read
  // only after the exit. A Windows pipe blocks, so the write in flight
  // completes as the reader takes it, released well after the bound.
  const release = process.platform === "win32" ? setTimeout(() => child.stderr.resume(), 1_000) : undefined;
  // Only to end a child that never ends by itself, failing the test.
  const guard = setTimeout(() => child.kill(), 60_000);
  try {
    const [code] = await exited;
    child.stderr.resume();
    await Promise.all([stderrEnded, stdoutEnded]);
    assert.equal(code, 3);
    assert.equal(stdout, "result");
    const handedOver = `warning: first\n${"x".repeat(1 << 22)}`;
    assert.ok(stderr.startsWith("warning: first\n"), stderr.slice(0, 80));
    assert.ok(handedOver.startsWith(stderr), "stderr holds only what was handed over, in order");
    assert.doesNotMatch(stderr, /warning: waiting/u);
  } finally {
    clearTimeout(release);
    clearTimeout(guard);
  }
});

// A synchronous call blocks the launcher's own timers, as an identity probe
// does; its one warning still arrives while the call runs, not after it.
test("a blocking call still running at its warning time is named once while it runs", () => {
  const helper = new URL("../operation-warning.mjs", import.meta.url).href;
  const source = `const { warnWhileBlocking } = await import(${JSON.stringify(helper)});
    const { spawnSync } = await import("node:child_process");
    const { writeSync } = await import("node:fs");
    const blocked = warnWhileBlocking("fixture probe",
      () => spawnSync(process.execPath, ["-e", "setTimeout(() => {}, 10000)"]), { after: 200 });
    writeSync(2, "returned " + blocked.status + String.fromCharCode(10));`;
  const child = spawnSync(process.execPath, ["--input-type=module", "-e", source],
    { encoding: "utf8", windowsHide: true, timeout: 60_000 });
  assert.equal(child.status, 0, child.stderr);
  const lines = child.stderr.split(/\r?\n/u).filter(Boolean);
  assert.equal(lines.length, 2, child.stderr);
  assert.match(lines[0], /^warning: fixture probe is still running after 10 seconds; attempt=\d+\/\d+; elapsed_ms=\d+$/u);
  assert.equal(lines[1], "returned 0");
});

test("registration validates exact replies, retries only ambiguous transport, and bounds stalled requests and bodies", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    for (const reply of [() => response({}, 503), () => response({ ...registrationAck(request), runId: "other" }),
      () => new Response("{"), () => new Response("x".repeat(65_537))]) {
      let calls = 0;
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => { calls++; return reply(); } });
      assert.equal(result.state, "unconfirmed");
      assert.equal(calls, 1);
    }
    for (const change of [{ ownerSessionId: "other" }, { creatorPid: -1 }, { trackingDurability: "volatile" },
      { runDir: join(root, "missing") }, { detached: false }]) {
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async (_url, options) => {
        const query = registrationQuery(request, root);
        Object.assign(query.run, change);
        return response(options.method === "POST" ? registrationAck(request) : query);
      } });
      assert.equal(result.state, "unconfirmed");
    }
    let calls = 0;
    const reset = Object.assign(new Error("reset"), { cause: { code: "ECONNRESET" } });
    const fetch = async (_url, options) => {
      calls++;
      if (calls === 1 || calls === 3) throw reset;
      return response(options.method === "POST" ? registrationAck(request) : registrationQuery(request, root));
    };
    request.registration = await registerTestRun(request, root, registrationEnv, { fetch });
    assert.equal(request.registration.state, "registered");
    assert.equal(calls, 4);
    request.stages.push(stage("changed"));
    assert.equal((await confirmDetachedRegistration(request, root, registrationEnv,
      { fetch: () => assert.fail("changed worker request cannot reuse registration") })).state, "unconfirmed");
    for (const stalledBody of [false, true]) {
      let signal;
      const result = await registerTestRun(request, root, registrationEnv, { budgetMs: 10,
        fetch: async (_url, options) => {
          signal = options.signal;
          return stalledBody ? new Response(new ReadableStream({ start() {} })) : await new Promise(() => {});
        } });
      assert.equal(result.state, "unconfirmed");
      assert.equal(result.reason, "registration-timeout");
      assert.equal(signal.aborted, true);
    }
  });
});
}
