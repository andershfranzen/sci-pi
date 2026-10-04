import { currentHost, store, totalPending } from "../store";
import { IconInbox, IconMenu, IconPlus, LogoMark } from "./Icons";
import { StatusDot } from "./Status";
import type { SessionStatus } from "../types";

export function Home({ onMenu, onNew }: { onMenu: () => void; onNew: () => void }) {
  const h = currentHost();
  const pending = totalPending();
  const counts = new Map<SessionStatus, number>();
  for (const x of store.hosts) for (const s of x.sessions.values()) counts.set(s.status, (counts.get(s.status) ?? 0) + 1);
  const running = counts.get("running") ?? 0;
  return (
    <div className="page">
      <header className="page-header">
        <button className="icon-btn menu-btn" onClick={onMenu} aria-label="Open sidebar">
          <IconMenu />
        </button>
        <h1>{h?.info?.host ?? h?.name ?? "outpost"}</h1>
      </header>
      <div className="page-scroll">
        <div className="home">
          <LogoMark size={40} />
          <div className="home-stats">
            {running > 0 && (
              <span>
                <StatusDot status="running" /> {running} running
              </span>
            )}
            {(counts.get("idle") ?? 0) > 0 && (
              <span>
                <StatusDot status="idle" /> {counts.get("idle")} idle
              </span>
            )}
          </div>
          {pending > 0 && (
            <a className="callout" href="#/inbox">
              <IconInbox />
              <span>
                {pending} permission request{pending === 1 ? "" : "s"} waiting
              </span>
            </a>
          )}
          <button className="btn btn-primary" onClick={onNew} disabled={!h?.info}>
            <IconPlus size={14} /> New session
          </button>
          <p className="dim small">Sessions keep running on the host when you close this tab.</p>
        </div>
      </div>
    </div>
  );
}
