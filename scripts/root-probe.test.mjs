#!/usr/bin/env node

// Optional local registrar. The required nine invoke these cases from
// scripts/parity.test.mjs so the gate count stays nine.

import test, { after } from "node:test";

import { registerRootProbeTests } from "./root-probe-harness.mjs";
import { assertTempClean, tempSnapshot } from "./test-temp.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

registerRootProbeTests(test);
