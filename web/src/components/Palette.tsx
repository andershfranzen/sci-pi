import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { selectHost, setNotify, store } from "../store";
import type { Route } from "../router";
import { navigate, sessionHash } from "../router";
import { requestNotifications } from "../notify";
import type { SearchHit } from "../types";
import { cx, fuzzyScore, isMac, modKey, relTime } from "../util";
import { useCompositionGuard } from "../keyboard";
import { useModalLayer } from "../overlays";
import { Modal } from "./Modal";
import { StatusDot } from "./Status";
import { IconBell, IconDiff, IconHome, IconInbox, IconKeyboard, IconMessage, IconPlus, IconSearch, IconServer, IconTerminal } from "./Icons";

interface PItem {
  id: string;
  group: string;
  label: ReactNode;
  sub?: ReactNode;
  icon?: ReactNode;
  run: () => void;
}

/** Render a search snippet, turning <<match>> markers into <mark>. */
function Snippet({ text }: { text: string }) {
  const parts = text.split(/(<<.*?>>)/g);
  return (
    <>
      {parts.map((p, i) => (p.startsWith("<<") && p.endsWith(">>") ? <mark key={i}>{p.slice(2, -2)}</mark> : <span key={i}>{p}</span>))}
    </>
  );
}

export function Palette({ route, onClose, onNew, onHelp }: { route: Route; onClose: () => void; onNew: () => void; onHelp: () => void }) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const composing = useCompositionGuard();
  useModalLayer(dialogRef, onClose);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const [hits, setHits] = useState<{ hostKey: string; hit: SearchHit }[]>([]);
  const [searching, setSearching] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);
  const multi = store.hosts.length > 1;

  // Full-text search fan-out across connected hosts.
  useEffect(() => {
    const query = q.trim();
    if (query.length < 2) {
      setHits([]);
      setSearching(false);
      return;
    }
    setSearching(true);
    let alive = true;
    const t = setTimeout(async () => {
      const hosts = store.hosts.filter((h) => h.conn === "open");
      const res = await Promise.allSettled(hosts.map((h) => h.api.search(query).then((r) => r.map((hit) => ({ hostKey: h.key, hit })))));
      if (!alive) return;
      setHits(res.flatMap((r) => (r.status === "fulfilled" ? r.value : [])));
      setSearching(false);
    }, 220);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [q]);

  const items = useMemo(() => {
    const go = (fn: () => void) => () => {
      onClose();
      fn();
    };
    const query = q.trim();
    const actions: PItem[] = [
      { id: "a:new", group: "Actions", label: "New session", icon: <IconPlus size={14} />, run: go(onNew) },
      { id: "a:inbox", group: "Actions", label: "Open inbox", icon: <IconInbox size={14} />, run: go(() => navigate("#/inbox")) },
      { id: "a:home", group: "Actions", label: "Go home", icon: <IconHome size={14} />, run: go(() => navigate("#/")) },
    ];
    if (route.name === "session") {
      const { host, id } = route;
      actions.push(
        { id: "a:chat", group: "Actions", label: "Show conversation", icon: <IconMessage size={14} />, run: go(() => navigate(sessionHash(host, id, "chat"))) },
        { id: "a:diff", group: "Actions", label: "Show diff", icon: <IconDiff size={14} />, run: go(() => navigate(sessionHash(host, id, "diff"))) },
        { id: "a:term", group: "Actions", label: "Open terminal", icon: <IconTerminal size={14} />, run: go(() => navigate(sessionHash(host, id, "terminal"))) },
      );
    }
    actions.push(
      {
        id: "a:bell",
        group: "Actions",
        label: store.notify ? "Turn notifications off" : "Turn notifications on",
        icon: <IconBell size={14} />,
        run: go(async () => setNotify(store.notify ? false : await requestNotifications())),
      },
      { id: "a:keys", group: "Actions", label: "Keyboard shortcuts", icon: <IconKeyboard size={14} />, run: go(onHelp) },
    );
    if (store.mode === "hub")
      for (const h of store.hosts)
        actions.push({ id: `a:host:${h.key}`, group: "Actions", label: `Switch to host ${h.name}`, icon: <IconServer size={14} />, run: go(() => selectHost(h.key)) });

    const sessions: (PItem & { score: number; ts: number })[] = [];
    for (const h of store.hosts)
      for (const s of h.sessions.values()) {
        const score = query ? Math.max(fuzzyScore(query, s.title), fuzzyScore(query, s.project) * 0.5) : 0;
        if (score < 0) continue;
        sessions.push({
          id: `s:${h.key}:${s.id}`,
          group: "Sessions",
          label: (
            <>
              {s.title || "Untitled session"}
              {s.archived && <span className="tag">archived</span>}
            </>
          ),
          sub: `${multi ? `${h.name} · ` : ""}${s.project.split("/").pop()} · ${relTime(s.updated_at)}`,
          icon: <StatusDot status={s.status} />,
          run: go(() => navigate(sessionHash(h.key, s.id))),
          score,
          ts: s.updated_at,
        });
      }
    sessions.sort((a, b) => (query ? b.score - a.score : b.ts - a.ts));

    const actionHits = query ? actions.map((a) => ({ a, s: fuzzyScore(query, String(a.label)) })).filter((x) => x.s >= 0).sort((x, y) => y.s - x.s).map((x) => x.a) : actions;

    // Group full-text hits by session.
    const groups = new Map<string, { hostKey: string; hits: SearchHit[] }>();
    for (const { hostKey, hit } of hits) {
      const k = `${hostKey}:${hit.session_id}`;
      const g = groups.get(k) ?? { hostKey, hits: [] };
      g.hits.push(hit);
      groups.set(k, g);
    }
    const search: PItem[] = [];
    for (const [k, g] of groups) {
      const h = store.hosts.find((x) => x.key === g.hostKey);
      g.hits.slice(0, 3).forEach((hit, i) => {
        search.push({
          id: `f:${k}:${hit.event_id}`,
          group: "Messages",
          label: i === 0 ? (h?.sessions.get(hit.session_id)?.title ?? hit.session_title) : "",
          sub: (
            <span className={cx("snippet", hit.role === "user" && "user")}>
              <span className="snippet-role">{hit.role === "user" ? "you" : "agent"}</span>
              <Snippet text={hit.snippet} />
            </span>
          ),
          icon: i === 0 ? <IconSearch size={14} /> : <span />,
          run: go(() => navigate(sessionHash(g.hostKey, hit.session_id, "chat", hit.event_id))),
        });
      });
    }

    return [...(query ? sessions.slice(0, 8) : []), ...actionHits, ...(query ? [] : sessions.slice(0, 6)), ...search];
  }, [q, hits, route, onClose, onNew, onHelp, multi]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    listRef.current?.querySelector(".pitem.active")?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  let lastGroup = "";
  return (
    <div className="modal-backdrop palette-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div ref={dialogRef} tabIndex={-1} className="palette" role="dialog" aria-modal="true" aria-label="Command palette">
        <div className="palette-input">
          <IconSearch size={15} />
          <input
            data-modal-autofocus
            value={q}
            placeholder="Search sessions, messages, actions…"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (composing(e.nativeEvent)) return;
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setSel((s) => Math.min(items.length - 1, s + 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setSel((s) => Math.max(0, s - 1));
              } else if (e.key === "Enter") {
                e.preventDefault();
                items[sel]?.run();
              }
            }}
            aria-label="Command palette search"
          />
          {searching && <span className="spinner dim" />}
          <kbd className="kbd">esc</kbd>
        </div>
        <div className="palette-list" ref={listRef}>
          {items.map((it, i) => {
            const header = it.group !== lastGroup ? it.group : null;
            lastGroup = it.group;
            return (
              <div key={it.id}>
                {header && <div className="pgroup">{header}</div>}
                <button className={cx("pitem", i === sel && "active", !it.label && "cont")} onMouseMove={() => setSel(i)} onClick={() => it.run()}>
                  <span className="pitem-icon">{it.icon}</span>
                  <span className="pitem-main">
                    {it.label && <span className="pitem-label">{it.label}</span>}
                    {it.sub && <span className="pitem-sub">{it.sub}</span>}
                  </span>
                </button>
              </div>
            );
          })}
          {items.length === 0 && !searching && <div className="pempty">Nothing found.</div>}
          {q.trim().length >= 2 && !searching && hits.length === 0 && items.length > 0 && <div className="pempty small">No message matches.</div>}
        </div>
        <div className="palette-foot dim">
          <span>
            <kbd className="kbd">↑↓</kbd> navigate
          </span>
          <span>
            <kbd className="kbd">↵</kbd> open
          </span>
          <span>
            <kbd className="kbd">{modKey}K</kbd> toggle
          </span>
        </div>
      </div>
    </div>
  );
}

