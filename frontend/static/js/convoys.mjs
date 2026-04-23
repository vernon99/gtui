export function convoyTaskIsFullyCompleted(entry) {
  if (!entry || typeof entry !== "object") return false;
  if (entry.all_closed === true) return true;
  if (entry.all_closed === false) return false;

  const total = Number(entry.total || 0);
  const open = Number(entry.open || 0);
  const closed = Number(entry.closed || 0);
  return total > 0 && open === 0 && closed > 0;
}

export function hiddenCompletedTaskIdsFromSnapshot(snapshot, hideCompletedConvoys = true) {
  if (!hideCompletedConvoys) return new Set();

  const taskIndex = snapshot?.convoys?.task_index || {};
  const hidden = new Set();
  for (const [taskId, entry] of Object.entries(taskIndex)) {
    if (convoyTaskIsFullyCompleted(entry)) {
      hidden.add(taskId);
    }
  }
  return hidden;
}
