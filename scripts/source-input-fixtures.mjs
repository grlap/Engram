// Shared real-Git conformance fixtures for the owner command and its adapters.
// The caller owns a fresh parent directory and its cleanup. No production
// admission logic lives here; expected verdicts describe the agreed contract.
import { chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, symlinkSync, unlinkSync, utimesSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

export const sourceRefusalCodes = Object.freeze(["input_unavailable", "deadline", "output_limit"]);

export const sourceCases = [
  { id: "clean", layout: "plain", status: "clean" },
  { id: "root-dirty", layout: "plain", status: "not_clean" },
  { id: "root-untracked", layout: "plain", status: "not_clean" },
  { id: "root-delete", layout: "plain", status: "not_clean" },
  { id: "staged-delete-present", layout: "plain", status: "not_clean" },
  { id: "root-mode", layout: "plain", status: "not_clean", skipOn: ["win32"] },
  { id: "root-symlink", layout: "plain", status: "not_clean", skipOn: ["win32"] },
  { id: "root-leading-quote", layout: "plain", status: "not_clean", skipOn: ["win32"] },
  { id: "root-newline", layout: "plain", status: "unavailable", skipOn: ["win32"] },
  { id: "root-ignored", layout: "plain", status: "clean" },
  { id: "rename", layout: "plain", status: "not_clean" },
  { id: "root-filter", layout: "plain", status: "unavailable" },
  { id: "root-assume-unchanged", layout: "plain", status: "clean" },
  { id: "root-skip-worktree", layout: "plain", status: "clean" },
  { id: "child-clean", layout: "child", status: "clean" },
  { id: "child-dirty", layout: "child", status: "not_clean" },
  { id: "child-untracked", layout: "child", status: "not_clean" },
  { id: "child-ignored", layout: "child", status: "clean" },
  { id: "child-off-pin", layout: "child", status: "not_clean" },
  { id: "child-off-pin-dirty", layout: "child", status: "not_clean" },
  { id: "child-uninitialized", layout: "child", status: "unavailable" },
  { id: "child-filter", layout: "child", status: "unavailable" },
  { id: "child-hide-untracked", layout: "child", status: "not_clean" },
  { id: "child-assume-unchanged", layout: "child", status: "clean" },
  { id: "child-skip-worktree", layout: "child", status: "clean" },
  { id: "nested-clean", layout: "nested", status: "clean" },
  { id: "nested-untracked", layout: "nested", status: "not_clean" },
  { id: "nested-off-pin", layout: "nested", status: "not_clean" },
  { id: "child-long-path-clean", layout: "child", status: "clean" },
  { id: "child-long-path-dirty", layout: "child", status: "not_clean" },
  { id: "root-autocrlf-clean", layout: "plain", status: "clean" },
  { id: "root-autocrlf-dirty", layout: "plain", status: "not_clean" },
];

function git(root, args, machineEnvironment, input) {
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.toUpperCase().startsWith("GIT_")) delete env[key];
  const nullDevice = process.platform === "win32" ? "NUL" : "/dev/null";
  Object.assign(env, { GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: nullDevice,
    GIT_ATTR_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0" });
  Object.assign(env, machineEnvironment);
  const result = spawnSync("git", ["-c", "user.name=Source Fixture", "-c", "user.email=fixture@example.invalid",
    ...(!machineEnvironment ? ["-c", "core.autocrlf=false"] : []),
    "-c", "core.longpaths=true", "-c", `core.hooksPath=${nullDevice}`, "-c", "commit.gpgsign=false",
    "-c", "protocol.file.allow=always", ...args], { cwd: root, env, input, encoding: "utf8", timeout: 30_000, maxBuffer: 1024 * 1024 });
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
    case "root-delete": unlinkSync(join(root, "tracked file.txt")); break;
    case "staged-delete-present": git(root, ["rm", "--cached", "--", "tracked file.txt"]); break;
    case "root-mode":
      git(root, ["config", "core.fileMode", "true"]);
      chmodSync(join(root, "tracked file.txt"), 0o755); break;
    case "root-symlink":
      ignored(root);
      symlinkSync("ignored/result.txt", join(root, "link")); break;
    case "root-leading-quote": writeFileSync(join(root, '\"quoted.txt'), "quoted\n"); break;
    case "root-newline": writeFileSync(join(root, "line\nbreak.txt"), "newline\n"); break;
    case "root-ignored": ignored(root); break;
    case "rename": git(root, ["mv", "tracked file.txt", "renamed file.txt"]); break;
    case "root-filter": git(root, ["config", "filter.fixture.clean", "must-not-run"]); break;
    case "child-dirty": change(child); break;
    case "child-long-path-dirty": writeFileSync(join(child, longTracked), "long tracked modified\n"); break;
    case "child-untracked": untracked(child); break;
    case "child-ignored": ignored(child); break;
    case "child-off-pin": git(child, ["commit", "--no-gpg-sign", "--allow-empty", "-qm", "Move child HEAD"]); break;
    case "child-off-pin-dirty":
      git(child, ["commit", "--no-gpg-sign", "--allow-empty", "-qm", "Move child HEAD"]);
      change(child); break;
    case "child-uninitialized": git(root, ["submodule", "deinit", "-f", "--", childName]); break;
    case "child-filter": git(child, ["config", "filter.fixture.process", "must-not-run"]); break;
    case "child-hide-untracked":
      git(child, ["config", "status.showUntrackedFiles", "no"]);
      git(child, ["config", "diff.ignoreSubmodules", "all"]);
      untracked(child); break;
    case "nested-untracked": untracked(nested); break;
    case "nested-off-pin": git(nested, ["commit", "--no-gpg-sign", "--allow-empty", "-qm", "Move nested HEAD"]); break;
    case "root-assume-unchanged": case "root-skip-worktree":
    case "child-assume-unchanged": case "child-skip-worktree": {
      const where = id.startsWith("child-") ? child : root;
      git(where, ["update-index", id.endsWith("assume-unchanged") ? "--assume-unchanged" : "--skip-worktree", "--", "tracked file.txt"]);
      change(where); break;
    }
    default: break;
  }
  const changes = [], childChanges = [], nestedChanges = [];
  const record = (where, path, status, origin = null, mode = "100644") => ({ path, status, origin,
    blob: git(where, ["-c", `core.autocrlf=${id.startsWith("root-autocrlf-") ? "true" : "false"}`,
      "hash-object", "--", path]), mode });
  switch (id) {
    case "root-dirty": case "root-autocrlf-dirty": changes.push(record(root, "tracked file.txt", " M")); break;
    case "root-untracked": changes.push(record(root, "new file.txt", "??")); break;
    case "root-delete": changes.push({ path: "tracked file.txt", status: " D", origin: null, blob: null, mode: null }); break;
    case "staged-delete-present":
      changes.push(record(root, "tracked file.txt", "??"), record(root, "tracked file.txt", "D ")); break;
    case "root-mode": changes.push(record(root, "tracked file.txt", " M", null, "100755")); break;
    case "root-symlink": changes.push({ path: "link", status: "??", origin: null,
      blob: git(root, ["hash-object", "--stdin"], undefined, "ignored/result.txt"), mode: "120000" }); break;
    case "root-leading-quote": changes.push(record(root, '\"quoted.txt', "??")); break;
    case "rename": changes.push(record(root, "renamed file.txt", "R ", "tracked file.txt")); break;
    case "child-dirty": case "child-off-pin-dirty": childChanges.push(record(child, "tracked file.txt", " M")); break;
    case "child-long-path-dirty": childChanges.push(record(child, longTracked.replaceAll("\\", "/"), " M")); break;
    case "child-untracked": case "child-hide-untracked": childChanges.push(record(child, "new file.txt", "??")); break;
    case "nested-untracked": nestedChanges.push(record(nested, "new file.txt", "??")); break;
    default: break;
  }
  const submodules = [];
  if (scenario.status !== "unavailable" && scenario.layout !== "plain") {
    submodules.push({ path: childName, commit: git(child, ["rev-parse", "HEAD"]), changes: childChanges });
    if (scenario.layout === "nested") submodules.push({ path: `${childName}/n`, commit: git(nested, ["rev-parse", "HEAD"]), changes: nestedChanges });
  }
  return { ...scenario, root, child, childName, nested, commit, tree, longTracked,
    sourceId: scenario.status === "unavailable" ? null : { commit, tree, submodules, changes } };
}

// Machine policy is opt-in for callers: pass the returned environment to the
// owner process, including its scratch directory, without changing host config.
export const machineEolCases = [
  { id: "global-true", autocrlf: "true" },
  { id: "system-true", autocrlf: "true", systemOnly: true },
  { id: "xdg-and-home", autocrlf: "true", xdg: true },
  { id: "xdg-only", autocrlf: "true", xdgOnly: true },
  { id: "global-input", autocrlf: "input" },
  { id: "global-false", autocrlf: "false" },
  ...["lf", "crlf", "native"].map((eol) => ({ id: `global-eol-${eol}`, autocrlf: "false", eol })),
  { id: "global-eol-uppercase", autocrlf: "false", eol: "CRLF" },
  { id: "root-false-child-global-true", autocrlf: "true", child: true, rootLocal: "false" },
  { id: "root-global-true-child-false", autocrlf: "true", child: true, childLocal: "false" },
  { id: "root-global-false-child-true", autocrlf: "false", child: true, childLocal: "true" },
  { id: "nested-global-true", autocrlf: "true", nested: true },
  { id: "global-long-path-clean", autocrlf: "true", child: true, longPath: true },
  { id: "global-long-path-dirty", autocrlf: "true", child: true, longPath: true, dirty: "long-child", status: "not_clean" },
  { id: "global-root-content-change", autocrlf: "true", dirty: "root", status: "not_clean" },
  { id: "global-child-content-change", autocrlf: "true", child: true, dirty: "child", status: "not_clean" },
  { id: "global-context-mismatch", autocrlf: "true", child: true, mismatch: true, status: "unavailable" },
  { id: "global-invalid", autocrlf: "true", invalid: true, status: "unavailable" },
  { id: "global-scratch-inside-root", autocrlf: "true", insideScratch: true, status: "unavailable" },
  { id: "unrelated-global-policy-isolated", autocrlf: "true", unrelated: true, status: "not_clean" },
].map((entry) => ({ status: "clean", ...entry }));

export function createMachineEolFixture(parent, id) {
  const scenario = machineEolCases.find((entry) => entry.id === id);
  if (!scenario || !isAbsolute(parent)) throw new Error(`invalid machine EOL fixture ${id}`);
  const layout = scenario.longPath ? "child-long-path-clean" : scenario.nested ? "nested-clean" : scenario.child ? "child-clean" : "clean";
  const fixture = createSourceFixture(parent, layout);
  const base = dirname(fixture.root);
  const home = join(base, "h");
  const xdg = join(base, "x");
  const scratch = join(base, "tmp");
  for (const path of [home, join(xdg, "git"), scratch]) mkdirSync(path, { recursive: true });
  const global = join(home, ".gitconfig");
  const system = join(base, "system.config");
  const policy = `[core]\nautocrlf = ${scenario.autocrlf}\n${scenario.eol ? `eol = ${scenario.eol}\n` : ""}`;
  writeFileSync(system, scenario.systemOnly ? policy : "[core]\nautocrlf = false\n");
  writeFileSync(global, scenario.systemOnly || scenario.xdgOnly ? "" : policy);
  if (scenario.xdg) writeFileSync(join(xdg, "git", "config"), "[core]\nautocrlf = false\neol = lf\n");
  if (scenario.xdgOnly) writeFileSync(join(xdg, "git", "config"), policy);
  const environment = { HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: xdg,
    GIT_CONFIG_NOSYSTEM: "0", GIT_CONFIG_SYSTEM: system, GIT_CONFIG_GLOBAL: undefined,
    TMPDIR: scratch, TMP: scratch, TEMP: scratch };
  const repositories = [fixture.root, ...(scenario.child || scenario.nested ? [fixture.child] : []),
    ...(scenario.nested ? [fixture.nested] : [])];
  if (scenario.eol) {
    for (const root of repositories) {
      writeFileSync(join(root, ".gitattributes"), "* text=auto\n");
      git(root, ["add", ".gitattributes"]);
      git(root, ["commit", "--no-gpg-sign", "-qm", "Set text conversion policy"]);
    }
  }
  if (scenario.rootLocal) git(fixture.root, ["config", "core.autocrlf", scenario.rootLocal]);
  if (scenario.childLocal) git(fixture.child, ["config", "core.autocrlf", scenario.childLocal]);
  if (scenario.longPath) {
    for (const root of repositories) git(root, ["config", "core.safecrlf", "warn"]);
  }
  // Real checkout performs conversion; changing only timestamps afterwards
  // defeats the index-stat shortcut without editing bytes or refreshing it.
  const checkoutBytes = [];
  let longCheckoutBytes;
  for (const root of repositories) {
    for (const name of ["tracked file.txt", ".gitignore", ".gitmodules", ".gitattributes",
      ...(scenario.longPath && root === fixture.child ? [fixture.longTracked] : [])]) {
      if (existsSync(join(root, name))) unlinkSync(join(root, name));
    }
    git(root, ["-c", "core.safecrlf=false", "checkout", "--", "."], environment);
    const path = join(root, "tracked file.txt");
    checkoutBytes.push(readFileSync(path, "utf8"));
    utimesSync(path, new Date("2001-01-01T00:00:00Z"), new Date("2001-01-01T00:00:00Z"));
    if (scenario.longPath && root === fixture.child) {
      const longPath = join(root, fixture.longTracked);
      longCheckoutBytes = readFileSync(longPath, "utf8");
      utimesSync(longPath, new Date("2001-01-01T00:00:00Z"), new Date("2001-01-01T00:00:00Z"));
    }
  }
  const ordinaryStatus = git(fixture.root, ["--no-optional-locks", "status", "--porcelain=v1", "--ignore-submodules=none"], environment);
  const isolatedStatus = git(fixture.root, ["--no-optional-locks", "status", "--porcelain=v1", "--ignore-submodules=none"]);
  if (scenario.dirty === "long-child") writeFileSync(join(fixture.child, fixture.longTracked), "changed actual long content\r\n");
  else if (scenario.dirty) writeFileSync(join(scenario.dirty === "child" ? fixture.child : fixture.root,
    "tracked file.txt"), "changed actual content\r\n");
  const dirtyPath = scenario.dirty === "long-child" ? fixture.longTracked.replaceAll("\\", "/") : "tracked file.txt";
  const expectedDirtyBlob = scenario.dirty ? git(scenario.dirty === "root" ? fixture.root : fixture.child,
    ["-c", "core.safecrlf=false", "hash-object", "--", dirtyPath], environment) : undefined;
  let contextualFallback;
  if (scenario.mismatch) {
    const childAdmin = git(fixture.child, ["rev-parse", "--absolute-git-dir"]).replaceAll("\\", "/");
    const childConfig = join(base, "child.config");
    writeFileSync(childConfig, "[core]\nautocrlf = false\n");
    // Match this admin directory itself, rather than repositories below it.
    writeFileSync(global, `${policy}[includeIf "gitdir/i:${childAdmin}"]\npath = ${childConfig.replaceAll("\\", "/")}\n`);
    contextualFallback = {
      root: git(fixture.root, ["config", "--get", "core.autocrlf"], environment),
      child: git(fixture.child, ["config", "--get", "core.autocrlf"], environment),
    };
  }
  if (scenario.invalid) writeFileSync(global, "[core]\nautocrlf = unrepresentable\n");
  if (scenario.unrelated) {
    const excludes = join(base, "excludes");
    writeFileSync(excludes, "untracked.txt\n");
    writeFileSync(global, `${policy}[core]\nfsmonitor = must-not-run\nexcludesFile = ${excludes.replaceAll("\\", "/")}\n[filter "outside"]\nprocess = must-not-run\n`);
    writeFileSync(join(fixture.root, "untracked.txt"), "must remain visible\n");
  }
  if (scenario.insideScratch) {
    const inside = join(fixture.root, "ignored", "scratch");
    mkdirSync(inside, { recursive: true });
    Object.assign(environment, { TMP: inside, TEMP: inside, TMPDIR: inside });
  }
  return { ...fixture, ...scenario, environment, scratch, ordinaryStatus, isolatedStatus, checkoutBytes, contextualFallback, longCheckoutBytes,
    dirtyPath, expectedDirtyBlob,
    scratchEntries: () => readdirSync(scratch) };
}
