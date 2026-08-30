import assert from "node:assert/strict";
import test from "node:test";

import { completeBridgeStartup } from "../src/bridgeStartup.ts";

test("an optional window query cannot block the ready handshake", async () => {
  const calls: string[] = [];

  await completeBridgeStartup({
    announceReady: async () => {
      calls.push("ready");
    },
    queryOptionalValue: async () => {
      calls.push("scale-factor");
      throw new Error("window unavailable");
    },
    applyOptionalValue: () => {
      calls.push("apply");
    },
    reportOptionalError: () => {
      calls.push("scale-factor-error");
    },
    isCancelled: () => false,
  });

  assert.deepEqual(calls, ["ready", "scale-factor", "scale-factor-error"]);
});

test("cancellation after readiness skips the optional window query", async () => {
  let cancelled = false;
  let queried = false;

  await completeBridgeStartup({
    announceReady: async () => {
      cancelled = true;
    },
    queryOptionalValue: async () => {
      queried = true;
      return 1;
    },
    applyOptionalValue: () => {},
    reportOptionalError: () => {},
    isCancelled: () => cancelled,
  });

  assert.equal(queried, false);
});
