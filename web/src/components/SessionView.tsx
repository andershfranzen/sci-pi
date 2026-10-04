import { lazy, Suspense, useEffect, useRef, useState } from "react";
import type { HostState } from "../store";
import { getTimeline, loadHistory, patchSession, peekLog, store } from "../store";
import type { Session } from "../types";
import type { SessionTab } from "../router";
import { navigate, sessionHash } from "../router";
import { basename, cx, fmtCost, fmtTokens, tildify } from "../util";
import { Timeline } from "./Timeline";
import { DiffView } from "./DiffView";
import { Composer } from "./Composer";
import { StatusPill } from "./Status";
import { IconArchive, IconBranch, IconEdit, IconExternal, IconFolder, IconMenu, IconMore, IconPin, IconPR, IconStop, IconTrash } from "./Icons";
import { Modal } from "./Modal";
import { Menu } from "./Menu";

const TerminalView = lazy(() => import("./TerminalView"));

export function SessionView({
  h,
  session,
  tab,
  focusEvent,
  onMenu,
}: {
  h: HostState;
  session: Session;
  tab: SessionTab;
  focusEvent: number | null;
  onMenu: () => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const sid = session.id;

  useEffect(() => {
    void loadHistory(h, sid);
  }, [h, sid]);

  const log = peekLog(h, sid);
  const items = getTimeline(h, sid);
  const busy = session.status === "running" || session.status === "awaiting_permission";
  const agentName = h.info?.agents.find((a) => a.id === session.agent)?.name ?? session.agent;
  const setTab = (t: SessionTab) => navigate(sessionHash(h.key, sid, t));
  const act = (fn: () => Promise<unknown>) => () => void fn().catch((e) => alert((e as Error).message));

  return (
    <div className="session">
      <header className="session-header">
        <div className="sh-row">
          <button className="icon-btn menu-btn" onClick={onMenu} aria-label="Open sidebar">
            <IconMenu />
          </button>
          {renaming ? (
            <RenameInput h={h} session={session} onDone={() => setRenaming(false)} />
          ) : (
            <h1 className="sh-title" title="Click to rename" onClick={() => setRenaming(true)}>
              {session.pinned && <IconPin size={12} className="pin-mark" />}
              {session.title || "Untitled session"}
            </h1>
          )}
          <StatusPill status={session.status} />
          {session.pr_url && (
            <a className="pr-badge" href={session.pr_url} target="_blank" rel="noopener noreferrer" title={session.pr_url}>
              <IconPR size={12} />
              <span>PR{/\/pull\/(\d+)/.exec(session.pr_url)?.[1] ? ` #${/\/pull\/(\d+)/.exec(session.pr_url)![1]}` : ""}</span>
              <IconExternal size={10} />
            </a>
          )}
          <div className="sh-actions">
            <StopButton h={h} session={session} />
            <Menu
              label="Session actions"
              trigger={<IconMore />}
              items={[
                { label: "Rename", icon: <IconEdit size={14} />, onSelect: () => setRenaming(true) },
                {
                  label: session.pinned ? "Unpin" : "Pin to top",
                  icon: <IconPin size={14} />,
                  onSelect: act(() => patchSession(h, sid, { pinned: !session.pinned })),
                },
                {
                  label: session.archived ? "Unarchive" : "Archive",
                  icon: <IconArchive size={14} />,
                  onSelect: act(() => patchSession(h, sid, { archived: !session.archived })),
                },
                { label: "Delete…", icon: <IconTrash size={14} />, danger: true, onSelect: () => setConfirmDelete(true) },
              ]}
            />
          </div>
        </div>
        <div className="sh-meta">
          <span className="chip agent">{agentName}</span>
          {store.hosts.length > 1 && <span className="chip">{h.name}</span>}
          {session.archived && <span className="chip">archived</span>}
          <span className="meta-item mono" title={session.cwd}>
            <IconFolder size={12} />
            {basename(session.project)}
            {session.cwd.replace(/\/$/, "") !== session.project.replace(/\/$/, "") && <span className="dim"> · {tildify(session.cwd, h.info?.home)}</span>}
          </span>
          {session.branch && (
            <span className="meta-item mono" title={session.base_commit ? `from ${session.base_commit.slice(0, 10)}` : undefined}>
              <IconBranch size={12} />
              {session.branch}
            </span>
          )}
          <UsageMeter session={session} />
        </div>
        {session.status_message && (session.status === "error" || session.status === "detached") && (
          <div className={cx("sh-banner", session.status === "error" && "error")}>{session.status_message}</div>
        )}
        <nav className="tabs" role="tablist">
          {(
            [
              ["chat", "Conversation"],
              ["diff", "Diff"],
              ["terminal", "Terminal"],
            ] as const
          ).map(([t, label]) => (
            <button key={t} role="tab" aria-selected={tab === t} className={cx("tab", tab === t && "active")} onClick={() => setTab(t)}>
              {label}
            </button>
          ))}
        </nav>
      </header>

      {tab === "chat" && (
        <>
          <Timeline
            items={items}
            h={h}
            session={session}
            focusEvent={focusEvent}
            loading={!log?.loaded && (log?.loading ?? true)}
            error={log?.error ?? null}
          />
          <Composer key={`${h.key}:${sid}`} h={h} session={session} busy={busy} />
        </>
      )}
      {tab === "diff" && <DiffView h={h} session={session} />}
      {tab === "terminal" && (
        <Suspense fallback={<div className="term-loading dim">Loading terminal…</div>}>
          <TerminalView h={h} session={session} />
        </Suspense>
      )}

      {confirmDelete && <DeleteDialog h={h} session={session} onClose={() => setConfirmDelete(false)} />}
    </div>
  );
}

function RenameInput({ h, session, onDone }: { h: HostState; session: Session; onDone: () => void }) {
  const [v, setV] = useState(session.title);
  const ref = useRef<HTMLInputElement>(null);
  const done = useRef(false);
  useEffect(() => {
    ref.current?.select();
  }, []);
  const commit = () => {
    if (done.current) return;
    done.current = true;
    const t = v.trim();
    if (t && t !== session.title) void patchSession(h, session.id, { title: t }).catch((e) => alert((e as Error).message));
    onDone();
  };
  return (
    <input
      ref={ref}
      className="sh-rename"
      value={v}
      aria-label="Session title"
      onChange={(e) => setV(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit();
        if (e.key === "Escape") {
          done.current = true;
          onDone();
        }
      }}
    />
  );
}

function StopButton({ h, session }: { h: HostState; session: Session }) {
  const [busy, setBusy] = useState(false);
  const can = !["stopped", "detached", "error"].includes(session.status);
  if (!can) return null;
  return (
    <button
      className="icon-btn"
      title="Stop agent (next prompt resumes it)"
      aria-label="Stop agent"
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        try {
          await h.api.stop(session.id);
        } catch (e) {
          alert((e as Error).message);
        } finally {
          setBusy(false);
        }
      }}
    >
      <IconStop />
    </button>
  );
}

