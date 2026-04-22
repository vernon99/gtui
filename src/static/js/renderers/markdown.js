import { esc, escapeAttr } from "./html.js";

function sanitizeMarkdownHref(href) {
  const value = String(href || "").trim();
  if (!value) return "";
  if (/^(https?:\/\/|mailto:|#)/i.test(value)) return value;
  if (value.startsWith("/")) return value;
  if (value.startsWith("./") || value.startsWith("../")) return value;
  return "";
}

function renderInlineMarkdown(text) {
  let html = esc(String(text ?? ""));
  const placeholders = [];
  html = html.replace(/`([^`]+)`/g, (_, code) => {
    const token = `@@GTUI_MD_${placeholders.length}@@`;
    placeholders.push(`<code>${code}</code>`);
    return token;
  });
  html = html.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, href) => {
    const safeHref = sanitizeMarkdownHref(href);
    if (!safeHref) return `${label} (${href})`;
    return `<a href="${escapeAttr(safeHref)}" target="_blank" rel="noreferrer">${label}</a>`;
  });
  html = html.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/__([^_]+)__/g, "<strong>$1</strong>");
  html = html.replace(/~~([^~]+)~~/g, "<s>$1</s>");
  html = html.replace(/(^|[\s(])\*([^*\n][^*\n]*?)\*(?=$|[\s).,!?:;])/g, "$1<em>$2</em>");
  html = html.replace(/(^|[\s(])_([^_\n][^_\n]*?)_(?=$|[\s).,!?:;])/g, "$1<em>$2</em>");
  placeholders.forEach((replacement, index) => {
    html = html.replace(`@@GTUI_MD_${index}@@`, replacement);
  });
  return html.replace(/\n/g, "<br>");
}

function isMarkdownBlockBoundary(line) {
  return /^\s*$/.test(line)
    || /^```/.test(line)
    || /^\s{0,3}#{1,6}\s+/.test(line)
    || /^\s{0,3}>\s?/.test(line)
    || /^\s{0,3}[-*+]\s+/.test(line)
    || /^\s{0,3}\d+\.\s+/.test(line);
}

export function renderMarkdown(text) {
  const lines = String(text ?? "").replace(/\r\n?/g, "\n").split("\n");
  const blocks = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      index += 1;
      continue;
    }

    const fenceMatch = line.match(/^```(\w+)?\s*$/);
    if (fenceMatch) {
      const language = fenceMatch[1] || "";
      const codeLines = [];
      index += 1;
      while (index < lines.length && !/^```/.test(lines[index])) {
        codeLines.push(lines[index]);
        index += 1;
      }
      if (index < lines.length) index += 1;
      blocks.push(`
        <pre><code${language ? ` class="language-${esc(language)}"` : ""}>${esc(codeLines.join("\n"))}</code></pre>
      `);
      continue;
    }

    const headingMatch = line.match(/^\s{0,3}(#{1,6})\s+(.+)$/);
    if (headingMatch) {
      const level = headingMatch[1].length;
      blocks.push(`<h${level}>${renderInlineMarkdown(headingMatch[2].trim())}</h${level}>`);
      index += 1;
      continue;
    }

    if (/^\s{0,3}>\s?/.test(line)) {
      const quoteLines = [];
      while (index < lines.length && /^\s{0,3}>\s?/.test(lines[index])) {
        quoteLines.push(lines[index].replace(/^\s{0,3}>\s?/, ""));
        index += 1;
      }
      blocks.push(`<blockquote>${renderMarkdown(quoteLines.join("\n"))}</blockquote>`);
      continue;
    }

    const listMatch = line.match(/^\s{0,3}((?:[-*+])|(?:\d+\.))\s+(.+)$/);
    if (listMatch) {
      const ordered = /\d+\./.test(listMatch[1]);
      const items = [];
      while (index < lines.length) {
        const current = lines[index];
        const bulletMatch = current.match(/^\s{0,3}((?:[-*+])|(?:\d+\.))\s+(.+)$/);
        if (!bulletMatch) break;
        const itemLines = [bulletMatch[2]];
        index += 1;
        while (index < lines.length) {
          const continuation = lines[index];
          if (!continuation.trim()) {
            index += 1;
            break;
          }
          if (/^\s{0,3}((?:[-*+])|(?:\d+\.))\s+/.test(continuation) || isMarkdownBlockBoundary(continuation)) {
            break;
          }
          itemLines.push(continuation.trim());
          index += 1;
        }
        items.push(`<li>${renderInlineMarkdown(itemLines.join("\n"))}</li>`);
      }
      blocks.push(`<${ordered ? "ol" : "ul"}>${items.join("")}</${ordered ? "ol" : "ul"}>`);
      continue;
    }

    const paragraphLines = [line];
    index += 1;
    while (index < lines.length && !isMarkdownBlockBoundary(lines[index])) {
      paragraphLines.push(lines[index]);
      index += 1;
    }
    blocks.push(`<p>${renderInlineMarkdown(paragraphLines.join("\n"))}</p>`);
  }

  if (!blocks.length) {
    return `<div class="codex-text"></div>`;
  }
  return `<div class="codex-text codex-markdown">${blocks.join("")}</div>`;
}
