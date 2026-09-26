// Test-only fixture ownership under this repository's target/ directory, with
// guarded recursive deletion and a read-only leak audit of each run root.
import { mkdirSync, mkdtempSync, readdirSync, realpathSync, rmSync, rmdirSync, lstatSync } from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

// Fixtures stay inside the repository (Greg's rule: test homes go under
// target/, never the system Temp folder). This module's resolved location
// names the repository, so a copied helper keeps its fixtures beside itself.
const repository = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const productRoot = join(repository, "target", "tmp", "engram");
const productChain = [join(repository, "target"), join(repository, "target", "tmp"), productRoot];
// Refuse a linked step before creating anything beneath it, and again after.
for (const path of productChain) refuseLink(path, "Test fixture product root", { missingOk: true });
mkdirSync(productRoot, { recursive: true });
for (const path of productChain) refuseLink(path, "Test fixture product root");
const ownsRun = !process.env.ENGRAM_TEST_RUN_ROOT;
export const fixtureRoot = ownsRun ? mkdtempSync(join(productRoot, `run-${process.pid}-`)) : resolve(process.env.ENGRAM_TEST_RUN_ROOT);
if (dirname(fixtureRoot) !== productRoot || !basename(fixtureRoot).startsWith("run-")) {
  throw new Error(`Test run root must be a unique run-* child of ${productRoot}: ${fixtureRoot}`);
}
// Git run in a fixture that lacks its own repository must report "not a
// repository" instead of climbing out into this checkout. This covers Node
// tests and the child processes of runs launched through this module.
process.env.GIT_CEILING_DIRECTORIES = fixtureRoot;
const owned = new Set();

function refuseLink(path, label, { missingOk = false } = {}) {
  let stat;
  try {
    stat = lstatSync(path);
  } catch (error) {
    if (missingOk && error.code === "ENOENT") return;
    throw error;
  }
  // lstat reports Windows junctions as symbolic links too.
  if (stat.isSymbolicLink()) throw new Error(`${label} must not be a link: ${path}`);
}

function strictlyBelow(root, path) {
  const rest = relative(root, path);
  return rest !== "" && !isAbsolute(rest) && !rest.split(sep).includes("..");
}

// Returns the path to delete, null when it is already gone, or throws a
// refusal. The root must lie below this repository's target/tmp/engram, the
// path strictly below the root, and no step from target/ down to the path may
// be a link.
function fixturePathToRemove(target, root) {
  const base = resolve(root);
  const path = resolve(target);
  if (!strictlyBelow(base, path)) throw new Error(`Refusing to delete outside the test fixture root ${base}: ${path}`);
  for (const step of productChain) refuseLink(step, "Test fixture product root");
  const anchor = realpathSync.native(productRoot);
  // Walk up from the path to the anchor before resolving anything: realpath
  // would follow a link and report its destination. Walking up also covers
  // links above the root.
  for (let current = path; ; current = dirname(current)) {
    try {
      refuseLink(current, "A step on the way to a deleted fixture path");
    } catch (error) {
      if (error.code === "ENOENT" && current === path) return null;
      throw error;
    }
    if (realpathSync.native(current) === anchor) break;
    if (dirname(current) === current) throw new Error(`Refusing to delete outside ${productRoot}: ${path}`);
  }
  const resolvedBase = realpathSync.native(base);
  if (!strictlyBelow(anchor, resolvedBase)) throw new Error(`Refusing to delete with a fixture root outside ${productRoot}: ${base}`);
  if (!strictlyBelow(resolvedBase, realpathSync.native(path))) {
    throw new Error(`Refusing to delete outside the resolved test fixture root ${base}: ${path}`);
  }
  return path;
}

const pause = (milliseconds) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);

// The only recursive delete in Node test support. It checks again before every
// attempt, so a step replaced by a link between attempts is refused rather
// than followed; what remains is the moment between the last check and the
// delete itself.
export function removeFixturePath(target, root = fixtureRoot) {
  for (let attempt = 0; ; attempt += 1) {
    const path = fixturePathToRemove(target, root);
    if (!path) return;
    try {
      rmSync(path, { recursive: true, force: true });
      return;
    } catch (error) {
      // The same errors and linear 25-125 ms schedule as rmSync maxRetries.
      if (attempt >= 5 || !["EBUSY", "EMFILE", "ENFILE", "ENOTEMPTY", "EPERM"].includes(error.code)) throw error;
      pause(25 * (attempt + 1));
    }
  }
}

