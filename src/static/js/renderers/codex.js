import { esc } from "./html.js";
import { renderMarkdown } from "./markdown.js";

function toolOutputKey(item) {
  return `codex-tool-output:${item?.call_id || item?.timestamp || item?.summary || ""}`;
}

function renderTranscriptItem(item, options) {
  const kind = String(item?.kind || "");
  const openDetails = options?.openDetails || new Set();
  const time = item?.time ? `<div class="codex-time">${esc(item.time)}</div>` : "";
  if (kind === "user" || kind === "assistant") {
    const phase = item?.phase ? `<span class="codex-phase">${esc(item.phase)}</span>` : "";
    const messageBody = kind === "assistant"
      ? renderMarkdown(item.text || "")
      : `<div class="codex-text">${esc(item.text || "")}</div>`;
    return `
      <div class="codex-item ${esc(kind)}">
        <div class="codex-head">
          <div class="codex-head-left">
            <span class="codex-role">${esc(kind)}</span>
            ${phase}
          </div>
          ${time}
        </div>
        <div class="codex-bubble">
          ${messageBody}
        </div>
      </div>
    `;
  }
  if (kind === "tool_call") {
    const tool = item?.tool || "tool";
    const summary = item?.summary || tool;
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
  if (kind === "tool_output") {
    const tool = item?.tool || "tool";
    const summary = item?.summary || tool;
    const outputKey = toolOutputKey(item);
    const isOpen = openDetails.has(outputKey);
    return `
      <div class="codex-item codex-inline-system ${isOpen ? "open" : ""}">
        <div class="codex-inline-row">
          <div class="codex-inline-main">
            <span class="chip memory">${esc(tool)}</span>
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

export function renderCodexTranscript(view, options = {}) {
  const items = view?.items || [];
  if (!items.length) {
    return `
      <div id="primary-terminal-log" class="primary-log-block codex-transcript" style="margin-top:12px;">
        <div class="empty">No Codex transcript items are visible for this session yet.</div>
      </div>
    `;
  }
  return `
    <div id="primary-terminal-log" class="primary-log-block codex-transcript" style="margin-top:12px;">
      ${items.map((item) => renderTranscriptItem(item, options)).join("")}
    </div>
  `;
}
