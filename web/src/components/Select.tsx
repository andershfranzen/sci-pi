// One custom select / menu for the whole app: chip or field trigger, popover on desktop,
// bottom sheet on phones, search for long lists, grouped options, ARIA listbox semantics.
import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { cx, fuzzyScore } from "../util";
import { useCompositionGuard } from "../keyboard";
import { focusPastAnchor, useModalLayer } from "../overlays";
import { IconCheck, IconChevronDown, IconSearch, IconX } from "./Icons";

export interface SelectOption {
  value: unknown;
  label: string;
  description?: string;
  group?: string;
  icon?: ReactNode;
  /** small right-aligned annotation, e.g. "default" */
  meta?: string;
}

const key = (v: unknown) => JSON.stringify(v ?? null);
const SHEET_QUERY = "(max-width: 639px)";

export function Select({
  value,
  options,
  onChange,
  label,
  variant = "chip",
  icon,
  display,
  disabled,
  busy,
  className,
  tone,
  searchPlaceholder,
  hideLabelNarrow = true,
  title,
}: {
  value: unknown;
  options: SelectOption[];
  onChange: (value: unknown) => void;
  /** accessible name; shown as the chip prefix and the sheet title */
  label: string;
  variant?: "chip" | "field";
  icon?: ReactNode;
  /** override for the trigger's value text */
  display?: string;
  disabled?: boolean;
  busy?: boolean;
  className?: string;
  /** colour accent for the chip value */
  tone?: "accent" | "warn" | "info";
  searchPlaceholder?: string;
  hideLabelNarrow?: boolean;
  title?: string;
}) {
  const [open, setOpen] = useState(false);
  const [sheet, setSheet] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const composing = useCompositionGuard();
  const id = useId();
  const current = options.find((o) => key(o.value) === key(value));
  const shown = display ?? current?.label ?? (value == null || value === "" ? "—" : String(value));

  const openMenu = () => {
    if (disabled) return;
    setSheet(typeof matchMedia !== "undefined" && matchMedia(SHEET_QUERY).matches);
    setOpen(true);
  };
  const close = useCallback((refocus = true) => {
    setOpen(false);
    if (refocus) requestAnimationFrame(() => triggerRef.current?.focus({ preventScroll: true }));
  }, []);

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className={cx(variant === "chip" ? "sel-chip" : "sel-field", open && "open", busy && "busy", tone && `tone-${tone}`, className)}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? `${id}-list` : undefined}
        aria-label={`${label}: ${shown}`}
        title={title ?? (current?.description ? `${label}: ${shown} — ${current.description}` : `${label}: ${shown}`)}
        disabled={disabled}
        onClick={() => (open ? close() : openMenu())}
        onKeyDown={(e) => {
          if (composing(e.nativeEvent)) return;
          if (["ArrowDown", "ArrowUp", "Enter", " "].includes(e.key)) {
            e.preventDefault();
            openMenu();
          }
        }}
      >
        {icon && <span className="sel-icon">{icon}</span>}
        {variant === "chip" && <span className={cx("sel-label", hideLabelNarrow && !!icon && "hide-sheet")}>{label}</span>}
        <span className="sel-value">{shown}</span>
        {busy ? <span className="spinner sel-spin" /> : <IconChevronDown size={12} className="sel-caret" />}
      </button>
      {open && (
        <SelectMenu
          id={id}
          anchor={triggerRef.current}
          sheet={sheet}
          label={label}
          options={options}
          value={value}
          searchPlaceholder={searchPlaceholder}
          onPick={(v) => {
            close();
            if (key(v) !== key(value)) onChange(v);
          }}
          onClose={close}
        />
      )}
    </>
  );
}

interface Row {
  kind: "group" | "opt";
  group?: string;
  opt?: SelectOption;
  index?: number; // index among selectable options
}

