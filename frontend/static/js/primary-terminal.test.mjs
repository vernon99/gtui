import test from "node:test";
import assert from "node:assert/strict";

import {
  findSnapshotPrimaryTerminalAgent,
  resolvePrimaryTerminalAgent,
  snapshotPrimaryTerminalIsDegraded,
} from "./primary-terminal.mjs";

test("findSnapshotPrimaryTerminalAgent locates mayor from snapshot agents", () => {
  const snapshot = {
    agents: [
      { target: "gtui/polecats/nux", role: "polecat" },
      { target: "mayor", role: "mayor" },
    ],
  };

  assert.deepEqual(findSnapshotPrimaryTerminalAgent(snapshot), { target: "mayor", role: "mayor" });
});

test("snapshotPrimaryTerminalIsDegraded treats empty status and missing mayor as degraded", () => {
  const snapshot = {
    status: { raw: "" },
    agents: [{ target: "gtui/polecats/nux", role: "polecat" }],
    errors: [{ command: "gt status --fast", error: "timed out after 3.0s" }],
  };

  assert.equal(snapshotPrimaryTerminalIsDegraded(snapshot), true);
});

test("resolvePrimaryTerminalAgent preserves cached mayor on degraded snapshots", () => {
  const snapshot = {
    status: { raw: "" },
    agents: [{ target: "gtui/polecats/nux", role: "polecat" }],
    errors: [{ command: "gt status --fast", error: "timed out after 3.0s" }],
  };
  const cached = { target: "mayor", role: "mayor", scope: "hq" };

  assert.deepEqual(resolvePrimaryTerminalAgent(snapshot, cached), cached);
});

test("resolvePrimaryTerminalAgent does not use cached mayor when snapshot is healthy", () => {
  const snapshot = {
    status: { raw: "town: hq" },
    agents: [{ target: "gtui/polecats/nux", role: "polecat" }],
    errors: [],
  };
  const cached = { target: "mayor", role: "mayor", scope: "hq" };

  assert.equal(resolvePrimaryTerminalAgent(snapshot, cached), null);
});
