#!/usr/bin/env node

// Adapt an author's immutable candidate to TermAl's existing mailbox request.
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { realpathSync } from "node:fs";
import { devNull } from "node:os";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual, promisify } from "node:util";

const usage = "node scripts/test-task.mjs WORK BRANCH --remote RUNNER [--focused SELECTION] --confirm-safe-to-publish";
const operationTimeoutMs = 60_000;
const execFileAsync = promisify(execFile);

async function execute(command, args, options) {
  try { return { status: 0, ...await execFileAsync(command, args, options) }; }
  catch (error) { return { status: typeof error.code === "number" ? error.code : null, error, stdout: error.stdout ?? "", stderr: error.stderr ?? "" }; }
}

export async function withSlowWarning(operation, work, action, warn = (text) => process.stderr.write(text)) {
  const started = Date.now();
  const timer = setTimeout(() => warn(`${JSON.stringify({ warning: "slow-operation", operation, work, elapsed_ms: Date.now() - started })}\n`), 10_000);
  try { return await action(); } finally { clearTimeout(timer); }
}

export function failureOutput(error) {
  return { outcome: error.outcome ?? "unknown", reason: error.message, ...error };
}

function refuse(check, reason, observed, remedy, facts = {}) {
  throw Object.assign(new Error(reason), { outcome: "refused", check, observed, remedy, ...facts });
}

function cliFailure(result) {
  return { status: result.status, code: result.error?.code ?? null, signal: result.error?.signal ?? result.signal ?? null,
    killed: result.error?.killed ?? false, stderr: result.stderr?.slice(-2000) ?? "" };
}

function argumentsFor(args) {
  const [work, branch, ...flags] = args;
  const options = { work, branch, mode: "full" };
  if (!/^w-[a-f0-9]{12}$/u.test(work ?? "")) {
    refuse("arguments", "The item's short work ref is required; full UUIDs are not accepted.", { work }, usage);
  }
  if (!branch || branch.startsWith("-")) refuse("arguments", "A branch is required and must not begin with '-'.", { branch }, usage);
  const seen = new Set();
  for (let index = 0; index < flags.length; index++) {
    const flag = flags[index];
    if (seen.has(flag)) refuse("arguments", "An option was repeated.", { flag }, usage);
    seen.add(flag);
    if (flag === "--confirm-safe-to-publish") options.confirmed = true;
    else if (flag === "--remote" || flag === "--focused") {
      const value = flags[++index];
      if (!value?.trim() || value.startsWith("--")) refuse("arguments", "An option needs a nonempty value that does not begin with --.", { flag, value }, usage);
      if (flag === "--remote") options.runner = value;
      else { options.mode = "focused"; options.selection = value; }
    } else refuse("arguments", "Unsupported option; this adapter is remote-only.", { flag }, usage);
  }
  if (!options.runner) refuse("arguments", "The remote runner is required.", { runner: options.runner }, usage);
  if (!options.confirmed) refuse("safe-to-publish", "Publication confirmation is required.", { confirmed: false }, "Inspect the whole candidate for secrets and publication authority, then pass --confirm-safe-to-publish.");
  return options;
}

