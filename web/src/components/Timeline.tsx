import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import type { HostState } from "../store";
import { dropInboxItem, emit } from "../store";
import type { TItem } from "../timeline";
import type { Attachment, Session, TurnRecovery } from "../types";
import { navigate, sessionHash } from "../router";
import { basename, clockTime, cx } from "../util";
import { Markdown } from "./Markdown";
import { PermissionCard, PlanCard, ToolCard } from "./ToolCard";
import { IconBrain, IconChevron, IconDiff, IconExternal, IconFile, IconFork, IconUndo } from "./Icons";
import { Modal } from "./Modal";
import { Select } from "./Select";
import { DiffFiles, DiffTotals, useDiff } from "./DiffView";
import { MessageActions } from "./MessageActions";
import "./Transcript.css";
import { useCompositionGuard } from "../keyboard";

function AttachmentImage({ h, name }: { h: HostState; name: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [preview, setPreview] = useState(false);
  useEffect(() => {
    let alive = true;
    let objectUrl: string | null = null;
    setUrl(null);
    setFailed(false);
    setPreview(false);
    h.api.attachment(name).then(
      (blob) => {
        if (!alive) return;
        objectUrl = URL.createObjectURL(blob);
        setUrl(objectUrl);
      },
      () => alive && setFailed(true),
    );
    return () => {
      alive = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [h, name]);
  if (!url) return <span className="fchip">{failed ? "Image unavailable" : "Loading image…"}</span>;
  return (
    <>
      <button type="button" className="thumb" onClick={() => setPreview(true)} aria-label={`Preview image ${name}`} title={name}>
        <img src={url} alt={name} loading="lazy" />
      </button>
      {preview && <Modal title="Attachment preview" onClose={() => setPreview(false)} wide>
        <img className="attachment-preview" src={url} alt={name} />
      </Modal>}
    </>
  );
}

function Attachments({ h, list }: { h: HostState; list: Attachment[] }) {
  if (!list.length) return null;
  return (
    <div className="bubble-attach">
      {list.map((a, i) =>
        a.type === "image" ? (
          <AttachmentImage key={i} h={h} name={a.name} />
        ) : (
          <span key={i} className="fchip" title={a.path}>
            <IconFile size={12} />
            <span className="mono">{basename(a.path)}</span>
          </span>
        ),
      )}
    </div>
  );
}

const UserBubble = memo(function UserBubble({ text, ts, h, attachments, onQuote }: { text: string; ts: number; h: HostState; attachments: Attachment[]; onQuote: (text: string) => void }) {
  return (
    <div className="tl-user">
      <div className="bubble" title={new Date(ts).toLocaleString()}>
        <Attachments h={h} list={attachments} />
        <span data-message-body>{text}</span>
      </div>
      <MessageActions text={text} onQuote={onQuote} />
    </div>
  );
});

const Thought = memo(function Thought({ text, live }: { text: string; live: boolean }) {
  const [open, setOpen] = useState(false);
  const preview = text.trim().split("\n")[0]?.slice(0, 140) ?? "";
  return (
    <div className={cx("thought", open && "open")}>
      <button className="thought-head" onClick={() => setOpen(!open)} aria-expanded={open}>
        <IconChevron size={12} className="chev" />
        <IconBrain size={13} />
        <span className={cx("thought-label", live && "shimmer")}>Thinking</span>
        {!open && <span className="thought-preview">{preview}</span>}
      </button>
      {open && <Markdown className="thought-body" text={text} />}
    </div>
  );
});

function stopLabel(r: string) {
  switch (r) {
    case "end_turn":
      return "Turn complete";
    case "cancelled":
      return "Cancelled";
    case "error":
    case "failed":
      return "Turn failed";
    case "interrupted":
      return "Turn interrupted";
    case "max_tokens":
      return "Stopped: max tokens";
    case "max_turn_requests":
      return "Stopped: max turn requests";
    case "refusal":
      return "Agent refused";
    default:
      return r.replace(/_/g, " ");
  }
}

// ------------------------------------------------------------------ turn footer

/** Files changed per turn are immutable (checkpoint diffs), so cache them for the page lifetime. */
const turnFileCache = new Map<string, number | Promise<number>>();

function useTurnFileCount(h: HostState, sid: string, turn: number, visible: boolean) {
  const key = `${h.key}:${sid}:${turn}`;
  const cached = turnFileCache.get(key);
  const [n, setN] = useState<number | null>(typeof cached === "number" ? cached : null);
  useEffect(() => {
    if (!visible || n !== null) return;
    let alive = true;
    let p = turnFileCache.get(key);
    if (p === undefined) {
      p = h.api.diff(sid, turn).then(
        (d) => {
          turnFileCache.set(key, d.files.length);
          return d.files.length;
        },
        (e) => {
          turnFileCache.delete(key);
          throw e;
        },
      );
      turnFileCache.set(key, p);
    }
    Promise.resolve(p).then(
      (v) => alive && setN(v),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [visible, key, h, sid, turn, n]);
  return n;
}

function TurnFooter({
  h,
  session,
  turn,
  stopReason,
  ts,
  reverted,
  recovery,
}: {
  h: HostState;
  session: Session;
  turn: number | null;
  stopReason: string;
  ts: number;
  reverted: boolean;
  recovery?: TurnRecovery | null;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [dlg, setDlg] = useState<null | "diff" | "revert" | "fork">(null);
  useEffect(() => {
    const el = ref.current;
    if (!el || turn === null) return;
    if (typeof IntersectionObserver === "undefined") return setVisible(true);
    const io = new IntersectionObserver((es) => es.some((e) => e.isIntersecting) && setVisible(true), { rootMargin: "200px" });
    io.observe(el);
    return () => io.disconnect();
  }, [turn]);
  const files = useTurnFileCount(h, session.id, turn ?? 0, visible && turn !== null);
  const busy = session.status === "running" || session.status === "awaiting_permission" || session.status === "starting";

  return (
    <div className={cx("tl-turn", (stopReason !== "end_turn" || (recovery && recovery.outcome !== "completed")) && "warn")} ref={ref}>
      <div className="turn-line">
        <span>
          {recovery && recovery.outcome !== "completed" ? `Turn ${recovery.outcome}` : stopLabel(stopReason)} · {clockTime(ts)}
        </span>
      </div>
      {recovery && recovery.outcome !== "completed" && (
        <div className="turn-line">
          <span>{recovery.completed_tools} of {recovery.started_tools} tools completed. {recovery.message}</span>
        </div>
      )}
      {turn !== null && (
        <div className="turn-actions">
          {files === null ? (
            <span className="turn-act dim">…</span>
          ) : files > 0 ? (
            <button className="turn-act" onClick={() => setDlg("diff")}>
              <IconDiff size={12} />
              {files} file{files === 1 ? "" : "s"} changed
            </button>
          ) : (
            <span className="turn-act dim">No file changes</span>
          )}
          {reverted ? (
            <span className="tag">reverted</span>
          ) : (
            <button
              className="turn-act"
              disabled={busy}
              title={busy ? "Wait for the turn to finish" : "Restore files to how they were before this turn"}
              onClick={() => setDlg("revert")}
            >
              <IconUndo size={12} />
              Revert
            </button>
          )}
          <button className="turn-act" disabled={busy} title="Start a new session from the end of this turn" onClick={() => setDlg("fork")}>
            <IconFork size={12} />
            Fork
          </button>
        </div>
      )}
      {dlg === "diff" && turn !== null && <TurnDiffModal h={h} sid={session.id} turn={turn} onClose={() => setDlg(null)} />}
      {dlg === "revert" && turn !== null && <RevertDialog h={h} session={session} turn={turn} onClose={() => setDlg(null)} />}
      {dlg === "fork" && turn !== null && <ForkDialog h={h} session={session} turn={turn} onClose={() => setDlg(null)} />}
    </div>
  );
}

function TurnDiffModal({ h, sid, turn, onClose }: { h: HostState; sid: string; turn: number; onClose: () => void }) {
  const { data, err, loading } = useDiff(h, sid, turn);
  return (
    <Modal title={`Changes in turn ${turn}`} onClose={onClose} wide>
      <div className="turn-diff">
        <div className="turn-diff-bar">{loading ? <span className="dim">Loading…</span> : <DiffTotals data={data} />}</div>
        <DiffFiles data={data} err={err} />
      </div>
    </Modal>
  );
}

function RevertDialog({ h, session, turn, onClose }: { h: HostState; session: Session; turn: number; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const later = Math.max(0, session.turns - turn);
  return (
    <Modal title={`Revert to before turn ${turn}?`} onClose={onClose} small>
      <p className="modal-text">
        Files in the worktree go back to how they were before turn {turn}
        {later > 0 ? `, undoing the changes from ${later} later turn${later === 1 ? "" : "s"} too` : ""}. Uncommitted edits made since then are lost. The conversation stays,
        and the agent is told on its next prompt.
      </p>
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
              await h.api.revert(session.id, turn);
              onClose();
            } catch (e) {
              setErr((e as Error).message);
              setBusy(false);
            }
          }}
        >
          {busy && <span className="spinner" />}
          Revert files
        </button>
      </div>
    </Modal>
  );
}

function ForkDialog({ h, session, turn, onClose }: { h: HostState; session: Session; turn: number; onClose: () => void }) {
  const agents = h.info?.agents ?? [];
  const [agent, setAgent] = useState(session.agent);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  return (
    <Modal title={`Fork from turn ${turn}`} onClose={onClose} small>
      <p className="modal-text">
        Starts a new session with the files from the end of turn {turn} in a fresh worktree, and hands the conversation so far to the agent you pick.
      </p>
      <div className="field">
        <span className="label">Agent</span>
        <Select
          variant="field"
          label="Agent"
          value={agent}
          onChange={(v) => setAgent(String(v))}
          options={agents.map((a) => ({ value: a.id, label: a.name, meta: a.id === session.agent ? "current" : undefined }))}
        />
      </div>
      {err && <div className="form-error">{err}</div>}
      <div className="modal-actions">
        <button className="btn btn-ghost" onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setErr(null);
            try {
              const s = await h.api.fork(session.id, { turn, ...(agent !== session.agent ? { agent } : {}) });
              h.sessions.set(s.id, s);
              emit();
              onClose();
              navigate(sessionHash(h.key, s.id));
            } catch (e) {
              setErr((e as Error).message);
              setBusy(false);
            }
          }}
        >
          {busy && <span className="spinner" />}
          <IconFork size={13} /> Fork
        </button>
      </div>
    </Modal>
  );
}

// ------------------------------------------------------------------ items

function Item({ it, h, session, live, onQuote }: { it: TItem; h: HostState; session: Session; live: boolean; onQuote: (text: string) => void }) {
  const sid = session.id;
  const choose = useCallback(
    async (optionId: string | null) => {
      if (it.t !== "perm") return;
      await h.api.permission(sid, it.requestId, optionId);
      dropInboxItem(h, it.requestId);
    },
    [h, sid, it],
  );
  switch (it.t) {
    case "user":
      return <UserBubble text={it.text} ts={it.ts} h={h} attachments={it.attachments} onQuote={onQuote} />;
    case "msg":
      if (it.role === "thought") return <Thought text={it.text} live={live} />;
      if (it.role === "user") return <UserBubble text={it.text} ts={it.ts} h={h} attachments={[]} onQuote={onQuote} />;
      return (
        <div className="tl-agent">
          <div data-message-body><Markdown text={it.text} /></div>
          <MessageActions text={it.text} onQuote={onQuote} />
        </div>
      );
    case "tool":
      return <ToolCard call={it.call} />;
    case "plan":
      return <PlanCard entries={it.entries} />;
    case "perm":
      return <PermissionCard toolCall={it.toolCall} options={it.options} resolved={it.resolved} onChoose={choose} />;
    case "turn":
      return <TurnFooter h={h} session={session} turn={it.turn} stopReason={it.stopReason} ts={it.ts} reverted={it.reverted} recovery={it.recovery} />;
    case "forked": {
      const exists = h.sessions.has(it.from);
      return (
        <div className="tl-forked">
          <IconFork size={13} />
          <span>
            Forked from{" "}
            {exists ? (
              <a href={sessionHash(h.key, it.from)}>{h.sessions.get(it.from)?.title || it.fromTitle}</a>
            ) : (
              <strong>{it.fromTitle}</strong>
            )}
            {it.turn !== null && <span className="dim"> · after turn {it.turn}</span>}
          </span>
        </div>
      );
    }
    case "sys":
      return (
        <div className={cx("tl-sys", it.level === "error" && "error", it.level === "ok" && "ok")}>
          {it.text}
          {it.href && (
            <>
              {" "}
              <a href={it.href} target="_blank" rel="noopener noreferrer" className="mono">
                {it.href.replace(/^https?:\/\/(www\.)?/, "")} <IconExternal size={10} />
              </a>
            </>
          )}
        </div>
      );
  }
}

const MemoItem = memo(Item);

export function Timeline({
  items,
  h,
  session,
  loading,
  error,
  focusEvent,
  footer,
  onQuote,
}: {
  items: TItem[];
  h: HostState;
  session: Session;
  loading: boolean;
  error: string | null;
  focusEvent?: number | null;
  footer?: ReactNode;
  onQuote: (text: string) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const lastSid = useRef<string | null>(null);
  const focused = useRef<number | null>(null);
  const running = session.status === "running";
  const guardComposition = useCompositionGuard();
  const readingLock = useRef(false);
  const [detached, setDetached] = useState(false);
  const [activePrompt, setActivePrompt] = useState("");
  const flashTimer = useRef<number | undefined>(undefined);
  const prompts = items.filter((it) => it.t === "user" || (it.t === "msg" && it.role === "user"));
  const activeIndex = prompts.findIndex((it) => it.key === activePrompt);
  useEffect(() => () => window.clearTimeout(flashTimer.current), []);
  const navigatePrompt = (key: string) => {
    const el = ref.current;
    const target = Array.from(el?.querySelectorAll<HTMLElement>("[data-prompt]") ?? []).find((node) => node.dataset.ev === key);
    if (!el || !target) return;
    stick.current = false;
    readingLock.current = true;
    setDetached(true);
    setActivePrompt(key);
    el.scrollTop += target.getBoundingClientRect().top - el.getBoundingClientRect().top - 12;
    target.focus({ preventScroll: true });
  };
  const latest = () => {
    const el = ref.current;
    stick.current = true;
    readingLock.current = false;
    setDetached(false);
    if (el) el.scrollTop = el.scrollHeight;
    setActivePrompt(prompts.at(-1)?.key ?? "");
  };

  const onScroll = () => {
    const el = ref.current;
    if (!el) return;
    stick.current = !readingLock.current && el.scrollHeight - el.scrollTop - el.clientHeight < 120;
    setDetached(!stick.current);
    const top = el.getBoundingClientRect().top;
    let key = "";
    for (const node of el.querySelectorAll<HTMLElement>("[data-prompt]")) {
      if (node.getBoundingClientRect().top <= top + 32) key = node.dataset.ev ?? "";
      else break;
    }
    setActivePrompt(key || prompts[0]?.key || "");
  };

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (lastSid.current !== session.id) {
      lastSid.current = session.id;
      stick.current = true;
      readingLock.current = false;
      focused.current = null;
      setDetached(false);
      setActivePrompt("");
      window.clearTimeout(flashTimer.current);
    }
    // Jump to a search hit: the item whose first event id is the closest one at or before it.
    if (focusEvent && focused.current !== focusEvent && items.length) {
      let target: HTMLElement | null = null;
      for (const node of el.querySelectorAll<HTMLElement>("[data-ev]")) {
        if (Number(node.dataset.ev) <= focusEvent) target = node;
        else break;
      }
      if (target) {
        focused.current = focusEvent;
        stick.current = false;
        readingLock.current = true;
        setDetached(true);
        target.scrollIntoView({ block: "center" });
        target.classList.add("flash");
        window.clearTimeout(flashTimer.current);
        flashTimer.current = window.setTimeout(() => target?.classList.remove("flash"), 1600);
        return;
      }
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const inner = el.firstElementChild;
    if (!inner) return;
    const ro = new ResizeObserver(() => {
      if (stick.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(inner);
    return () => ro.disconnect();
  }, []);

  const lastIdx = items.length - 1;
  return (
    <div className="transcript-pane">
      {prompts.length > 0 && <nav className="transcript-navigation" aria-label="Conversation turns">
        <button type="button" className="transcript-step" disabled={activeIndex <= 0} onClick={() => navigatePrompt(prompts[activeIndex - 1].key)} aria-label="Previous prompt">Previous</button>
        <Select
          variant="field"
          className="transcript-prompt-picker"
          label="Go to prompt"
          value={activePrompt}
          display={activePrompt ? undefined : "Go to prompt…"}
          options={prompts.map((it, index) => ({ value: it.key, label: `${index + 1}. ${it.t === "user" || it.t === "msg" ? it.text.replace(/\s+/g, " ").slice(0, 90) || "Attached prompt" : ""}` }))}
          onChange={(key) => requestAnimationFrame(() => navigatePrompt(String(key)))}
        />
        <button type="button" className="transcript-step" disabled={activeIndex >= prompts.length - 1} onClick={() => navigatePrompt(prompts[activeIndex + 1].key)} aria-label="Next prompt">Next</button>
      </nav>}
    <div className="timeline-scroll" ref={ref} onScroll={onScroll}
      onWheel={() => { readingLock.current = false; }}
      onTouchStart={() => { readingLock.current = false; }}
      onPointerDown={(event) => { if (event.target === event.currentTarget) readingLock.current = false; }}
      onKeyDown={(event) => {
        if (guardComposition(event.nativeEvent)) return;
        if (["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "].includes(event.key)) readingLock.current = false;
      }}>
      <div className="timeline">
        {loading && items.length === 0 && <div className="tl-loading">Loading history…</div>}
        {error && <div className="tl-sys error">Failed to load history: {error}</div>}
        {!loading && items.length === 0 && !error && <div className="tl-empty">No messages yet.</div>}
        {items.map((it, i) => (
          <div key={it.key} data-ev={it.key} data-prompt={it.t === "user" || (it.t === "msg" && it.role === "user") ? "" : undefined} tabIndex={-1} className="tl-item">
            <MemoItem it={it} h={h} session={session} live={running && i === lastIdx} onQuote={onQuote} />
          </div>
        ))}
        {running && (
          <div className="tl-working">
            <span className="working-dots">
              <i />
              <i />
              <i />
            </span>
          </div>
        )}
        {footer}
      </div>
    </div>
      {detached && <button type="button" className="transcript-latest" onClick={latest}>Jump to latest ↓</button>}
    </div>
  );
}
