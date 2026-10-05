// Adapted from DeepSeek Harness ui-primitives/src/keyboard-composition.ts.
// Copyright (c) 2026 DeepSeek. MIT notice: web/public/DEEPSEEK-LICENSE.
// The bounded closing-key window follows ui-conversation/src/client/input/editor/keymap.ts.
import { useCallback, useLayoutEffect, useRef } from "react";

export interface CompositionObserver {
  guards(event: KeyboardEvent): boolean;
  dispose(): void;
}

/** Protect the key committing an IME candidate, including engines with a late closing keydown. */
export function observeComposition(doc: Document): CompositionObserver {
  let composing = false;
  let composingUntil = 0;
  let previous: KeyboardEvent | null = null;
  let guarded = false;
  const start = () => { composing = true; composingUntil = 0; };
  const end = () => { composing = false; composingUntil = performance.now() + 10; };
  const release = () => { composingUntil = 0; };
  const blur = () => { composing = false; composingUntil = 0; };
  doc.addEventListener("compositionstart", start, true);
  doc.addEventListener("compositionend", end, true);
  doc.addEventListener("keyup", release, true);
  doc.defaultView?.addEventListener("blur", blur);
  return {
    guards(event: KeyboardEvent) {
      // Capture and bubble handlers must agree about the same native event.
      if (event === previous) return guarded;
      previous = event;
      guarded = composing || performance.now() < composingUntil || event.isComposing || event.keyCode === 229;
      composingUntil = 0;
      return guarded;
    },
    dispose() {
      doc.removeEventListener("compositionstart", start, true);
      doc.removeEventListener("compositionend", end, true);
      doc.removeEventListener("keyup", release, true);
      doc.defaultView?.removeEventListener("blur", blur);
    },
  };
}

export function useCompositionGuard() {
  const observer = useRef<CompositionObserver | null>(null);
  useLayoutEffect(() => {
    const current = observeComposition(document);
    observer.current = current;
    return () => { current.dispose(); observer.current = null; };
  }, []);
  return useCallback((event: KeyboardEvent) => observer.current?.guards(event) ?? (event.isComposing || event.keyCode === 229), []);
}
