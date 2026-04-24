import { esc } from "./renderers/html.js";
import { hiddenCompletedIdsFromSnapshot } from "./convoys.mjs";
import { describeSnapshotHealth } from "./health.mjs";
import {
  findSnapshotPrimaryTerminalAgent,
  resolvePrimaryTerminalAgent,
  snapshotPrimaryTerminalIsDegraded,
} from "./primary-terminal.mjs";
import { describeRigRuntime } from "./rigs.mjs";
import {
  getTranscriptView,
  hasTranscriptItems,
  renderPrimaryTranscript,
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
      selectionCleared: false,
      selectedScope: "all",
      includeSystem: false,
      hideCompleted: "all",
      primaryInjectDraft: "",
      primaryComposerFocusPending: true,
      primarySending: false,
      gtControlInFlight: false,
      gtControlAction: "",
      rigControlInFlight: false,
      rigControlAction: "",
      rigControlScope: "",
      scopeMenuOpen: false,
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
      document.body.classList.toggle("task-spine-tab-active", app.activeTab === "task-spine");
      if (app.activeTab === "mayor") {
        window.requestAnimationFrame(() => restorePrimaryLogState());
      }
      if (app.activeTab === "task-spine") {
        window.requestAnimationFrame(updateGraphViewportMap);
      }
    }

    function selectTab(tab) {
      const panel = [...document.querySelectorAll("[data-tab-panel]")].find((item) => item.dataset.tabPanel === tab);
      if (!panel) return;
      const previousTab = app.activeTab;
      app.activeTab = tab;
      syncTabs();
      if (tab === "mayor" && previousTab !== "mayor") {
        requestPrimaryComposerFocus();
      }
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
      return {
        hooked: 0,
        in_progress: 1,
        blocked: 2,
        open: 3,
        deferred: 4,
        pinned: 5,
        closed: 6,
        commit: 7,
      }[status] ?? 9;
    }

    function exactStatusTone(status) {
      return {
        hooked: "hooked",
        in_progress: "in_progress",
        blocked: "blocked",
        open: "open",
        closed: "closed",
        deferred: "deferred",
        pinned: "pinned",
        commit: "commit",
      }[status] || "";
    }

    function gtNodeStatus(node) {
      if (!node) return "";
      if (node.kind === "commit") return "commit";
      return node.status || "";
    }

    function nodeTone(node) {
      return exactStatusTone(gtNodeStatus(node)) || "unknown";
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

    function nodeHasLabel(node, label) {
      return (node.labels || []).some((value) => String(value) === label);
    }

    function isIdentityGraphNode(node) {
      const description = String(node.description || "");
      const normalizedDescription = description.trimStart().toLowerCase();
      if (nodeHasLabel(node, "gt:rig")) return true;
      if (
        normalizedDescription.startsWith("rig identity bead for ")
        || normalizedDescription.startsWith("polecat identity bead for ")
      ) {
        return true;
      }
      if (!nodeHasLabel(node, "gt:agent")) return false;
      const id = String(node.id || "").toLowerCase();
      const title = String(node.title || "").toLowerCase();
      return /^role_type:\s*polecat\s*$/im.test(description)
        || id.includes("-polecat-")
        || title.includes("-polecat-");
    }

    function scopeLabel(scope) {
      return scope === "all" ? "All" : scope === "hq" ? "HQ" : scope;
    }

    function rigSnapshotInfo(scope) {
      const normalized = normalizeScope(scope);
      if (!normalized || normalized === "hq") return null;
      return (app.snapshot?.rigs || []).find((rig) => {
        const name = normalizeScope(rig.name);
        const rigScope = normalizeScope(rig.scope || rig.name);
        return name === normalized || rigScope === normalized;
      }) || null;
    }

    function rigRuntimeSummary(rig) {
      return describeRigRuntime(rig, app.snapshot?.agents || []);
    }

    function rigMenuMeta(scope) {
      if (scope === "all") return "view";
      if (scope === "hq") return "town";
      const rig = rigSnapshotInfo(scope);
      if (!rig) return "";
      return rigRuntimeSummary(rig).label;
    }

    function rigControlForScope(scope) {
      const rig = rigSnapshotInfo(scope);
      if (!rig) return null;
      const name = String(rig.name || scope);
      const runtime = rigRuntimeSummary(rig);
      if (runtime.blocked) {
        return {
          action: "",
          rig: name,
          label: runtime.label,
          ariaLabel: `${scopeLabel(scope)} is ${runtime.label}`,
          tone: "",
          disabled: true,
        };
      }
      if (app.rigControlInFlight && app.rigControlScope === name) {
        const action = app.rigControlAction || (runtime.running ? "stop" : "run");
        return {
          action,
          rig: name,
          label: action === "stop" ? "Stop" : "Run",
          ariaLabel: action === "stop" ? `Stopping ${name}` : `Starting ${name}`,
          tone: action === "stop" ? "danger" : "ready",
          disabled: true,
        };
      }
      if (runtime.running) {
        return {
          action: "stop",
          rig: name,
          label: "Stop",
          ariaLabel: `Stop ${name}`,
          tone: "danger",
          disabled: false,
        };
      }
      return {
        action: "run",
        rig: name,
        label: "Run",
        ariaLabel: `Run ${name}`,
        tone: "ready",
        disabled: false,
      };
    }

    function scopeOptions() {
      const scopes = new Set(["hq"]);
      const snapshot = app.snapshot || {};
      (snapshot.rigs || []).forEach((rig) => {
        const scope = normalizeScope(rig.scope || rig.name);
        if (scope) scopes.add(scope);
      });
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

      return ["all", ...[...scopes].sort((a, b) => {
        if (a === "hq") return -1;
        if (b === "hq") return 1;
        return a.localeCompare(b);
      })];
    }

    function syncScopeSelector() {
      const root = document.getElementById("scope-menu-root");
      const button = document.getElementById("scope-menu-button");
      const value = document.getElementById("scope-menu-value");
      const menu = document.getElementById("scope-menu");
      if (!root || !button || !value || !menu) return;

      const options = scopeOptions();
      if (app.selectedScope !== "all" && !options.includes(app.selectedScope)) {
        app.selectedScope = "all";
      }
      value.textContent = scopeLabel(app.selectedScope);
      root.classList.toggle("open", app.scopeMenuOpen);
      button.setAttribute("aria-expanded", app.scopeMenuOpen ? "true" : "false");
      menu.hidden = !app.scopeMenuOpen;

      menu.innerHTML = options.map((scope) => {
        const control = rigControlForScope(scope);
        const optionButton = `
          <button
            type="button"
            class="app-dropdown-item ${control ? "app-dropdown-item-main" : ""}"
            role="menuitemradio"
            aria-checked="${scope === app.selectedScope ? "true" : "false"}"
            data-scope-option="${esc(scope)}"
          >
            <span class="app-dropdown-item-label">${esc(scopeLabel(scope))}</span>
            <span class="app-dropdown-item-meta">${esc(rigMenuMeta(scope))}</span>
          </button>
        `;
        if (!control) return optionButton;
        return `
          <div class="app-dropdown-row" role="none">
            ${optionButton}
            <button
              type="button"
              class="app-dropdown-action ${esc(control.tone)}"
              role="menuitem"
              ${control.action ? `data-rig-action="${esc(control.action)}"` : ""}
              data-rig="${esc(control.rig)}"
              aria-label="${esc(control.ariaLabel)}"
              title="${esc(control.ariaLabel)}"
              ${control.disabled ? "disabled" : ""}
            >${esc(control.label)}</button>
          </div>
        `;
      }).join("");
    }

    function scopeMenuButtons() {
      return [...document.querySelectorAll("#scope-menu button:not([disabled])")];
    }

    function focusCurrentScopeMenuItem() {
      const items = scopeMenuButtons();
      const current = items.find((item) => item.dataset.scopeOption === app.selectedScope) || items[0];
      current?.focus();
    }

    function focusAdjacentScopeMenuItem(delta) {
      const items = scopeMenuButtons();
      if (!items.length) return;
      const current = document.activeElement;
      const index = items.includes(current) ? items.indexOf(current) : 0;
      items[(index + delta + items.length) % items.length]?.focus();
    }

    function setScopeMenuOpen(open, focusCurrent = false) {
      app.scopeMenuOpen = open;
      syncScopeSelector();
      if (open && focusCurrent) {
        window.requestAnimationFrame(focusCurrentScopeMenuItem);
      }
    }

    function selectScope(scope) {
      app.selectedScope = scope;
      setScopeMenuOpen(false);
      ensureSelection();
      renderAll();
      fetchPrimaryTerminal(true);
    }

    function visibleGraphNodes() {
      const nodes = app.snapshot?.graph?.nodes || [];
      const hiddenCompleted = hiddenCompletedIds();
      return nodes.filter((node) => {
        if (isIdentityGraphNode(node)) return false;
        if (!matchesScope(node.scope)) return false;
        if (!(app.includeSystem || !node.is_system)) return false;
        if (node.kind === "task" && hiddenCompleted.has(node.id)) return false;
        if (node.kind === "commit" && node.parent && hiddenCompleted.has(node.parent)) return false;
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

    function hiddenCompletedIds() {
      return hiddenCompletedIdsFromSnapshot(app.snapshot, app.hideCompleted);
    }

    function hiddenCompletedVisibleNodeCount(hiddenCompleted) {
      const nodes = app.snapshot?.graph?.nodes || [];
      return nodes.filter((node) => {
        if (isIdentityGraphNode(node)) return false;
        if (!matchesScope(node.scope)) return false;
        if (!(app.includeSystem || !node.is_system)) return false;
        if (node.kind === "task" && hiddenCompleted.has(node.id)) return true;
        return node.kind === "commit" && node.parent && hiddenCompleted.has(node.parent);
      }).length;
    }

    function filteredTaskNodes() {
      return visibleGraphNodes().filter((node) => node.kind === "task" && !node.is_system);
    }

    function visibleTaskStatusCounts() {
      const counts = {};
      filteredTaskNodes().forEach((task) => {
        const status = String(task.status || "");
        counts[status] = (counts[status] || 0) + 1;
      });
      return Object.entries(counts).sort(([a], [b]) => {
        return nodeStatusOrder(a) - nodeStatusOrder(b) || a.localeCompare(b);
      });
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
        const taskStatus = taskId ? (node?.status || hook.status || "") : "";
        const taskStored = taskId ? (node?.status || hook.status || "") : "";
        const isSystem = Boolean(node?.is_system || (hook.title || "").startsWith("mol-"));
        const lastEvent = (agent.events || []).at(-1) || null;
        const churn = summarizeAgentChurn(agent);
        return {
          ...agent,
          taskId,
          taskTitle,
          taskStatus,
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
          || nodeStatusOrder(a.taskStatus || "")
          - nodeStatusOrder(b.taskStatus || "")
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

    function focusPrimaryComposerIfPending() {
      if (!app.primaryComposerFocusPending || app.activeTab !== "mayor" || document.hidden) return;
      const box = document.getElementById("primary-inject-message");
      if (!box || box.disabled) return;
      app.primaryComposerFocusPending = false;
      if (document.activeElement === box) return;
      box.focus({ preventScroll: true });
      const end = box.value.length;
      box.selectionStart = end;
      box.selectionEnd = end;
    }

    function requestPrimaryComposerFocus() {
      app.primaryComposerFocusPending = true;
      window.requestAnimationFrame(focusPrimaryComposerIfPending);
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
        app.selectionCleared = false;
        return;
      }
      const map = new Set(nodes.map((node) => node.id));
      if (app.selectedNodeId && map.has(app.selectedNodeId)) return;
      if (!app.selectedNodeId && app.selectionCleared) return;
      const next =
        nodes.find((node) => node.status === "hooked" && node.kind === "task") ||
        nodes.find((node) => node.status === "in_progress" && node.kind === "task") ||
        nodes.find((node) => node.status === "blocked" && node.kind === "task") ||
        nodes.find((node) => node.kind === "task") ||
        nodes[0];
      app.selectedNodeId = next.id;
      app.selectionCleared = false;
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

    function graphEdgePath(source, target) {
      const startX = source.x + source.width;
      const startY = source.y + source.height / 2;
      const endX = target.x;
      const endY = target.y + target.height / 2;
      const delta = Math.max(60, (endX - startX) * 0.45);
      return `M ${startX} ${startY} C ${startX + delta} ${startY}, ${endX - delta} ${endY}, ${endX} ${endY}`;
    }

    function renderGraphMapPreview(nodes, edges, positions, width, height) {
      const preview = document.getElementById("graph-map-preview");
      if (!preview) return;
      preview.setAttribute("viewBox", `0 0 ${width} ${height}`);
      preview.setAttribute("preserveAspectRatio", "none");

      const edgeHtml = edges
        .filter((edge) => positions.has(edge.source) && positions.has(edge.target))
        .map((edge) => {
          const source = positions.get(edge.source);
          const target = positions.get(edge.target);
          return `<path class="graph-map-edge ${esc(edge.kind)}" d="${graphEdgePath(source, target)}"></path>`;
        })
        .join("");
      const nodeHtml = nodes.map((node) => {
        const pos = positions.get(node.id);
        const statusCls = nodeTone(node);
        const selectedCls = node.id === app.selectedNodeId ? "selected" : "";
        const rx = node.kind === "commit" ? pos.height / 2 : 14;
        return `
          <rect
            class="graph-map-node ${esc(node.kind)} ${esc(statusCls)} ${selectedCls}"
            x="${pos.x}"
            y="${pos.y}"
            width="${pos.width}"
            height="${pos.height}"
            rx="${rx}"
          ></rect>
        `;
      }).join("");
      preview.innerHTML = `${edgeHtml}${nodeHtml}`;
    }

    function updateGraphViewportMap() {
      const wrap = document.querySelector(".graph-wrap");
      const stage = document.getElementById("graph-stage");
      const map = document.getElementById("graph-viewport-map");
      const content = document.getElementById("graph-map-content");
      const view = document.getElementById("graph-map-view");
      if (!wrap || !stage || !map || !content || !view) return;

      const contentWidth = stage.offsetWidth || wrap.scrollWidth || 0;
      const contentHeight = stage.offsetHeight || wrap.scrollHeight || 0;
      const viewportWidth = wrap.clientWidth || 0;
      const viewportHeight = wrap.clientHeight || 0;
      const needsMap = contentWidth > 0
        && contentHeight > 0
        && (contentWidth > viewportWidth + 2 || contentHeight > viewportHeight + 2);
      map.hidden = !needsMap;
      if (!needsMap) return;

      const surfaceAspect = contentWidth / contentHeight;
      const mapInset = 4;
      const maxOuterWidth = Math.min(156, Math.max(96, viewportWidth * 0.24));
      const maxOuterHeight = Math.min(108, Math.max(72, viewportHeight * 0.24));
      const maxMapWidth = Math.max(40, maxOuterWidth - mapInset * 2);
      const maxMapHeight = Math.max(40, maxOuterHeight - mapInset * 2);
      let mapWidth = maxMapWidth;
      let mapHeight = mapWidth / surfaceAspect;
      if (mapHeight > maxMapHeight) {
        mapHeight = maxMapHeight;
        mapWidth = mapHeight * surfaceAspect;
      }
      mapWidth = Math.round(mapWidth);
      mapHeight = Math.round(mapHeight);
      const scaleX = mapWidth / contentWidth;
      const scaleY = mapHeight / contentHeight;

      map.style.width = `${mapWidth + mapInset * 2}px`;
      map.style.height = `${mapHeight + mapInset * 2}px`;
      content.style.width = `${mapWidth}px`;
      content.style.height = `${mapHeight}px`;
      content.style.transform = `translate(${mapInset}px, ${mapInset}px)`;
      const viewWidth = Math.min(mapWidth, Math.max(10, Math.round(viewportWidth * scaleX)));
      const viewHeight = Math.min(mapHeight, Math.max(10, Math.round(viewportHeight * scaleY)));
      const viewX = Math.min(Math.max(0, mapWidth - viewWidth), Math.max(0, Math.round(wrap.scrollLeft * scaleX)));
      const viewY = Math.min(Math.max(0, mapHeight - viewHeight), Math.max(0, Math.round(wrap.scrollTop * scaleY)));
      view.style.width = `${viewWidth}px`;
      view.style.height = `${viewHeight}px`;
      view.style.transform = `translate(${viewX}px, ${viewY}px)`;
    }

    function centerGraphAtMinimapPoint(event) {
      const wrap = document.querySelector(".graph-wrap");
      const stage = document.getElementById("graph-stage");
      const content = document.getElementById("graph-map-content");
      if (!wrap || !stage || !content) return;
      const rect = content.getBoundingClientRect();
      if (!rect.width || !rect.height) return;

      const localX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
      const localY = Math.max(0, Math.min(rect.height, event.clientY - rect.top));
      const contentWidth = stage.offsetWidth || wrap.scrollWidth || 0;
      const contentHeight = stage.offsetHeight || wrap.scrollHeight || 0;
      const graphX = (localX / rect.width) * contentWidth;
      const graphY = (localY / rect.height) * contentHeight;
      const maxLeft = Math.max(0, wrap.scrollWidth - wrap.clientWidth);
      const maxTop = Math.max(0, wrap.scrollHeight - wrap.clientHeight);
      wrap.scrollLeft = Math.max(0, Math.min(maxLeft, graphX - wrap.clientWidth / 2));
      wrap.scrollTop = Math.max(0, Math.min(maxTop, graphY - wrap.clientHeight / 2));
      updateGraphViewportMap();
    }

    function initGraphPan() {
      const wrap = document.querySelector(".graph-wrap");
      if (!wrap || wrap.dataset.panReady === "1") return;
      wrap.dataset.panReady = "1";
      const map = document.getElementById("graph-viewport-map");

      const endPan = (event) => {
        const pan = app.graphPan;
        if (!pan.active) return;
        if (event && event.pointerId !== undefined && pan.pointerId !== null && event.pointerId !== pan.pointerId) return;
        const isClick = Boolean(event && event.type === "pointerup" && !pan.moved);
        const shouldSelectNode = Boolean(isClick && pan.downNodeId);
        const shouldClearSelection = Boolean(isClick && !pan.downNodeId && app.selectedNodeId);
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
          app.selectionCleared = false;
          renderAll();
        } else if (shouldClearSelection) {
          app.selectedNodeId = null;
          app.selectionCleared = true;
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
        event.preventDefault();
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
          window.getSelection()?.removeAllRanges();
        }
        if (!pan.moved) return;
        event.preventDefault();
        wrap.scrollLeft = pan.startScrollLeft - dx;
        wrap.scrollTop = pan.startScrollTop - dy;
        app.suppressGraphClick = true;
        updateGraphViewportMap();
      });

      wrap.addEventListener("scroll", updateGraphViewportMap, { passive: true });
      wrap.addEventListener("pointerup", endPan);
      wrap.addEventListener("pointercancel", endPan);
      wrap.addEventListener("lostpointercapture", endPan);

      if (map && map.dataset.centerReady !== "1") {
        map.dataset.centerReady = "1";
        let mapPointerId = null;
        const endMapPointer = () => {
          mapPointerId = null;
        };
        map.addEventListener("pointerdown", (event) => {
          if (event.button !== 0) return;
          mapPointerId = event.pointerId;
          centerGraphAtMinimapPoint(event);
          event.preventDefault();
          event.stopPropagation();
          if (map.setPointerCapture) {
            try {
              map.setPointerCapture(event.pointerId);
            } catch {}
          }
        });
        map.addEventListener("pointermove", (event) => {
          if (mapPointerId !== event.pointerId) return;
          centerGraphAtMinimapPoint(event);
          event.preventDefault();
          event.stopPropagation();
        });
        map.addEventListener("pointerup", endMapPointer);
        map.addEventListener("pointercancel", endMapPointer);
        map.addEventListener("click", (event) => {
          event.preventDefault();
          event.stopPropagation();
        });
      }
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
        host.innerHTML = `
          <div class="empty">
            Loading primary terminal state for ${esc(targetAgent.target)}.
          </div>
        `;
        renderMayorEvents();
        return;
      }

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

      host.innerHTML = `
        <div class="mayor-terminal-surface">
          ${usesTranscript
            ? renderPrimaryTranscript(transcriptView, { openDetails: app.openDetails })
            : `<pre id="primary-terminal-log" class="log-block primary-log-block">${esc(terminalText)}</pre>`}
          <div class="stack primary-composer">
            <textarea
              id="primary-inject-message"
              data-primary-target="${esc(agent.target)}"
              placeholder="Message ${esc(agent.target)}"
              ${agent.has_session ? "" : "disabled"}
            >${esc(app.primaryInjectDraft)}</textarea>
            <div class="primary-composer-actions">
              <button type="button" id="primary-inject-submit" data-primary-target="${esc(agent.target)}" ${(agent.has_session && !app.primarySending) ? "" : "disabled"}>${app.primarySending ? "Sending..." : "Send"}</button>
              <button type="button" data-pause-agent="${esc(agent.target)}">Pause</button>
            </div>
          </div>
        </div>
      `;
      renderMayorEvents();
      restorePrimaryLogState();
      restoreTmuxLogStates(host);
      restorePrimaryComposerState(composerState);
      focusPrimaryComposerIfPending();
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
        if (node.linked_commit_count) chips.push(`<span class="chip commit">${esc(node.linked_commit_count)} commit${node.linked_commit_count === 1 ? "" : "s"}</span>`);
      } else {
        chips.push(`<span class="chip commit">${esc(node.short_sha || "commit")}</span>`);
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
            return nodeStatusOrder(gtNodeStatus(na)) - nodeStatusOrder(gtNodeStatus(nb)) || a.localeCompare(b);
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
          return nodeStatusOrder(gtNodeStatus(na)) - nodeStatusOrder(gtNodeStatus(nb)) || a.localeCompare(b);
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
      const hiddenCompleted = hiddenCompletedIds();
      const hiddenCompletedCount = hiddenCompletedVisibleNodeCount(hiddenCompleted);
      graphSummary.textContent = app.hideCompleted !== "none" && hiddenCompletedCount
        ? `${nodes.length} visible nodes · ${hiddenCompletedCount} completed item${hiddenCompletedCount === 1 ? "" : "s"} hidden`
        : `${nodes.length} visible nodes`;

      const stage = document.getElementById("graph-stage");
      const svg = document.getElementById("graph-svg");
      const nodesHost = document.getElementById("graph-nodes");

      if (!nodes.length) {
        stage.style.width = "100%";
        stage.style.height = "560px";
        svg.innerHTML = "";
        nodesHost.innerHTML = `<div class="empty" style="margin: 18px;">No graph nodes match the current filter.</div>`;
        const map = document.getElementById("graph-viewport-map");
        if (map) map.hidden = true;
        const preview = document.getElementById("graph-map-preview");
        if (preview) preview.innerHTML = "";
        return;
      }

      const metrics = estimateNodeMetrics(nodes);
      const { positions, width, height } = computeLayout(nodes, edges, metrics);
      renderGraphMapPreview(nodes, edges, positions, width, height);
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
          const active = edge.source === selected || edge.target === selected;
          const muted = selected && !active;
          return `<path class="graph-edge ${esc(edge.kind)} ${active ? "active" : ""} ${muted ? "muted" : ""}" d="${graphEdgePath(source, target)}"></path>`;
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
      window.requestAnimationFrame(updateGraphViewportMap);
    }

    function renderFocus() {
      const host = document.getElementById("focus-panel");
      const node = getSelectedNode();
      if (!node) {
        host.innerHTML = `<div class="empty">Select a graph node to inspect task state, linked commits, and controls.</div>`;
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
      const canRetry = isTask && !node.is_system && (node.status === "hooked" || node.status === "in_progress");
      const defaultTarget = selectedAgents[0]?.target || "";
      const globalTargets = scopedAgents.filter((agent) => agent.has_session).map((agent) => agent.target);
      const targetOptions = [...new Set([defaultTarget, ...globalTargets].filter(Boolean))];

      const memoryRows = taskMemory.length
        ? taskMemory.slice(0, 8).map((entry) => `
            <div class="memory-row">
              <div class="memory-top">
                <div>
                  <div><strong>${esc(entry.short_sha || "commit")}</strong> ${entry.branch ? `· ${esc(entry.branch)}` : ""}</div>
                  <div class="subtle">${esc(entry.source)} ${entry.repo_label ? `· ${entry.repo_label}` : ""}</div>
                </div>
                ${entry.repo_id ? `<button type="button" data-diff-repo="${esc(entry.repo_id)}" data-diff-sha="${esc(entry.sha)}">Load diff</button>` : ""}
              </div>
              <div>${esc(entry.subject || "(no subject)")}</div>
            </div>
          `).join("")
        : `<div class="empty">No linked commits yet.</div>`;

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
        node.linked_commit_count ? `<span class="chip commit">${esc(node.linked_commit_count)} linked commits</span>` : "",
      ].filter(Boolean).join("");
      const descriptionKey = `focus-description:${node.id}`;
      const descriptionOpen = app.openDetails.has(descriptionKey);
      const descriptionHtml = node.description
        ? `
          <div class="focus-description-details ${descriptionOpen ? "open" : ""}">
            <div class="focus-description-summary">
              <span>Description</span>
              <div class="codex-inline-right">
                <button
                  type="button"
                  class="codex-inline-toggle"
                  data-inline-toggle="${esc(descriptionKey)}"
                  aria-expanded="${descriptionOpen ? "true" : "false"}"
                  aria-label="${descriptionOpen ? "Collapse description" : "Expand description"}"
                ><span class="codex-inline-chevron" aria-hidden="true">▸</span></button>
              </div>
            </div>
            ${descriptionOpen ? `<div class="focus-description">${esc(node.description)}</div>` : ""}
          </div>
        `
        : "";

      host.innerHTML = `
        <div class="card focus-summary-card">
          <h3>${esc(node.kind === "commit" ? "Commit Focus" : "Task Focus")}</h3>
          <div class="focus-title">${esc(node.title)}</div>
          <div class="subtle mono focus-id">${esc(node.id)}</div>
          <div class="node-meta focus-meta">${detailRows}</div>
          ${descriptionHtml}
        </div>

        <div class="card">
          <h3>Intervention</h3>
          <div class="stack">
            <button type="button" data-retry-task="${esc(node.id)}" ${canRetry ? "" : "disabled"}>Retry task</button>
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
          <h3>Linked Commits</h3>
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
                ${group.memory?.length ? `<span class="chip commit">${esc(group.memory.length)} commit link${group.memory.length === 1 ? "" : "s"}</span>` : ""}
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
          <h3>Linked Commits</h3>
          <div class="stack">
            ${taskId
              ? (taskMemory.length
                  ? taskMemory.slice(0, 8).map((entry) => `
                      <div class="memory-row">
                        <div class="memory-top">
                          <div>
                            <div><strong>${esc(entry.short_sha || "commit")}</strong> ${entry.branch ? `· ${esc(entry.branch)}` : ""}</div>
                            <div class="subtle">${esc(entry.source)} ${entry.repo_label ? `· ${esc(entry.repo_label)}` : ""}</div>
                          </div>
                          ${entry.repo_id ? `<button type="button" data-diff-repo="${esc(entry.repo_id)}" data-diff-sha="${esc(entry.sha)}">Load diff</button>` : ""}
                        </div>
                        <div>${esc(entry.subject || "(no subject)")}</div>
                      </div>
                    `).join("")
                  : `<div class="empty">No linked commits attached to ${esc(taskId)}.</div>`)
              : `<div class="empty">Select a task or commit node to inspect linked commits.</div>`}
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
      const statusCounts = visibleTaskStatusCounts();
      statusHost.innerHTML = `
        <div class="card">
          <h3>Visible Task Statuses</h3>
          <div class="node-meta">
            ${statusCounts.length
              ? statusCounts.map(([key, value]) => `<span class="chip ${esc(exactStatusTone(key))}">${esc(key || "unknown")} ${esc(value)}</span>`).join("")
              : `<span class="chip">No visible task nodes</span>`}
          </div>
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
      renderTaskSpineLegend();
    }

    function renderTaskSpineLegend() {
      const host = document.getElementById("task-spine-legend-body");
      if (!host) return;
      const statusCounts = visibleTaskStatusCounts();
      const statusRows = statusCounts.map(([status, count]) => `
        <tr>
          <td><span class="chip ${esc(exactStatusTone(status))}">${esc(status || "unknown")}</span></td>
          <td>${esc(count)} visible task node${count === 1 ? "" : "s"} with this raw GT status.</td>
        </tr>
      `).join("");
      const memoryRow = `
        <tr>
          <td><span class="chip commit">linked commit</span></td>
          <td>Commit node linked to GT tasks; not a GT task status.</td>
        </tr>
      `;
      host.innerHTML = statusRows ? `${statusRows}${memoryRow}` : `
        <tr>
          <td><span class="chip">empty</span></td>
          <td>No raw GT statuses on visible task nodes.</td>
        </tr>
      `;
    }

    function getSnapshotHealth(snapshot) {
      return describeSnapshotHealth(snapshot, { loading: !app.lastSuccessMs });
    }

    function updateLivePill(snapshot, health = getSnapshotHealth(snapshot)) {
      const pill = document.getElementById("live-pill");
      const label = document.getElementById("live-label");
      const tooltip = document.getElementById("live-pill-tooltip");
      pill.classList.remove("stale", "error", "loading", "partial", "stopped");
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
      if (health.tone === "partial") {
        pill.classList.add("partial");
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
      syncScopeSelector();
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
      document.getElementById("git-panel").innerHTML = `<div class="empty loading-empty">Git history is loading.</div>`;
      document.getElementById("crew-summary").textContent = "loading";
      document.getElementById("crew-panel").innerHTML = `<div class="empty loading-empty">Crew workspaces are loading.</div>`;
      document.getElementById("stores-summary").textContent = "loading";
      document.getElementById("stores-panel").innerHTML = `<div class="empty loading-empty">Bead stores are loading.</div>`;
      document.getElementById("status-panel").innerHTML = `<div class="empty loading-empty">Status legend is loading.</div>`;
      document.getElementById("raw-status").textContent = "Waiting for gt status output...";
      document.getElementById("raw-vitals").textContent = "Waiting for gt vitals output...";
      document.getElementById("mayor-events-panel").innerHTML = `<div class="empty loading-empty">Mayor events are loading.</div>`;
      document.getElementById("alerts-panel").innerHTML = `<div class="empty loading-empty">Attention items are loading.</div>`;
      renderTaskSpineLegend(snapshot || {});
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

    async function runRigControl(action, rig) {
      if (app.rigControlInFlight || !rig) return;
      app.rigControlInFlight = true;
      app.rigControlAction = action === "stop" ? "stop" : "run";
      app.rigControlScope = rig;
      syncScopeSelector();
      try {
        const data = await postAction(action === "stop" ? "stop_rig" : "run_rig", { rig }, { refresh: false, successToast: false });
        if (data?.ok === false) {
          showToast(data.output || `Failed to ${app.rigControlAction} ${rig}.`, false);
          await fetchSnapshot(true);
          return;
        }
        showToast(data?.output || `Rig ${rig} ${app.rigControlAction === "stop" ? "shutdown" : "startup"} requested.`, true);
        await fetchSnapshot(true);
        window.setTimeout(() => fetchSnapshot(true), 1200);
        window.setTimeout(() => fetchSnapshot(true), 3200);
      } catch (error) {
        showToast(String(error), false);
      } finally {
        app.rigControlInFlight = false;
        app.rigControlAction = "";
        app.rigControlScope = "";
        syncScopeSelector();
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
    document.getElementById("scope-menu-button").addEventListener("click", () => {
      setScopeMenuOpen(!app.scopeMenuOpen, true);
    });
    document.getElementById("scope-menu-button").addEventListener("keydown", (event) => {
      if (!["ArrowDown", "Enter", " "].includes(event.key)) return;
      event.preventDefault();
      setScopeMenuOpen(true, true);
    });
    document.getElementById("scope-menu").addEventListener("click", async (event) => {
      event.stopPropagation();
      const actionButton = event.target?.closest?.("[data-rig-action]");
      if (actionButton) {
        event.preventDefault();
        const action = actionButton.dataset.rigAction || "run";
        const rig = actionButton.dataset.rig || "";
        await runRigControl(action, rig);
        return;
      }
      const scopeButton = event.target?.closest?.("[data-scope-option]");
      if (scopeButton) {
        event.preventDefault();
        selectScope(scopeButton.dataset.scopeOption || "all");
      }
    });
    document.getElementById("scope-menu").addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setScopeMenuOpen(false);
        document.getElementById("scope-menu-button")?.focus();
        return;
      }
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        focusAdjacentScopeMenuItem(event.key === "ArrowDown" ? 1 : -1);
      }
    });
    document.addEventListener("click", (event) => {
      const root = document.getElementById("scope-menu-root");
      if (!app.scopeMenuOpen || root?.contains(event.target)) return;
      setScopeMenuOpen(false);
    });
    document.getElementById("include-system").addEventListener("change", (event) => {
      app.includeSystem = event.target.checked;
      ensureSelection();
      renderAll();
    });
    document.getElementById("hide-completed").addEventListener("change", (event) => {
      app.hideCompleted = event.target.value;
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
    window.addEventListener("resize", () => window.requestAnimationFrame(updateGraphViewportMap));

    document.addEventListener("click", async (event) => {
      if (app.suppressGraphClick) {
        app.suppressGraphClick = false;
        if (event.target.closest(".graph-wrap")) {
          event.preventDefault();
          return;
        }
      }
      const target = event.target.closest("[data-node-id], [data-inline-toggle], [data-retry-task], [data-pause-agent], [data-pause-target], [data-select-target], [data-diff-repo], #inject-submit, #primary-inject-submit");
      if (!target) return;

      if (target.dataset.inlineToggle) {
        const key = target.dataset.inlineToggle;
        if (app.openDetails.has(key)) {
          app.openDetails.delete(key);
        } else {
          app.openDetails.add(key);
        }
        if (key.startsWith("focus-description:")) {
          renderFocus();
        } else {
          renderPrimaryTerminal();
        }
        return;
      }

      if (target.dataset.nodeId) {
        app.selectedNodeId = target.dataset.nodeId;
        app.selectionCleared = false;
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
