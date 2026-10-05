import { useCallback, useId, useLayoutEffect, useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { cx } from "../util";
import { createPortal } from "react-dom";
import { useCompositionGuard } from "../keyboard";
import { focusPastAnchor } from "../overlays";

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  danger?: boolean;
  disabled?: boolean;
  onSelect: () => void;
}

/** A small popover menu anchored to its trigger button. */
export function Menu({
  trigger,
  items,
  label,
  align = "right",
  className,
}: {
  trigger: ReactNode;
  items: MenuItem[];
  label: string;
  align?: "left" | "right";
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popRef = useRef<HTMLDivElement>(null);
  const id = useId();
  const composing = useCompositionGuard();
  const [position, setPosition] = useState<CSSProperties>({ position: "fixed", left: 0, top: 0, right: "auto", zIndex: 90 });
  const close = useCallback((refocus = true) => {
    setOpen(false);
    if (refocus) triggerRef.current?.focus({ preventScroll: true });
  }, []);
  // Portal/anchor behavior follows DeepSeek Harness ui-primitives/src/Menu.tsx.
  // MIT notice: web/public/DEEPSEEK-LICENSE.
  const place = useCallback(() => {
    const anchor = triggerRef.current;
    const popup = popRef.current;
    if (!anchor || !popup) return;
    const rect = anchor.getBoundingClientRect();
    const width = Math.min(232, window.innerWidth - 16);
    const height = Math.min(popup.scrollHeight, window.innerHeight - 16);
    const left = Math.max(8, Math.min(align === "left" ? rect.left : rect.right - width, window.innerWidth - width - 8));
    const below = window.innerHeight - rect.bottom - 8;
    const top = below >= height ? rect.bottom + 4 : Math.max(8, rect.top - height - 4);
    setPosition({ position: "fixed", visibility: "visible", left, top, right: "auto", width, minWidth: 0, maxHeight: window.innerHeight - 16, overflowY: "auto", zIndex: 90 });
  }, [align]);
  useLayoutEffect(() => {
    if (!open) return;
    place();
    (popRef.current?.querySelector<HTMLElement>("button:not(:disabled)") ?? popRef.current)?.focus({ preventScroll: true });
  }, [open, place]);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent | TouchEvent) => {
      const target = e.target as Node;
      if (!triggerRef.current?.contains(target) && !popRef.current?.contains(target)) close(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (composing(e) || e.ctrlKey || e.metaKey || e.altKey) return;
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
        return;
      }
      if (!popRef.current?.contains(document.activeElement)) return;
      const buttons = [...popRef.current.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
      const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
      if (["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) {
        e.preventDefault();
        e.stopPropagation();
        const next = e.key === "Home" ? 0 : e.key === "End" ? buttons.length - 1 : (index + (e.key === "ArrowUp" ? -1 : 1) + buttons.length) % buttons.length;
        buttons[next]?.focus();
      } else if (e.key === "Tab") {
        e.preventDefault();
        e.stopPropagation();
        close(false);
        focusPastAnchor(triggerRef.current, e.shiftKey, popRef.current);
      }
    };
    const onScroll = (e: Event) => { if (!popRef.current?.contains(e.target as Node)) place(); };
    document.addEventListener("mousedown", onDown, true);
    document.addEventListener("touchstart", onDown, true);
    document.addEventListener("keydown", onKey, true);
    window.addEventListener("resize", place);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("mousedown", onDown, true);
      document.removeEventListener("touchstart", onDown, true);
      document.removeEventListener("keydown", onKey, true);
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [open, composing, close, place]);
  return (
    <div className={cx("menu-wrap", className)}>
      <button
        ref={triggerRef}
        type="button"
        className="icon-btn"
        aria-label={label}
        title={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        onKeyDown={(e) => {
          if (composing(e.nativeEvent)) return;
          if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); setOpen(true); }
        }}
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          setOpen(!open);
        }}
      >
        {trigger}
      </button>
      {open && createPortal(
        <div ref={popRef} id={id} style={position} tabIndex={-1} className="menu" role="menu" aria-label={label}>
          {items.map((it) => (
            <button
              key={it.label}
              role="menuitem"
              className={cx("menu-item", it.danger && "danger")}
              disabled={it.disabled}
              onClick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                close();
                it.onSelect();
              }}
            >
              {it.icon}
              <span>{it.label}</span>
            </button>
          ))}
        </div>
      , document.body)}
    </div>
  );
}
