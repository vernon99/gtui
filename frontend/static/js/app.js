import { esc } from "./renderers/html.js";
import { hiddenCompletedTaskIdsFromSnapshot } from "./convoys.mjs";
import { describeSnapshotHealth } from "./health.mjs";
import {
  findSnapshotPrimaryTerminalAgent,
  resolvePrimaryTerminalAgent,
  snapshotPrimaryTerminalIsDegraded,
} from "./primary-terminal.mjs";
import {
  getTranscriptView,
  hasTranscriptItems,
  renderPrimaryTranscript,
  transcriptBadgeText,
  transcriptLabel,
  transcriptTitleNoun,
} from "./renderers/index.js";

    function syncWindowChrome() {
      const platform = navigator.userAgentData?.platform || navigator.platform || navigator.userAgent || "";
      document.documentElement.classList.toggle("tauri-macos", Boolean(window.__TAURI__) && /mac/i.test(platform));
    }

    syncWindowChrome();

    const { invoke } = window.__TAURI__.core;
    const currentWindow = window.__TAURI__.window?.getCurrentWindow?.() ?? null;

    const PRIMARY_TERMINAL_FETCH_TIMEOUT_MS = 6000;
    const PRIMARY_TERMINAL_POLL_MS = 5000;
    const SNAPSHOT_POLL_MS = 5000;
    const PRIMARY_SELECTION_FREEZE_MS = 2500;

    const app = {
      snapshot: null,
      primaryTerminal: null,
      primaryTerminalInFlight: false,
      primaryTerminalRequestId: 0,
      primaryTerminalFetchStartedAt: 0,
      primaryTerminalDataKey: "",
      primaryTerminalRenderedKey: "",
      lastPrimaryTerminalAgent: null,
      primaryLogPinnedBottom: true,
      primaryLogScrollTop: 0,
      tmuxLogScrollStates: new Map(),
      primaryPointerSelecting: false,
      primarySelectionFrozen: false,
      primarySelectionFreezeUntil: 0,
      openDetails: new Set(),
      graphPan: {
        active: false,
        pointerId: null,
        startX: 0,
        startY: 0,
        startScrollLeft: 0,
        startScrollTop: 0,
        moved: false,
        downNodeId: "",
      },
      suppressGraphClick: false,
      selectedNodeId: null,
      selectedScope: "all",
      includeSystem: false,
      hideCompletedConvoys: true,
      primaryInjectDraft: "",
      primarySending: false,
      gtControlInFlight: false,
      gtControlAction: "",
      activeTab: "mayor",
      bootStartedMs: Date.now(),
      lastSuccessMs: 0,
      inFlight: false,
      diffCache: new Map(),
      diffKey: "",
      toastTimer: null,
    };

    const metricConfig = [
      { key: "hooked_tasks", label: "Hooked", cls: "accent-running", sub: (v) => v ? "attached to live agent hooks" : "nothing currently hooked" },
      { key: "in_progress_tasks", label: "In Progress", cls: "accent-running", sub: (v) => v ? "explicitly claimed tasks" : "no claimed tasks" },
      { key: "blocked_tasks", label: "Blocked", cls: "accent-stuck", sub: (v) => v ? "blocked by dependencies" : "nothing blocked" },
      { key: "open_tasks", label: "Open", cls: "accent-ready", sub: (v) => v ? "available/open tasks" : "no open tasks visible" },
      { key: "closed_tasks", label: "Closed", cls: "accent-done", sub: (v) => v ? "completed tasks" : "no closed tasks visible" },
      { key: "deferred_tasks", label: "Deferred", cls: "accent-ice", sub: (v) => v ? "deliberately on ice" : "nothing deferred" },
      { key: "pinned_tasks", label: "Pinned", cls: "accent-ice", sub: (v) => v ? "persistent reference tasks" : "nothing pinned" },
      { key: "active_agents", label: "Agents", cls: "accent-ready", sub: (v) => `${v} core live sessions visible` },
      { key: "active_polecats", label: "Polecats", cls: "accent-memory", sub: (v) => `${v} worker cats visible` },
      { key: "command_errors", label: "Errors", cls: "accent-stuck", sub: (v) => v ? "poll degraded" : "clean polling cycle" },
    ];

    function bindWindowControls() {
      const bind = (id, handler) => {
        const element = document.getElementById(id);
        if (!element) return;
        element.addEventListener("click", async (event) => {
          event.preventDefault();
          event.stopPropagation();
          if (!currentWindow) return;
          try {
            await handler();
          } catch (error) {
            console.error(`window control ${id} failed`, error);
          }
        });
      };

      bind("window-close", () => currentWindow.close());
      bind("window-minimize", () => currentWindow.minimize());
      bind("window-zoom", () => currentWindow.toggleMaximize());
    }

    bindWindowControls();

    function syncTabs() {
      document.querySelectorAll("[data-tab-target]").forEach((button) => {
        const active = button.dataset.tabTarget === app.activeTab;
        button.classList.toggle("active", active);
        button.setAttribute("aria-selected", active ? "true" : "false");
        button.tabIndex = active ? 0 : -1;
      });
      document.querySelectorAll("[data-tab-panel]").forEach((panel) => {
        const active = panel.dataset.tabPanel === app.activeTab;
        panel.hidden = !active;
        panel.classList.toggle("active", active);
      });
      document.body.classList.toggle("mayor-tab-active", app.activeTab === "mayor");
      if (app.activeTab === "mayor") {
        window.requestAnimationFrame(() => restorePrimaryLogState());
      }
    }

    function selectTab(tab) {
      const panel = [...document.querySelectorAll("[data-tab-panel]")].find((item) => item.dataset.tabPanel === tab);
      if (!panel) return;
      app.activeTab = tab;
      syncTabs();
    }

    function formatTime(value) {
      if (!value) return "Unknown";
      const date = new Date(value);
      if (Number.isNaN(date.getTime())) return value;
      return new Intl.DateTimeFormat([], {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }).format(date);
    }

    function timeAgo(value) {
      if (!value) return "unknown";
      const date = new Date(value);
      if (Number.isNaN(date.getTime())) return value;
      const seconds = Math.max(0, Math.floor((Date.now() - date.getTime()) / 1000));
      if (seconds < 2) return "just now";
      if (seconds < 60) return `${seconds}s ago`;
      const minutes = Math.floor(seconds / 60);
      if (minutes < 60) return `${minutes}m ago`;
      const hours = Math.floor(minutes / 60);
      if (hours < 24) return `${hours}h ago`;
      return `${Math.floor(hours / 24)}d ago`;
    }

    function nodeStatusOrder(status) {
      return { running: 0, stuck: 1, ready: 2, ice: 3, done: 4, memory: 5 }[status] ?? 9;
    }

    function exactStatusTone(status) {
      return {
        hooked: "running",
        in_progress: "running",
        blocked: "stuck",
        open: "ready",
        closed: "done",
        deferred: "ice",
        pinned: "ice",
      }[status] || "";
    }

    function nodeTone(node) {
      if (!node) return "ready";
      if (node.kind === "commit") return "memory";
      return exactStatusTone(node.status) || "ready";
    }

    function normalizeScope(scope) {
      if (!scope || scope === "all") return "";
      if (scope === "town") return "hq";
      return String(scope);
    }

    function matchesScope(scope) {
      if (app.selectedScope === "all") return true;
      return normalizeScope(scope) === app.selectedScope;
    }

    function syncScopeSelector() {
      const select = document.getElementById("scope-select");
      const scopes = new Set(["hq"]);
      const snapshot = app.snapshot || {};
      (snapshot.graph?.nodes || []).forEach((node) => {
        const scope = normalizeScope(node.scope);
        if (scope) scopes.add(scope);
      });
      (snapshot.agents || []).forEach((agent) => {
        const scope = normalizeScope(agent.scope);
        if (scope) scopes.add(scope);
      });
      (snapshot.crews || []).forEach((crew) => {
        const scope = normalizeScope(crew.rig);
        if (scope) scopes.add(scope);
      });
      (snapshot.stores || []).forEach((store) => {
        const scope = normalizeScope(store.scope);
        if (scope) scopes.add(scope);
      });
      (snapshot.git?.repos || []).forEach((repo) => {
        (repo.scopes || [repo.scope]).forEach((scope) => {
          const normalized = normalizeScope(scope);
          if (normalized) scopes.add(normalized);
        });
      });

      const options = ["all", ...[...scopes].sort((a, b) => {
        if (a === "hq") return -1;
        if (b === "hq") return 1;
        return a.localeCompare(b);
      })];
      if (app.selectedScope !== "all" && !options.includes(app.selectedScope)) {
        app.selectedScope = "all";
      }
      select.innerHTML = options.map((scope) => `
        <option value="${esc(scope)}" ${scope === app.selectedScope ? "selected" : ""}>
          ${esc(scope === "all" ? "All" : scope === "hq" ? "HQ" : scope)}
        </option>
      `).join("");
    }

    function visibleGraphNodes() {
      const nodes = app.snapshot?.graph?.nodes || [];
      const hiddenTasks = hiddenCompletedTaskIds();
      return nodes.filter((node) => {
        if (!matchesScope(node.scope)) return false;
        if (!(app.includeSystem || !node.is_system)) return false;
        if (node.kind === "task" && hiddenTasks.has(node.id)) return false;
        if (node.kind === "commit" && node.parent && hiddenTasks.has(node.parent)) return false;
        return true;
      });
    }

    function visibleAgents(includePolecats = true) {
      const agents = app.snapshot?.agents || [];
      return agents.filter((agent) => {
        if (!matchesScope(agent.scope)) return false;
        if (!includePolecats && agent.role === "polecat") return false;
        return true;
      });
    }

    function visiblePolecats() {
      return visibleAgents(true).filter((agent) => agent.role === "polecat");
    }

    function visibleFeedGroups() {
      const groups = app.snapshot?.activity?.groups || [];
      return groups.filter((group) => {
        if (!matchesScope(group.scope)) return false;
        return app.includeSystem || !group.is_system;
      });
    }

    function visibleUnassignedAgents() {
      const agents = app.snapshot?.activity?.unassigned_agents || [];
      return agents.filter((agent) => matchesScope(agent.scope));
    }

    function visibleRepos() {
      const repos = app.snapshot?.git?.repos || [];
      return repos.filter((repo) => {
        const scopes = (repo.scopes && repo.scopes.length) ? repo.scopes : [repo.scope];
        return app.selectedScope === "all" || scopes.some((scope) => matchesScope(scope));
      });
    }

    function hiddenCompletedTaskIds() {
      return hiddenCompletedTaskIdsFromSnapshot(app.snapshot, app.hideCompletedConvoys);
    }

    function filteredTaskNodes() {
      return visibleGraphNodes().filter((node) => node.kind === "task" && !node.is_system);
    }

    function getFilteredSummary() {
      const tasks = filteredTaskNodes();
      const agents = visibleAgents(false);
      const polecats = visiblePolecats();
      return {
        hooked_tasks: tasks.filter((node) => node.status === "hooked").length,
        in_progress_tasks: tasks.filter((node) => node.status === "in_progress").length,
        blocked_tasks: tasks.filter((node) => node.status === "blocked").length,
        open_tasks: tasks.filter((node) => node.status === "open").length,
        closed_tasks: tasks.filter((node) => node.status === "closed").length,
        deferred_tasks: tasks.filter((node) => node.status === "deferred").length,
        pinned_tasks: tasks.filter((node) => node.status === "pinned").length,
        active_agents: agents.filter((agent) => agent.has_session).length,
        active_polecats: polecats.filter((agent) => agent.has_session).length,
        command_errors: app.snapshot?.errors?.length || 0,
      };
    }

    function hasCollectedSnapshot(snapshot) {
      if (!snapshot) return false;
      if (Number(snapshot.generation_ms || 0) > 0) return true;
      if ((snapshot.errors || []).length > 0) return true;
      if ((snapshot.graph?.nodes || []).length > 0) return true;
      if ((snapshot.stores || []).length > 0) return true;
      if ((snapshot.agents || []).length > 0) return true;
      if ((snapshot.crews || []).length > 0) return true;
      if ((snapshot.git?.repos || []).length > 0) return true;
      if (String(snapshot.status?.raw || snapshot.vitals_raw || "").trim()) return true;
      return false;
    }

    function loadingAgeSeconds() {
      return Math.max(0, Math.floor((Date.now() - app.bootStartedMs) / 1000));
    }

    function getNodeMap() {
      const nodes = app.snapshot?.graph?.nodes || [];
      return new Map(nodes.map((node) => [node.id, node]));
    }

    function getSelectedNode() {
      const map = getNodeMap();
      return map.get(app.selectedNodeId) || null;
    }

    function formatHookState(status) {
      if (!status) return "unknown";
      return String(status).replaceAll("_", " ");
    }

    function summarizeAgentChurn(agent) {
      const lastEvent = (agent.events || []).at(-1);
      return lastEvent?.message || "";
    }

    function formatRosterSummary(roster, singularLabel, pluralLabel, labels = {}) {
      const total = roster.length;
      const attached = roster.filter((agent) => agent.taskId).length;
      const idle = roster.filter((agent) => !agent.taskId && agent.has_session).length;
      const noSession = Math.max(0, total - attached - idle);
      const label = total === 1 ? singularLabel : pluralLabel;
      const attachedLabel = labels.attached || "attached";
      const idleLabel = labels.idle || "idle";
      const noSessionLabel = labels.noSession || "no session";
      const parts = [`${total} ${label}`, `${attached} ${attachedLabel}`, `${idle} ${idleLabel}`];
      if (noSession) parts.push(`${noSession} ${noSessionLabel}`);
      return parts.join(" · ");
    }

    function buildAgentRoster(roleFilter = "agent") {
      const nodeMap = getNodeMap();
      const source = roleFilter === "polecat" ? visiblePolecats() : visibleAgents(false);
      return source.map((agent) => {
        const hook = agent.hook || {};
        const taskId = hook.bead_id || "";
        const node = taskId ? nodeMap.get(taskId) : null;
        const recentTask = agent.recent_task || null;
        const fallbackTaskId = taskId ? "" : (recentTask?.task_id || "");
        const fallbackNode = fallbackTaskId ? nodeMap.get(fallbackTaskId) : null;
        const taskTitle = node?.title || hook.title || "";
        const taskDerived = node?.ui_status || (taskId ? "running" : "");
        const taskStored = taskId ? (node?.status || hook.status || "") : "";
        const isSystem = Boolean(node?.is_system || (hook.title || "").startsWith("mol-"));
        const lastEvent = (agent.events || []).at(-1) || null;
        const churn = summarizeAgentChurn(agent);
        return {
          ...agent,
          taskId,
          taskTitle,
          taskDerived,
          taskStored,
          isSystem,
          fallbackTaskId,
          fallbackTaskTitle: fallbackNode?.title || "",
          recentTask,
          lastEvent,
          churn,
        };
      }).sort((a, b) => {
        const aRank =
          a.taskId ? 0 :
          a.has_session ? 1 :
          2;
        const bRank =
          b.taskId ? 0 :
          b.has_session ? 1 :
          2;
        return aRank - bRank
          || nodeStatusOrder(a.taskDerived || "")
          - nodeStatusOrder(b.taskDerived || "")
          || a.target.localeCompare(b.target);
      });
    }

    function getPrimaryTerminalAgent() {
      const snapshotAgent = findSnapshotPrimaryTerminalAgent(app.snapshot);
      if (snapshotAgent) {
        app.lastPrimaryTerminalAgent = snapshotAgent;
        return snapshotAgent;
      }
      return resolvePrimaryTerminalAgent(app.snapshot, app.lastPrimaryTerminalAgent);
    }

    function getPrimaryTerminalViewAgent() {
      const agent = getPrimaryTerminalAgent();
      if (!agent) return null;
      const live = app.primaryTerminal;
      if (!live || live.target !== agent.target) return null;
      return {
        ...agent,
        ...live,
        hook: live.hook || agent.hook,
        events: live.events || agent.events,
        log_lines: live.log_lines || [],
      };
    }

    function terminalHeading(agent) {
      if (!agent) return "No primary terminal available";
      if (agent.role === "mayor") return "Mayor Terminal";
      if (agent.role === "deacon") return "Deacon Terminal";
      if (agent.role === "witness") return `${agent.scope || "Rig"} Witness Terminal`;
      if (agent.role === "refinery") return `${agent.scope || "Rig"} Refinery Terminal`;
      if (agent.role === "crew") return `${agent.scope || "Rig"} Crew Terminal`;
      if (agent.role === "polecat") return `${agent.scope || "Rig"} Polecat Terminal`;
      return `${agent.target} Terminal`;
    }

    function primarySurfaceHeading(agent) {
      if (!agent) return "No primary view available";
      const transcriptView = getTranscriptView(agent);
      if (hasTranscriptItems(transcriptView)) {
        return terminalHeading(agent).replace("Terminal", transcriptTitleNoun(transcriptView));
      }
      return terminalHeading(agent);
    }

    function buildPrimaryTerminalDataKey(agent) {
      if (!agent) return "none";
      const transcriptView = getTranscriptView(agent);
      const logLines = agent.log_lines || [];
      const events = agent.events || [];
      const lastLog = logLines.length ? String(logLines[logLines.length - 1] || "") : "";
      const lastEvent = events.length
        ? `${events[events.length - 1]?.time || ""}:${events[events.length - 1]?.message || events[events.length - 1]?.raw || ""}`
        : "";
      return [
        agent.target || "",
        agent.render_mode || "",
        transcriptView.provider || "",
        transcriptView.revision || "",
        transcriptView.updated_at || "",
        (transcriptView.items || []).length,
        agent.capture_error || "",
        logLines.length,
        lastLog,
        events.length,
        lastEvent,
        agent.hook?.bead_id || "",
        agent.hook?.status || "",
        agent.has_session ? "1" : "0",
        agent.runtime_state || "",
        agent.session_name || "",
      ].join("||");
    }

    function normalizePrimaryFreezeState() {
      if (app.primaryPointerSelecting && Date.now() >= app.primarySelectionFreezeUntil && !hasPrimaryLogSelection()) {
        app.primaryPointerSelecting = false;
      }
    }

    function hasPrimaryLogSelection() {
      const log = document.getElementById("primary-terminal-log");
      const selection = window.getSelection?.();
      if (!log || !selection || selection.rangeCount === 0 || selection.isCollapsed) return false;
      return log.contains(selection.anchorNode) || log.contains(selection.focusNode);
    }

    function capturePrimaryComposerState() {
      const box = document.getElementById("primary-inject-message");
      if (!box) return null;
      return {
        focused: document.activeElement === box,
        selectionStart: box.selectionStart ?? box.value.length,
        selectionEnd: box.selectionEnd ?? box.value.length,
        scrollTop: box.scrollTop,
      };
    }

    function restorePrimaryComposerState(state) {
      if (!state?.focused) return;
      const box = document.getElementById("primary-inject-message");
      if (!box || box.disabled) return;
      box.focus({ preventScroll: true });
      const max = box.value.length;
      const start = Math.max(0, Math.min(Number(state.selectionStart ?? max), max));
      const end = Math.max(start, Math.min(Number(state.selectionEnd ?? max), max));
      box.selectionStart = start;
      box.selectionEnd = end;
      box.scrollTop = Number(state.scrollTop ?? 0);
    }

    function shouldFreezePrimaryTerminal() {
      normalizePrimaryFreezeState();
      return (app.primaryPointerSelecting && hasPrimaryLogSelection())
        || Date.now() < app.primarySelectionFreezeUntil;
    }

    function syncPrimarySelectionState() {
      const nextFrozen = shouldFreezePrimaryTerminal();
      const changed = nextFrozen !== app.primarySelectionFrozen;
      app.primarySelectionFrozen = nextFrozen;
      if (changed && !nextFrozen && !app.primarySending) {
        renderPrimaryTerminal();
      }
    }

    function ensureSelection() {
      const nodes = visibleGraphNodes();
      if (!nodes.length) {
        app.selectedNodeId = null;
        return;
      }
      const map = new Set(nodes.map((node) => node.id));
      if (app.selectedNodeId && map.has(app.selectedNodeId)) return;
      const next =
        nodes.find((node) => node.ui_status === "running" && node.kind === "task") ||
        nodes.find((node) => node.ui_status === "stuck" && node.kind === "task") ||
        nodes.find((node) => node.kind === "task") ||
        nodes[0];
      app.selectedNodeId = next.id;
    }

    function renderMetrics() {
      const summary = getFilteredSummary();
      const host = document.getElementById("metrics");
      host.innerHTML = `
        <div class="metrics-table-wrap">
          <table class="metrics-table">
            <tbody>
              <tr>
                ${metricConfig.map((item) => {
                  const value = Number(summary?.[item.key] ?? 0);
                  return `
                    <td>
                      <div class="metrics-table-label">${esc(item.label)}</div>
                      <div class="metrics-table-value ${esc(item.cls)}">${esc(value)}</div>
                      <div class="metrics-table-sub">${esc(item.sub(value))}</div>
                    </td>
                  `;
                }).join("")}
              </tr>
            </tbody>
          </table>
        </div>
      `;
    }

    function initGraphPan() {
      const wrap = document.querySelector(".graph-wrap");
      if (!wrap || wrap.dataset.panReady === "1") return;
      wrap.dataset.panReady = "1";

      const endPan = (event) => {
        const pan = app.graphPan;
        if (!pan.active) return;
        if (event && event.pointerId !== undefined && pan.pointerId !== null && event.pointerId !== pan.pointerId) return;
        const shouldSelectNode = Boolean(event && event.type === "pointerup" && !pan.moved && pan.downNodeId);
        const nodeId = pan.downNodeId;
        if (pan.pointerId !== null && wrap.releasePointerCapture) {
          try {
            wrap.releasePointerCapture(pan.pointerId);
          } catch {}
        }
        wrap.classList.remove("dragging");
        pan.active = false;
        pan.pointerId = null;
        pan.startX = 0;
        pan.startY = 0;
        pan.startScrollLeft = 0;
        pan.startScrollTop = 0;
        pan.moved = false;
        pan.downNodeId = "";
        if (shouldSelectNode && nodeId) {
          app.selectedNodeId = nodeId;
          renderAll();
        }
      };

      wrap.addEventListener("pointerdown", (event) => {
        if (event.button !== 0) return;
        if (event.target.closest("summary, a, input, textarea, select, option")) return;
        const pan = app.graphPan;
        const nodeTarget = event.target.closest("[data-node-id]");
        pan.active = true;
        pan.pointerId = event.pointerId;
        pan.startX = event.clientX;
        pan.startY = event.clientY;
        pan.startScrollLeft = wrap.scrollLeft;
        pan.startScrollTop = wrap.scrollTop;
        pan.moved = false;
        pan.downNodeId = nodeTarget?.dataset.nodeId || "";
        wrap.classList.add("dragging");
        if (wrap.setPointerCapture) {
          try {
            wrap.setPointerCapture(event.pointerId);
          } catch {}
        }
      });

      wrap.addEventListener("pointermove", (event) => {
        const pan = app.graphPan;
        if (!pan.active || event.pointerId !== pan.pointerId) return;
        const dx = event.clientX - pan.startX;
        const dy = event.clientY - pan.startY;
        if (!pan.moved && Math.hypot(dx, dy) > 4) {
          pan.moved = true;
        }
        if (!pan.moved) return;
        wrap.scrollLeft = pan.startScrollLeft - dx;
        wrap.scrollTop = pan.startScrollTop - dy;
        app.suppressGraphClick = true;
      });

      wrap.addEventListener("pointerup", endPan);
      wrap.addEventListener("pointercancel", endPan);
      wrap.addEventListener("lostpointercapture", endPan);
    }

    function capturePrimaryLogState() {
      const log = document.getElementById("primary-terminal-log");
      if (!log) return;
      const maxScrollTop = Math.max(0, log.scrollHeight - log.clientHeight);
      const pinnedBottom = maxScrollTop - log.scrollTop <= 8;
      app.primaryLogPinnedBottom = pinnedBottom;
      if (!pinnedBottom) {
        app.primaryLogScrollTop = log.scrollTop;
      }
    }

    function restorePrimaryLogState() {
      const log = document.getElementById("primary-terminal-log");
      if (!log) return;
      if (app.primaryLogPinnedBottom) {
        log.scrollTop = log.scrollHeight;
        return;
      }
      const maxScrollTop = Math.max(0, log.scrollHeight - log.clientHeight);
      log.scrollTop = Math.min(Math.max(0, app.primaryLogScrollTop || 0), maxScrollTop);
    }

    function captureTmuxLogStates(root = document) {
      const logs = root?.querySelectorAll?.("[data-log-scroll-key]") || [];
      logs.forEach((log) => {
        const key = log.dataset.logScrollKey;
        if (!key) return;
        const maxScrollTop = Math.max(0, log.scrollHeight - log.clientHeight);
        app.tmuxLogScrollStates.set(key, {
          pinnedBottom: maxScrollTop - log.scrollTop <= 8,
          offsetFromBottom: Math.max(0, maxScrollTop - log.scrollTop),
          initialized: true,
        });
      });
    }

    function restoreTmuxLogStates(root = document) {
      const logs = root?.querySelectorAll?.("[data-log-scroll-key]") || [];
      logs.forEach((log) => {
        const key = log.dataset.logScrollKey;
        if (!key) return;
        const state = app.tmuxLogScrollStates.get(key);
        if (!state?.initialized) {
          log.scrollTop = log.scrollHeight;
          app.tmuxLogScrollStates.set(key, {
            pinnedBottom: true,
            offsetFromBottom: 0,
            initialized: true,
          });
          return;
        }
        if (state.pinnedBottom) {
          log.scrollTop = log.scrollHeight;
          return;
        }
        const maxScrollTop = Math.max(0, log.scrollHeight - log.clientHeight);
        log.scrollTop = Math.max(0, maxScrollTop - Number(state.offsetFromBottom || 0));
      });
    }

    function renderPrimaryTerminal() {
      const host = document.getElementById("primary-terminal");
      const summaryHost = document.getElementById("primary-terminal-summary");
      capturePrimaryLogState();
      captureTmuxLogStates(host);
      const composerState = capturePrimaryComposerState();
      const targetAgent = getPrimaryTerminalAgent();
      const agent = getPrimaryTerminalViewAgent();
      const tmuxErrorText = String(app.primaryTerminal?.capture_error || "")
        || String(((app.snapshot?.errors || []).find((error) => String(error.command || "").includes("tmux")) || {}).error || "");
      const services = app.primaryTerminal?.services || app.snapshot?.status?.services || [];
      const daemonDown = services.some((service) => service.includes("daemon (stopped)"));
      const doltDown = services.some((service) => service.includes("dolt (stopped"));
      const tmuxDown =
        services.some((service) => service.includes("tmux") && (service.includes("PID 0") || service.includes("0 sessions")))
        || tmuxErrorText.includes("no server running");
      const scopeLabel = app.selectedScope === "all" ? "All" : (app.selectedScope === "hq" ? "HQ" : app.selectedScope);
      const serviceNotes = [
        daemonDown ? "daemon stopped" : "",
        doltDown ? "dolt stopped" : "",
        tmuxDown ? "tmux stopped" : "",
      ].filter(Boolean);
      const snapshotIsDegraded = snapshotPrimaryTerminalIsDegraded(app.snapshot);
      const snapshotAgent = findSnapshotPrimaryTerminalAgent(app.snapshot);
      const usingCachedPrimaryAgent = Boolean(targetAgent && !snapshotAgent);
      const fallbackNote = usingCachedPrimaryAgent
        ? "showing last known controller state while the snapshot is degraded"
        : "";

      if (!targetAgent) {
        app.primaryTerminalRenderedKey = `none::${scopeLabel}::${serviceNotes.join("|")}`;
        summaryHost.textContent = snapshotIsDegraded
          ? `${scopeLabel} · controller terminal temporarily unavailable while the snapshot is degraded${serviceNotes.length ? ` · ${serviceNotes.join(" · ")}` : ""}`
          : `${scopeLabel} · no controller terminal visible${serviceNotes.length ? ` · ${serviceNotes.join(" · ")}` : ""}`;
        host.innerHTML = `
          <div class="empty">
            ${snapshotIsDegraded
              ? "Primary controller state is temporarily unavailable while GT data is degraded."
              : "No primary controller terminal is visible for the current scope."}
            ${tmuxDown ? ` GT tmux is currently stopped, so live pane capture is unavailable.` : ""}
          </div>
        `;
        renderMayorEvents();
        return;
      }

      if (!agent) {
        app.primaryTerminalRenderedKey = `loading::${targetAgent.target || ""}::${scopeLabel}::${serviceNotes.join("|")}`;
        summaryHost.textContent = `${targetAgent.target} · ${targetAgent.role} · loading terminal state · ${scopeLabel}${serviceNotes.length ? ` · ${serviceNotes.join(" · ")}` : ""}${fallbackNote ? ` · ${fallbackNote}` : ""}`;
        host.innerHTML = `
          <div class="empty">
            Loading primary terminal state for ${esc(targetAgent.target)}.
          </div>
        `;
        renderMayorEvents();
        return;
      }

      const hookState = formatHookState(agent.hook?.status || "");
      const transcriptView = getTranscriptView(agent);
      const usesTranscript = hasTranscriptItems(transcriptView);
      const terminalText = agent.log_lines?.length
        ? agent.log_lines.join("\n")
        : (tmuxDown
            ? [
                "Live terminal unavailable: GT tmux is currently stopped.",
                serviceNotes.length ? `Service state: ${serviceNotes.join(" · ")}` : "",
                "Bring the GT services back up, then refresh this page.",
              ].filter(Boolean).join("\n")
            : (tmuxErrorText
                ? "Live terminal unavailable: tmux capture failed during this poll."
                : "No pane log stream available for this terminal right now."));
      app.primaryTerminalRenderedKey = buildPrimaryTerminalDataKey(agent);

      summaryHost.textContent = `${agent.target} · ${agent.role} · ${usesTranscript ? transcriptLabel(transcriptView) : "terminal"} · ${scopeLabel}${serviceNotes.length ? ` · ${serviceNotes.join(" · ")}` : ""}${fallbackNote ? ` · ${fallbackNote}` : ""}`;
      host.innerHTML = `
        <div class="terminal-shell">
          <div class="card mayor-terminal-card">
            <div class="agent-top">
              <div>
                <div class="feed-title">${esc(primarySurfaceHeading(agent))}</div>
                <div class="subtle">${esc(agent.target)} ${usesTranscript && transcriptView.session_name ? `· ${esc(transcriptView.session_name)}` : (agent.session_name ? `· ${esc(agent.session_name)}` : "")}</div>
              </div>
              <div class="tiny-actions">
                <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
                <button type="button" data-focus-primary-inject="${esc(agent.target)}">Write</button>
              </div>
            </div>
            <div class="node-meta" style="margin-top: 12px;">
              <span class="chip ${agent.has_session ? "running" : ""}">${agent.has_session ? "session live" : "no session"}</span>
              <span class="chip ${esc(exactStatusTone(agent.hook?.status || ""))}">${esc(hookState || "no hook")}</span>
              ${agent.current_command ? `<span class="chip">${esc(agent.current_command)}</span>` : ""}
              ${agent.scope ? `<span class="chip">${esc(agent.scope)}</span>` : ""}
              ${usesTranscript ? `<span class="chip memory">${esc(transcriptBadgeText(transcriptView))}</span>` : ""}
              ${usesTranscript && transcriptView.updated_at ? `<span class="chip">${esc(timeAgo(transcriptView.updated_at))}</span>` : ""}
              ${agent.log_lines?.length && !usesTranscript ? `<span class="chip">history ${esc(agent.log_lines.length)} lines</span>` : ""}
            </div>
            ${usesTranscript
              ? renderPrimaryTranscript(transcriptView, { openDetails: app.openDetails })
              : `<pre id="primary-terminal-log" class="log-block primary-log-block" style="margin-top:12px;">${esc(terminalText)}</pre>`}
            <div class="stack primary-composer" style="margin-top:12px;">
              <label class="subtle" for="primary-inject-message">Write to ${esc(agent.target)}</label>
              <textarea
                id="primary-inject-message"
                data-primary-target="${esc(agent.target)}"
                placeholder="Write directly to this terminal target. Enter sends. Cmd/Ctrl+Enter adds a new line."
                ${agent.has_session ? "" : "disabled"}
              >${esc(app.primaryInjectDraft)}</textarea>
              <div class="tiny-actions">
                <button type="button" id="primary-inject-submit" data-primary-target="${esc(agent.target)}" ${(agent.has_session && !app.primarySending) ? "" : "disabled"}>${app.primarySending ? "Sending…" : "Send to terminal"}</button>
              </div>
            </div>
          </div>
        </div>
      `;
      renderMayorEvents();
      restorePrimaryLogState();
      restoreTmuxLogStates(host);
      restorePrimaryComposerState(composerState);
    }

    function renderMayorEvents() {
      const host = document.getElementById("mayor-events-panel");
      if (!host) return;
      const agent = getPrimaryTerminalViewAgent() || getPrimaryTerminalAgent();
      const events = agent?.events || [];
      if (!agent) {
        host.innerHTML = `<div class="empty">No controller events are available yet.</div>`;
        return;
      }
      host.innerHTML = events.length
        ? `<div class="event-list">${events.slice(-12).map((event) => `
            <div class="event-item">
              <div class="event-time">${esc(event.time || "")}</div>
              <div><strong>${esc(event.symbol || "·")}</strong> ${esc(event.message || event.raw || "")}</div>
            </div>
          `).join("")}</div>`
        : `<div class="empty">No recent GT events for this controller.</div>`;
    }

    function renderAlerts() {
      const host = document.getElementById("alerts-panel");
      if (!host) return;
      const alerts = app.snapshot?.alerts || [];
      host.innerHTML = alerts.length
        ? `<div class="alert-list">${alerts.map((alert) => `<div class="alert-item">${esc(alert)}</div>`).join("")}</div>`
        : `<div class="empty">No attention items in the current snapshot.</div>`;
    }

    function estimateNodeMetrics(nodes) {
      const host = document.createElement("div");
      host.style.position = "absolute";
      host.style.left = "-100000px";
      host.style.top = "0";
      host.style.visibility = "hidden";
      host.style.pointerEvents = "none";
      host.style.zIndex = "-1";
      document.body.appendChild(host);

      const metrics = new Map();
      for (const node of nodes) {
        const el = document.createElement("div");
        el.className = `node ${node.kind} ${nodeTone(node)}`;
        el.style.left = "0";
        el.style.top = "0";
        el.style.position = "absolute";
        el.style.transform = "none";
        el.innerHTML = renderGraphNodeInner(node);
        host.appendChild(el);
        metrics.set(node.id, {
          width: Math.ceil(el.getBoundingClientRect().width),
          height: Math.ceil(el.getBoundingClientRect().height),
        });
      }
      host.remove();
      return metrics;
    }

    function renderGraphNodeInner(node) {
      const statusCls = nodeTone(node);
      const chips = [];
      if (node.kind === "task") {
        if (node.status) chips.push(`<span class="chip ${esc(exactStatusTone(node.status))}">${esc(node.status)}</span>`);
        if (node.priority !== null && node.priority !== undefined) chips.push(`<span class="chip">P${esc(node.priority)}</span>`);
        if (node.type) chips.push(`<span class="chip">${esc(node.type)}</span>`);
        if (node.agent_targets?.length) chips.push(`<span class="chip">${esc(node.agent_targets.length)} agent${node.agent_targets.length === 1 ? "" : "s"}</span>`);
        if (node.linked_commit_count) chips.push(`<span class="chip memory">${esc(node.linked_commit_count)} commit${node.linked_commit_count === 1 ? "" : "s"}</span>`);
      } else {
        chips.push(`<span class="chip memory">${esc(node.short_sha || "commit")}</span>`);
        if (node.available_local) chips.push(`<span class="chip">local diff</span>`);
      }

      return `
        <div class="node-top">
          <div class="node-id">${esc(node.kind === "commit" ? (node.short_sha || node.id) : node.id)}</div>
          <span class="status-dot ${esc(statusCls)}"></span>
        </div>
        <div class="node-title">${esc(node.kind === "commit" ? (node.branch ? `${node.short_sha} · ${node.branch}` : (node.short_sha || node.title)) : node.title)}</div>
        <div class="node-meta">${chips.join("")}</div>
      `;
    }

    function computeLayout(nodes, edges, metrics) {
      const visibleIds = new Set(nodes.map((node) => node.id));
      const nodeMap = new Map(nodes.map((node) => [node.id, node]));
      const outgoing = new Map();
      const incoming = new Map();
      const undirected = new Map();

      for (const node of nodes) {
        outgoing.set(node.id, []);
        incoming.set(node.id, []);
        undirected.set(node.id, []);
      }

      for (const edge of edges) {
        if (!visibleIds.has(edge.source) || !visibleIds.has(edge.target)) continue;
        outgoing.get(edge.source).push(edge);
        incoming.get(edge.target).push(edge);
        undirected.get(edge.source).push(edge.target);
        undirected.get(edge.target).push(edge.source);
      }

      const visited = new Set();
      const components = [];
      for (const node of nodes) {
        if (visited.has(node.id)) continue;
        const queue = [node.id];
        visited.add(node.id);
        const ids = [];
        while (queue.length) {
          const current = queue.shift();
          ids.push(current);
          for (const next of undirected.get(current) || []) {
            if (visited.has(next)) continue;
            visited.add(next);
            queue.push(next);
          }
        }
        components.push(ids);
      }

      const positions = new Map();
      let totalHeight = 0;
      let maxWidth = 0;
      const componentPadding = 56;
      const nodeGap = 24;
      const columnGap = 44;

      const sortRankNodes = (rankIds, rank, ranks, currentPositions) => {
        if (rank === 0) {
          return [...rankIds].sort((a, b) => {
            const na = nodeMap.get(a);
            const nb = nodeMap.get(b);
            return nodeStatusOrder(na.ui_status) - nodeStatusOrder(nb.ui_status) || a.localeCompare(b);
          });
        }
        return [...rankIds].sort((a, b) => {
          const aParents = (incoming.get(a) || []).map((edge) => edge.source).filter((id) => currentPositions.has(id));
          const bParents = (incoming.get(b) || []).map((edge) => edge.source).filter((id) => currentPositions.has(id));
          const aY = aParents.length ? aParents.reduce((sum, id) => sum + currentPositions.get(id).y, 0) / aParents.length : 0;
          const bY = bParents.length ? bParents.reduce((sum, id) => sum + currentPositions.get(id).y, 0) / bParents.length : 0;
          if (aY !== bY) return aY - bY;
          const na = nodeMap.get(a);
          const nb = nodeMap.get(b);
          return nodeStatusOrder(na.ui_status) - nodeStatusOrder(nb.ui_status) || a.localeCompare(b);
        });
      };

      for (const componentIds of components) {
        const componentSet = new Set(componentIds);
        const inDegree = new Map(componentIds.map((id) => [id, 0]));
        const ranks = new Map(componentIds.map((id) => [id, 0]));
        const queue = [];

        for (const id of componentIds) {
          const degree = (incoming.get(id) || []).filter((edge) => componentSet.has(edge.source)).length;
          inDegree.set(id, degree);
          if (degree === 0) queue.push(id);
        }

        queue.sort();
        const topo = [];
        while (queue.length) {
          const current = queue.shift();
          topo.push(current);
          for (const edge of outgoing.get(current) || []) {
            if (!componentSet.has(edge.target)) continue;
            const step = edge.kind === "parent" ? 0.6 : 1;
            ranks.set(edge.target, Math.max(ranks.get(edge.target) || 0, (ranks.get(current) || 0) + step));
            const nextDegree = (inDegree.get(edge.target) || 0) - 1;
            inDegree.set(edge.target, nextDegree);
            if (nextDegree === 0) queue.push(edge.target);
          }
        }

        if (topo.length < componentIds.length) {
          for (const id of componentIds) {
            if (!topo.includes(id)) topo.push(id);
          }
        }

        const byRank = new Map();
        for (const id of topo) {
          const rank = Math.round((ranks.get(id) || 0) * 10) / 10;
          if (!byRank.has(rank)) byRank.set(rank, []);
          byRank.get(rank).push(id);
        }

        const sortedRanks = [...byRank.keys()].sort((a, b) => a - b);
        const currentPositions = new Map();
        let componentHeight = 0;
        const rankWidths = sortedRanks.map((rank) => {
          const ids = byRank.get(rank) || [];
          return Math.max(...ids.map((id) => metrics.get(id)?.width || (nodeMap.get(id).kind === "commit" ? 176 : 244)));
        });
        const rankOffsets = [];
        let currentX = 28;
        rankWidths.forEach((width, index) => {
          rankOffsets[index] = currentX;
          currentX += width + columnGap;
        });
        for (const rank of sortedRanks) {
          const ids = sortRankNodes(byRank.get(rank), rank, ranks, currentPositions);
          let currentY = totalHeight + 28;
          ids.forEach((id, index) => {
            const node = nodeMap.get(id);
            const width = metrics.get(id)?.width || (node.kind === "commit" ? 176 : 244);
            const height = metrics.get(id)?.height || (node.kind === "commit" ? 58 : 92);
            const x = rankOffsets[sortedRanks.indexOf(rank)];
            const y = currentY;
            currentPositions.set(id, { x, y, width, height });
            positions.set(id, { x, y, width, height });
            currentY += height + nodeGap;
            componentHeight = Math.max(componentHeight, y - totalHeight + height + 28);
            maxWidth = Math.max(maxWidth, x + width + 40);
          });
        }

        totalHeight += componentHeight + componentPadding;
      }

      return {
        positions,
        width: Math.max(maxWidth, 900),
        height: Math.max(totalHeight, 540),
      };
    }

    function renderGraph() {
      const nodes = visibleGraphNodes();
      const edges = app.snapshot?.graph?.edges || [];
      const graphSummary = document.getElementById("graph-summary");
      const hiddenCompletedTasks = hiddenCompletedTaskIds();
      graphSummary.textContent = app.hideCompletedConvoys && hiddenCompletedTasks.size
        ? `${nodes.length} visible nodes · ${hiddenCompletedTasks.size} completed task${hiddenCompletedTasks.size === 1 ? "" : "s"} hidden`
        : `${nodes.length} visible nodes`;

      const stage = document.getElementById("graph-stage");
      const svg = document.getElementById("graph-svg");
      const nodesHost = document.getElementById("graph-nodes");

      if (!nodes.length) {
        stage.style.width = "100%";
        stage.style.height = "560px";
        svg.innerHTML = "";
        nodesHost.innerHTML = `<div class="empty" style="margin: 18px;">No graph nodes match the current filter.</div>`;
        return;
      }

      const metrics = estimateNodeMetrics(nodes);
      const { positions, width, height } = computeLayout(nodes, edges, metrics);
      stage.style.width = `${width}px`;
      stage.style.height = `${height}px`;
      svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
      svg.setAttribute("width", width);
      svg.setAttribute("height", height);

      const selected = app.selectedNodeId;
      const neighbors = new Set();
      for (const edge of edges) {
        if (edge.source === selected) neighbors.add(edge.target);
        if (edge.target === selected) neighbors.add(edge.source);
      }

      svg.innerHTML = edges
        .filter((edge) => positions.has(edge.source) && positions.has(edge.target))
        .map((edge) => {
          const source = positions.get(edge.source);
          const target = positions.get(edge.target);
          const startX = source.x + source.width;
          const startY = source.y + source.height / 2;
          const endX = target.x;
          const endY = target.y + target.height / 2;
          const delta = Math.max(60, (endX - startX) * 0.45);
          const path = `M ${startX} ${startY} C ${startX + delta} ${startY}, ${endX - delta} ${endY}, ${endX} ${endY}`;
          const active = edge.source === selected || edge.target === selected;
          const muted = selected && !active;
          return `<path class="graph-edge ${esc(edge.kind)} ${active ? "active" : ""} ${muted ? "muted" : ""}" d="${path}"></path>`;
        })
        .join("");

      nodesHost.innerHTML = nodes.map((node) => {
        const pos = positions.get(node.id);
        const statusCls = nodeTone(node);
        const selectedCls = node.id === selected ? "selected" : "";
        const mutedCls = selected && node.id !== selected && !neighbors.has(node.id) ? "muted" : "";
        return `
          <button
            type="button"
            class="node ${esc(node.kind)} ${esc(statusCls)} ${selectedCls} ${mutedCls}"
            style="left:${pos.x}px; top:${pos.y}px; width:${pos.width}px;"
            data-node-id="${esc(node.id)}"
          >
            ${renderGraphNodeInner(node)}
          </button>
        `;
      }).join("");
    }

    function renderFocus() {
      const host = document.getElementById("focus-panel");
      const node = getSelectedNode();
      if (!node) {
        host.innerHTML = `<div class="empty">Select a graph node to inspect task state, memory, and controls.</div>`;
        return;
      }

      const isTask = node.kind === "task";
      const taskId = isTask ? node.id : node.parent;
      const taskMemory = app.snapshot?.git?.task_memory?.[taskId] || [];
      const scopedAgents = visibleAgents(true);
      const agents = (node.agent_targets || [])
        .map((target) => scopedAgents.find((agent) => agent.target === target))
        .filter(Boolean);
      const selectedAgents = agents.length
        ? agents
        : (node.kind === "commit" ? [] : []);
      const canRetry = isTask && !node.is_system && (node.status === "hooked" || node.status === "in_progress" || node.ui_status === "running");
      const defaultTarget = selectedAgents[0]?.target || "";
      const globalTargets = scopedAgents.filter((agent) => agent.has_session).map((agent) => agent.target);
      const targetOptions = [...new Set([defaultTarget, ...globalTargets].filter(Boolean))];

      const memoryRows = taskMemory.length
        ? taskMemory.slice(0, 8).map((entry) => `
            <div class="memory-row">
              <div class="memory-top">
                <div>
                  <div><strong>${esc(entry.short_sha || "memory")}</strong> ${entry.branch ? `· ${esc(entry.branch)}` : ""}</div>
                  <div class="subtle">${esc(entry.source)} ${entry.repo_label ? `· ${entry.repo_label}` : ""}</div>
                </div>
                ${entry.repo_id ? `<button type="button" data-diff-repo="${esc(entry.repo_id)}" data-diff-sha="${esc(entry.sha)}">Load diff</button>` : ""}
              </div>
              <div>${esc(entry.subject || "(no subject)")}</div>
            </div>
          `).join("")
        : `<div class="empty">No task-linked git memory yet.</div>`;

      const agentRows = selectedAgents.length
        ? selectedAgents.map((agent) => `
            <div class="agent-row">
              <div class="agent-top">
                <div>
                  <div><strong>${esc(agent.target)}</strong></div>
                  <div class="subtle">${esc(agent.role)} · ${esc(agent.kind)} ${agent.session_name ? `· ${esc(agent.session_name)}` : ""}</div>
                </div>
                <div class="tiny-actions">
                  <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
                  <button type="button" data-select-target="${esc(agent.target)}">Inject</button>
                </div>
              </div>
              <div class="subtle mono">${esc(agent.current_path || "")}</div>
            </div>
          `).join("")
        : `<div class="empty">No hooked agents on this node.</div>`;

      const detailRows = [
        `<span class="chip ${esc(exactStatusTone(node.status))}">${esc(node.status || "unknown")}</span>`,
        node.type ? `<span class="chip">${esc(node.type)}</span>` : "",
        node.scope ? `<span class="chip">${esc(node.scope)}</span>` : "",
        node.priority !== null && node.priority !== undefined ? `<span class="chip">P${esc(node.priority)}</span>` : "",
        node.assignee ? `<span class="chip">${esc(node.assignee)}</span>` : "",
        node.linked_commit_count ? `<span class="chip memory">${esc(node.linked_commit_count)} linked commits</span>` : "",
      ].filter(Boolean).join("");

      host.innerHTML = `
        <div class="card">
          <h3>${esc(node.kind === "commit" ? "Commit Focus" : "Task Focus")}</h3>
          <div class="focus-title">${esc(node.title)}</div>
          <div class="subtle mono" style="margin-top:8px;">${esc(node.kind === "commit" ? node.id : node.id)}</div>
          <div class="node-meta" style="margin-top:12px;">${detailRows}</div>
        </div>

        <div class="card">
          <h3>Description</h3>
          <div class="focus-description">${esc(node.description || "No description recorded.")}</div>
        </div>

        <div class="card">
          <h3>Intervention</h3>
          <div class="stack">
            <button type="button" data-retry-task="${esc(node.id)}" ${canRetry ? "" : "disabled"}>Retry task</button>
            <div class="subtle">${canRetry ? "Uses safe reset primitives only." : "Retry is enabled only for non-system running tasks."}</div>
            <div class="stack">
              <label class="subtle" for="inject-target">Inject target</label>
              <select id="inject-target">
                ${targetOptions.length
                  ? targetOptions.map((target) => `<option value="${esc(target)}" ${target === defaultTarget ? "selected" : ""}>${esc(target)}</option>`).join("")
                  : `<option value="">No live agent available</option>`}
              </select>
              <textarea id="inject-message" placeholder="Inject instruction to the selected agent."></textarea>
              <div class="tiny-actions">
                <button type="button" data-pause-target="${esc(defaultTarget)}" ${defaultTarget ? "" : "disabled"}>Pause selected agent</button>
                <button type="button" id="inject-submit" ${targetOptions.length ? "" : "disabled"}>Send instruction</button>
              </div>
            </div>
          </div>
        </div>

        <div class="card">
          <h3>Hooked Agents</h3>
          <div class="stack">${agentRows}</div>
        </div>

        <div class="card">
          <h3>Task Memory</h3>
          <div class="stack">${memoryRows}</div>
        </div>
      `;
    }

    function renderFeed() {
      const host = document.getElementById("feed-list");
      captureTmuxLogStates(host);
      const groups = visibleFeedGroups();
      const unassigned = visibleUnassignedAgents();
      const totalSections = groups.length + (unassigned.length ? 1 : 0);
      host.classList.toggle("single-feed-group", totalSections === 1);
      document.getElementById("feed-summary").textContent = `${groups.length} task groups`;

      if (!groups.length && !unassigned.length) {
        host.innerHTML = `<div class="empty">No grouped activity yet.</div>`;
        return;
      }

      const groupCards = groups.map((group) => {
        const agentsHtml = group.agents.map((agent) => {
          const eventList = agent.events?.length
            ? `<div class="event-list">${agent.events.map((event) => `
                <div class="event-item">
                  <div class="event-time">${esc(event.time || "")}</div>
                  <div><strong>${esc(event.symbol || "·")}</strong> ${esc(event.message || event.raw || "")}</div>
                </div>
              `).join("")}</div>`
            : `<div class="empty">No recent GT events for this agent.</div>`;

          return `
            <article class="feed-card">
              <div class="agent-top">
                <div>
                  <div class="feed-title">${esc(agent.target)}</div>
                  <div class="subtle">${esc(agent.role)} · ${esc(agent.kind)} ${agent.session_name ? `· ${esc(agent.session_name)}` : ""}</div>
                </div>
                <div class="tiny-actions">
                  <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
                  <button type="button" data-select-target="${esc(agent.target)}">Inject</button>
                </div>
              </div>
              <div class="subtle mono">${esc(agent.current_path || "")}</div>
              <div class="stack">
                ${eventList}
              </div>
            </article>
          `;
        }).join("");

        return `
          <section class="group-card">
            <div class="group-header">
              <div class="group-top">
                <div>
                  <div class="feed-title">${esc(group.title)}</div>
                  <div class="subtle mono">${esc(group.task_id)}</div>
                </div>
                <div class="node-meta">
                  <span class="chip ${esc(exactStatusTone(group.stored_status))}">${esc(group.stored_status || "unknown")}</span>
                </div>
              </div>
              <div class="node-meta">
                <span class="chip">${esc(group.agent_count)} agent${group.agent_count === 1 ? "" : "s"}</span>
                ${group.memory?.length ? `<span class="chip memory">${esc(group.memory.length)} memory link${group.memory.length === 1 ? "" : "s"}</span>` : ""}
                ${group.is_system ? `<span class="chip">system</span>` : ""}
              </div>
            </div>
            <div class="agent-grid ${group.agents.length === 1 ? "single-agent" : ""}">${agentsHtml}</div>
          </section>
        `;
      }).join("");

      const unassignedHtml = unassigned.length
        ? `
          <section class="group-card">
            <div class="group-header">
              <div class="group-top">
                <div>
                  <div class="feed-title">Unassigned Agents</div>
                  <div class="subtle">Live agents with no hooked task in snapshot.</div>
                </div>
              </div>
            </div>
            <div class="agent-grid">
              ${unassigned.map((agent) => `
                <article class="feed-card">
                  <div class="agent-top">
                    <div>
                      <div class="feed-title">${esc(agent.target)}</div>
                      <div class="subtle">${esc(agent.role)} · ${esc(agent.kind)}</div>
                    </div>
                    <div class="tiny-actions">
                      <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
                      <button type="button" data-select-target="${esc(agent.target)}">Inject</button>
                    </div>
                  </div>
                  <div class="subtle mono">${esc(agent.current_path || "")}</div>
                </article>
              `).join("")}
            </div>
          </section>
        `
        : "";

      host.innerHTML = groupCards + unassignedHtml;
      restoreTmuxLogStates(host);
    }

    function renderRosterCards(roster) {
      return roster.map((agent) => {
        const isPolecat = agent.role === "polecat";
        const runtimeState = String(agent.runtime_state || "").trim();
        const hookState = formatHookState(agent.hook?.status || "");
        const hookTone = exactStatusTone(agent.taskStored || "") || exactStatusTone(agent.hook?.status || "") || "";
        const churn = agent.churn || (agent.has_session ? "Session is live but there is no readable churn line yet." : "No live session visible.");
        const eventSummary = agent.lastEvent
          ? `${agent.lastEvent.time || ""} ${agent.lastEvent.symbol || "·"} ${agent.lastEvent.message || agent.lastEvent.raw || ""}`.trim()
          : "";
        const presenceLabel = isPolecat
          ? (runtimeState || (agent.has_session ? "session live" : "no session"))
          : (agent.has_session ? "session live" : "no session");
        const presenceClass = isPolecat
          ? ({
              working: "running",
              stuck: "stuck",
              done: "done",
              idle: "",
            }[runtimeState] || (agent.has_session ? "running" : ""))
          : (agent.has_session ? "running" : "");
        const displayTaskLabel = agent.taskId ? "Current Task" : (agent.fallbackTaskId ? "Latest Task" : "Current Task");
        const displayTaskTitle = agent.taskTitle || agent.fallbackTaskTitle || "No hooked task";
        const displayTaskMeta = agent.taskId || agent.fallbackTaskId || agent.current_path || "";
        const inferredChip = !agent.taskId && agent.fallbackTaskId && agent.recentTask
          ? `<span class="chip">${esc(agent.recentTask.kind === "done" ? "last done" : "last assigned")} ${esc(agent.fallbackTaskId)}</span>`
          : "";
        const inferredNote = !agent.taskId && agent.fallbackTaskId && agent.recentTask
          ? `<div class="subtle">Hook is empty; latest task inferred from feed history.</div>`
          : "";
        const eventTailKey = `${agent.target}::event-tail`;
        const eventTail = agent.events?.length
          ? `<details data-preserve-open-key="${esc(eventTailKey)}" ${app.openDetails.has(eventTailKey) ? "open" : ""}><summary>Recent Events</summary><div class="event-list">${agent.events.map((event) => `
                <div class="event-item">
                  <div class="event-time">${esc(event.time || "")}</div>
                  <div><strong>${esc(event.symbol || "·")}</strong> ${esc(event.message || event.raw || "")}</div>
                </div>
              `).join("")}</div></details>`
          : "";

        return `
          <article class="group-card roster-card">
            <div class="roster-main">
              <div class="agent-top">
                <div>
                  <div class="feed-title">${esc(agent.target)}</div>
                  <div class="subtle">${esc(agent.role)} · ${esc(agent.kind)} ${agent.session_name ? `· ${esc(agent.session_name)}` : ""}</div>
                </div>
                <div class="tiny-actions">
                  <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
                  <button type="button" data-select-target="${esc(agent.target)}">Inject</button>
                </div>
              </div>

              <div class="node-meta">
                <span class="chip ${esc(presenceClass)}">${esc(presenceLabel)}</span>
                <span class="chip ${esc(hookTone)}">${esc(hookState || "no hook")}</span>
                ${agent.current_command ? `<span class="chip">${esc(agent.current_command)}</span>` : ""}
                ${agent.scope ? `<span class="chip">${esc(agent.scope)}</span>` : ""}
              </div>

              <div class="roster-task">
                <div class="roster-churn-label">${esc(displayTaskLabel)}</div>
                <div class="roster-task-title">${esc(displayTaskTitle)}</div>
                <div class="subtle mono">${esc(displayTaskMeta)}</div>
                <div class="node-meta">
                  ${agent.taskStored ? `<span class="chip ${esc(exactStatusTone(agent.taskStored))}">${esc(agent.taskStored)}</span>` : ""}
                  ${inferredChip}
                  ${agent.isSystem ? `<span class="chip">system</span>` : ""}
                </div>
                ${inferredNote}
              </div>

              ${isPolecat ? "" : `
                <div class="roster-churn">
                  <div class="roster-churn-label">Current Churn</div>
                  <div class="roster-churn-text">${esc(churn)}</div>
                  ${eventSummary ? `<div class="subtle">${esc(eventSummary)}</div>` : ""}
                </div>
              `}

              <div class="subtle mono">${esc(agent.current_path || "")}</div>
              ${eventTail}
            </div>
          </article>
        `;
      }).join("");
    }

    function renderPolecats() {
      const host = document.getElementById("polecat-roster");
      captureTmuxLogStates(host);
      const roster = buildAgentRoster("polecat");
      document.getElementById("polecat-summary").textContent =
        formatRosterSummary(roster, "polecat", "polecats", {
          attached: "hooked",
          idle: "idle",
          noSession: "no session",
        });

      if (!roster.length) {
        host.innerHTML = `<div class="empty">No polecats visible for the current scope.</div>`;
        return;
      }

      host.innerHTML = renderRosterCards(roster);
      restoreTmuxLogStates(host);
    }

    function renderAgentRoster() {
      const host = document.getElementById("agent-roster");
      captureTmuxLogStates(host);
      const roster = buildAgentRoster("agent");
      document.getElementById("agent-summary").textContent =
        formatRosterSummary(roster, "agent surface", "agent surfaces", {
          attached: "task-attached",
          idle: "live-unattached",
          noSession: "no session",
        });

      if (!roster.length) {
        host.innerHTML = `<div class="empty">No visible agents detected in the current snapshot.</div>`;
        return;
      }

      host.innerHTML = renderRosterCards(roster);
      restoreTmuxLogStates(host);
    }

    function renderGit() {
      const host = document.getElementById("git-panel");
      const git = app.snapshot?.git || {};
      const repos = visibleRepos();
      const visibleRepoIds = new Set(repos.map((repo) => repo.id));
      const recent = (git.recent_commits || []).filter((commit) => visibleRepoIds.has(commit.repo_id));
      document.getElementById("git-summary").textContent = `${repos.length} repo surfaces`;

      const selected = getSelectedNode();
      const taskId = selected?.kind === "task" ? selected.id : selected?.parent;
      const taskMemory = taskId ? (git.task_memory?.[taskId] || []) : [];

      const memoryBlock = `
        <div class="card">
          <h3>Selected Task Memory</h3>
          <div class="stack">
            ${taskId
              ? (taskMemory.length
                  ? taskMemory.slice(0, 8).map((entry) => `
                      <div class="memory-row">
                        <div class="memory-top">
                          <div>
                            <div><strong>${esc(entry.short_sha || "memory")}</strong> ${entry.branch ? `· ${esc(entry.branch)}` : ""}</div>
                            <div class="subtle">${esc(entry.source)} ${entry.repo_label ? `· ${esc(entry.repo_label)}` : ""}</div>
                          </div>
                          ${entry.repo_id ? `<button type="button" data-diff-repo="${esc(entry.repo_id)}" data-diff-sha="${esc(entry.sha)}">Load diff</button>` : ""}
                        </div>
                        <div>${esc(entry.subject || "(no subject)")}</div>
                      </div>
                    `).join("")
                  : `<div class="empty">No commit lineage or merge memory attached to ${esc(taskId)}.</div>`)
              : `<div class="empty">Select a task or commit node to inspect task-linked git memory.</div>`}
          </div>
        </div>
      `;

      const recentBlock = `
        <div class="card">
          <h3>Recent Commits</h3>
          <div class="stack">
            ${recent.length
              ? recent.slice(0, 12).map((commit) => `
                  <div class="commit-row">
                    <div class="commit-top">
                      <div>
                        <div><strong>${esc(commit.short_sha)}</strong> · ${esc(commit.subject)}</div>
                        <div class="subtle">${esc(commit.repo_label)} · ${esc(timeAgo(commit.committed_at))}</div>
                      </div>
                      <button type="button" data-diff-repo="${esc(commit.repo_id)}" data-diff-sha="${esc(commit.sha)}">Load diff</button>
                    </div>
                    <div class="node-meta">
                      ${commit.task_ids?.length ? commit.task_ids.map((taskId) => `<span class="chip memory">${esc(taskId)}</span>`).join("") : `<span class="chip">no task id in subject</span>`}
                    </div>
                  </div>
                `).join("")
              : `<div class="empty">No recent commits visible.</div>`}
          </div>
        </div>
      `;

      const repoBlock = `
        <div class="card">
          <h3>Branches & Worktrees</h3>
          <div class="stack">
            ${repos.length
              ? repos.map((repo) => `
                  <article class="repo-card">
                    <div class="group-top">
                      <div>
                        <div><strong>${esc(repo.label)}</strong></div>
                        <div class="repo-summary mono">${esc(repo.root)}</div>
                      </div>
                      <div class="node-meta">
                        <span class="chip ${repo.status?.dirty ? "stuck" : "done"}">${repo.status?.dirty ? "dirty" : "clean"}</span>
                        <span class="chip">${esc(repo.status?.branch || "unknown branch")}</span>
                      </div>
                    </div>
                    <details>
                      <summary>Branches (${esc(repo.branches?.length || 0)})</summary>
                      <div class="stack" style="margin-top: 10px;">
                        ${(repo.branches || []).map((branch) => `
                          <div class="memory-row">
                            <div class="memory-top">
                              <div><strong>${branch.current ? "* " : ""}${esc(branch.name)}</strong></div>
                              <div class="subtle">${esc(timeAgo(branch.committed_at))}</div>
                            </div>
                            <div class="subtle mono">${esc(branch.short_sha)}</div>
                            <div>${esc(branch.subject || "")}</div>
                          </div>
                        `).join("") || `<div class="empty">No branch data.</div>`}
                      </div>
                    </details>
                    <details style="margin-top: 10px;">
                      <summary>Worktrees (${esc(repo.worktrees?.length || 0)})</summary>
                      <div class="stack" style="margin-top: 10px;">
                        ${(repo.worktrees || []).map((worktree) => `
                          <div class="memory-row">
                            <div><strong>${esc(worktree.branch || "(detached)")}</strong></div>
                            <div class="subtle mono">${esc(worktree.path)}</div>
                          </div>
                        `).join("") || `<div class="empty">No worktree data.</div>`}
                      </div>
                    </details>
                  </article>
                `).join("")
              : `<div class="empty">No repo data available.</div>`}
          </div>
        </div>
      `;

      const diffText = app.diffCache.get(app.diffKey) || "Select a commit or memory row and load a diff.";
      const diffBlock = `
        <div class="card">
          <h3>Diff Viewer</h3>
          <pre>${esc(diffText)}</pre>
        </div>
      `;

      host.innerHTML = memoryBlock + recentBlock + repoBlock + diffBlock;
    }

    function renderActions(actions) {
      const host = document.getElementById("actions-list");
      if (!actions || !actions.length) {
        host.innerHTML = `<div class="empty">No GTUI actions have been sent from this page yet.</div>`;
        return;
      }
      host.innerHTML = actions.map((action) => `
        <div class="action-item ${action.ok ? "ok" : "bad"}">
          <div class="group-top">
            <div><strong>${esc(action.kind)}</strong></div>
            <div class="subtle">${esc(timeAgo(action.timestamp))}</div>
          </div>
          <div class="subtle mono" style="margin-top:8px;">${esc(action.command || "")}</div>
          <div style="margin-top:8px;">${esc(action.output || "")}</div>
        </div>
      `).join("");
    }

    function renderOverview(snapshot) {
      const crewHost = document.getElementById("crew-panel");
      const crews = (snapshot.crews || []).filter((crew) => matchesScope(crew.rig));
      const agents = visibleAgents(true);
      document.getElementById("crew-summary").textContent = `${crews.length} crew workspace${crews.length === 1 ? "" : "s"} · ${agents.length} visible agent surfaces`;

      crewHost.innerHTML = crews.length
        ? crews.map((crew) => {
            const target = `${crew.rig}/crew/${crew.name}`;
            const agent = agents.find((item) => item.target === target);
            const hook = agent?.hook || {};
            const riskyModified = Array.isArray(crew.git_risky_modified) ? crew.git_risky_modified.length : 0;
            const riskyUntracked = Array.isArray(crew.git_risky_untracked) ? crew.git_risky_untracked.length : 0;
            const benignModified = Array.isArray(crew.git_benign_modified) ? crew.git_benign_modified.length : 0;
            const benignUntracked = Array.isArray(crew.git_benign_untracked) ? crew.git_benign_untracked.length : 0;
            const benignCount = benignModified + benignUntracked;
            return `
              <div class="agent-row">
                <div class="agent-top">
                  <div>
                    <div><strong>${esc(target)}</strong></div>
                    <div class="subtle mono">${esc(crew.path || "")}</div>
                  </div>
                  <div class="node-meta">
                    <span class="chip ${crew.has_session ? "running" : ""}">${crew.has_session ? "running" : "stopped"}</span>
                    <span class="chip ${esc(crew.git_status_tone || (crew.git_clean ? "done" : "stuck"))}">${esc(crew.git_status_label || (crew.git_clean ? "git clean" : "repo changes"))}</span>
                  </div>
                </div>
                <div class="node-meta">
                  <span class="chip">branch ${esc(crew.branch || "unknown")}</span>
                  <span class="chip">mail ${esc(crew.mail_unread ?? 0)} unread</span>
                  ${riskyModified ? `<span class="chip stuck">modified ${esc(riskyModified)}</span>` : ""}
                  ${riskyUntracked ? `<span class="chip stuck">untracked ${esc(riskyUntracked)}</span>` : ""}
                  ${benignCount ? `<span class="chip memory">local state ${esc(benignCount)}</span>` : ""}
                  ${hook?.bead_id ? `<span class="chip ${esc(exactStatusTone(hook.status || ""))}">${esc(hook.status || "hooked")} ${esc(hook.bead_id)}</span>` : ""}
                </div>
              </div>
            `;
          }).join("")
        : `<div class="empty">No crew workspaces detected.</div>`;

      const storesHost = document.getElementById("stores-panel");
      const stores = (snapshot.stores || []).filter((store) => matchesScope(store.scope));
      document.getElementById("stores-summary").textContent = `${stores.length} bead store${stores.length === 1 ? "" : "s"}`;
      storesHost.innerHTML = stores.length
        ? stores.map((store) => {
            const exactCounts = Object.entries(store.exact_status_counts || {});
            const summaryEntries = Object.entries(store.summary || {});
            return `
              <div class="memory-row">
                <div class="memory-top">
                  <div>
                    <div><strong>${esc(store.name)}</strong></div>
                    <div class="subtle mono">${esc(store.path || "")}</div>
                  </div>
                  <div class="node-meta">
                    <span class="chip ${store.available ? "done" : "stuck"}">${store.available ? "available" : "unavailable"}</span>
                    <span class="chip">total ${esc(store.total ?? 0)}</span>
                    <span class="chip ${store.hooked ? "running" : ""}">hooked ${esc(store.hooked ?? 0)}</span>
                    <span class="chip ${store.blocked ? "stuck" : ""}">blocked ${esc(store.blocked ?? 0)}</span>
                  </div>
                </div>
                ${summaryEntries.length ? `<div class="node-meta">${summaryEntries.map(([key, value]) => `<span class="chip">${esc(key.replaceAll("_", " "))} ${esc(value)}</span>`).join("")}</div>` : ""}
                ${exactCounts.length ? `<div class="node-meta">${exactCounts.map(([key, value]) => `<span class="chip ${esc(exactStatusTone(key))}">${esc(key)} ${esc(value)}</span>`).join("")}</div>` : ""}
                ${store.error ? `<pre>${esc(store.error)}</pre>` : ""}
              </div>
            `;
          }).join("")
        : `<div class="empty">No bead stores discovered.</div>`;

      const statusHost = document.getElementById("status-panel");
      const visibleTasks = filteredTaskNodes();
      const storedCounts = {};
      visibleTasks.forEach((task) => {
        storedCounts[task.status] = (storedCounts[task.status] || 0) + 1;
      });
      statusHost.innerHTML = `
        <div class="card">
          <h3>Visible Task Statuses</h3>
          <div class="node-meta">
            ${Object.entries(storedCounts).length
              ? Object.entries(storedCounts).map(([key, value]) => `<span class="chip ${esc(exactStatusTone(key))}">${esc(key)} ${esc(value)}</span>`).join("")
              : `<span class="chip">No visible task nodes</span>`}
          </div>
        </div>
        <div class="stack">
          ${(snapshot.status_legend || []).map((item) => `
            <div class="memory-row">
              <div class="memory-top">
                <div><strong>${esc(item.icon)} ${esc(item.name)}</strong></div>
              </div>
              <div>${esc(item.meaning)}</div>
            </div>
          `).join("")}
        </div>
      `;

      document.getElementById("raw-status").textContent = snapshot.status?.raw || "No gt status output.";
      document.getElementById("raw-vitals").textContent = snapshot.vitals_raw || "No gt vitals output.";
    }

    function renderErrors(errors) {
      const panel = document.getElementById("errors-panel");
      const host = document.getElementById("errors-list");
      if (!errors || !errors.length) {
        panel.hidden = true;
        host.innerHTML = "";
        return;
      }
      panel.hidden = false;
      host.innerHTML = errors.map((error) => `
        <details class="error-item">
          <summary>${esc(error.command)} (${esc(error.duration_ms)} ms)</summary>
          <pre style="margin-top:10px;">${esc(error.error || "")}

cwd: ${esc(error.cwd || "")}
returncode: ${esc(error.returncode ?? "")}</pre>
        </details>
      `).join("");
    }

    function renderChrome(snapshot) {
      document.getElementById("gt-root").textContent = snapshot.gt_root || "";
      document.getElementById("snapshot-stamp").textContent =
        `${formatTime(snapshot.generated_at)} · ${timeAgo(snapshot.generated_at)} · ${snapshot.generation_ms || 0} ms`;
      document.getElementById("service-stamp").textContent =
        (snapshot.status?.services || []).join(" | ") || "No service line parsed";
      const scopeLabel = app.selectedScope === "all" ? "All" : (app.selectedScope === "hq" ? "HQ" : app.selectedScope);
      document.getElementById("footer-right").textContent =
        `${snapshot.status?.town || "Town"} · ${scopeLabel} · ${snapshot.status?.overseer || "no overseer parsed"}`;
      document.getElementById("footer-left").textContent =
        `Polling every ${SNAPSHOT_POLL_MS / 1000}s · gt ${snapshot.timings?.gt_commands_ms || 0} ms · agents ${snapshot.timings?.agent_commands_ms || 0} ms · bd ${snapshot.timings?.bd_commands_ms || 0} ms · git ${snapshot.timings?.git_commands_ms || 0} ms`;
    }

    function getSnapshotHealth(snapshot) {
      return describeSnapshotHealth(snapshot, { loading: !app.lastSuccessMs });
    }

    function updateLivePill(snapshot, health = getSnapshotHealth(snapshot)) {
      const pill = document.getElementById("live-pill");
      const label = document.getElementById("live-label");
      const tooltip = document.getElementById("live-pill-tooltip");
      pill.classList.remove("stale", "error", "loading", "stopped");
      const details = [...health.details];
      if (snapshot?.generated_at) {
        details.push(`Observed: ${formatTime(snapshot.generated_at)} (${timeAgo(snapshot.generated_at)})`);
      }
      tooltip.innerHTML = details
        .map((detail) => `<div class="pill-tooltip-row">${esc(detail)}</div>`)
        .join("");
      pill.removeAttribute("title");
      pill.setAttribute("aria-label", `${health.label}. ${details.join(" ")}`.trim());
      if (health.tone === "loading") {
        pill.classList.add("loading");
        label.textContent = health.label;
        return;
      }
      if (health.tone === "error") {
        pill.classList.add("error");
        label.textContent = health.label;
        return;
      }
      if (health.tone === "stopped") {
        pill.classList.add("stopped");
        label.textContent = health.label;
        return;
      }
      label.textContent = health.label;
    }

    function updateGtControlButton(snapshot, health = getSnapshotHealth(snapshot)) {
      const button = document.getElementById("gt-control-button");
      const action = app.gtControlInFlight
        ? (app.gtControlAction || health.controlAction || "run")
        : (health.controlAction || "run");
      button.hidden = false;
      button.dataset.gtControl = action;
      button.classList.remove("run", "stop", "busy");
      button.classList.add(action === "stop" ? "stop" : "run");
      if (app.gtControlInFlight) {
        button.classList.add("busy");
        button.disabled = true;
        button.textContent = action === "stop" ? "Stopping…" : "Starting…";
        button.setAttribute("aria-label", button.textContent);
        return;
      }
      button.disabled = health.tone === "loading";
      button.textContent = health.tone === "loading" ? "Waiting…" : (health.controlLabel || "Run GT");
      button.setAttribute(
        "aria-label",
        health.tone === "loading" ? "Waiting for GT status." : (health.controlLabel || "Run GT"),
      );
    }

    function updateControlSurfaceState(snapshot) {
      const health = getSnapshotHealth(snapshot);
      updateGtControlButton(snapshot, health);
      updateLivePill(snapshot, health);
    }

    function renderLoadingState(snapshot = null) {
      const age = loadingAgeSeconds();
      document.getElementById("gt-root").textContent = snapshot?.gt_root || "Resolving GT root...";
      document.getElementById("snapshot-stamp").textContent = `Collecting first snapshot · ${age}s`;
      document.getElementById("service-stamp").textContent = "Waiting for gt status and bead stores";
      document.getElementById("footer-left").textContent = "Initial poll is still running.";
      document.getElementById("footer-right").textContent = "Building live GT view";
      document.getElementById("metrics").innerHTML = `
        <div class="loading-state">
          <div class="loading-spinner" aria-hidden="true"></div>
          <div>
            <div class="loading-title">Collecting live GT data</div>
            <div class="loading-copy">The first poll runs gt, bd, tmux, and git commands before rendering the dashboard.</div>
          </div>
        </div>
      `;
      document.getElementById("primary-terminal-summary").textContent = "Waiting for first snapshot";
      document.getElementById("primary-terminal").innerHTML = `<div class="empty loading-empty">Terminal surfaces will appear after the first live poll finishes.</div>`;
      document.getElementById("graph-summary").textContent = "loading";
      document.getElementById("graph-svg").innerHTML = "";
      document.getElementById("graph-nodes").innerHTML = `<div class="empty loading-empty" style="margin: 18px;">Task spine is loading from GT and bead stores.</div>`;
      document.getElementById("focus-panel").innerHTML = `<div class="empty loading-empty">Focus controls will appear with the first snapshot.</div>`;
      document.getElementById("polecat-summary").textContent = "loading";
      document.getElementById("polecat-roster").innerHTML = `<div class="empty loading-empty">Polecat roster is loading.</div>`;
      document.getElementById("agent-summary").textContent = "loading";
      document.getElementById("agent-roster").innerHTML = `<div class="empty loading-empty">Agent roster is loading.</div>`;
      document.getElementById("feed-summary").textContent = "loading";
      document.getElementById("feed-list").innerHTML = `<div class="empty loading-empty">Swarm activity is loading.</div>`;
      document.getElementById("git-summary").textContent = "loading";
      document.getElementById("git-panel").innerHTML = `<div class="empty loading-empty">Git memory is loading.</div>`;
      document.getElementById("crew-summary").textContent = "loading";
      document.getElementById("crew-panel").innerHTML = `<div class="empty loading-empty">Crew workspaces are loading.</div>`;
      document.getElementById("stores-summary").textContent = "loading";
      document.getElementById("stores-panel").innerHTML = `<div class="empty loading-empty">Bead stores are loading.</div>`;
      document.getElementById("status-panel").innerHTML = `<div class="empty loading-empty">Status legend is loading.</div>`;
      document.getElementById("raw-status").textContent = "Waiting for gt status output...";
      document.getElementById("raw-vitals").textContent = "Waiting for gt vitals output...";
      document.getElementById("mayor-events-panel").innerHTML = `<div class="empty loading-empty">Mayor events are loading.</div>`;
      document.getElementById("alerts-panel").innerHTML = `<div class="empty loading-empty">Attention items are loading.</div>`;
      renderActions(snapshot?.actions || []);
      renderErrors([]);
      updateControlSurfaceState(snapshot || {});
    }

    function renderAll() {
      if (!app.snapshot) return;
      const freezePrimary = shouldFreezePrimaryTerminal();
      syncScopeSelector();
      ensureSelection();
      renderChrome(app.snapshot);
      if (!freezePrimary) renderPrimaryTerminal();
      renderMayorEvents();
      renderAlerts();
      renderMetrics();
      renderGraph();
      renderFocus();
      renderPolecats();
      renderAgentRoster();
      renderFeed();
      renderGit();
      renderOverview(app.snapshot);
      renderActions(app.snapshot.actions || []);
      renderErrors(app.snapshot.errors || []);
      updateControlSurfaceState(app.snapshot);
    }

    function showToast(message, ok = true) {
      const toast = document.getElementById("toast");
      toast.textContent = message;
      toast.className = `toast show ${ok ? "ok" : "bad"}`;
      window.clearTimeout(app.toastTimer);
      app.toastTimer = window.setTimeout(() => {
        toast.className = "toast";
      }, 3200);
    }

    async function fetchSnapshot(force = false) {
      if (app.inFlight && !force) return;
      app.inFlight = true;
      document.getElementById("refresh-button").disabled = true;
      try {
        const data = await invoke("get_snapshot");
        const snapshotAgent = findSnapshotPrimaryTerminalAgent(data);
        if (snapshotAgent) {
          app.lastPrimaryTerminalAgent = snapshotAgent;
        }
        if (!hasCollectedSnapshot(data) && !app.lastSuccessMs) {
          app.snapshot = data;
          renderLoadingState(data);
          return;
        }
        const changed = !app.snapshot || app.snapshot.generated_at !== data.generated_at;
        app.snapshot = data;
        app.lastSuccessMs = Date.now();
        if (force || changed) {
          renderAll();
        } else {
          updateControlSurfaceState(app.snapshot);
        }
      } catch (error) {
        if (!app.lastSuccessMs) {
          renderLoadingState(app.snapshot);
          document.getElementById("snapshot-stamp").textContent = `Snapshot request failed · ${String(error)}`;
        }
        const fallbackState = app.snapshot
          ? {
              ...app.snapshot,
              errors: [...(app.snapshot.errors || []), { error: String(error) }],
            }
          : { errors: [{ error: String(error) }] };
        updateControlSurfaceState(fallbackState);
      } finally {
        app.inFlight = false;
        document.getElementById("refresh-button").disabled = false;
      }
    }

    async function fetchPrimaryTerminal(force = false) {
      if (!app.lastSuccessMs) return;
      const snapshotAgent = findSnapshotPrimaryTerminalAgent(app.snapshot);
      if (!snapshotAgent) {
        if (!snapshotPrimaryTerminalIsDegraded(app.snapshot)) {
          app.primaryTerminal = null;
          app.primaryTerminalDataKey = "none";
          app.lastPrimaryTerminalAgent = null;
        }
        if (!shouldFreezePrimaryTerminal()) {
          renderPrimaryTerminal();
        }
        return;
      }
      app.lastPrimaryTerminalAgent = snapshotAgent;
      if (app.primaryTerminalInFlight) {
        const stuck = (Date.now() - app.primaryTerminalFetchStartedAt) >= PRIMARY_TERMINAL_FETCH_TIMEOUT_MS;
        if (!force && !stuck) return;
      }
      const requestId = app.primaryTerminalRequestId + 1;
      app.primaryTerminalRequestId = requestId;
      app.primaryTerminalInFlight = true;
      app.primaryTerminalFetchStartedAt = Date.now();
      try {
        const data = await invoke("get_terminal", { target: snapshotAgent.target });
        if (requestId !== app.primaryTerminalRequestId) return;
        app.primaryTerminal = data;
        app.primaryTerminalDataKey = buildPrimaryTerminalDataKey(getPrimaryTerminalViewAgent());
        if (!shouldFreezePrimaryTerminal() && (force || app.primaryTerminalDataKey !== app.primaryTerminalRenderedKey)) {
          renderPrimaryTerminal();
        }
      } catch (error) {
        console.error(error);
      } finally {
        if (requestId === app.primaryTerminalRequestId) {
          app.primaryTerminalInFlight = false;
          app.primaryTerminalFetchStartedAt = 0;
        }
      }
    }

    async function postAction(command, args, options = {}) {
      const refresh = options.refresh !== false;
      const showSuccessToast = options.successToast !== false;
      const data = await invoke(command, args);
      const ok = data?.ok !== false;
      if (showSuccessToast) {
        showToast(data.output || `${data.kind} sent`, ok);
      }
      if (refresh) {
        await fetchSnapshot(true);
      }
      return data;
    }

    async function runGtControl(action) {
      if (app.gtControlInFlight) return;
      app.gtControlInFlight = true;
      app.gtControlAction = action === "stop" ? "stop" : "run";
      updateGtControlButton(app.snapshot || {});
      try {
        await postAction(action === "stop" ? "stop_gt" : "run_gt", {}, { refresh: false });
        await fetchSnapshot(true);
        window.setTimeout(() => fetchSnapshot(true), 1200);
        window.setTimeout(() => fetchSnapshot(true), 3200);
      } catch (error) {
        showToast(String(error), false);
      } finally {
        app.gtControlInFlight = false;
        app.gtControlAction = "";
        updateGtControlButton(app.snapshot || {});
      }
    }

    async function loadDiff(repoId, sha) {
      const key = `${repoId}:${sha}`;
      if (app.diffCache.has(key)) {
        app.diffKey = key;
        renderGit();
        return;
      }
      try {
        const data = await invoke("get_git_diff", { repo: repoId, sha });
        app.diffCache.set(key, data.text || "");
        app.diffKey = key;
        renderGit();
      } catch (error) {
        showToast(String(error), false);
      }
    }

    document.querySelectorAll("[data-tab-target]").forEach((button) => {
      button.addEventListener("click", () => selectTab(button.dataset.tabTarget));
      button.addEventListener("keydown", (event) => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
        event.preventDefault();
        const tabs = [...document.querySelectorAll("[data-tab-target]")];
        const current = tabs.indexOf(button);
        const nextIndex =
          event.key === "Home" ? 0 :
          event.key === "End" ? tabs.length - 1 :
          event.key === "ArrowLeft" ? (current - 1 + tabs.length) % tabs.length :
          (current + 1) % tabs.length;
        tabs[nextIndex]?.focus();
        selectTab(tabs[nextIndex]?.dataset.tabTarget || "mayor");
      });
    });

    document.getElementById("refresh-button").addEventListener("click", () => fetchSnapshot(true));
    document.getElementById("gt-control-button").addEventListener("click", async (event) => {
      const action = event.currentTarget?.dataset.gtControl || "run";
      await runGtControl(action);
    });
    document.getElementById("scope-select").addEventListener("change", (event) => {
      app.selectedScope = event.target.value;
      ensureSelection();
      renderAll();
      fetchPrimaryTerminal(true);
    });
    document.getElementById("include-system").addEventListener("change", (event) => {
      app.includeSystem = event.target.checked;
      ensureSelection();
      renderAll();
    });
    document.getElementById("hide-completed-convoys").addEventListener("change", (event) => {
      app.hideCompletedConvoys = event.target.checked;
      ensureSelection();
      renderAll();
    });

    document.addEventListener("input", (event) => {
      if (event.target?.id === "primary-inject-message") {
        app.primaryInjectDraft = event.target.value;
      }
    });

    document.addEventListener("toggle", (event) => {
      const detail = event.target;
      if (!(detail instanceof HTMLDetailsElement)) return;
      const key = detail.dataset.preserveOpenKey;
      if (!key) return;
      if (detail.open) {
        app.openDetails.add(key);
        window.requestAnimationFrame(() => restoreTmuxLogStates(detail));
      } else {
        app.openDetails.delete(key);
      }
    }, true);

    document.addEventListener("scroll", (event) => {
      const log = event.target;
      if (!(log instanceof HTMLElement)) return;
      const key = log.dataset.logScrollKey;
      if (!key) return;
      const maxScrollTop = Math.max(0, log.scrollHeight - log.clientHeight);
      app.tmuxLogScrollStates.set(key, {
        pinnedBottom: maxScrollTop - log.scrollTop <= 8,
        offsetFromBottom: Math.max(0, maxScrollTop - log.scrollTop),
        initialized: true,
      });
    }, true);

    document.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      const log = document.getElementById("primary-terminal-log");
      if (!log || !log.contains(event.target)) return;
      if (event.target.closest("button, textarea, input, select")) return;
      app.primaryPointerSelecting = true;
      app.primarySelectionFreezeUntil = Date.now() + PRIMARY_SELECTION_FREEZE_MS;
      syncPrimarySelectionState();
    });

    document.addEventListener("mouseup", () => {
      if (!app.primaryPointerSelecting) return;
      app.primaryPointerSelecting = false;
      app.primarySelectionFreezeUntil = hasPrimaryLogSelection() ? (Date.now() + PRIMARY_SELECTION_FREEZE_MS) : 0;
      window.setTimeout(syncPrimarySelectionState, 0);
    });

    document.addEventListener("selectionchange", () => {
      if (hasPrimaryLogSelection()) {
        app.primarySelectionFreezeUntil = Date.now() + PRIMARY_SELECTION_FREEZE_MS;
      } else if (!app.primaryPointerSelecting) {
        app.primarySelectionFreezeUntil = 0;
      }
      syncPrimarySelectionState();
    });

    window.addEventListener("blur", () => {
      app.primaryPointerSelecting = false;
      if (!hasPrimaryLogSelection()) {
        app.primarySelectionFreezeUntil = 0;
      }
      syncPrimarySelectionState();
    });

    document.addEventListener("visibilitychange", () => {
      if (document.hidden) {
        app.primaryPointerSelecting = false;
        if (!hasPrimaryLogSelection()) {
          app.primarySelectionFreezeUntil = 0;
        }
        syncPrimarySelectionState();
        return;
      }
      fetchPrimaryTerminal(true);
      fetchSnapshot(false);
    });

    window.addEventListener("focus", () => {
      fetchPrimaryTerminal(true);
      fetchSnapshot(false);
    });

    document.addEventListener("click", async (event) => {
      if (app.suppressGraphClick) {
        app.suppressGraphClick = false;
        if (event.target.closest(".graph-wrap")) {
          event.preventDefault();
          return;
        }
      }
      const target = event.target.closest("[data-node-id], [data-inline-toggle], [data-retry-task], [data-pause-agent], [data-pause-target], [data-select-target], [data-focus-primary-inject], [data-diff-repo], #inject-submit, #primary-inject-submit");
      if (!target) return;

      if (target.dataset.inlineToggle) {
        const key = target.dataset.inlineToggle;
        if (app.openDetails.has(key)) {
          app.openDetails.delete(key);
        } else {
          app.openDetails.add(key);
        }
        renderPrimaryTerminal();
        return;
      }

      if (target.dataset.nodeId) {
        app.selectedNodeId = target.dataset.nodeId;
        renderAll();
        return;
      }

      if (target.dataset.retryTask) {
        try {
          await postAction("retry_task", { taskId: target.dataset.retryTask });
        } catch (error) {
          showToast(String(error), false);
        }
        return;
      }

      if (target.dataset.pauseAgent) {
        try {
          await postAction("pause_agent", { agentId: target.dataset.pauseAgent });
        } catch (error) {
          showToast(String(error), false);
        }
        return;
      }

      if (target.dataset.pauseTarget) {
        const injectTarget = document.getElementById("inject-target");
        const selectedTarget = injectTarget?.value || target.dataset.pauseTarget;
        if (!selectedTarget) {
          showToast("No target selected.", false);
          return;
        }
        try {
          await postAction("pause_agent", { agentId: selectedTarget });
        } catch (error) {
          showToast(String(error), false);
        }
        return;
      }

      if (target.dataset.selectTarget) {
        const injectTarget = document.getElementById("inject-target");
        if (injectTarget) injectTarget.value = target.dataset.selectTarget;
        const messageBox = document.getElementById("inject-message");
        if (messageBox) messageBox.focus();
        return;
      }

      if (target.dataset.focusPrimaryInject) {
        const messageBox = document.getElementById("primary-inject-message");
        if (messageBox) messageBox.focus();
        return;
      }

      if (target.dataset.diffRepo && target.dataset.diffSha) {
        await loadDiff(target.dataset.diffRepo, target.dataset.diffSha);
        return;
      }

      if (target.id === "inject-submit") {
        const injectTarget = document.getElementById("inject-target");
        const messageBox = document.getElementById("inject-message");
        const selectedTarget = injectTarget?.value || "";
        const message = messageBox?.value || "";
        if (!selectedTarget || !message.trim()) {
          showToast("Pick a target and enter an instruction.", false);
          return;
        }
        try {
          await postAction("inject_message", { agentId: selectedTarget, message });
          messageBox.value = "";
        } catch (error) {
          showToast(String(error), false);
        }
        return;
      }

      if (target.id === "primary-inject-submit") {
        const messageBox = document.getElementById("primary-inject-message");
        const selectedTarget = target.dataset.primaryTarget || messageBox?.dataset.primaryTarget || "";
        const message = messageBox?.value || app.primaryInjectDraft || "";
        if (app.primarySending) {
          return;
        }
        if (!selectedTarget || !message.trim()) {
          showToast("Enter a message for this terminal.", false);
          return;
        }
        app.primarySending = true;
        renderPrimaryTerminal();
        try {
          const data = await postAction("write_terminal", { agentId: selectedTarget, text: message }, { refresh: false, successToast: false });
          if (data.terminal && data.terminal.target === selectedTarget) {
            app.primaryTerminal = data.terminal;
          }
          app.primaryInjectDraft = "";
          if (messageBox) messageBox.value = "";
          renderPrimaryTerminal();
        } catch (error) {
          showToast(String(error), false);
        } finally {
          app.primarySending = false;
          renderPrimaryTerminal();
        }
        window.setTimeout(() => fetchPrimaryTerminal(true), 120);
        window.setTimeout(() => fetchSnapshot(false), 200);
      }
    });

    document.addEventListener("keydown", async (event) => {
      if (event.target?.id !== "primary-inject-message") return;
      if (event.key !== "Enter" || event.isComposing) return;
      const messageBox = event.target;
      if (event.metaKey || event.ctrlKey || event.shiftKey) {
        event.preventDefault();
        const start = messageBox.selectionStart ?? messageBox.value.length;
        const end = messageBox.selectionEnd ?? messageBox.value.length;
        const nextValue = `${messageBox.value.slice(0, start)}\n${messageBox.value.slice(end)}`;
        messageBox.value = nextValue;
        messageBox.selectionStart = start + 1;
        messageBox.selectionEnd = start + 1;
        app.primaryInjectDraft = nextValue;
        return;
      }
      event.preventDefault();
      const button = document.getElementById("primary-inject-submit");
      if (button) button.click();
    });

    syncTabs();
    initGraphPan();
    renderLoadingState();
    fetchSnapshot(true).then(() => {
      if (app.lastSuccessMs) fetchPrimaryTerminal(true);
    });
    window.setInterval(() => {
      if (!app.lastSuccessMs) renderLoadingState(app.snapshot);
    }, 1000);
    window.setInterval(() => fetchPrimaryTerminal(false), PRIMARY_TERMINAL_POLL_MS);
    window.setInterval(() => fetchSnapshot(false), SNAPSHOT_POLL_MS);
