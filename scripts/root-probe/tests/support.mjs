import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { writeFileSync, rmSync } from "node:fs";
import { fixtureHome } from "../../test-temp.mjs";

export function createTestSupport({ RootJournal, WindowsAclReader }) {
function silentWatch() {
  const watcher = new EventEmitter();
  watcher.close = () => {};
  return watcher;
}

function fakeJournalStartup(t, { onWrite, onRemove } = {}) {
  const directory = fixtureHome("rpj-startup-", t);
  const watcher = silentWatch();
  let callback;
  let now = 0;
  const writes = [];
  const removals = [];
  const journal = new RootJournal(directory, {
    watchFn: (_directory, _options, report) => {
      callback = report;
      return watcher;
    },
    startupWrite: (path, payload, options) => {
      writes.push({ path, payload, options });
      if (onWrite) onWrite({ path, payload, options, report: callback, writes });
      else writeFileSync(path, payload, options);
    },
    startupRemove: (path) => {
      removals.push(path);
      if (onRemove) onRemove(path);
      else rmSync(path);
    },
    startupNow: () => now,
  });
  t.after(() => journal.close());
  return { directory, journal, watcher, writes, removals,
    report: (...args) => callback(...args), setNow: (value) => { now = value; } };
}

function fakeAclReader(t, { killCloses = true, endCloses = true } = {}) {
  const directory = fixtureHome("rpj-reader-", t);
  const child = new EventEmitter();
  child.stdin = new EventEmitter();
  child.stdout = new EventEmitter();
  child.stderr = new EventEmitter();
  const calls = [];
  let kills = 0;
  let spawns = 0;
  child.stdin.write = (request) => { calls.push(request); };
  child.stdin.end = (request) => {
    calls.push(request);
    if (endCloses) queueMicrotask(() => child.emit("close", 0, null));
  };
  child.kill = () => {
    kills += 1;
    if (killCloses) queueMicrotask(() => child.emit("close", null, "SIGTERM"));
    return killCloses;
  };
  const reader = new WindowsAclReader(directory, {
    spawnImpl(program, args, options) {
      spawns += 1;
      assert.equal(program, "pwsh");
      assert.ok(args.includes("-NonInteractive"));
      assert.match(args.at(-1), /GetSecurityDescriptorSddlForm\('Access'\)/u);
      assert.equal(options.env.ROOT_PROBE_ACL_PATH, directory);
      return child;
    },
  });
  return { reader, directory, child, calls, get kills() { return kills; }, get spawns() { return spawns; } };
}

const drainAclEvents = () => new Promise((resolveDrain) => setImmediate(resolveDrain));
  return { silentWatch, fakeJournalStartup, fakeAclReader, drainAclEvents };
}
