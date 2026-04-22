import { esc } from "./html.js";
import { renderMarkdown } from "./markdown.js";

function toolOutputKey(item) {
  return `claude-tool-output:${item?.call_id || item?.timestamp || item?.summary || ""}`;
}

function renderMessage(item) {
  const kind = String(item?.kind || "");
  const time = item?.time ? `<div class="codex-time">${esc(item.time)}</div>` : "";
  const label = kind === "assistant" ? "claude" : "user";
  const messageBody = kind === "assistant"
    ? renderMarkdown(item.text || "")
    : `<div class="codex-text">${esc(item.text || "")}</div>`;
  return `
    <div class="codex-item ${esc(kind)}">
      <div class="codex-head">
        <div class="codex-head-left">
          <span class="codex-role">${esc(label)}</span>
          ${item?.model ? `<span class="codex-phase">${esc(item.model)}</span>` : ""}
        </div>
        ${time}
      </div>
      <div class="codex-bubble">
        ${messageBody}
      </div>
    </div>
  `;
}

function renderToolCall(item) {
  const tool = item?.tool || "tool";
  const summary = item?.summary || tool;
  const time = item?.time ? `<div class="codex-time">${esc(item.time)}</div>` : "";
  return `
    <div class="codex-item codex-inline-row">
      <div class="codex-inline-main">
        <span class="chip memory">${esc(tool)}</span>
        <span class="codex-inline-text">${esc(summary)}</span>
      </div>
      <div class="codex-inline-right">
        ${time}
      </div>
    </div>
  `;
}

function renderToolOutput(item, options) {
  const openDetails = options?.openDetails || new Set();
  const tool = item?.tool || "tool";
  const summary = item?.summary || tool;
  const outputKey = toolOutputKey(item);
  const isOpen = openDetails.has(outputKey);
  const time = item?.time ? `<div class="codex-time">${esc(item.time)}</div>` : "";
  return `
    <div class="codex-item codex-inline-system ${isOpen ? "open" : ""}">
      <div class="codex-inline-row">
        <div class="codex-inline-main">
          <span class="chip ${item?.is_error ? "stuck" : "memory"}">${esc(tool)}</span>
          <span class="codex-inline-text">${esc(summary)}</span>
        </div>
        <div class="codex-inline-right">
          ${time}
          <button
            type="button"
            class="codex-inline-toggle"
            data-inline-toggle="${esc(outputKey)}"
            aria-expanded="${isOpen ? "true" : "false"}"
            aria-label="${isOpen ? "Collapse output" : "Expand output"}"
          ><span class="codex-inline-chevron" aria-hidden="true">▸</span></button>
        </div>
      </div>
      ${isOpen ? `<pre class="codex-output">${esc(item.text || "")}</pre>` : ""}
    </div>
  `;
}

function renderTranscriptItem(item, options) {
  const kind = String(item?.kind || "");
  if (kind === "user" || kind === "assistant") return renderMessage(item);
  if (kind === "tool_call") return renderToolCall(item);
  if (kind === "tool_output") return renderToolOutput(item, options);
  const time = item?.time ? `<div class="codex-time">${esc(item.time)}</div>` : "";
  if (kind === "reasoning") {
    return `
      <div class="codex-item codex-inline-row">
        <div class="codex-inline-main">
          <span class="chip ice">${esc(item.summary || "Thinking...")}</span>
        </div>
        <div class="codex-inline-right">
          ${time}
        </div>
      </div>
    `;
  }
  const summary = item?.summary || item?.event_type || "event";
  return `
    <div class="codex-item event">
      <div class="codex-head">
        <div class="codex-head-left">
          <span class="codex-role">system</span>
        </div>
        ${time}
      </div>
      <div class="codex-bubble">
        <div class="codex-event-text">${esc(summary)}</div>
      </div>
    </div>
  `;
}

export function renderClaudeTranscript(view, options = {}) {
  const items = view?.items || [];
  if (!items.length) {
    return `
      <div id="primary-terminal-log" class="primary-log-block codex-transcript" style="margin-top:12px;">
        <div class="empty">No Claude transcript items are visible for this session yet.</div>
      </div>
    `;
  }
  return `
    <div id="primary-terminal-log" class="primary-log-block codex-transcript" style="margin-top:12px;">
      ${items.map((item) => renderTranscriptItem(item, options)).join("")}
    </div>
  `;
}
