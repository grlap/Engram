// Optional TermAl transport adapter. The host owns indexing, waits and credit.
import { realpathSync } from "node:fs";
import { createHash } from "node:crypto";

const contract = "termal-durable-test-run-registration";
const version = 1;
const budgetMs = 8_000;
const responseLimit = 65_536;
const hostMessageLimit = 384;
const contextKeys = ["TERMAL_BASE_URL", "TERMAL_SESSION_ID", "TERMAL_CLI"];
const states = new Set(["untracked", "unconfirmed", "registered-but-not-indexed", "registered"]);
const unconfirmed = (reason) => ({ state: "unconfirmed", reason, nativeWaitReady: false });
const httpStatus = (value) => Number.isInteger(value) && value >= 300 && value <= 599 ? value : undefined;
function hostMessage(value) {
  if (typeof value !== "string") return null;
  const normalized = value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, " ").trim();
  return normalized ? Array.from(normalized).slice(0, hostMessageLimit).join("") : null;
}
const immutableFields = ["runId", "root", "stages", "notifyTo", "full", "scope", "detached",
  "creatorPid", "owner", "started", "requiredBinaryEnv", "prerequisiteFiles", "needsCargo", "liveEngram"];

export function validateRequestMessageSelection(request) {
  const value = request.requestMessageId;
  if (value === undefined) return;
  // Check the transport spelling here. The host validates the retained native
  // or relayed message and publishes the selected id in durable registration.
  if (typeof value !== "string" || !(/^(?:mailbox-message-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}|relay-message-[A-Za-z0-9_-]{1,1024})$/u.test(value))) {
    throw new Error("invalid-request-message-id");
  }
}

export function registrationConfirmed(registration) {
  return ["registered", "registered-but-not-indexed"].includes(registration?.state);
}

function requestIdentity(request) {
  const ordered = (value) => Array.isArray(value) ? value.map(ordered)
    : value && typeof value === "object"
      ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, ordered(value[key])])) : value;
  const fields = Object.fromEntries(immutableFields.map((key) => [key, request[key] ?? null]));
  // Absent input keeps the historical identity byte-for-byte. The host uses
  // an explicit input kind to choose its binding, so that kind is immutable.
  if (request.input !== undefined) fields.input = request.input;
  if (request.requestMessageId !== undefined) fields.requestMessageId = request.requestMessageId;
  return createHash("sha256").update(JSON.stringify(ordered(fields))).digest("hex");
}

function transportCode(error) {
  for (let depth = 0; error && depth < 4; depth++, error = error.cause) {
    if (typeof error.code === "string") return error.code;
  }
}

function configuration(request, env) {
  if (!contextKeys.some((key) => env[key] !== undefined)) return { standalone: true };
  if (!env.TERMAL_BASE_URL || !env.TERMAL_SESSION_ID) throw new Error("partial-context");
  if (!/^[A-Za-z0-9_-]{1,128}$/u.test(env.TERMAL_SESSION_ID) || request.owner !== env.TERMAL_SESSION_ID) {
    throw new Error("invalid-context");
  }
  let base;
  try { base = new URL(env.TERMAL_BASE_URL); } catch { throw new Error("invalid-context"); }
  if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.search || base.hash) {
    throw new Error("invalid-context");
  }
  return { base, session: env.TERMAL_SESSION_ID };
}

function nativePath(path) {
  return realpathSync.native(path).replace(/^\\\\\?\\/u, "").replaceAll("\\", "/");
}

async function json(response) {
  if (!response.body) throw new Error("invalid-response");
  const chunks = [];
  let length = 0;
  for await (const chunk of response.body) {
    length += chunk.length;
    if (length > responseLimit) throw new Error("invalid-response");
    chunks.push(chunk);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); }
  catch { throw new Error("invalid-response"); }
}

async function query(request, directory, host, send, signal) {
  const url = new URL(`/api/test-runs/${encodeURIComponent(request.runId)}`, host.base);
  let response;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try { response = await send(url, { signal, redirect: "manual" }); break; }
    catch (error) {
      if (attempt || signal.aborted || transportCode(error) !== "ECONNRESET") {
        throw new Error("query-unconfirmed");
      }
    }
  }
  if (!response.ok) throw new Error("query-refused");
  const { run, source } = await json(response);
  if (!run || run.runId !== request.runId || run.ownerSessionId !== host.session
      || run.initiatingSessionId !== host.session || run.registrationState !== "registered"
      || run.trackingDurability !== "durable" || !Array.isArray(run.trackingProvenances)
      || !run.trackingProvenances.includes("launcherRegistered")
      || run.creatorPid !== request.creatorPid || run.startedAt !== request.started
      || run.detached !== request.detached || nativePath(run.runDir) !== nativePath(directory)
      || nativePath(run.worktree) !== nativePath(request.root)
      || !["index", "registration"].includes(source)
      || (request.requestMessageId !== undefined && run.requestMessageId !== request.requestMessageId)) {
    throw new Error("query-identity-mismatch");
  }
  return source;
}

