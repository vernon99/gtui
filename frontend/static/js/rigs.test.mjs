import test from "node:test";
import assert from "node:assert/strict";

import { describeRigRuntime } from "./rigs.mjs";

test("describeRigRuntime ignores idle registered polecats", () => {
  const runtime = describeRigRuntime(
    {
      name: "gastown",
      status: "operational",
      witness: "stopped",
      refinery: "stopped",
      polecats: 2,
    },
    [
      {
        target: "gastown/polecats/chrome",
        role: "polecat",
        scope: "gastown",
        runtime_state: "idle",
        has_session: false,
        polecat: { state: "idle", session_running: false },
      },
    ],
  );

  assert.equal(runtime.running, false);
  assert.equal(runtime.label, "stopped");
  assert.equal(runtime.activePolecats, 0);
});

test("describeRigRuntime treats active polecat sessions as running", () => {
  const runtime = describeRigRuntime(
    {
      name: "gastown",
      status: "operational",
      witness: "stopped",
      refinery: "stopped",
    },
    [
      {
        target: "gastown/polecats/chrome",
        role: "polecat",
        scope: "gastown",
        runtime_state: "working",
        has_session: false,
        polecat: { state: "working", session_running: true },
      },
    ],
  );

  assert.equal(runtime.running, true);
  assert.equal(runtime.label, "running");
  assert.equal(runtime.activePolecats, 1);
});

test("describeRigRuntime treats witness or refinery as running", () => {
  assert.equal(
    describeRigRuntime({ name: "gtui", status: "operational", witness: "running", refinery: "stopped" }).running,
    true,
  );
  assert.equal(
    describeRigRuntime({ name: "gtui", status: "operational", witness: "stopped", refinery: "running" }).running,
    true,
  );
});

test("describeRigRuntime blocks docked rigs", () => {
  const runtime = describeRigRuntime({ name: "gtui", status: "docked", witness: "running", refinery: "running" });

  assert.equal(runtime.running, true);
  assert.equal(runtime.blocked, true);
  assert.equal(runtime.label, "docked");
});

test("describeRigRuntime treats parked idle rigs as runnable", () => {
  const runtime = describeRigRuntime({ name: "gtui", status: "parked", witness: "stopped", refinery: "stopped" });

  assert.equal(runtime.running, false);
  assert.equal(runtime.blocked, false);
  assert.equal(runtime.label, "parked");
});
