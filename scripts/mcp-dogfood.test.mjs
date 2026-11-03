#!/usr/bin/env node

import assert from "node:assert/strict";
import { observeSource } from "./work-session-fixture.mjs";
import { createHash, randomUUID } from "node:crypto";

import { fixtureHome as ownedFixtureHome, removeFixtureHomes as cleanupFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import nodeTest, { after } from "node:test";
import { CLOSE_BUDGET_MS, HostLoadSamples, McpCloseObservation } from "./mcp-close.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

import { assertTerseShow } from "./terse-show-assertions.mjs";

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");

import { createSupport } from "./mcp-dogfood/support.mjs";
import { registerTimingDiagnostics, registerCloseDiagnostics, registerPhaseDiagnostics, registerArgumentRefusal, registerReadOnlySurface, registerBuildIdentity, registerUnknownArguments } from "./mcp-dogfood/transport.mjs";
import { registerTextRendering, registerAttributionAndEvidence } from "./mcp-dogfood/rendering.mjs";
import { registerWaiverAndSuccessorTests, registerReleaseRecovery, registerLandingRecords, registerLandingDiagnostics, registerFocusTargeting, registerDecompositionRetry, registerDetachedAndAncestorWork, registerPhoenixCreation, registerPhoenixPlanning, registerNestedStartup, registerAmbientLifecycle } from "./mcp-dogfood/lifecycle.mjs";
import { registerStatusRecovery, registerBlockerDiscovery, registerPeekAndMemory, registerParentContext, registerListingAndRecovery, registerShortAndBlockedListing } from "./mcp-dogfood/discovery-memory.mjs";
import { registerTaskSyncSettings, registerFileIntakeAndReplay } from "./mcp-dogfood/settings-import.mjs";
import { registerCriterionLinks, registerAcceptanceEvaluation, registerEvaluationHistory } from "./mcp-dogfood/evaluation.mjs";

const support = createSupport({ root, target, binary });

// Keep the original scenario registration order across responsibility modules.
registerTimingDiagnostics(support);
registerCloseDiagnostics(support);
registerPhaseDiagnostics(support);
registerTextRendering(support);
registerWaiverAndSuccessorTests(support);
registerStatusRecovery(support);
registerArgumentRefusal(support);
registerReleaseRecovery(support);
registerAttributionAndEvidence(support);
registerTaskSyncSettings(support);
registerReadOnlySurface(support);
registerLandingRecords(support);
registerBlockerDiscovery(support);
registerLandingDiagnostics(support);
registerCriterionLinks(support);
registerPeekAndMemory(support);
registerFocusTargeting(support);
registerParentContext(support);
registerDecompositionRetry(support);
registerListingAndRecovery(support);
registerDetachedAndAncestorWork(support);
registerBuildIdentity(support);
registerPhoenixCreation(support);
registerUnknownArguments(support);
registerShortAndBlockedListing(support);
registerPhoenixPlanning(support);
registerFileIntakeAndReplay(support);
registerNestedStartup(support);
registerAcceptanceEvaluation(support);
registerAmbientLifecycle(support);
registerEvaluationHistory(support);
