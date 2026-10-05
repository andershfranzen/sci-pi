import { useRef } from "react";
import { useCopyText } from "./useCopyText";

export function MessageActions({ text, onQuote }: { text: string; onQuote: (text: string) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const { copy, status } = useCopyText();
  const quote = () => {
    const selection = window.getSelection();
    const message = ref.current?.parentElement?.querySelector("[data-message-body]");
    let value = text;
    if (selection && !selection.isCollapsed && selection.rangeCount && message) {
      const range = selection.getRangeAt(0);
      if (message.contains(range.startContainer) && message.contains(range.endContainer)) {
        const contents = range.cloneContents();
        contents.querySelectorAll(".transcript-code-toolbar, .transcript-status").forEach(node => node.remove());
        contents.querySelectorAll("br").forEach(node => node.replaceWith("\n"));
        contents.querySelectorAll("p, pre, li, tr, blockquote").forEach(node => node.after("\n"));
        value = contents.textContent || text;
      }
    }
    onQuote(value);
  };
  return <div className="transcript-actions" ref={ref}>
    <button type="button" onClick={() => void copy(text)}>Copy message</button>
    <button type="button" onMouseDown={(event) => event.preventDefault()} onClick={quote}>Quote</button>
    <span className="transcript-status" role="status">{status}</span>
  </div>;
}
