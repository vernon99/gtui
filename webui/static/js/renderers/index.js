import { renderClaudeTranscript } from "./claude.js";
import { renderCodexTranscript } from "./codex.js";

export function getTranscriptView(agent) {
  if (agent?.transcript_view?.available) return agent.transcript_view;
  if (agent?.codex_view?.available) return agent.codex_view;
  if (agent?.claude_view?.available) return agent.claude_view;
  return agent?.transcript_view || agent?.codex_view || agent?.claude_view || {};
}

export function transcriptProvider(view) {
  const provider = String(view?.provider || "").toLowerCase();
  if (provider) return provider;
  const source = String(view?.source || "").toLowerCase();
  if (source.includes("claude")) return "claude";
  if (source.includes("codex")) return "codex";
  return "transcript";
}

export function hasTranscriptItems(view) {
  return Boolean(view?.available && (view.items || []).length);
}

export function transcriptLabel(view) {
  const provider = transcriptProvider(view);
  if (provider === "claude") return "Claude transcript";
  if (provider === "codex") return "Codex transcript";
  return "transcript";
}

export function transcriptTitleNoun(view) {
  const provider = transcriptProvider(view);
  if (provider === "claude") return "Claude";
  if (provider === "codex") return "Codex";
  return "Transcript";
}

export function transcriptBadgeText(view) {
  const provider = transcriptProvider(view);
  const count = (view?.items || []).length;
  if (provider === "claude") return `claude ${count} items`;
  if (provider === "codex") return `codex ${count} items`;
  return `${count} transcript items`;
}

export function renderPrimaryTranscript(view, options = {}) {
  const provider = transcriptProvider(view);
  if (provider === "claude") return renderClaudeTranscript(view, options);
  return renderCodexTranscript(view, options);
}
