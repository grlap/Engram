import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { chmodSync, closeSync, constants, copyFileSync, existsSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, readSync, realpathSync, rmdirSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { devNull } from "node:os";
import { once } from "node:events";
import { createServer } from "node:http";
import { basename, delimiter, dirname, isAbsolute, join, relative, resolve } from "node:path";
import test, { after, describe } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION, WINDOWS_LIMITATION } from "./review-freeze-fingerprint.mjs";
import { powerShellEnvironment, requestStop, runJobCommand, stopController, stopProtocol, withStopLock } from "./launcher-stop.mjs";
import { registerTestRun, confirmDetachedRegistration } from "./test-run-registration.mjs";
import {
  commandKind, completionMetadata, countTests, createRun, diagnostics, executable, executeRun, fileProbes, inspectFailure, machineRecord, notifyRun,
  processHost, processIdentity, recoverRun, renameWithRetry, requiredStages, sameCreation, selectsTests, startDetached, summarize,
  systemProbes, wellFormedCreated, wmiFileTime,
} from "./test-launcher.mjs";
import { fixtureHome, fixtureRoot, removeFixtureHomes, removeFixturePath, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { createSupport } from "./test-launcher-tests/support.mjs";
import { registerRegistrationTests } from "./test-launcher-tests/registration.mjs";
import { registerOwnRunWaitTests } from "./test-launcher-tests/own-run-wait.mjs";
import { registerExecutionFixtures, registerExecutionAdmission, registerExecutionEnvironment } from "./test-launcher-tests/execution.mjs";
import { registerLifecycleTests } from "./test-launcher-tests/lifecycle.mjs";
import { registerCompletionDiagnostics, registerCompletionReceipts } from "./test-launcher-tests/completion.mjs";
import { registerRecoveryLiveness, registerRecoveryProcesses } from "./test-launcher-tests/recovery.mjs";
import { registerCountTests } from "./test-launcher-tests/counts.mjs";
import { registerStopTests } from "./test-launcher-tests/stop.mjs";
import { registerPosixStopTests } from "./test-launcher-tests/posix-stop.mjs";
import { registerCommitInputTests } from "./test-launcher-tests/commit-input.mjs";
import { registerRequestMessageTests } from "./test-launcher-tests/request-message.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));
// This test file runs in its own Node test process. Isolate both explicit Git
// children and in-process fingerprint calls from developer configuration.
for (const key of Object.keys(process.env)) if ((key.toUpperCase().startsWith("GIT_") || key.toUpperCase().startsWith("TERMAL_"))) delete process.env[key];
// Restore the fixture ceiling the scrub removed: Git in a fixture never climbs
// out into this checkout.
Object.assign(process.env, { GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : devNull,
  GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0", GIT_CEILING_DIRECTORIES: fixtureRoot });
// Candidate conformance must use the binary built from this checkout, even
// when the outer launcher used an installed owner to admit its own input.
process.env.ENGRAM_SOURCE_BINARY = resolve(import.meta.dirname, "..", process.env.CARGO_TARGET_DIR || "target",
  "debug", process.platform === "win32" ? "engram.exe" : "engram");
const env = { ...process.env, TERMAL_SESSION_ID: "fixture-owner", TERMAL_CLI: process.execPath };

const support = createSupport({ env, entryUrl: import.meta.url });

// Preserve the original registration order, including interleaved responsibilities.
registerRegistrationTests(support);
registerOwnRunWaitTests(support);
registerExecutionFixtures(support);
registerLifecycleTests(support);
registerCompletionDiagnostics(support);
registerExecutionAdmission(support);
registerRecoveryLiveness(support);
registerCompletionReceipts(support);
registerExecutionEnvironment(support);
registerCountTests(support);
registerRecoveryProcesses(support);
registerStopTests(support);
registerPosixStopTests(support);
registerCommitInputTests(support);
registerRequestMessageTests(support);
