import { createElement, memo, useMemo, useState, type ReactNode } from "react";
import { Marked } from "marked";
import { useCopyText } from "./useCopyText";
import "./Transcript.css";

// Agent output is untrusted: escape HTML, allow only safe links, and never fetch images.
const SAFE_URL = /^(https?:|mailto:|#|\/(?!\/)|\.{0,2}\/)/i;
function esc(s: string) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}
interface Code { text: string; language: string }
// Independent toolbar inspired by DeepSeek Harness packages/client/ui-primitives/src/CodeToolbar.tsx;
// upstream license notice: /web/public/DEEPSEEK-LICENSE. No highlighting dependencies are reused.
function CodeBlock({ code }: { code: Code }) {
  const [wrap, setWrap] = useState(false);
  const { copy, status } = useCopyText();
  return <div className="transcript-code">
    <div className="transcript-code-toolbar">
      <span className="transcript-code-language">{code.language || "Code"}</span>
      <button type="button" onClick={() => void copy(code.text)}>Copy code</button>
      <button type="button" aria-pressed={wrap} onClick={() => setWrap(!wrap)}>Wrap</button>
    </div>
    <span className="transcript-status" role="status">{status}</span>
    <pre className={wrap ? "transcript-code-wrap" : undefined}><code>{code.text}</code></pre>
  </div>;
}

function renderMarkdown(text: string) {
  const codes: Code[] = [];
  const md = new Marked({ gfm: true, breaks: false });
  md.use({ renderer: {
    html({ text }) { return esc(text); },
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
    code({ text, lang }) {
      const index = codes.push({ text, language: (lang || "").trim().split(/\s+/)[0] }) - 1;
      return `<pre data-code-index="${index}"></pre>`;
    },
  } });
  // Parse the complete document together so reference links keep their document scope.
  const document = new DOMParser().parseFromString(md.parse(text, { async: false }) as string, "text/html");
  function nodeToReact(node: Node, key: string): ReactNode {
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (!(node instanceof HTMLElement)) return null;
    const index = node.getAttribute("data-code-index");
    if (index !== null) return <CodeBlock key={key} code={codes[Number(index)]} />;
    const props: Record<string, unknown> = { key };
    for (const attribute of node.attributes) {
      const name = attribute.name === "class" ? "className" : attribute.name === "tabindex" ? "tabIndex" : attribute.name;
      if (name === "checked" || name === "disabled") props[name] = true;
      else props[name] = attribute.value;
    }
    if (node.tagName === "INPUT") props.readOnly = true;
    const children = Array.from(node.childNodes, (child, i) => nodeToReact(child, `${key}.${i}`));
    return createElement(node.tagName.toLowerCase(), props, ...(children.length ? children : []));
  }
  return Array.from(document.body.childNodes, (node, i) => nodeToReact(node, String(i)));
}

export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  const content = useMemo(() => renderMarkdown(text), [text]);
  return <div className={`md ${className ?? ""}`}>{content}</div>;
});