// A single shared budget covers POST, response bodies, confirmation and a
// detached worker's confirmation. Even an unresponsive transport cannot
// postpone execution past it. A timeout is not evidence of failed persistence.
async function bounded(milliseconds, operation) {
  const controller = new AbortController();
  let timer;
  const expired = new Promise((_, reject) => {
    timer = setTimeout(() => { controller.abort(); reject(new Error("registration-timeout")); }, milliseconds);
  });
  try { return await Promise.race([operation(controller.signal), expired]); }
  finally { clearTimeout(timer); controller.abort(); }
}

function reason(error) {
  return new Set(["partial-context", "invalid-context", "invalid-response", "query-unconfirmed", "query-refused",
    "query-identity-mismatch", "registration-timeout", "registration-refused", "invalid-acknowledgement",
    "invalid-request-message-id"])
    .has(error?.message) ? error.message : "registration-unconfirmed";
}

export async function registerTestRun(request, directory, env = process.env, options = {}) {
  let host;
  try { validateRequestMessageSelection(request); host = configuration(request, env); }
  catch (error) { return unconfirmed(reason(error)); }
  if (host.standalone) return { state: "untracked", reason: "standalone", nativeWaitReady: false };
  const send = options.fetch ?? globalThis.fetch;
  // Injection only shortens the production bound for deterministic fixtures.
  const allowance = Math.max(0, Math.min(budgetMs, options.budgetMs ?? budgetMs));
  const started = performance.now();
  const identity = requestIdentity(request);
  let refusal;
  try {
    const source = await bounded(allowance, async (signal) => {
      const url = new URL(`/api/sessions/${encodeURIComponent(host.session)}/test-runs`, host.base);
      const body = JSON.stringify({ runId: request.runId, runDirectory: directory,
        ...(request.requestMessageId === undefined ? {} : { requestMessageId: request.requestMessageId }) });
      for (let attempt = 0; attempt < 2; attempt += 1) {
        let response;
        try {
          response = await send(url, { method: "POST", headers: { "content-type": "application/json" },
            body, signal, redirect: "manual" });
        } catch (error) {
          // Only ambiguous transport loss replays the identical registration.
          if (attempt || signal.aborted || !["ECONNRESET", "EPIPE", "UND_ERR_SOCKET"].includes(transportCode(error))) {
            throw new Error("registration-unconfirmed");
          }
          continue;
        }
        if (!response.ok) {
          // Keep known status even if the bounded error body cannot be read.
          // Only the host's error string is diagnostic; never echo its raw body.
          refusal = { httpStatus: httpStatus(response.status), hostMessage: null };
          try { refusal.hostMessage = hostMessage((await json(response))?.error); }
          catch { /* Malformed, oversized or interrupted body: status only. */ }
          if (signal.aborted) throw new Error("registration-timeout");
          throw new Error("registration-refused");
        }
        const ack = await json(response);
        if (ack.contract !== contract || ack.version !== version || ack.runId !== request.runId || ack.registered !== true) {
          throw new Error("invalid-acknowledgement");
        }
        return await query(request, directory, host, send, signal);
      }
      throw new Error("registration-unconfirmed");
    });
    if (requestIdentity(request) !== identity) return unconfirmed("request-changed");
    return { state: source === "index" ? "registered" : "registered-but-not-indexed", requestIdentity: identity,
      reason: source === "index" ? "confirmed-index" : "registration-only",
      nativeWaitReady: source === "index", hostOrigin: host.base.origin,
      remainingMs: Math.max(0, allowance - (performance.now() - started)) };
  } catch (error) { return { ...unconfirmed(reason(error)), ...(refusal ?? {}) }; }
}

export async function confirmDetachedRegistration(request, directory, env = process.env, options = {}) {
  const registration = request.registration;
  // Unconfirmed is sticky: late indexing cannot turn an ambiguous launch
  // into an observed or credited one. Historical/standalone runs keep fallback.
  if (!registrationConfirmed(registration)) return registration;
  try {
    validateRequestMessageSelection(request);
    const host = configuration(request, env);
    if (host.standalone || host.base.origin !== registration.hostOrigin) return unconfirmed("confirmation-unavailable");
    if (registration.requestIdentity !== requestIdentity(request)) return unconfirmed("request-changed");
    const remaining = registration.remainingMs;
    if (!Number.isFinite(remaining) || remaining <= 0) return unconfirmed("registration-timeout");
    const source = await bounded(Math.min(budgetMs, remaining), (signal) =>
      query(request, directory, host, options.fetch ?? globalThis.fetch, signal));
    if (source === "registration" || registration.state === "registered-but-not-indexed") {
      return { ...registration, state: "registered-but-not-indexed", reason: "registration-only", nativeWaitReady: false, remainingMs: 0 };
    }
    return { ...registration, remainingMs: 0 };
  } catch (error) { return unconfirmed(reason(error)); }
}