function SelectMenu({
  id,
  anchor,
  sheet,
  label,
  options,
  value,
  searchPlaceholder,
  onPick,
  onClose,
}: {
  id: string;
  anchor: HTMLElement | null;
  sheet: boolean;
  label: string;
  options: SelectOption[];
  value: unknown;
  searchPlaceholder?: string;
  onPick: (v: unknown) => void;
  onClose: (refocus?: boolean) => void;
}) {
  const searchable = options.length > 8;
  const [q, setQ] = useState("");
  const popRef = useRef<HTMLDivElement>(null);
  const composing = useCompositionGuard();
  useModalLayer(popRef, () => onClose(), sheet);
  const listRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const typeahead = useRef({ buf: "", t: 0 });
  const [style, setStyle] = useState<CSSProperties>({ visibility: "hidden" });

  // Every whitespace-separated term must match somewhere (label, id, group, description);
  // label matches rank highest.
  const filtered = useMemo(() => {
    const terms = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
    if (!terms.length) return options;
    return options
      .map((o, i) => {
        let s = 0;
        for (const t of terms) {
          const best = Math.max(
            fuzzyScore(t, o.label) * 1.5,
            fuzzyScore(t, String(o.value)),
            o.group ? fuzzyScore(t, o.group) * 0.6 : -1,
            o.description && o.description.toLowerCase().includes(t) ? 1 : -1,
          );
          if (best < 0) return null;
          s += best;
        }
        return { o, i, s };
      })
      .filter((x): x is { o: SelectOption; i: number; s: number } => x !== null)
      .sort((a, b) => b.s - a.s || a.i - b.i)
      .map((x) => x.o);
  }, [q, options]);

  // Keep group order when not searching; while searching show a flat ranked list.
  const rows: Row[] = useMemo(() => {
    const out: Row[] = [];
    let idx = 0;
    let last: string | undefined = "\u0000";
    for (const o of filtered) {
      if (!q.trim() && o.group !== undefined && o.group !== last) out.push({ kind: "group", group: o.group });
      last = o.group;
      out.push({ kind: "opt", opt: o, index: idx++ });
    }
    return out;
  }, [filtered, q]);

  const selIdx = filtered.findIndex((o) => key(o.value) === key(value));
  const [active, setActive] = useState(Math.max(0, selIdx));
  useEffect(() => setActive(q.trim() ? 0 : Math.max(0, selIdx)), [q]); // eslint-disable-line react-hooks/exhaustive-deps

  // Position the desktop popover next to its trigger, flipping above when there's no room.
  const place = useCallback(() => {
    if (sheet || !anchor) return;
    const r = anchor.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const width = Math.min(vw - 16, Math.max(r.width, options.some((o) => o.description) ? 340 : 240));
    const below = vh - r.bottom - 12;
    const above = r.top - 12;
    const up = below < 280 && above > below;
    const left = Math.min(Math.max(8, r.left), vw - width - 8);
    setStyle({
      position: "fixed",
      left,
      width,
      maxHeight: Math.min(440, (up ? above : below) - 4),
      ...(up ? { bottom: vh - r.top + 6 } : { top: r.bottom + 6 }),
      transformOrigin: up ? "bottom left" : "top left",
    });
  }, [anchor, sheet, options]);

  useLayoutEffect(() => {
    place();
  }, [place]);

  useEffect(() => {
    const onDown = (e: MouseEvent | TouchEvent) => {
      const t = e.target as Node;
      if (popRef.current?.contains(t) || anchor?.contains(t)) return;
      onClose(false);
    };
    const onScroll = (e: Event) => {
      if (popRef.current?.contains(e.target as Node)) return;
      place();
    };
    document.addEventListener("mousedown", onDown, true);
    document.addEventListener("touchstart", onDown, true);
    window.addEventListener("resize", place);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("mousedown", onDown, true);
      document.removeEventListener("touchstart", onDown, true);
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [anchor, onClose, place]);

  // Initial focus: the search box on desktop; the list on phones (no surprise keyboard).
  useEffect(() => {
    requestAnimationFrame(() => {
      if (searchable && !sheet) searchRef.current?.focus();
      else listRef.current?.focus({ preventScroll: true });
    });
  }, [searchable, sheet]);

  // Open with the current value centred; afterwards keep the active row in view.
  const centred = useRef(false);
  useEffect(() => {
    if (!sheet && style.visibility === "hidden") return;
    const el = listRef.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`);
    if (!el) return;
    el.scrollIntoView({ block: centred.current ? "nearest" : "center" });
    centred.current = true;
  }, [active, rows, style, sheet]);

  const onKey = (e: KeyboardEvent) => {
    if (composing(e.nativeEvent)) return;
    const n = filtered.length;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        if (n) setActive((a) => (a + 1) % n);
        return;
      case "ArrowUp":
        e.preventDefault();
        if (n) setActive((a) => (a - 1 + n) % n);
        return;
      case "Home":
        if (e.target === searchRef.current) return;
        e.preventDefault();
        setActive(0);
        return;
      case "End":
        if (e.target === searchRef.current) return;
        e.preventDefault();
        setActive(n - 1);
        return;
      case "PageDown":
        e.preventDefault();
        setActive((a) => Math.min(n - 1, a + 8));
        return;
      case "PageUp":
        e.preventDefault();
        setActive((a) => Math.max(0, a - 8));
        return;
      case "Enter":
        e.preventDefault();
        if (filtered[active]) onPick(filtered[active].value);
        return;
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        onClose();
        return;
      case "Tab":
        if (sheet) return;
        e.preventDefault();
        e.stopPropagation();
        onClose(false);
        focusPastAnchor(anchor, e.shiftKey, popRef.current);
        return;
    }
    // Type-ahead when there's no search box (or focus is on the list).
    if (e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey && e.target !== searchRef.current) {
      if (searchable) {
        searchRef.current?.focus();
        return; // the keystroke lands in the search box
      }
      const now = Date.now();
      const ta = typeahead.current;
      ta.buf = now - ta.t > 600 ? e.key.toLowerCase() : ta.buf + e.key.toLowerCase();
      ta.t = now;
      const start = ta.buf.length === 1 ? active + 1 : active;
      for (let k = 0; k < n; k++) {
        const i = (start + k) % n;
        if (filtered[i].label.toLowerCase().startsWith(ta.buf)) {
          setActive(i);
          break;
        }
      }
    }
  };

  const activeId = filtered[active] ? `${id}-opt-${active}` : undefined;

  const body = (
    <div
      ref={popRef}
      className={cx("sel-pop", sheet && "sheet")}
      style={sheet ? undefined : style}
      onKeyDown={onKey}
      role="dialog"
      aria-modal={sheet || undefined}
      aria-label={label}
    >
      {sheet && (
        <div className="sel-sheet-head">
          <span className="sel-grip" />
          <span className="sel-sheet-title">{label}</span>
          <button className="icon-btn" aria-label="Close" onClick={() => onClose()}>
            <IconX size={16} />
          </button>
        </div>
      )}
      {searchable && (
        <div className="sel-search">
          <IconSearch size={14} />
          <input
            ref={searchRef}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={searchPlaceholder ?? `Search ${label.toLowerCase()}…`}
            aria-label={`Search ${label}`}
            aria-controls={`${id}-list`}
            aria-activedescendant={activeId}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
          />
          {q && (
            <button className="icon-btn tiny" aria-label="Clear search" onClick={() => setQ("")}>
              <IconX size={11} />
            </button>
          )}
        </div>
      )}
      <div ref={listRef} data-modal-autofocus={sheet || undefined} className="sel-list" role="listbox" id={`${id}-list`} aria-label={label} aria-activedescendant={activeId} tabIndex={-1}>
        {rows.map((r, i) =>
          r.kind === "group" ? (
            <div key={`g${i}`} className="sel-group" role="presentation">
              {r.group}
            </div>
          ) : (
            <div
              key={`o${r.index}`}
              id={`${id}-opt-${r.index}`}
              data-idx={r.index}
              role="option"
              aria-selected={key(r.opt!.value) === key(value)}
              className={cx("sel-opt", r.index === active && "active", key(r.opt!.value) === key(value) && "selected")}
              onMouseMove={() => r.index !== active && setActive(r.index!)}
              onClick={() => onPick(r.opt!.value)}
            >
              <span className="sel-check">{key(r.opt!.value) === key(value) && <IconCheck size={13} />}</span>
              {r.opt!.icon && <span className="sel-opt-icon">{r.opt!.icon}</span>}
              <span className="sel-opt-main">
                <span className="sel-opt-label">
                  {r.opt!.label}
                  {q.trim() && r.opt!.group && <span className="sel-opt-group">{r.opt!.group}</span>}
                </span>
                {r.opt!.description && <span className="sel-opt-desc">{r.opt!.description}</span>}
              </span>
              {r.opt!.meta && <span className="sel-opt-meta">{r.opt!.meta}</span>}
            </div>
          ),
        )}
        {filtered.length === 0 && <div className="sel-empty">No matches for “{q}”</div>}
      </div>
    </div>
  );

  return createPortal(
    sheet ? (
      <div className="sel-scrim" onMouseDown={(e) => e.target === e.currentTarget && onClose(false)}>
        {body}
      </div>
    ) : (
      body
    ),
    document.body,
  );
}