function UsageMeter({ session }: { session: Session }) {
  const u = session.usage;
  if (!u) return null;
  const pct = u.size ? Math.min(100, (u.used / u.size) * 100) : 0;
  return (
    <span className="usage" title={`${u.used.toLocaleString()} / ${u.size.toLocaleString()} tokens`}>
      <span className={cx("usage-bar", pct > 85 && "hot")}>
        <span style={{ width: `${pct}%` }} />
      </span>
      <span className="mono">
        {fmtTokens(u.used)}/{fmtTokens(u.size)}
      </span>
      {u.cost && <span className="mono dim">{fmtCost(u.cost)}</span>}
    </span>
  );
}

function DeleteDialog({ h, session, onClose }: { h: HostState; session: Session; onClose: () => void }) {
  const [removeWt, setRemoveWt] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  return (
    <Modal title="Delete session?" onClose={onClose} small>
      <p className="modal-text">
        This stops the agent and deletes the event log for <strong>{session.title || "this session"}</strong>. It can't be undone. Archive it instead to keep the history.
      </p>
      {session.branch && (
        <label className="check">
          <input type="checkbox" checked={removeWt} onChange={(e) => setRemoveWt(e.target.checked)} />
          <span>
            Also remove the worktree and branch <code>{session.branch}</code>
          </span>
        </label>
      )}
      {err && <div className="form-error">{err}</div>}
      <div className="modal-actions">
        <button className="btn btn-ghost" onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn btn-danger"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setErr(null);
            try {
              await h.api.deleteSession(session.id, removeWt);
              h.sessions.delete(session.id);
              h.logs.delete(session.id);
              onClose();
              navigate("#/");
            } catch (e) {
              setErr((e as Error).message);
              setBusy(false);
            }
          }}
        >
          {busy && <span className="spinner" />}
          Delete
        </button>
      </div>
    </Modal>
  );
}