/** Each entry: groups of keys (pressed together), joined by `sep`, plus a description. */
export const SHORTCUTS: { keys: string[][]; sep?: string; note?: string; desc: string }[] = [
  { keys: [[modKey, "K"]], note: isMac ? undefined : "outside the terminal", desc: "Command palette: sessions, message search, actions" },
  { keys: [["N"]], desc: "New session" },
  { keys: [["?"]], desc: "Show this help" },
  { keys: [["Alt", "1"], ["2"], ["3"]], sep: "/", desc: "Conversation / Diff / Terminal tab" },
  { keys: [["Alt", "↑"], ["↓"]], sep: "/", desc: "Previous / next session" },
  { keys: [["G"], ["I"]], sep: "then", desc: "Go to inbox" },
  { keys: [["Enter"]], desc: "Send prompt (queued while the agent works)" },
  { keys: [["Shift", "Enter"]], desc: "New line" },
  { keys: [["↑"]], note: "empty composer", desc: "Recall previous prompt" },
  { keys: [["@"]], desc: "Mention a file" },
  { keys: [["/"]], note: "at start", desc: "Slash command" },
  { keys: [[modKey, "Enter"]], desc: "Start session (new-session dialog)" },
  { keys: [["Esc"]], desc: "Close dialog or picker" },
];

export function ShortcutsOverlay({ onClose }: { onClose: () => void }) {
  return (
    <Modal title="Keyboard shortcuts" onClose={onClose} small>
      <div className="shortcuts">
          <table>
            <tbody>
              {SHORTCUTS.map((sc) => (
                <tr key={sc.desc}>
                  <td>
                    {sc.keys.map((group, gi) => (
                      <span key={gi}>
                        {gi > 0 && <span className="dim"> {sc.sep} </span>}
                        {group.map((k) => (
                          <kbd key={k} className="kbd">
                            {k}
                          </kbd>
                        ))}
                      </span>
                    ))}
                    {sc.note && <span className="dim small"> {sc.note}</span>}
                  </td>
                  <td>{sc.desc}</td>
                </tr>
              ))}
            </tbody>
          </table>
      </div>
    </Modal>
  );
}