// Called once by the admitted detached executor, never by a reader or notifier.
// Unlike run registration, this POST has no host idempotency contract: even a
// lost response must not be retried, since it could create a second wake.
export async function registerOwnRunWait(request, directory, env = process.env, options = {}) {
  const unavailable = { state: "unavailable", reason: "own-run-registration-unconfirmed" };
  if (request.detached !== true || request.notifyTo || !registrationConfirmed(request.registration)) return unavailable;
  let host;
  try { host = configuration(request, env); }
  catch { return unavailable; }
  if (host.standalone || host.base.origin !== request.registration.hostOrigin) return unavailable;
  const send = options.fetch ?? globalThis.fetch;
  let refusal;
  try {
    return await bounded(Math.max(0, Math.min(budgetMs, options.budgetMs ?? budgetMs)), async (signal) => {
      const url = new URL(`/api/sessions/${encodeURIComponent(host.session)}/test-run-waits`, host.base);
      const response = await send(url, { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ runIds: [request.runId], mode: "all" }), signal, redirect: "manual" });
      if (!response.ok) {
        refusal = { httpStatus: httpStatus(response.status), hostMessage: null };
        try { refusal.hostMessage = hostMessage((await json(response))?.error); }
        catch { /* Retain only the status if the bounded body is unavailable. */ }
        if (signal.aborted) throw new Error("registration-timeout");
        throw new Error("test-run-wait-refused");
      }
      const ack = await json(response);
      const wait = ack?.wait;
      const run = wait?.runs?.[0];
      const token = (value) => typeof value === "string" && /^[A-Za-z0-9_-]{1,128}$/u.test(value);
      const singleton = (value) => Array.isArray(value) && value.length === 1 && value[0] === request.runId;
      if (response.status !== 201 || !token(ack?.waitId) || !token(ack.serverInstanceId)
          || !Number.isSafeInteger(ack.revision) || ack.revision < 0 || !singleton(ack.runIds) || ack.mode !== "all"
          || typeof ack.resumePromptQueued !== "boolean" || typeof ack.resumeDispatchRequested !== "boolean"
          || wait?.id !== ack.waitId || wait.sessionId !== host.session || !singleton(wait.runIds) || wait.mode !== "all"
          || !Array.isArray(wait.runs) || wait.runs.length !== 1 || run?.runId !== request.runId
          || run.ownerSessionId !== host.session || run.startedAt !== request.started
          || run.preset !== (request.full === true ? "full" : "focused")
          || nativePath(run.runDir) !== nativePath(directory) || nativePath(run.worktree) !== nativePath(request.root)) {
        throw new Error("invalid-wait-acknowledgement");
      }
      return { state: "registered", waitId: ack.waitId, runId: request.runId, sessionId: host.session,
        hostOrigin: host.base.origin };
    });
  } catch (error) {
    return { state: error?.message === "test-run-wait-refused" ? "refused" : "unconfirmed",
      reason: error?.message === "test-run-wait-refused" ? "test-run-wait-refused" : "test-run-wait-unconfirmed",
      ...(refusal ? { ...refusal } : {}) };
  }
}

export function ownRunWaitLine(wait) {
  if (!wait) return null;
  if (wait.state === "registered" && typeof wait.waitId === "string" && /^[A-Za-z0-9_-]{1,128}$/u.test(wait.waitId)) {
    return `host-wait: registered; waitId=${wait.waitId}; host resumes the initiating session`;
  }
  const state = wait.state === "refused" ? "refused" : wait.state === "unavailable" ? "unavailable" : "unconfirmed";
  const status = httpStatus(wait.httpStatus);
  const message = hostMessage(wait.hostMessage);
  return `host-wait: ${state}; ${state === "unconfirmed" ? "a wait may already exist; do not retry automatically" : "no confirmed completion wake"}`
    + (status === undefined ? "" : `; POST HTTP ${status}; ${message === null ? "host message unavailable" : `host message=${JSON.stringify(message)}`}`);
}

export function registrationLine(registration, wait) {
  if (!registration) return null; // Preserve historical notification bodies.
  const state = states.has(registration.state) ? registration.state : "unconfirmed";
  const credit = registrationConfirmed({ state })
    ? "registration succeeded; host check credit decided at delivery" : "not host check credit";
  const status = state === "unconfirmed" ? httpStatus(registration.httpStatus) : undefined;
  const message = hostMessage(registration.hostMessage);
  const detail = status === undefined ? ""
    : `; POST HTTP ${status}; ${message === null ? "host message unavailable" : `host message=${JSON.stringify(message)}`}`;
  const nativeWait = wait ? wait.state === "registered" ? "registered" : "unconfirmed"
    : state === "registered" && registration.nativeWaitReady === true ? "ready" : "unavailable; use --notify";
  return `registration: ${state}; native-wait=${nativeWait}; ${credit}${detail}`;
}
