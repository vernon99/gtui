const HIDE_COMPLETED_ALL = "all";
const HIDE_COMPLETED_OLDER_THAN_7_DAYS = "older_than_7_days";
const HIDE_COMPLETED_NONE = "none";
const SEVEN_DAYS_MS = 7 * 24 * 60 * 60 * 1000;

function normalizeHidePolicy(policy) {
  if (policy === true) return HIDE_COMPLETED_ALL;
  if (policy === false) return HIDE_COMPLETED_NONE;
  return policy || HIDE_COMPLETED_ALL;
}

function compactRecord(record) {
  if (!record || typeof record !== "object") return {};
  return Object.fromEntries(
    Object.entries(record).filter(([, value]) => value !== null && value !== undefined && value !== ""),
  );
}

function timestampMs(value) {
  if (typeof value !== "string" || !value.trim()) return null;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

function completionTimestampMs(item) {
  for (const key of ["closed_at", "completed_at", "updated_at", "created_at"]) {
    const ms = timestampMs(item?.[key]);
    if (ms !== null) return ms;
  }
  return null;
}

function itemIsCompleted(item) {
  if (!item || typeof item !== "object") return false;
  const status = String(item.status || "").toLowerCase();
  return status === "closed" || status === "done" || status === "completed";
}

function completedItemMatchesPolicy(item, policy, nowMs) {
  if (!itemIsCompleted(item)) return false;
  if (policy === HIDE_COMPLETED_ALL) return true;
  if (policy === HIDE_COMPLETED_OLDER_THAN_7_DAYS) {
    const completedAt = completionTimestampMs(item);
    return completedAt !== null && completedAt <= nowMs - SEVEN_DAYS_MS;
  }
  return false;
}

export function convoyIsFullyCompleted(convoy) {
  if (!convoy || typeof convoy !== "object") return false;

  const status = String(convoy.status || "").toLowerCase();
  if (status === "closed" || status === "done" || status === "completed") return true;

  const total = Number(convoy.total || 0);
  const completed = Number(convoy.completed || convoy.closed || 0);
  const hasOpenCount = convoy.open !== null && convoy.open !== undefined && convoy.open !== "";
  const open = Number(convoy.open || 0);
  return total > 0 && (completed >= total || (hasOpenCount && open === 0 && completed > 0));
}

function completedConvoyMatchesPolicy(convoy, policy, nowMs) {
  if (!convoyIsFullyCompleted(convoy)) return false;
  if (policy === HIDE_COMPLETED_ALL) return true;
  if (policy === HIDE_COMPLETED_OLDER_THAN_7_DAYS) {
    const completedAt = completionTimestampMs(convoy);
    return completedAt !== null && completedAt <= nowMs - SEVEN_DAYS_MS;
  }
  return false;
}

function externalTaskId(taskId) {
  if (typeof taskId !== "string" || !taskId.startsWith("external:")) return "";
  const parts = taskId.split(":");
  return parts[parts.length - 1] || "";
}

function taskEntryMatchesPolicy(entry, convoyRows, policy, nowMs) {
  if (!entry || typeof entry !== "object") return false;

  const convoyIds = Array.isArray(entry.convoy_ids) ? entry.convoy_ids : [];
  if (!convoyIds.length) {
    if (policy !== HIDE_COMPLETED_ALL) return false;
    if (entry.all_closed === true) return true;
    if (entry.all_closed === false) return false;
    return convoyIsFullyCompleted(entry);
  }

  return convoyIds.every((convoyId) => {
    const row = convoyRows.get(convoyId);
    if (!row) return policy === HIDE_COMPLETED_ALL && entry.all_closed === true;
    return completedConvoyMatchesPolicy(row, policy, nowMs);
  });
}

export function hiddenCompletedIdsFromSnapshot(
  snapshot,
  hideCompleted = HIDE_COMPLETED_ALL,
  nowMs = Date.now(),
) {
  const policy = normalizeHidePolicy(hideCompleted);
  if (policy === HIDE_COMPLETED_NONE) return new Set();

  const convoyRows = new Map();
  for (const convoy of snapshot?.convoys?.convoys || []) {
    const id = convoy?.id;
    if (typeof id === "string" && id) convoyRows.set(id, convoy);
  }

  const nodes = snapshot?.graph?.nodes || [];
  const taskIndex = snapshot?.convoys?.task_index || {};
  const hidden = new Set();
  for (const node of nodes) {
    const id = node?.id;
    if (typeof id !== "string" || !id) continue;

    const convoyRow = convoyRows.get(id);
    const isConvoyNode = node?.type === "convoy" || Boolean(convoyRow);
    if (isConvoyNode) {
      const convoy = { ...compactRecord(node), ...compactRecord(convoyRow) };
      if (completedConvoyMatchesPolicy(convoy, policy, nowMs)) hidden.add(id);
    }

    const convoyEntry = taskIndex[id];
    if (convoyEntry && !taskEntryMatchesPolicy(convoyEntry, convoyRows, policy, nowMs)) continue;
    if (completedItemMatchesPolicy(node, policy, nowMs)) hidden.add(id);
  }

  for (const [taskId, entry] of Object.entries(taskIndex)) {
    if (!taskEntryMatchesPolicy(entry, convoyRows, policy, nowMs)) continue;
    hidden.add(taskId);
    const normalized = externalTaskId(taskId);
    if (normalized) hidden.add(normalized);
  }

  return hidden;
}
