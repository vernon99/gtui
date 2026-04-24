import test from "node:test";
import assert from "node:assert/strict";

import {
  convoyIsFullyCompleted,
  hiddenCompletedIdsFromSnapshot,
} from "./convoys.mjs";

const NOW_MS = Date.parse("2026-04-23T12:00:00Z");

test("convoyIsFullyCompleted accepts closed status or completed totals", () => {
  assert.equal(convoyIsFullyCompleted({ status: "closed", completed: 0, total: 0 }), true);
  assert.equal(convoyIsFullyCompleted({ status: "open", completed: 2, total: 2 }), true);
  assert.equal(convoyIsFullyCompleted({ status: "open", open: 0, closed: 3, total: 3 }), true);
  assert.equal(convoyIsFullyCompleted({ status: "open", completed: 1, total: 2 }), false);
});

test("hiddenCompletedIdsFromSnapshot hides completed items without hiding mixed convoy work", () => {
  const snapshot = {
    convoys: {
      convoys: [
        { id: "hq-cv-done", status: "closed", tracked_ids: ["hq-done-piece", "external:gui:gui-done-piece"], completed: 2, total: 2 },
        { id: "hq-cv-open", status: "open", tracked_ids: ["hq-open-piece"], completed: 0, total: 1 },
      ],
      task_index: {
        "hq-open-piece": {
          total: 1,
          open: 1,
          closed: 0,
          convoy_ids: ["hq-cv-open"],
          all_closed: false,
        },
        "hq-mixed-piece": {
          total: 2,
          open: 1,
          closed: 1,
          convoy_ids: ["hq-cv-open", "hq-cv-done"],
          all_closed: false,
        },
        "hq-done-piece": {
          total: 1,
          open: 0,
          closed: 1,
          convoy_ids: ["hq-cv-done"],
          all_closed: true,
        },
        "external:gui:gui-done-piece": {
          total: 1,
          open: 0,
          closed: 1,
          convoy_ids: ["hq-cv-done"],
          all_closed: true,
        },
      },
    },
    graph: {
      nodes: [
        { id: "hq-cv-done", kind: "task", type: "convoy", status: "closed" },
        { id: "hq-cv-open", kind: "task", type: "convoy", status: "open" },
        { id: "hq-open-piece", kind: "task", type: "task", status: "open" },
        { id: "hq-mixed-piece", kind: "task", type: "task", status: "closed" },
        { id: "hq-done-piece", kind: "task", type: "task", status: "closed" },
        { id: "gui-done-piece", kind: "task", type: "task", status: "closed" },
        { id: "hq-closed-epic", kind: "task", type: "epic", status: "closed" },
        { id: "hq-standalone-closed", kind: "task", type: "task", status: "closed" },
      ],
    },
  };

  const hidden = hiddenCompletedIdsFromSnapshot(snapshot, "all", NOW_MS);

  assert.deepEqual([...hidden].sort(), [
    "external:gui:gui-done-piece",
    "gui-done-piece",
    "hq-closed-epic",
    "hq-cv-done",
    "hq-done-piece",
    "hq-standalone-closed",
  ]);
  assert.equal(hidden.has("hq-open-piece"), false);
  assert.equal(hidden.has("hq-mixed-piece"), false);
});

test("hiddenCompletedIdsFromSnapshot supports the older-than-7-days policy", () => {
  const snapshot = {
    convoys: {
      convoys: [
        { id: "hq-cv-old", status: "closed", closed_at: "2026-04-01T00:00:00Z" },
        { id: "hq-cv-recent", status: "closed", closed_at: "2026-04-20T00:00:00Z" },
        { id: "hq-cv-unknown-age", status: "closed" },
      ],
      task_index: {
        "hq-old-piece": { convoy_ids: ["hq-cv-old"], all_closed: true },
        "hq-recent-piece": { convoy_ids: ["hq-cv-recent"], all_closed: true },
        "hq-unknown-age-piece": { convoy_ids: ["hq-cv-unknown-age"], all_closed: true },
      },
    },
    graph: {
      nodes: [
        { id: "hq-cv-old", kind: "task", type: "convoy", status: "closed" },
        { id: "hq-cv-recent", kind: "task", type: "convoy", status: "closed" },
        { id: "hq-cv-unknown-age", kind: "task", type: "convoy", status: "closed" },
        { id: "hq-old-piece", kind: "task", type: "task", status: "closed" },
        { id: "hq-recent-piece", kind: "task", type: "task", status: "closed" },
        { id: "hq-unknown-age-piece", kind: "task", type: "task", status: "closed" },
        { id: "hq-old-epic", kind: "task", type: "epic", status: "closed", closed_at: "2026-04-01T00:00:00Z" },
        { id: "hq-recent-epic", kind: "task", type: "epic", status: "closed", closed_at: "2026-04-20T00:00:00Z" },
      ],
    },
  };

  const hidden = hiddenCompletedIdsFromSnapshot(snapshot, "older_than_7_days", NOW_MS);

  assert.deepEqual([...hidden].sort(), ["hq-cv-old", "hq-old-epic", "hq-old-piece"]);
});

test("hiddenCompletedIdsFromSnapshot respects none and legacy boolean policies", () => {
  const snapshot = {
    convoys: {
      convoys: [{ id: "hq-cv-done", status: "closed" }],
    },
    graph: {
      nodes: [{ id: "hq-cv-done", kind: "task", type: "convoy", status: "closed" }],
    },
  };

  assert.deepEqual([...hiddenCompletedIdsFromSnapshot(snapshot, "none", NOW_MS)], []);
  assert.deepEqual([...hiddenCompletedIdsFromSnapshot(snapshot, false, NOW_MS)], []);
  assert.deepEqual([...hiddenCompletedIdsFromSnapshot(snapshot, true, NOW_MS)], ["hq-cv-done"]);
});
