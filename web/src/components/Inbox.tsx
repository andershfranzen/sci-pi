import { useEffect } from "react";
import type { HostState } from "../store";
import { dropInboxItem, refreshAllInboxes, refreshInbox, store } from "../store";
import type { InboxItem } from "../types";
import { sessionHash } from "../router";
import { relTime, useTick } from "../util";
import { PermissionCard } from "./ToolCard";
import { IconCheck, IconExternal, IconMenu, IconRefresh } from "./Icons";

export function Inbox({ onMenu }: { onMenu: () => void }) {
  useTick(30_000);
  useEffect(() => {
    refreshAllInboxes();
  }, []);

  const items: { h: HostState; it: InboxItem }[] = [];
  for (const h of store.hosts) for (const it of h.inbox) items.push({ h, it });
  items.sort((a, b) => b.it.ts - a.it.ts);
  const multi = store.hosts.length > 1;
  const loaded = store.hosts.every((h) => h.inboxLoaded || h.conn === "auth");

  return (
    <div className="page">
      <header className="page-header">
        <button className="icon-btn menu-btn" onClick={onMenu} aria-label="Open sidebar">
          <IconMenu />
        </button>
        <h1>Inbox</h1>
        {items.length > 0 && <span className="badge attn">{items.length}</span>}
        <span className="spacer" />
        <button className="icon-btn" onClick={refreshAllInboxes} title="Refresh" aria-label="Refresh">
          <IconRefresh />
        </button>
      </header>
      <div className="page-scroll">
        <div className="inbox">
          {items.map(({ h, it }) => {
            const s = h.sessions.get(it.session_id);
            const href = sessionHash(h.key, it.session_id);
            return (
              <PermissionCard
                key={`${h.key}:${it.request_id}`}
                compact
                toolCall={it.tool_call}
                options={it.options}
                resolved={null}
                onChoose={async (optionId) => {
                  await h.api.permission(it.session_id, it.request_id, optionId);
                  dropInboxItem(h, it.request_id);
                  refreshInbox(h, 800);
                }}
                header={
                  <div className="inbox-head">
                    <a className="inbox-session" href={href}>
                      {s?.title || it.session_title || "Session"}
                    </a>
                    {multi && <span className="chip">{h.name}</span>}
                    <time className="dim">{relTime(it.ts)}</time>
                    <a className="icon-btn tiny" href={href} title="Open session" aria-label="Open session">
                      <IconExternal size={13} />
                    </a>
                  </div>
                }
              />
            );
          })}
          {items.length === 0 && (
            <div className="empty-state">
              <div className="empty-icon">
                <IconCheck size={22} />
              </div>
              <div className="empty-title">{loaded ? "All clear" : "Loading…"}</div>
              {loaded && <div className="dim">No agent is waiting on you right now.</div>}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
