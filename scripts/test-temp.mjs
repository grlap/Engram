// Test-only fixture ownership and read-only user-Temp leak detection.
import { mkdirSync, mkdtempSync, readdirSync, rmSync, rmdirSync, lstatSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

const productRoot = join(tmpdir(), "engram");
mkdirSync(productRoot, { recursive: true });
if (lstatSync(productRoot).isSymbolicLink()) throw new Error("Test fixture product root must not be a symlink");
const ownsRun = !process.env.ENGRAM_TEST_RUN_ROOT;
export const fixtureRoot = ownsRun ? mkdtempSync(join(productRoot, `run-${process.pid}-`)) : resolve(process.env.ENGRAM_TEST_RUN_ROOT);
if (dirname(fixtureRoot) !== productRoot || !basename(fixtureRoot).startsWith("run-")) throw new Error("Test run root must be a unique Temp/engram/run-* child");
const owned = new Set();

export function fixtureHome(prefix, context) {
  if (!/^[a-z0-9-]+-$/u.test(prefix)) throw new Error("Invalid test fixture prefix");
  mkdirSync(fixtureRoot, { recursive: true });
  if (lstatSync(fixtureRoot).isSymbolicLink()) throw new Error("Test fixture root must not be a symlink");
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
      rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 25 });
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
  if (root === fixtureRoot && ownsRun) rmdirSync(root);
}

// Used by both Rust gate launchers; checks even when the child gate fails.
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
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
