import { memo, useMemo } from "react";
import { Marked } from "marked";

// Agent output is untrusted (prompt injection can reach it), and the page holds daemon
// tokens, so raw HTML is escaped, links are limited to safe schemes and images are not
// loaded (they would be a silent exfiltration channel); they render as links instead.
const SAFE_URL = /^(https?:|mailto:|#|\/(?!\/)|\.{0,2}\/)/i;

function esc(s: string) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

const md = new Marked({ gfm: true, breaks: false });
md.use({
  renderer: {
    html({ text }) {
      return esc(text);
    },
    link({ href, title, tokens }) {
      const inner = this.parser.parseInline(tokens);
      if (!href || !SAFE_URL.test(href)) return inner;
      const t = title ? ` title="${esc(title)}"` : "";
      return `<a href="${esc(href)}"${t} target="_blank" rel="noopener noreferrer">${inner}</a>`;
    },
    image({ href, text }) {
      const label = esc(text || href || "image");
      if (!href || !/^https?:/i.test(href)) return `[${label}]`;
      return `<a href="${esc(href)}" target="_blank" rel="noopener noreferrer">[image: ${label}]</a>`;
    },
  },
});

export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  const html = useMemo(() => md.parse(text, { async: false }) as string, [text]);
  return <div className={`md ${className ?? ""}`} dangerouslySetInnerHTML={{ __html: html }} />;
});
