function normalizeServiceState(services = []) {
  const state = {
    daemon: "unknown",
    dolt: "unknown",
    tmux: "unknown",
  };

  for (const rawService of services) {
    const service = String(rawService || "");
    if (service.startsWith("daemon ")) {
      state.daemon = service.includes("(stopped)") ? "stopped" : "running";
    } else if (service.startsWith("dolt ")) {
      state.dolt = service.includes("(stopped)") ? "stopped" : "running";
    } else if (service.startsWith("tmux ")) {
      state.tmux = service.includes("PID 0") || service.includes("0 sessions") ? "stopped" : "running";
    }
  }

  return state;
}

function isCoreCommandError(command) {
  const value = String(command || "");
  return value.includes("gt status --fast");
}

function coreServicesAreRunning(serviceState) {
  return ["daemon", "dolt", "tmux"].every((key) => serviceState[key] === "running");
}

export function describeSnapshotHealth(snapshot, options = {}) {
  const loading = Boolean(options.loading);
  if (loading) {
    return {
      tone: "loading",
      label: "Loading",
      details: ["Collecting first live GT snapshot."],
      operational: false,
      controlAction: "run",
      controlLabel: "Run GT",
    };
  }

  const statusRaw = String(snapshot?.status?.raw || "").trim();
  const services = Array.isArray(snapshot?.status?.services) ? snapshot.status.services : [];
  const serviceState = normalizeServiceState(services);
  const errors = Array.isArray(snapshot?.errors) ? snapshot.errors : [];
  const degradedReasons = [];
  const downReasons = [];
  const serviceDetails = [
    `daemon: ${serviceState.daemon}`,
    `dolt: ${serviceState.dolt}`,
    `tmux: ${serviceState.tmux}`,
  ];
  const details = [];

  if (!statusRaw) {
    degradedReasons.push("GT status is unavailable.");
  }

  if (serviceState.daemon === "stopped") downReasons.push("daemon is stopped.");
  if (serviceState.dolt === "stopped") downReasons.push("dolt is stopped.");
  if (serviceState.tmux === "stopped") downReasons.push("tmux is stopped.");

  const commands = errors
    .map((error) => String(error?.command || "").trim())
    .filter(Boolean);
  const coreErrorCommands = commands.filter(isCoreCommandError);

  if (!statusRaw && coreErrorCommands.length) {
    degradedReasons.push("Core GT status collection failed.");
  }

  if (commands.length) {
    const summary = commands.slice(0, 4).join(", ");
    details.push(
      `Failed commands: ${summary}${commands.length > 4 ? ", ..." : ""}`,
    );
  }

  if (snapshot?.generated_at) {
    details.push(`Snapshot: ${snapshot.generated_at}`);
  }

  const operational = coreServicesAreRunning(serviceState);
  const tone = downReasons.length ? "stopped" : degradedReasons.length ? "error" : "live";
  const label = downReasons.length ? "Stopped" : degradedReasons.length ? "Degraded" : "Live";

  return {
    tone,
    label,
    details: [...downReasons, ...degradedReasons, ...serviceDetails, ...details],
    operational,
    controlAction: operational ? "stop" : "run",
    controlLabel: operational ? "Stop GT" : "Run GT",
  };
}
