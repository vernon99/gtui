function normalize(value) {
  return String(value || "").trim().toLowerCase();
}

function polecatState(agent) {
  return normalize(agent?.runtime_state || agent?.polecat?.state);
}

function polecatMatchesRig(agent, rigName) {
  if (normalize(agent?.role) !== "polecat") return false;
  const scope = normalize(agent?.scope);
  const target = normalize(agent?.target);
  return scope === rigName || target.startsWith(`${rigName}/polecats/`);
}

function polecatIsActive(agent) {
  if (agent?.has_session === true || agent?.polecat?.session_running === true) return true;
  return ["working", "done", "stuck"].includes(polecatState(agent));
}

export function describeRigRuntime(rig, agents = []) {
  if (!rig) return { label: "", running: false, blocked: false, activePolecats: 0 };

  const name = normalize(rig.name || rig.scope);
  const status = normalize(rig.status);
  const witness = normalize(rig.witness);
  const refinery = normalize(rig.refinery);
  const activePolecats = Array.isArray(agents)
    ? agents.filter((agent) => polecatMatchesRig(agent, name) && polecatIsActive(agent)).length
    : 0;
  const running = witness === "running" || refinery === "running" || activePolecats > 0;

  if (status === "docked") {
    return { label: status, running, blocked: true, activePolecats };
  }
  if (running) {
    return { label: "running", running: true, blocked: false, activePolecats };
  }
  return {
    label: status === "operational" ? "stopped" : (status || "stopped"),
    running: false,
    blocked: false,
    activePolecats,
  };
}
