import { useState, type MouseEvent } from "react";
import type { HostState } from "../store";
import { archivedSessions, currentHost, patchSession, reconnectHost, selectHost, setNotify, sortedSessions, store, totalPending } from "../store";
import type { Route } from "../router";
import { sessionHash } from "../router";
import { notificationsSupported, requestNotifications } from "../notify";
import type { Session } from "../types";
import { basename, cx, fuzzyScore, modKey, relTime, useTick } from "../util";
import { ConnDot, StatusDot } from "./Status";
import { IconArchive, IconBell, IconBellOff, IconChevron, IconInbox, IconKeyboard, IconPin, IconPlus, IconRefresh, IconSearch, IconX, LogoMark } from "./Icons";

function matches(s: Session, q: string) {
  if (!q) return true;
  return [s.title, s.project, s.branch ?? "", s.agent].some((f) => fuzzyScore(q, f) >= 0);
}

export function Sidebar({
  route,
  open,
  onClose,
  onNew,
  onPalette,
  onHelp,
}: {
  route: Route;
  open: boolean;
  onClose: () => void;
  onNew: () => void;
  onPalette: () => void;
  onHelp: () => void;
}) {
  useTick(30_000);
  const [filter, setFilter] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  const host = currentHost();
  const pending = totalPending();
  const hub = store.mode === "hub";
  const active = host ? sortedSessions(host).filter((s) => matches(s, filter)) : [];
  const archived = host ? archivedSessions(host).filter((s) => matches(s, filter)) : [];
  const row = (s: Session) => (
    <SessionRow
      key={s.id}
      h={host!}
      s={s}
      active={route.name === "session" && route.host === host!.key && route.id === s.id}
      onClose={onClose}
    />
  );

  return (
    <>
      <aside className={cx("sidebar", open && "open")} aria-label="Sessions">
        <div className="sb-top">
          <a className="brand" href="#/" onClick={onClose}>
            <LogoMark />
            <span>sci-pi</span>
          </a>
          <span className="spacer" />
          <button className="icon-btn" onClick={onPalette} title={`Command palette (${modKey}K)`} aria-label="Command palette">
            <IconSearch />
          </button>
          <BellButton />
        </div>

        {hub ? (
          <div className="hosts">
            {store.hosts.map((h) => (
              <HostRow key={h.key} h={h} active={h.key === host?.key} />
            ))}
            {store.hosts.length === 0 && <div className="sb-empty">No hosts configured.</div>}
          </div>
        ) : (
          host && (
            <div className="hosts single">
              <HostRow h={host} active={false} />
            </div>
          )
        )}

        <nav className="sb-nav">
          <a className={cx("sb-link", route.name === "inbox" && "active")} href="#/inbox" onClick={onClose}>
            <IconInbox />
            <span>Inbox</span>
            {pending > 0 && <span className="badge attn">{pending}</span>}
          </a>
          <button className="sb-link" onClick={onNew} disabled={!host?.info}>
            <IconPlus />
            <span>New session</span>
            <kbd className="kbd">N</kbd>
          </button>
        </nav>

        <div className="sb-section">
          <span>Sessions</span>
          {host && <span className="dim">{host.sessions.size}</span>}
        </div>
        {host && host.sessions.size > 4 && (
          <div className="sb-filter">
            <IconSearch size={13} />
            <input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Filter sessions"
              aria-label="Filter sessions"
              onKeyDown={(e) => e.key === "Escape" && setFilter("")}
            />
            {filter && (
              <button className="icon-btn tiny" aria-label="Clear filter" onClick={() => setFilter("")}>
                <IconX size={11} />
              </button>
            )}
          </div>
        )}
        <div className="sb-list">
          {active.map(row)}
          {host && filter && active.length === 0 && archived.length === 0 && <div className="sb-empty">No sessions match “{filter}”.</div>}
          {archived.length > 0 && (
            <>
              <button className={cx("sb-group", (showArchived || !!filter) && "open")} onClick={() => setShowArchived(!showArchived)} aria-expanded={showArchived || !!filter}>
                <IconChevron size={11} className="chev" />
                <IconArchive size={12} />
                <span>Archived</span>
                <span className="dim">{archived.length}</span>
              </button>
              {(showArchived || filter) && archived.map(row)}
            </>
          )}
          {host && host.sessionsLoaded && host.sessions.size === 0 && (
            <div className="sb-empty">
              No sessions yet.{" "}
              <button className="linkish" onClick={onNew}>
                Start one
              </button>
            </div>
          )}
          {host && !host.sessionsLoaded && <div className="sb-empty">{host.conn === "auth" ? "Token rejected." : "Connecting…"}</div>}
        </div>

        <div className="sb-foot">
          <button className="icon-btn tiny" onClick={onHelp} title="Keyboard shortcuts (?)" aria-label="Keyboard shortcuts">
            <IconKeyboard size={13} />
          </button>
          {host?.info?.viewer ? (
            <span title={host.info.tailnet_url ?? undefined}>
              Signed in via Tailscale as <strong>{host.info.viewer}</strong>
            </span>
          ) : (
            <span>{host?.info ? `sci-pi ${host.info.version}` : ""}</span>
          )}
        </div>
      </aside>
      <div className={cx("scrim", open && "open")} onClick={onClose} />
    </>
  );
}