export function fixtureHome(prefix, context) {
  if (!/^[a-z0-9-]+-$/u.test(prefix)) throw new Error("Invalid test fixture prefix");
  // Check the whole chain on every call, not only at import: a step replaced
  // by a link later must not let this create anything through it.
  const checkChain = (missingOk) => {
    for (const path of productChain) refuseLink(path, "Test fixture product root");
    refuseLink(fixtureRoot, "Test fixture root", { missingOk });
  };
  checkChain(true);
  mkdirSync(fixtureRoot, { recursive: true });
  checkChain(false);
  const home = mkdtempSync(join(fixtureRoot, prefix));
  owned.add(home);
  // Also cover setup failures before a test reaches its own try/finally.
  context?.after(() => removeFixtureHomes(home));
  return home;
}

export function removeFixtureHomes(...homes) {
  const errors = [];
  for (const home of homes) {
    if (!owned.has(home)) continue;
    try {
      removeFixturePath(home);
      owned.delete(home);
    } catch (error) {
      errors.push(new Error(`Engram test fixture cleanup FAILED: ${home}: ${error.message}`, { cause: error }));
    }
  }
  // A finally block must not replace the assertion that caused cleanup.
  // Retain failed paths in owned; the run audit enforces any residue.
  if (errors.length) console.error(new AggregateError(errors, "Fixture cleanup failed"));
}

export async function closeFixtureClients(...clients) {
  const results = await Promise.allSettled(clients.filter(Boolean).map((client) => Promise.resolve().then(() => client.close())));
  const errors = results.filter((result) => result.status === "rejected").map((result) => result.reason);
  if (errors.length) throw new AggregateError(errors, "Fixture process shutdown failed");
}

function entries(path) {
  try { return readdirSync(path).sort(); }
  catch (error) { if (error.code === "ENOENT") return []; throw error; }
}

export function tempSnapshot(root = fixtureRoot) {
  return entries(root);
}

export function assertTempClean(before, root = fixtureRoot) {
  const after = tempSnapshot(root);
  const added = after.filter((name) => !before.includes(name));
  console.log(`Temp audit: run=${root}; before=${before.length} after=${after.length}; new=${added.length}; remaining=${after.length}`);
  if (after.length) {
    throw new Error(`Temp fixture leak: run=${root}; new entries=${JSON.stringify(added)}; remaining=${JSON.stringify(after)}`);
  }
  if (root === fixtureRoot && ownsRun) removeEmptyRunRoot();
}

// Removes this process's own run root, never recursively, and only while it
// is still a direct child of target/tmp/engram reached without any link.
function removeEmptyRunRoot() {
  for (const step of productChain) refuseLink(step, "Test fixture product root");
  try {
    refuseLink(fixtureRoot, "Test fixture root");
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  if (dirname(realpathSync.native(fixtureRoot)) !== realpathSync.native(productRoot)) {
    throw new Error(`Refusing to delete a run root outside ${productRoot}: ${fixtureRoot}`);
  }
  rmdirSync(fixtureRoot);
}

// Used by both Rust gate launchers; checks even when the child gate fails.
// Node resolves this module's own URL through links (macOS temp paths run
// through /var -> /private/var), so resolve the invoked path the same way.
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  const [separator, program, ...args] = process.argv.slice(2);
  const before = tempSnapshot();
  try {
    if (separator !== "--" || !program) throw new Error("Usage: node scripts/test-temp.mjs -- PROGRAM [ARG ...]");
    const result = spawnSync(program, args, { stdio: "inherit", env: { ...process.env, ENGRAM_TEST_RUN_ROOT: fixtureRoot } });
    if (result.error) throw result.error;
    process.exitCode = result.status ?? 1;
  } finally {
    assertTempClean(before);
  }
}
