// A real loopback TLS smart-HTTP endpoint. Git remains the protocol server;
// this fixture only supplies CGI framing and a deterministic competing push.
import https from "node:https";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const root = path.resolve(process.argv[2]);
const children = new Set();
const run = (args, options = {}) => new Promise((resolve, reject) => {
  const child = spawn("git", args, { cwd: root, windowsHide: true, ...options });
  children.add(child);
  const output = [];
  const errors = [];
  child.stdout.on("data", chunk => output.push(chunk));
  child.stderr.on("data", chunk => errors.push(chunk));
  child.on("error", reject);
  child.on("close", code => {
    children.delete(child);
    if (code !== 0) reject(new Error(`fixture Git exited ${code}: ${Buffer.concat(errors)}`));
    else resolve(Buffer.concat(output));
  });
  if (options.input) child.stdin.end(options.input);
  else child.stdin.end();
});

const server = https.createServer({
  key: fs.readFileSync(path.join(root, "key.pem")),
  cert: fs.readFileSync(path.join(root, "cert.pem")),
}, async (request, response) => {
  try {
    const url = new URL(request.url, "https://127.0.0.1");
    if (!["/data.git/info/refs", "/data.git/git-upload-pack", "/data.git/git-receive-pack"].includes(url.pathname)) {
      response.writeHead(404).end();
      return;
    }
    const chunks = [];
    let length = 0;
    for await (const chunk of request) {
      length += chunk.length;
      if (length > 8 * 1024 * 1024) throw new Error("fixture request exceeds bound");
      chunks.push(chunk);
    }
    const body = Buffer.concat(chunks);
    const raceFile = path.join(root, "race.json");
    if (request.method === "GET" && url.searchParams.get("service") === "git-receive-pack" && fs.existsSync(raceFile)) {
      const race = JSON.parse(fs.readFileSync(raceFile, "utf8"));
      fs.renameSync(raceFile, path.join(root, "race-fired.json"));
      await run(["-c", "protocol.file.allow=always", "push", "--porcelain", "--", path.join(root, "data.git"), `${race.commit}:refs/heads/engram-data`], {
        cwd: race.repository,
        env: { ...process.env, GIT_ALLOW_PROTOCOL: "file" },
      });
    }
    const bytes = await run(["http-backend"], {
      input: body,
      env: {
        ...process.env,
        GIT_PROJECT_ROOT: root,
        GIT_HTTP_EXPORT_ALL: "1",
        PATH_INFO: url.pathname,
        QUERY_STRING: url.search.slice(1),
        REQUEST_METHOD: request.method,
        CONTENT_TYPE: request.headers["content-type"] ?? "",
        CONTENT_LENGTH: String(body.length),
        REMOTE_USER: "fixture",
        REMOTE_ADDR: "127.0.0.1",
        SERVER_PROTOCOL: "HTTP/1.1",
        HTTP_GIT_PROTOCOL: request.headers["git-protocol"] ?? "",
      },
    });
    const split = bytes.indexOf("\r\n\r\n");
    if (split < 0) throw new Error("Git CGI response has no header boundary");
    let status = 200;
    const headers = {};
    for (const line of bytes.subarray(0, split).toString("utf8").split("\r\n")) {
      const colon = line.indexOf(":");
      if (colon < 0) throw new Error("malformed Git CGI header");
      const key = line.slice(0, colon);
      const value = line.slice(colon + 1).trim();
      if (key.toLowerCase() === "status") status = Number(value.split(" ")[0]);
      else headers[key] = value;
    }
    fs.appendFileSync(path.join(root, "requests"), `${request.method} ${url.pathname}\n`);
    response.writeHead(status, headers).end(bytes.subarray(split + 4));
  } catch (error) {
    fs.appendFileSync(path.join(root, "server-errors"), `${error.stack}\n`);
    response.writeHead(500).end("fixture backend failed");
  }
});
server.requestTimeout = 60_000;
// A later child of the same fixture serves the same endpoint again.
server.listen(Number(process.argv[3] ?? 0), "127.0.0.1", () => {
  // Close the complete record before making its final name visible.
  const temporary = path.join(root, "endpoint.tmp");
  fs.writeFileSync(temporary, `https://127.0.0.1:${server.address().port}/data.git\n`);
  fs.renameSync(temporary, path.join(root, "endpoint"));
});
process.on("SIGTERM", () => {
  for (const child of children) child.kill();
  server.close(() => process.exit(0));
});
