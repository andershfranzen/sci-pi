// Adapted from DeepSeek Harness ui-primitives/src/useModalLayer.ts.
// Copyright (c) 2026 DeepSeek. MIT notice: web/public/DEEPSEEK-LICENSE.
import { useLayoutEffect, useRef, type RefObject } from "react";
import { observeComposition } from "./keyboard";

interface ModalLayer { element: HTMLElement; }
const layers = new WeakMap<Document, ModalLayer[]>();
const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])';

function visible(element: HTMLElement) {
  return !element.closest("[inert], [hidden]") && element.getClientRects().length > 0 && getComputedStyle(element).visibility !== "hidden";
}

export function focusableWithin(scope: ParentNode) {
  return [...scope.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(node => visible(node) && node.tabIndex >= 0 && !node.matches(":disabled"));
}

/** Settle a portaled picker/menu's Tab gesture relative to its invoking control. */
export function focusPastAnchor(anchor: HTMLElement | null, backward: boolean, popup: HTMLElement | null) {
  if (!anchor) return;
  const scope = anchor.closest('[role="dialog"][aria-modal="true"]') ?? anchor.ownerDocument.body;
  const targets = focusableWithin(scope).filter(node => !popup?.contains(node));
  const index = targets.indexOf(anchor);
  const next = targets[(index + (backward ? -1 : 1) + targets.length) % targets.length] ?? anchor;
  next.focus();
}

/** Open overlays own keyboard gestures before background session navigation. */
export function foregroundOverlay(doc: Document) {
  return [...doc.querySelectorAll<HTMLElement>('[role="dialog"], [role="menu"], [role="listbox"]')].filter(visible).at(-1);
}

/** Trap only the foreground dialog; preserve portaled picker ownership and invoking focus. */
export function useModalLayer(ref: RefObject<HTMLElement | null>, onClose: () => void, active = true) {
  const close = useRef(onClose);
  close.current = onClose;
  useLayoutEffect(() => {
    const element = ref.current;
    if (!active || !element) return;
    const doc = element.ownerDocument;
    const composition = observeComposition(doc);
    const previous = doc.activeElement;
    const stack = layers.get(doc) ?? [];
    layers.set(doc, stack);
    const layer = { element };
    // Child layout effects run first when nested dialogs mount together.
    const child = stack.findIndex((entry) => element.contains(entry.element));
    stack.splice(child < 0 ? stack.length : child, 0, layer);
    const targets = () => focusableWithin(element);
    const initial = [...element.querySelectorAll<HTMLElement>("[data-modal-autofocus]")].find(visible) ?? targets()[0] ?? element;
    if (stack.at(-1) === layer && !element.contains(doc.activeElement)) initial.focus({ preventScroll: true });

    const keydown = (event: KeyboardEvent) => {
      if (stack.at(-1) !== layer || event.defaultPrevented || composition.guards(event) || event.ctrlKey || event.altKey || event.metaKey) return;
      const foreground = foregroundOverlay(doc);
      const focus = doc.activeElement;
      if (event.key === "Escape" && !event.shiftKey) {
        // A portaled select/menu closes itself; never dismiss its parent as well.
        if (foreground && foreground !== element && !element.contains(foreground)) return;
        event.preventDefault();
        if (!event.repeat) close.current();
        return;
      }
      if (event.key !== "Tab") return;
      const items = targets();
      const first = items[0] ?? element;
      const last = items.at(-1) ?? element;
      const atEdge = event.shiftKey ? focus === first : focus === last;
      if (focus === element || !(focus instanceof HTMLElement) || !items.includes(focus) || atEdge) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      }
    };
    doc.addEventListener("keydown", keydown);
    return () => {
      composition.dispose();
      doc.removeEventListener("keydown", keydown);
      const wasTop = stack.at(-1) === layer;
      stack.splice(stack.indexOf(layer), 1);
      if (!stack.length) layers.delete(doc);
      if (wasTop) {
        const target = previous instanceof HTMLElement && previous.isConnected ? previous : stack.at(-1)?.element;
        target?.focus({ preventScroll: true });
      }
    };
  }, [ref, active]);
}
