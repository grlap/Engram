// A local bare remote advances exactly once, when Git first asks to push.
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const repo = process.argv[2];
const pending = path.join(__dirname, "incoming.json");
function git(args, stdio) {
  const run = spawnSync("git", args, { stdio });
  if (run.error || run.status !== 0) process.exit(run.status || 1);
}
fs.appendFileSync(path.join(__dirname, "receive-count"), "receive\n");
if (fs.existsSync(pending)) {
  const { checkout, commit } = JSON.parse(fs.readFileSync(pending, "utf8"));
  git(["-C", repo, "fetch", "--no-tags", checkout, commit], "ignore");
  git(["-C", repo, "update-ref", "refs/heads/engram-data", commit], "ignore");
  fs.renameSync(pending, pending + ".done");
}
git(["receive-pack", repo], "inherit");