function nativePath(path) {
  const normalized = realpathSync.native(path).replace(/^\\\\\?\\/u, "").replaceAll("\\", "/");
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

export async function runTestTask(args, { cwd = process.cwd(), env = process.env, run = execute, fetch: fetchRequest = globalThis.fetch } = {}) {
  const options = argumentsFor(args);
  const { work, branch, runner, mode, selection } = options;
  const facts = { published: false, dispatched: false };
  const invoke = (operation, command, argv) => withSlowWarning(operation, work, () => run(command, argv, { cwd, env: { ...env, GIT_TERMINAL_PROMPT: "0", GIT_GRAFT_FILE: devNull }, encoding: "utf8", windowsHide: true, timeout: operationTimeoutMs, maxBuffer: 4 * 1024 * 1024 }));
  const git = (argv) => invoke(`git ${argv.find((arg) => ["rev-parse", "show", "for-each-ref", "remote", "ls-remote", "merge-base", "push"].includes(arg))}`, "git", ["--no-replace-objects", "-c", "maintenance.auto=false", ...argv]);
  const checked = (result, check, remedy) => {
    if (result.error || result.status !== 0) refuse(check, "The operation did not complete successfully.", { status: result.status, code: result.error?.code, stderr: result.stderr?.slice(-2000) }, remedy, facts);
    return result.stdout.trim();
  };
  if (!env.TERMAL_CLI || !env.TERMAL_SESSION_ID || !env.TERMAL_BASE_URL) {
    refuse("host-context", "The existing TermAl CLI, caller session and base URL are required.", { cli: Boolean(env.TERMAL_CLI), session: Boolean(env.TERMAL_SESSION_ID), base: Boolean(env.TERMAL_BASE_URL) }, "Run from the TermAl session that holds and named this worktree.");
  }
  let base;
  try {
    base = new URL(env.TERMAL_BASE_URL);
    if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.search || base.hash) throw new Error("invalid URL");
  } catch {
    refuse("host-context", "TERMAL_BASE_URL is not an HTTP(S) base URL without credentials, query or fragment.", { validBase: false }, "Use the host-provided TermAl base URL.");
  }
  const root = checked(await git(["rev-parse", "--show-toplevel"]), "repository", "Run in the candidate's named Git worktree.");
  const refs = checked(await git(["for-each-ref", "--format=%(refname)", "refs/heads/", "refs/remotes/", "refs/tags/"]), "branch", "Use an existing, unambiguous branch name.").split("\n");
  const candidates = branch.startsWith("refs/") ? [branch] : [`refs/heads/${branch}`, `refs/remotes/${branch}`, `refs/tags/${branch}`];
  const matches = refs.filter((ref) => candidates.includes(ref));
  if (matches.length !== 1 || !/^refs\/(?:heads|remotes)\//u.test(matches[0])) {
    refuse("branch", "The branch is missing or ambiguous with another ref.", { branch, matches }, "Pass one full refs/heads/... or refs/remotes/... branch name.");
  }
  const commit = checked(await git(["rev-parse", "--verify", `${matches[0]}^{commit}`]), "branch", "Use an existing branch pointing to a commit.");
  const metadata = checked(await git(["show", "-s", "--format=%T%n%P", commit]), "candidate", "Use one candidate commit with one published parent.").split("\n");
  const [tree, parentLine = ""] = metadata;
  const parents = parentLine.trim().split(/\s+/u).filter(Boolean);
  if (!/^[a-f0-9]{40}$/u.test(commit) || !/^[a-f0-9]{40}$/u.test(tree) || parents.length !== 1) {
    refuse("parent-on-origin", "The candidate must have exactly one parent.", { commit, tree, parents }, "Create a candidate on the published base; do not use a root or merge commit.");
  }
  const ref = `refs/heads/author-test/${work}-${commit.slice(0, 8)}`;
  Object.assign(facts, { ref, commit, tree });
  const pushUrls = checked(await git(["remote", "get-url", "--push", "--all", "origin"]), "origin", "Configure origin with one intended publication destination.").split("\n");
  if (pushUrls.length !== 1) refuse("origin", "Origin has multiple push destinations.", { destinations: pushUrls.length }, "Use a repository with exactly one intended origin push destination.", facts);
  const advertised = checked(await git(["ls-remote", "--", pushUrls[0]]), "origin", "Restore access to origin, then inspect the remote before trying again.").split("\n").filter(Boolean).map((line) => line.split(/\s+/u));
  if (advertised.some(([, remoteRef]) => remoteRef === ref)) {
    refuse("author-test-ref", "The author-test ref already exists; it will not be updated.", { ref }, "Recover the existing request and its evidence; for another intentional request use the existing mailbox interface with that retained input.", facts);
  }
  const parent = parents[0];
  let publishedParent = advertised.some(([oid]) => oid === parent);
  for (const [oid] of advertised) {
    if (publishedParent) break;
    if (/^[a-f0-9]{40}$/u.test(oid)) publishedParent = (await git(["merge-base", "--is-ancestor", parent, oid])).status === 0;
  }
  if (!publishedParent) refuse("parent-on-origin", "The candidate's parent was not proven reachable from origin's advertised refs.", { parent, advertisedRefs: advertised.length }, "Fetch origin and create a candidate whose parent is already published, then retry.", facts);
  let rows;
  try {
    rows = await withSlowWarning("named-root presence", work, async () => {
      const response = await fetchRequest(new URL("/api/engram/named-roots", base), { signal: AbortSignal.timeout(operationTimeoutMs) });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return response.json();
    });
    if (!Array.isArray(rows)) throw new Error("expected a root-row array");
  } catch (error) {
    refuse("cached-root-presence", "Cached named-root presence could not be read.", { error: error.message }, "Restore the host connection, name the worktree if necessary, and retry next turn.", facts);
  }
  const matching = rows.filter((row) => {
    if (!row || (row.shortRef !== work && row.workId !== work) || row.namingSession !== env.TERMAL_SESSION_ID || typeof row.root !== "string") return false;
    try { return nativePath(row.root) === nativePath(root); } catch { return false; }
  });
  if (matching.length !== 1) refuse("cached-root-presence", "There is no unique cached root named by this caller for this item and worktree; this is not a live-claim check.", { work, root, matchingRows: matching.length }, "Claim the item and name this worktree in the current session, then retry next turn.", facts);
  // The absence check and normal push are deliberately not an atomic create.
  // Dispatch requires Git's new-ref status; any race is surfaced, never retried.
  const pushed = await git(["-c", "push.followTags=false", "-c", "push.recurseSubmodules=no", "-c", "remote.origin.mirror=false",
    "push", "--porcelain", "--no-follow-tags", "--recurse-submodules=no", "origin", `${commit}:${ref}`]);
  const pushedAt = new Date().toISOString();
  const update = (pushed.stdout ?? "").split("\n").map((line) => line.split("\t")).find((fields) => fields[1]?.split(":").at(-1) === ref);
  if (pushed.error || pushed.status !== 0 || update?.[0] !== "*") {
    const status = update?.[0] ?? null;
    const oldOid = status === "=" ? commit : update?.[2]?.match(/^([a-f0-9]+)\.\./u)?.[1] ?? null;
    refuse("publication", "Push did not confirm a new ref; no request was dispatched. A check-then-push race may already have updated the ref.",
      { ref, status, old_oid: oldOid, new_oid: commit, exitCode: pushed.status, code: pushed.error?.code, stderr: pushed.stderr?.slice(-2000) },
      "Inspect the exact author-test ref and its disposition. Do not retry, delete or update it automatically.",
      { ...facts, outcome: status && status !== "*" ? "refused" : "unknown", published: status === " " ? true : status && status !== "*" ? false : null, ...(status === " " ? { pushed_at: pushedAt } : {}) });
  }
  facts.published = true;
  facts.pushed_at = pushedAt;
  const request = { mode, targets: [{ workRef: work, use: { kind: "committed_input", expected_commit: commit, purpose: "verification" } }] };
  const requestKey = `test-task-${randomUUID()}`;
  const message = `Test this immutable candidate from origin.\nRef: ${ref}\nCommit: ${commit}\nTree: ${tree}\nMode: ${mode}\n${selection ? `Focused selection (literal; do not reinterpret as shell text): ${selection}\n` : ""}Return the actual launcher result through existing completion delivery; request acceptance is not a run result.`;
  Object.assign(facts, { request_key: requestKey, recipient: runner, request_message: message, test_request: request });
  const cli = (argv) => invoke(`mailbox ${argv[0]}`, env.TERMAL_CLI, ["mailbox", ...argv, "--json"]);
  const sent = await cli(["send", "--to", runner, "--message", message, "--idempotency-key", requestKey, "--test-request-json", JSON.stringify(request)]);
  if (sent.error || sent.status !== 0) {
    const observed = cliFailure(sent);
    const usageRefusal = observed.status === 2 && !observed.signal && !observed.killed;
    const neverStarted = observed.status === null && ["ENOENT", "EACCES", "EINVAL"].includes(observed.code) && !observed.signal && !observed.killed;
    const refused = usageRefusal || neverStarted;
    refuse("request-send", neverStarted ? "The TermAl CLI could not start; nothing was dispatched and publication already happened." : usageRefusal ? "The TermAl CLI refused its arguments before dispatch; publication already happened." : "Publication succeeded but dispatch outcome is unknown.", observed,
      refused ? "Correct the CLI path, permissions or invocation using its diagnostic, then deliberately send an existing mailbox request for the retained commit/ref. Re-running this script refuses its existing ref." : "Recover the original send using this identical message, route and request key; do not mint another intent or delete the ref.",
      { ...facts, outcome: refused ? "refused" : "unknown", dispatched: refused ? false : null });
  }
  let receipt;
  try {
    receipt = JSON.parse(sent.stdout);
    if (typeof receipt.messageId !== "string" || !receipt.messageId) throw new Error("receipt has no messageId");
  } catch (error) {
    refuse("request-send", "Publication succeeded but dispatch outcome is unknown.", { error: error.message }, "Recover the original send using this identical message, route and request key; do not mint another intent or delete the ref.", { ...facts, outcome: "unknown", dispatched: null });
  }
  Object.assign(facts, { dispatched: true, message_id: receipt.messageId });
  const read = await cli(["read-message", "--message-id", receipt.messageId]);
  if (read.error || read.status !== 0) {
    refuse("captured-association", "Push and dispatch succeeded, but the exact captured association is unknown.", cliFailure(read),
      "Read this exact message through the existing mailbox interface using its diagnostic; do not dispatch again while its disposition is unknown.", { ...facts, outcome: "unknown" });
  }
  let captured;
  try {
    const exact = JSON.parse(read.stdout);
    if (exact.id !== receipt.messageId || exact.testRequest?.requestSendKey !== requestKey || exact.testRequest.mode !== mode || exact.testRequest.targets?.length !== 1 || !isDeepStrictEqual(exact.testRequest.targets[0].target, request.targets[0])) throw new Error("exact message identity, request key or typed request does not match");
    captured = exact.testRequest.targets[0];
  } catch (error) {
    refuse("captured-association", "Push and dispatch succeeded, but the exact captured association is unknown.", { error: error.message }, "Read this exact message through the existing mailbox interface; do not dispatch again while its disposition is unknown.", { ...facts, outcome: "unknown" });
  }
  if (!captured.association || captured.uncreditedReason != null) {
    refuse("captured-association", "Push and dispatch already happened, but the host did not capture a credited association.", { uncreditedReason: captured.uncreditedReason ?? "association missing" }, "Resolve the original request with the runner, reclaim the item and re-name the root, then deliberately send a corrected existing mailbox request with the retained commit/ref and a new request key. Re-running this script refuses the retained ref.", facts);
  }
  return { outcome: "pending", message_id: receipt.messageId, request_key: requestKey, ref, commit, tree, pushed_at: facts.pushed_at, run_id: null };
}

if (process.argv[1] && nativePath(process.argv[1]) === nativePath(fileURLToPath(import.meta.url))) {
  try { process.stdout.write(`${JSON.stringify(await runTestTask(process.argv.slice(2)))}\n`); }
  catch (error) {
    process.stderr.write(`${JSON.stringify(failureOutput(error))}\n`);
    process.exitCode = 1;
  }
}
