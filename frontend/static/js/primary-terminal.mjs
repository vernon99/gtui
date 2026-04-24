export function findSnapshotPrimaryTerminalAgent(snapshot) {
  const agents = snapshot?.agents || [];
  return agents.find((agent) => agent.role === "mayor" || agent.target === "mayor") || null;
}

export function snapshotPrimaryTerminalIsDegraded(snapshot) {
  if (!snapshot || typeof snapshot !== "object") return false;
  if (findSnapshotPrimaryTerminalAgent(snapshot)) return false;

  const statusRaw = String(snapshot?.status?.raw || "").trim();
  if (!statusRaw) return true;

  const errors = Array.isArray(snapshot?.errors) ? snapshot.errors : [];
  return errors.some((error) => {
    const command = String(error?.command || "");
    return command.includes("gt status") || command.includes("tmux");
  });
}

export function resolvePrimaryTerminalAgent(snapshot, cachedAgent) {
  const liveAgent = findSnapshotPrimaryTerminalAgent(snapshot);
  if (liveAgent) return liveAgent;
  if (cachedAgent && snapshotPrimaryTerminalIsDegraded(snapshot)) return cachedAgent;
  return null;
}
