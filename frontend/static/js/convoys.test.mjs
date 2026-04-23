import test from "node:test";
import assert from "node:assert/strict";

import {
  convoyTaskIsFullyCompleted,
  hiddenCompletedTaskIdsFromSnapshot,
} from "./convoys.mjs";

test("convoyTaskIsFullyCompleted prefers explicit all_closed", () => {
  assert.equal(convoyTaskIsFullyCompleted({ all_closed: true, open: 1 }), true);
  assert.equal(convoyTaskIsFullyCompleted({ all_closed: false, open: 0, closed: 3 }), false);
});

test("hiddenCompletedTaskIdsFromSnapshot hides only fully completed convoy tasks", () => {
  const snapshot = {
    convoys: {
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
      },
    },
    graph: {
      nodes: [
        { id: "hq-open-piece", kind: "task", status: "closed" },
        { id: "hq-mixed-piece", kind: "task", status: "closed" },
        { id: "hq-done-piece", kind: "task", status: "closed" },
        { id: "hq-standalone-closed", kind: "task", status: "closed" },
      ],
    },
  };

  const hidden = hiddenCompletedTaskIdsFromSnapshot(snapshot, true);

  assert.deepEqual([...hidden].sort(), ["hq-done-piece"]);
  assert.equal(hidden.has("hq-mixed-piece"), false);
  assert.equal(hidden.has("hq-standalone-closed"), false);
});

test("hiddenCompletedTaskIdsFromSnapshot supports legacy snapshots without all_closed", () => {
  const snapshot = {
    convoys: {
      task_index: {
        "hq-legacy-done": {
          total: 1,
          open: 0,
          closed: 1,
          convoy_ids: ["hq-cv-done"],
        },
        "hq-legacy-open": {
          total: 1,
          open: 1,
          closed: 0,
          convoy_ids: ["hq-cv-open"],
        },
      },
    },
  };

  const hidden = hiddenCompletedTaskIdsFromSnapshot(snapshot, true);

  assert.deepEqual([...hidden].sort(), ["hq-legacy-done"]);
});

test("hiddenCompletedTaskIdsFromSnapshot respects the toggle", () => {
  const snapshot = {
    convoys: {
      task_index: {
        "hq-done-piece": {
          total: 1,
          open: 0,
          closed: 1,
          convoy_ids: ["hq-cv-done"],
          all_closed: true,
        },
      },
    },
  };

  assert.deepEqual([...hiddenCompletedTaskIdsFromSnapshot(snapshot, false)], []);
});
