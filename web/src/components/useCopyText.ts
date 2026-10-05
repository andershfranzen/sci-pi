import { useEffect, useRef, useState } from "react";
// Independent clipboard lifecycle inspired by DeepSeek Harness
// packages/client/ui-chat/src/client/chat/MessageIconActions.tsx; see /web/public/DEEPSEEK-LICENSE.

export function useCopyText() {
  const [status, setStatus] = useState("");
  const timer = useRef<number | undefined>(undefined);
  const generation = useRef(0);
  useEffect(() => () => {
    generation.current++;
    window.clearTimeout(timer.current);
  }, []);
  const copy = async (text: string) => {
    const request = ++generation.current;
    window.clearTimeout(timer.current);
    setStatus("");
    let result: string;
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard unavailable");
      await navigator.clipboard.writeText(text);
      result = "Copied";
    } catch {
      result = "Copy failed. Select the text and copy manually.";
    }
    if (request !== generation.current) return;
    setStatus(result);
    timer.current = window.setTimeout(() => setStatus(""), 4000);
  };
  return { copy, status };
}
