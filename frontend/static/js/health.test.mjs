import test from "node:test";
import assert from "node:assert/strict";

import { describeSnapshotHealth } from "./health.mjs";

test("describeSnapshotHealth reports loading when requested", () => {
  const health = describeSnapshotHealth({}, { loading: true });
  assert.equal(health.tone, "loading");
  assert.equal(health.label, "Loading");
  assert.equal(health.controlLabel, "Run GT");
});

test("describeSnapshotHealth reports live from healthy gt status and services", () => {
  const health = describeSnapshotHealth({
    generated_at: "2026-04-23T01:23:45-07:00",
    status: {
      raw: "Town: gt",
      services: [
        "daemon (PID 1)",
        "dolt (PID 2, :3307, ~/gt/.dolt-data)",
        "tmux (-L gt-abc, PID 3, 9 sessions, /tmp/tmux-501/gt-abc)",
      ],
    },
    errors: [],
  });

  assert.equal(health.tone, "live");
  assert.equal(health.label, "Live");
  assert.equal(health.operational, true);
  assert.equal(health.controlLabel, "Stop GT");
  assert.match(health.details.join("\n"), /daemon: running/);
});

test("describeSnapshotHealth reports degraded when gt status is unavailable", () => {
  const health = describeSnapshotHealth({
    status: { raw: "", services: [] },
    errors: [{ command: "gt status --fast", error: "timed out after 3.0s" }],
  });

  assert.equal(health.tone, "error");
  assert.equal(health.label, "Degraded");
  assert.equal(health.operational, false);
  assert.equal(health.controlLabel, "Run GT");
  assert.match(health.details.join("\n"), /GT status is unavailable/);
  assert.match(health.details.join("\n"), /Failed commands: gt status --fast/);
});

test("describeSnapshotHealth stays live for non-core collector errors when GT state is healthy", () => {
  const health = describeSnapshotHealth({
    status: {
      raw: "Town: gt",
      services: [
        "daemon (PID 1)",
        "dolt (PID 2, :3307, ~/gt/.dolt-data)",
        "tmux (-L gt-abc, PID 3, 9 sessions, /tmp/tmux-501/gt-abc)",
      ],
    },
    errors: [
      { command: "bd list --all --json --limit 300", error: "timed out after 6.0s" },
    ],
  });

  assert.equal(health.tone, "live");
  assert.equal(health.label, "Live");
  assert.match(health.details.join("\n"), /Failed commands: bd list --all --json --limit 300/);
});

test("describeSnapshotHealth stays live when services are healthy and a preserved gt status frame includes a status timeout", () => {
  const health = describeSnapshotHealth({
    status: {
      raw: "Town: gt",
      services: [
        "daemon (PID 1)",
        "dolt (PID 2, :3307, ~/gt/.dolt-data)",
        "tmux (-L gt-abc, PID 3, 9 sessions, /tmp/tmux-501/gt-abc)",
      ],
    },
    errors: [{ command: "gt status --fast", error: "timed out after 3.0s" }],
  });

  assert.equal(health.tone, "live");
  assert.equal(health.label, "Live");
  assert.equal(health.controlLabel, "Stop GT");
  assert.match(health.details.join("\n"), /Failed commands: gt status --fast/);
});

test("describeSnapshotHealth reports stopped when core services are stopped", () => {
  const health = describeSnapshotHealth({
    status: {
      raw: "Town: gt",
      services: [
        "daemon (stopped)",
        "dolt (PID 2, :3307, ~/gt/.dolt-data)",
        "tmux (-L gt-abc, PID 0, 0 sessions, /tmp/tmux-501/gt-abc)",
      ],
    },
    errors: [],
  });

  assert.equal(health.tone, "stopped");
  assert.equal(health.label, "Stopped");
  assert.equal(health.operational, false);
  assert.equal(health.controlLabel, "Run GT");
  assert.match(health.details.join("\n"), /daemon is stopped/);
  assert.match(health.details.join("\n"), /tmux is stopped/);
});

test("describeSnapshotHealth reports partial when only tmux is stopped", () => {
  const health = describeSnapshotHealth({
    status: {
      raw: "Town: gt",
      services: [
        "daemon (PID 1)",
        "dolt (PID 2, :3307, ~/gt/.dolt-data)",
        "tmux (-L gt-abc, PID 0, 0 sessions, /tmp/tmux-501/gt-abc)",
      ],
    },
    errors: [],
  });

  assert.equal(health.tone, "partial");
  assert.equal(health.label, "Partial");
  assert.equal(health.operational, false);
  assert.equal(health.controlAction, "run");
  assert.equal(health.controlLabel, "Restore GT");
  assert.match(health.details.join("\n"), /tmux is stopped/);
});
