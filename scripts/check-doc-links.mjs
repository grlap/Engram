#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");

function markdownFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      if (
        [".git", ".beads", "target", "target-verify", "node_modules"].includes(
          entry.name,
        )
      )
        return [];
      return markdownFiles(path);
    }
    return entry.isFile() && extname(entry.name) === ".md" ? [path] : [];
  });
}

function slug(text) {
  return text
    .toLocaleLowerCase("en-US")
    .replaceAll(/<[^>]*>/gu, "")
    .replaceAll(/[`*_~]/gu, "")
    .replaceAll(/[^\p{Letter}\p{Number}\s_-]/gu, "")
    .trim()
    .replaceAll(/\s/gu, "-");
}

function anchors(markdown) {
  const seen = new Map();
  const result = new Set();
  for (const match of markdown.matchAll(/^#{1,6}\s+(.+?)\s*#*\s*$/gmu)) {
    const base = slug(match[1]);
    const count = seen.get(base) ?? 0;
    seen.set(base, count + 1);
    result.add(count === 0 ? base : `${base}-${count}`);
  }
  return result;
}

function lineAt(markdown, offset) {
  return markdown.slice(0, offset).split("\n").length;
}

const failures = [];
for (const source of markdownFiles(root)) {
  const markdown = readFileSync(source, "utf8");
  for (const match of markdown.matchAll(/\[[^\]]*\]\(([^)]+)\)/gu)) {
    const destination = match[1].trim().replace(/^<|>$/gu, "");
    if (/^(?:[a-z]+:|#)/iu.test(destination)) continue;
    const [encodedPath, fragment] = destination.split("#", 2);
    const target = resolve(dirname(source), decodeURIComponent(encodedPath));
    const location = `${source.slice(root.length + 1)}:${lineAt(markdown, match.index)}`;
    if (!existsSync(target)) {
      failures.push(`${location}: missing ${destination}`);
      continue;
    }
    if (statSync(target).isFile() && fragment) {
      const targetAnchors = anchors(readFileSync(target, "utf8"));
      const decodedFragment = decodeURIComponent(fragment);
      if (!targetAnchors.has(decodedFragment)) {
        failures.push(`${location}: missing anchor #${decodedFragment} in ${encodedPath}`);
      }
    }
  }
}

if (failures.length > 0) {
  process.stderr.write(`${failures.join("\n")}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("Documentation links are valid.\n");
}
