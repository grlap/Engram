// Shared real-Git conformance fixtures for the owner command and its adapters.
// The caller owns a fresh parent directory and its cleanup. No production
// admission logic lives here; expected verdicts describe the agreed contract.
import { mkdirSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

export const sourceRefusalCodes = Object.freeze(["input_unavailable", "deadline", "output_limit"]);

export const sourceCases = [
  { id: "clean", layout: "plain", status: "clean" },
  { id: "root-dirty", layout: "plain", status: "not_clean" },
  { id: "root-untracked", layout: "plain", status: "not_clean" },
  { id: "root-ignored", layout: "plain", status: "clean" },
  { id: "rename", layout: "plain", status: "not_clean" },
  { id: "root-filter", layout: "plain", status: "unavailable" },
  { id: "root-assume-unchanged", layout: "plain", status: "clean" },
  { id: "root-skip-worktree", layout: "plain", status: "clean" },
  { id: "child-clean", layout: "child", status: "clean" },
  { id: "child-dirty", layout: "child", status: "unavailable" },
  { id: "child-untracked", layout: "child", status: "unavailable" },
  { id: "child-ignored", layout: "child", status: "clean" },
  { id: "child-off-pin", layout: "child", status: "unavailable" },
  { id: "child-uninitialized", layout: "child", status: "unavailable" },
  { id: "child-filter", layout: "child", status: "unavailable" },
  { id: "child-hide-untracked", layout: "child", status: "unavailable" },
  { id: "child-assume-unchanged", layout: "child", status: "clean" },
  { id: "child-skip-worktree", layout: "child", status: "clean" },
  { id: "nested-clean", layout: "nested", status: "clean" },
  { id: "nested-untracked", layout: "nested", status: "unavailable" },
  { id: "child-long-path-clean", layout: "child", status: "clean" },
  { id: "child-long-path-dirty", layout: "child", status: "unavailable" },
  { id: "root-autocrlf-clean", layout: "plain", status: "clean" },
  { id: "root-autocrlf-dirty", layout: "plain", status: "not_clean" },
];

function git(root, args) {
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.toUpperCase().startsWith("GIT_")) delete env[key];
  const nullDevice = process.platform === "win32" ? "NUL" : "/dev/null";
  Object.assign(env, { GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: nullDevice,
    GIT_ATTR_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0" });
  const result = spawnSync("git", ["-c", "user.name=Source Fixture", "-c", "user.email=fixture@example.invalid",
    "-c", "core.autocrlf=false", "-c", "core.longpaths=true", "-c", `core.hooksPath=${nullDevice}`, "-c", "commit.gpgsign=false",
    "-c", "protocol.file.allow=always", ...args], { cwd: root, env, encoding: "utf8", timeout: 30_000, maxBuffer: 1024 * 1024 });
  if (result.error || result.status !== 0) throw new Error(`fixture git ${args.join(" ")}: ${result.error ?? result.stderr}`);
  return result.stdout.trim();
}

function repository(root) {
  mkdirSync(root, { recursive: true });
  git(root, ["init", "-q", "--template=", "-b", "main"]);
  writeFileSync(join(root, "tracked file.txt"), "original\n");
  writeFileSync(join(root, ".gitignore"), "ignored/\n");
  git(root, ["add", "."]);
  git(root, ["commit", "--no-gpg-sign", "-qm", "Create source fixture"]);
}

export function createSourceFixture(parent, id) {
  if (!isAbsolute(parent)) throw new Error("source fixture parent must be absolute");
  const scenario = sourceCases.find((entry) => entry.id === id);
  if (!scenario) throw new Error(`unknown source fixture ${id}`);
  // Keep recursive .git/modules/.../objects paths below Windows MAX_PATH.
  // Human-readable scenario names remain in the table and test titles.
  const base = resolve(parent, String(sourceCases.indexOf(scenario)));
  if (dirname(base) !== resolve(parent)) throw new Error("source fixture must stay below its parent");
  const root = join(base, id === "child-clean" ? "r s" : "r");
  repository(root);
  const childName = id === "child-clean" ? "c s" : "c";
  const child = join(root, childName);
  const nested = join(child, "n");
  // Three short-enough components, but a relative path beyond MAX_PATH.
  const longTracked = join(...Array.from({ length: 3 }, (_, i) => `${i}${"x".repeat(95)}`), "tracked.txt");
  if (scenario.layout !== "plain") {
    const seed = join(base, "s");
    repository(seed);
    if (id.startsWith("child-long-path-")) {
      mkdirSync(dirname(join(seed, longTracked)), { recursive: true });
      writeFileSync(join(seed, longTracked), "long tracked original\n");
      git(seed, ["add", "."]);
      git(seed, ["commit", "--no-gpg-sign", "-qm", "Track long child path"]);
    }
    if (scenario.layout === "nested") {
      const nestedSeed = join(base, "d");
      repository(nestedSeed);
      git(seed, ["submodule", "add", "-q", nestedSeed, "n"]);
      git(seed, ["commit", "--no-gpg-sign", "-qam", "Pin nested fixture"]);
    }
    git(root, ["submodule", "add", "-q", seed, childName]);
    git(root, ["commit", "--no-gpg-sign", "-qam", "Pin child fixture"]);
    if (scenario.layout === "nested") git(root, ["submodule", "update", "--init", "--recursive", "-q"]);
  }
  if (id.startsWith("child-long-path-")) {
    // The production invocation must override both levels without editing them.
    git(root, ["config", "core.longpaths", "false"]);
    git(child, ["config", "core.longpaths", "false"]);
  }
  if (id.startsWith("root-autocrlf-")) {
    git(root, ["config", "core.autocrlf", "true"]);
    git(root, ["config", "core.safecrlf", "warn"]);
    // A real checkout records the converted file's stat in the index. Merely
    // rewriting LF as CRLF can make Git status report a size-only change.
    unlinkSync(join(root, "tracked file.txt"));
    git(root, ["-c", "core.autocrlf=true", "-c", "core.safecrlf=false",
      "checkout", "--", "tracked file.txt"]);
  }
  const commit = git(root, ["rev-parse", "HEAD"]);
  const tree = git(root, ["rev-parse", "HEAD^{tree}"]);
  const change = (where) => writeFileSync(join(where, "tracked file.txt"), "modified\n");
  const untracked = (where) => writeFileSync(join(where, "new file.txt"), "new\n");
  const ignored = (where) => { mkdirSync(join(where, "ignored")); writeFileSync(join(where, "ignored", "result.txt"), "generated\n"); };
  switch (id) {
    case "root-dirty": change(root); break;
    case "root-autocrlf-dirty": writeFileSync(join(root, "tracked file.txt"), "modified CRLF content\r\n"); break;
    case "root-untracked": untracked(root); break;
    case "root-ignored": ignored(root); break;
    case "rename": git(root, ["mv", "tracked file.txt", "renamed file.txt"]); break;
    case "root-filter": git(root, ["config", "filter.fixture.clean", "must-not-run"]); break;
    case "child-dirty": change(child); break;
    case "child-long-path-dirty": writeFileSync(join(child, longTracked), "long tracked modified\n"); break;
    case "child-untracked": untracked(child); break;
    case "child-ignored": ignored(child); break;
    case "child-off-pin": git(child, ["commit", "--no-gpg-sign", "--allow-empty", "-qm", "Move child HEAD"]); break;
    case "child-uninitialized": git(root, ["submodule", "deinit", "-f", "--", childName]); break;
    case "child-filter": git(child, ["config", "filter.fixture.process", "must-not-run"]); break;
    case "child-hide-untracked":
      git(child, ["config", "status.showUntrackedFiles", "no"]);
      git(child, ["config", "diff.ignoreSubmodules", "all"]);
      untracked(child); break;
    case "nested-untracked": untracked(nested); break;
    case "root-assume-unchanged": case "root-skip-worktree":
    case "child-assume-unchanged": case "child-skip-worktree": {
      const where = id.startsWith("child-") ? child : root;
      git(where, ["update-index", id.endsWith("assume-unchanged") ? "--assume-unchanged" : "--skip-worktree", "--", "tracked file.txt"]);
      change(where); break;
    }
    default: break;
  }
  return { ...scenario, root, child, nested, commit, tree };
}
