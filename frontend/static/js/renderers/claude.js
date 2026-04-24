import { esc } from "./html.js";
import { renderMarkdown } from "./markdown.js";

function toolOutputKey(item) {
  return `claude-tool-output:${item?.call_id || item?.timestamp || item?.summary || ""}`;
}

function clipInline(value, max = 72) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 3)).trimEnd()}...`;
}

function isBashItem(item) {
  const kind = String(item?.kind || "");
  return (kind === "tool_call" || kind === "tool_output") && item?.tool === "Bash";
}

function stripBashWrapper(summary) {
  const text = String(summary || "").trim();
  const match = text.match(/^Bash\((.*)\)$/s);
  return match ? match[1].trim() : text;
}

function parseLegacyBashSummary(item) {
  const text = stripBashWrapper(item?.summary || "");
  const parsed = {};
  const commandToken = "command=";
  const descriptionToken = ", description=";
  const descriptionIndex = text.indexOf(descriptionToken);
  if (descriptionIndex >= 0) {
    const commandPart = text.slice(0, descriptionIndex).trim();
    const description = text.slice(descriptionIndex + descriptionToken.length).trim();
    if (commandPart.startsWith(commandToken)) {
      parsed.command = commandPart.slice(commandToken.length).trim();
    }
    if (description) parsed.description = description;
    return parsed;
  }
  if (text.startsWith(commandToken)) {
    parsed.command = text.slice(commandToken.length).trim();
    return parsed;
  }
  if (text) parsed.command = text;
  return parsed;
}

function bashCommand(item) {
  return String(item?.command || parseLegacyBashSummary(item).command || "").trim();
}

function bashDescription(item) {
  return String(item?.description || parseLegacyBashSummary(item).description || "").trim();
}

function bashCallText(item) {
  const command = bashCommand(item);
  const description = bashDescription(item);
  if (command && description) return `${description}: ${command}`;
  return command || description || stripBashWrapper(item?.summary || "") || "Bash call";
}

function bashCallLabel(item) {
  const description = bashDescription(item);
  if (description) return clipInline(description, 42);
  const command = bashCommand(item);
  if (command) return clipInline(command, 42);
  return clipInline(stripBashWrapper(item?.summary || ""), 42) || "command";
}

function groupBashSequence(sequence) {
  const calls = [];
  for (const item of sequence) {
    if (item?.kind === "tool_call") {
      calls.push({ call: item, outputs: [] });
      continue;
    }
    if (item?.kind !== "tool_output") continue;
    const callId = item?.call_id || "";
    let index = -1;
    if (callId) {
      for (let candidate = calls.length - 1; candidate >= 0; candidate -= 1) {
        if (calls[candidate].call?.call_id === callId) {
          index = candidate;
          break;
        }
      }
    }
    if (index < 0) index = calls.length - 1;
    if (index >= 0) {
      calls[index].outputs.push(item);
    } else {
      calls.push({ call: null, outputs: [item] });
    }
  }
  return calls;
}

function groupClaudeItems(items) {
  const grouped = [];
  for (let index = 0; index < items.length; index += 1) {
    const item = items[index];
    if (!isBashItem(item)) {
      grouped.push(item);
      continue;
    }

    const sequence = [];
    while (index < items.length && isBashItem(items[index])) {
      sequence.push(items[index]);
      index += 1;
    }
    index -= 1;

    const calls = groupBashSequence(sequence);
    if (calls.length > 1) {
      grouped.push({ kind: "bash_group", calls });
    } else {
      grouped.push(...sequence);
    }
  }
  return grouped;
}

function bashGroupKey(calls) {
  const first = calls[0]?.call || calls[0]?.outputs?.[0] || {};
  const last = calls[calls.length - 1]?.call || calls[calls.length - 1]?.outputs?.[0] || {};
  return `claude-bash-group:${first.call_id || first.timestamp || ""}:${calls.length}:${last.call_id || last.timestamp || ""}`;
}

function bashOutputKey(groupKey, call, index) {
  const source = call.call || call.outputs?.[0] || {};
  return `claude-bash-output:${groupKey}:${index}:${source.call_id || source.timestamp || ""}`;
}

function bashGroupSummary(calls) {
  const labels = [];
  for (const call of calls) {
    const label = bashCallLabel(call.call || call.outputs?.[0]);
    if (label && !labels.includes(label)) labels.push(label);
    if (labels.length >= 4) break;
  }
  const suffix = calls.length > labels.length ? ", etc." : "";
  return `${calls.length} calls executed: ${labels.join(", ")}${suffix}`;
}

function bashGroupTime(calls) {
  const first = calls[0]?.call || calls[0]?.outputs?.[0] || {};
  const finalCall = calls[calls.length - 1] || {};
  const finalOutputs = finalCall.outputs || [];
  const last = finalOutputs[finalOutputs.length - 1] || finalCall.call || {};
  const start = first.time || "";
  const end = last.time || "";
  if (!start) return end ? `<div class="codex-time">${esc(end)}</div>` : "";
  if (!end || start === end) return `<div class="codex-time">${esc(start)}</div>`;
  return `<div class="codex-time">${esc(`${start}-${end}`)}</div>`;
}

function renderBashGroupCall(call, groupKey, index, options) {
  const openDetails = options?.openDetails || new Set();
  const source = call.call || call.outputs?.[0] || {};
  const outputText = call.outputs
    .map((output) => output?.text || "")
    .filter((text) => text.trim())
    .join("\n\n");
  const outputKey = bashOutputKey(groupKey, call, index);
  const isOpen = openDetails.has(outputKey);
  const hasOutput = outputText.trim().length > 0;
  const hasError = call.outputs.some((output) => output?.is_error);
  const errorOutput = call.outputs.find((output) => output?.is_error);
  const errorTitle = errorOutput?.summary
    ? `Command returned an error: ${errorOutput.summary}`
    : "Command returned an error";
  const time = source?.time ? `<div class="codex-time">${esc(source.time)}</div>` : "";
  return `
    <div class="claude-bash-call${isOpen ? " open" : ""}${hasError ? " error" : ""}">
      <div class="claude-bash-call-row">
        <div class="claude-bash-command-wrap">
          ${hasError ? `<span class="claude-bash-call-error" role="img" aria-label="${esc(errorTitle)}" title="${esc(errorTitle)}">!</span>` : ""}
          <div class="claude-bash-command">${esc(bashCallText(source))}</div>
        </div>
        <div class="codex-inline-right">
          ${time}
          ${hasOutput ? `
            <button
              type="button"
              class="codex-inline-toggle"
              data-inline-toggle="${esc(outputKey)}"
              aria-expanded="${isOpen ? "true" : "false"}"
              aria-label="${isOpen ? "Collapse output" : "Expand output"}"
            ><span class="codex-inline-chevron" aria-hidden="true">▸</span></button>
          ` : ""}
        </div>
      </div>
      ${hasOutput && isOpen ? `<pre class="codex-output">${esc(outputText)}</pre>` : ""}
    </div>
  `;
}

function renderBashGroup(entry, options) {
  const openDetails = options?.openDetails || new Set();
  const calls = entry?.calls || [];
  const groupKey = bashGroupKey(calls);
  const isOpen = openDetails.has(groupKey);
  const hasError = calls.some((call) => call.outputs.some((output) => output?.is_error));
  return `
    <div class="codex-item codex-inline-system claude-bash-group ${isOpen ? "open" : ""}">
      <div class="codex-inline-row">
        <div class="codex-inline-main">
          <span class="chip ${hasError ? "stuck" : "memory"}">Bash</span>
          <span class="codex-inline-text">${esc(bashGroupSummary(calls))}</span>
        </div>
        <div class="codex-inline-right">
          ${bashGroupTime(calls)}
          <button
            type="button"
            class="codex-inline-toggle"
            data-inline-toggle="${esc(groupKey)}"
            aria-expanded="${isOpen ? "true" : "false"}"
            aria-label="${isOpen ? "Collapse Bash calls" : "Expand Bash calls"}"
          ><span class="codex-inline-chevron" aria-hidden="true">▸</span></button>
        </div>
      </div>
      ${isOpen ? `
        <div class="claude-bash-calls">
          ${calls.map((call, index) => renderBashGroupCall(call, groupKey, index, options)).join("")}
        </div>
      ` : ""}
    </div>
  `;
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
  const summary = tool === "Bash" ? bashCallText(item) : (item?.summary || tool);
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
  if (kind === "bash_group") return renderBashGroup(item, options);
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
      ${groupClaudeItems(items).map((item) => renderTranscriptItem(item, options)).join("")}
    </div>
  `;
}