function SessionRow({ h, s, active, onClose }: { h: HostState; s: Session; active: boolean; onClose: () => void }) {
  const toggle = (patch: { pinned?: boolean; archived?: boolean }) => (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    void patchSession(h, s.id, patch).catch((err) => alert((err as Error).message));
  };
  return (
    <a className={cx("srow", active && "active", s.archived && "archived")} href={sessionHash(h.key, s.id)} onClick={onClose}>
      <StatusDot status={s.status} title={s.status_message ?? undefined} />
      <span className="srow-main">
        <span className="srow-title">
          {s.pinned && <IconPin size={11} className="pin-mark" />}
          {s.title || "Untitled session"}
        </span>
        <span className="srow-sub">
          <span>{h.info?.agents.find((a) => a.id === s.agent)?.name ?? s.agent}</span>
          <span className="sep">·</span>
          <span className="mono">
            {basename(s.project)}
            {s.branch ? `/${s.branch.replace(/^sci-pi\//, "")}` : ""}
          </span>
        </span>
      </span>
      <span className="srow-side">
        <time>{relTime(s.updated_at)}</time>
        {s.pending_permissions > 0 && <span className="badge attn">{s.pending_permissions}</span>}
        {s.queued > 0 && !s.pending_permissions && <span className="badge" title="queued prompts">{s.queued}</span>}
      </span>
      <span className="srow-actions">
        <button className={cx("icon-btn tiny", s.pinned && "on")} title={s.pinned ? "Unpin" : "Pin to top"} aria-label={s.pinned ? "Unpin" : "Pin"} onClick={toggle({ pinned: !s.pinned })}>
          <IconPin size={12} />
        </button>
        <button className="icon-btn tiny" title={s.archived ? "Unarchive" : "Archive"} aria-label={s.archived ? "Unarchive" : "Archive"} onClick={toggle({ archived: !s.archived })}>
          <IconArchive size={12} />
        </button>
      </span>
    </a>
  );
}

function HostRow({ h, active }: { h: HostState; active: boolean }) {
  const pending = [...h.sessions.values()].reduce((n, s) => n + (s.pending_permissions || 0), 0);
  const bad = h.conn === "auth" || h.hub?.status === "error" || (h.conn === "reconnecting" && !!h.connError);
  const hub = store.mode === "hub";
  const name = hub ? h.name : (h.info?.host ?? h.name);
  return (
    <div
      className={cx("host", active && "active", hub && "clickable", h.hub?.discovered && "discovered")}
      onClick={hub ? () => selectHost(h.key) : undefined}
      role={hub ? "button" : undefined}
      tabIndex={hub ? 0 : undefined}
      onKeyDown={hub ? (e) => e.key === "Enter" && selectHost(h.key) : undefined}
      title={h.hub?.error ?? h.connError ?? h.api.url}
    >
      <ConnDot conn={h.conn} hub={h.hub?.status} />
      <span className="host-name">{name}</span>
      {h.hub && <span className={cx("transport", `t-${h.hub.transport}`)}>{h.hub.transport}</span>}
      {h.hub?.discovered && (
        <span className="discovered-mark" title="Discovered on the tailnet (not in hosts.toml)">
          auto
        </span>
      )}
      <span className="spacer" />
      {pending > 0 && <span className="badge attn">{pending}</span>}
      {bad && (
        <button
          className="icon-btn tiny"
          title="Reconnect"
          aria-label="Reconnect"
          onClick={(e) => {
            e.stopPropagation();
            reconnectHost(h.key);
          }}
        >
          <IconRefresh size={12} />
        </button>
      )}
    </div>
  );
}

function BellButton() {
  if (!notificationsSupported()) return null;
  const on = store.notify && Notification.permission === "granted";
  return (
    <button
      className={cx("icon-btn", on && "on")}
      title={on ? "Notifications on (click to disable)" : "Notify me about approvals and finished turns"}
      aria-label="Toggle notifications"
      aria-pressed={on}
      onClick={async () => {
        if (on) setNotify(false);
        else setNotify(await requestNotifications());
      }}
    >
      {on ? <IconBell /> : <IconBellOff />}
    </button>
  );
}
