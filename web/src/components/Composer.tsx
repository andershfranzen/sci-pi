import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { HostState } from "../store";
import { getTimeline } from "../store";
import { promptHistory } from "../timeline";
import type { AttachmentIn, ConfigOption, QueueItem, Session, SlashCommand } from "../types";
import { basename, cx, fuzzyScore, isTouch, readFileBase64 } from "../util";
import { IconArrowUp, IconAt, IconCheck, IconEdit, IconFile, IconImage, IconTrash, IconX, IconZap } from "./Icons";

interface PendingImage {
  id: string;
  mime_type: string;
  data: string;
  url: string;
  name: string;
  size: number;
}

type Picker =
  | { kind: "slash"; query: string; items: SlashCommand[] }
  | { kind: "mention"; query: string; start: number; items: string[]; loading: boolean };

const MAX_IMAGE = 10 * 1024 * 1024;

export function Composer({ h, session, busy }: { h: HostState; session: Session; busy: boolean }) {
  const draftKey = `sci-pi.draft.${h.key}.${session.id}`;
  const [text, setText] = useState(() => {
    try {
      return sessionStorage.getItem(draftKey) ?? "";
    } catch {
      return "";
    }
  });
  const [files, setFiles] = useState<string[]>([]);
  const [images, setImages] = useState<PendingImage[]>([]);
  const [sending, setSending] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [picker, setPicker] = useState<Picker | null>(null);
  const [sel, setSel] = useState(0);
  const [dragging, setDragging] = useState(false);
  const [hist, setHist] = useState<number | null>(null);
  const ta = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const mentionSeq = useRef(0);

  useEffect(() => {
    try {
      if (text) sessionStorage.setItem(draftKey, text);
      else sessionStorage.removeItem(draftKey);
    } catch {
      /* ignore */
    }
  }, [draftKey, text]);

  useEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [text]);

  useEffect(() => () => images.forEach((i) => URL.revokeObjectURL(i.url)), []); // eslint-disable-line react-hooks/exhaustive-deps

  // ------------------------------------------------------------ pickers

  const updatePicker = (value: string, caret: number) => {
    const slash = /^\/(\S*)$/.exec(value);
    if (slash && session.commands?.length) {
      const q = slash[1];
      const items = session.commands
        .map((c) => ({ c, s: q ? Math.max(fuzzyScore(q, c.name) * 2, fuzzyScore(q, c.description) * 0.3) : 0 }))
        .filter((x) => x.s >= 0)
        .sort((a, b) => b.s - a.s)
        .slice(0, 50)
        .map((x) => x.c);
      setPicker({ kind: "slash", query: q, items });
      setSel(0);
      return;
    }
    const before = value.slice(0, caret);
    const m = /(?:^|\s)@([^\s@]*)$/.exec(before);
    if (m) {
      const q = m[1];
      const start = caret - q.length - 1;
      setPicker((p) => ({
        kind: "mention",
        query: q,
        start,
        items: p?.kind === "mention" ? p.items : [],
        loading: true,
      }));
      setSel(0);
      const seq = ++mentionSeq.current;
      setTimeout(async () => {
        if (seq !== mentionSeq.current) return;
        try {
          const items = await h.api.files(session.id, q);
          if (seq !== mentionSeq.current) return;
          setPicker((p) => (p?.kind === "mention" ? { ...p, items, loading: false } : p));
        } catch {
          setPicker((p) => (p?.kind === "mention" ? { ...p, items: [], loading: false } : p));
        }
      }, 120);
      return;
    }
    if (picker) setPicker(null);
  };

  const choose = (i: number) => {
    if (!picker) return;
    if (picker.kind === "slash") {
      const c = picker.items[i];
      if (!c) return;
      const v = `/${c.name} `;
      setText(v);
      setPicker(null);
      requestAnimationFrame(() => {
        ta.current?.focus();
        ta.current?.setSelectionRange(v.length, v.length);
      });
      return;
    }
    const path = picker.items[i];
    if (!path) return;
    const caret = ta.current?.selectionStart ?? text.length;
    const v = (text.slice(0, picker.start) + text.slice(caret)).replace(/ {2,}/g, " ");
    setText(v);
    setFiles((f) => (f.includes(path) ? f : [...f, path]));
    setPicker(null);
    mentionSeq.current++;
    requestAnimationFrame(() => {
      ta.current?.focus();
      ta.current?.setSelectionRange(picker.start, picker.start);
    });
  };

  // ------------------------------------------------------------ images

  const addImages = async (list: FileList | File[]) => {
    const arr = [...list].filter((f) => f.type.startsWith("image/"));
    if (!arr.length) return;
    setErr(null);
    for (const f of arr) {
      if (f.size > MAX_IMAGE) {
        setErr(`${f.name || "image"} is larger than 10 MB`);
        continue;
      }
      const data = await readFileBase64(f);
      setImages((cur) => [
        ...cur,
        {
          id: Math.random().toString(36).slice(2),
          mime_type: f.type,
          data,
          url: URL.createObjectURL(f),
          name: f.name || "pasted image",
          size: f.size,
        },
      ]);
    }
  };

  const removeImage = (id: string) =>
    setImages((cur) => {
      const it = cur.find((i) => i.id === id);
      if (it) URL.revokeObjectURL(it.url);
      return cur.filter((i) => i.id !== id);
    });

  // ------------------------------------------------------------ history

  const history = useMemo(() => promptHistory(getTimeline(h, session.id)), [h, session.id, session.turns]); // eslint-disable-line react-hooks/exhaustive-deps

  const recall = (dir: -1 | 1) => {
    if (!history.length) return false;
    let next: number | null;
    if (hist === null) {
      if (dir === 1) return false;
      next = history.length - 1;
    } else {
      next = hist + dir;
      if (next < 0) next = 0;
      if (next >= history.length) next = null;
    }
    setHist(next);
    const v = next === null ? "" : history[next];
    setText(v);
    requestAnimationFrame(() => ta.current?.setSelectionRange(v.length, v.length));
    return true;
  };

  // ------------------------------------------------------------ send

  const canSend = (text.trim() || files.length || images.length) && !sending;

  const send = async () => {
    if (!canSend) return;
    setSending(true);
    setErr(null);
    const attachments: AttachmentIn[] = [
      ...files.map((path) => ({ type: "file" as const, path })),
      ...images.map((i) => ({ type: "image" as const, mime_type: i.mime_type, data: i.data })),
    ];
    try {
      await h.api.prompt(session.id, text.trim(), attachments);
      setText("");
      setFiles([]);
      images.forEach((i) => URL.revokeObjectURL(i.url));
      setImages([]);
      setHist(null);
      setPicker(null);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setSending(false);
      if (!isTouch()) ta.current?.focus();
    }
  };

  const cancel = async () => {
    setCancelling(true);
    try {
      await h.api.cancel(session.id);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setCancelling(false);
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (picker && picker.items.length) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSel((s) => (s + 1) % picker.items.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setSel((s) => (s - 1 + picker.items.length) % picker.items.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        choose(sel);
        return;
      }
    }
    if (picker && e.key === "Escape") {
      e.preventDefault();
      setPicker(null);
      return;
    }
    if (e.key === "ArrowUp" && !e.shiftKey && (text === "" || (hist !== null && text === history[hist]))) {
      if (recall(-1)) e.preventDefault();
      return;
    }
    if (e.key === "ArrowDown" && hist !== null && text === history[hist]) {
      if (recall(1)) e.preventDefault();
      return;
    }
    if (e.key === "Backspace" && text === "" && (files.length || images.length)) {
      if (images.length) removeImage(images[images.length - 1].id);
      else setFiles((f) => f.slice(0, -1));
      return;
    }
    // Enter sends on keyboards; on touch devices Enter inserts a newline and the button sends.
    if (e.key === "Enter" && !e.shiftKey && !isTouch()) {
      e.preventDefault();
      void send();
    }
  };

  const placeholder = busy
    ? "Queue a follow-up…"
    : session.status === "stopped" || session.status === "detached"
      ? "Send a prompt to resume…"
      : typeof matchMedia !== "undefined" && matchMedia("(max-width: 600px)").matches
        ? "Message the agent…"
        : "Message the agent — @ for files, / for commands";

  return (
    <div className="composer">
      {session.queue?.length > 0 && <QueuePanel h={h} session={session} />}
      {err && <div className="composer-status error">{err}</div>}
      <div
        className={cx("composer-box", dragging && "dragging")}
        onDragOver={(e) => {
          if ([...e.dataTransfer.items].some((i) => i.kind === "file")) {
            e.preventDefault();
            setDragging(true);
          }
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node)) setDragging(false);
        }}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          void addImages(e.dataTransfer.files);
        }}
      >
        {picker && <PickerList picker={picker} sel={sel} onHover={setSel} onChoose={choose} />}
        {(files.length > 0 || images.length > 0) && (
          <div className="attach-row">
            {images.map((i) => (
              <div key={i.id} className="thumb" title={`${i.name} · ${Math.round(i.size / 1024)} KB`}>
                <img src={i.url} alt={i.name} />
                <button className="thumb-x" aria-label="Remove image" onClick={() => removeImage(i.id)}>
                  <IconX size={11} />
                </button>
              </div>
            ))}
            {files.map((f) => (
              <span key={f} className="fchip" title={f}>
                <IconFile size={12} />
                <span className="mono">{basename(f)}</span>
                <button aria-label={`Remove ${f}`} onClick={() => setFiles((cur) => cur.filter((x) => x !== f))}>
                  <IconX size={11} />
                </button>
              </span>
            ))}
          </div>
        )}
        <div className="composer-row">
          <textarea
            ref={ta}
            rows={1}
            value={text}
            placeholder={placeholder}
            onChange={(e) => {
              setText(e.target.value);
              if (hist !== null && e.target.value !== history[hist]) setHist(null);
              updatePicker(e.target.value, e.target.selectionStart);
            }}
            onSelect={(e) => {
              if (picker?.kind === "mention") updatePicker(e.currentTarget.value, e.currentTarget.selectionStart);
            }}
            onBlur={() => setTimeout(() => setPicker(null), 150)}
            onKeyDown={onKeyDown}
            onPaste={(e) => {
              const imgs = [...e.clipboardData.files].filter((f) => f.type.startsWith("image/"));
              if (imgs.length) {
                e.preventDefault();
                void addImages(imgs);
              }
            }}
            aria-label="Prompt"
          />
          <div className="composer-actions">
            <input
              ref={fileInput}
              type="file"
              accept="image/*"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) void addImages(e.target.files);
                e.target.value = "";
              }}
            />
            <button className="icon-btn" title="Mention a file (@)" aria-label="Mention a file" onClick={() => insertAt()}>
              <IconAt size={16} />
            </button>
            <button className="icon-btn" title="Attach images (or paste / drop)" aria-label="Attach images" onClick={() => fileInput.current?.click()}>
              <IconImage size={16} />
            </button>
            {busy && (
              <button className="btn btn-ghost btn-sm" onClick={cancel} disabled={cancelling} title="Cancel the running turn and clear the queue">
                <IconX size={13} />
                <span className="hide-narrow">Cancel</span>
              </button>
            )}
            <button className="btn btn-primary btn-icon" onClick={send} disabled={!canSend} aria-label="Send" title={busy ? "Queue (Enter)" : "Send (Enter)"}>
              {sending ? <span className="spinner" /> : <IconArrowUp size={16} />}
            </button>
          </div>
        </div>
      </div>
      <ConfigBar h={h} session={session} />
    </div>
  );

  function insertAt() {
    const el = ta.current;
    const caret = el?.selectionStart ?? text.length;
    const pre = text.slice(0, caret);
    const ins = pre && !/\s$/.test(pre) ? " @" : "@";
    const v = pre + ins + text.slice(caret);
    setText(v);
    const pos = caret + ins.length;
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(pos, pos);
      updatePicker(v, pos);
    });
  }
}

function PickerList({
  picker,
  sel,
  onHover,
  onChoose,
}: {
  picker: Picker;
  sel: number;
  onHover: (i: number) => void;
  onChoose: (i: number) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector(".pick.active")?.scrollIntoView({ block: "nearest" });
  }, [sel]);
  const empty = picker.items.length === 0;
  return (
    <div className="picker" ref={ref} role="listbox" onMouseDown={(e) => e.preventDefault()}>
      <div className="picker-head">{picker.kind === "slash" ? "Commands" : picker.query ? `Files matching “${picker.query}”` : "Files"}</div>
      {picker.kind === "slash"
        ? picker.items.map((c, i) => (
            <button key={c.name} role="option" aria-selected={i === sel} className={cx("pick", i === sel && "active")} onMouseEnter={() => onHover(i)} onClick={() => onChoose(i)}>
              <span className="mono pick-name">/{c.name}</span>
              {c.hint && <span className="mono pick-hint">{c.hint}</span>}
              <span className="pick-desc">{c.description}</span>
            </button>
          ))
        : picker.items.map((p, i) => {
            const slash = p.lastIndexOf("/");
            return (
              <button key={p} role="option" aria-selected={i === sel} className={cx("pick", i === sel && "active")} onMouseEnter={() => onHover(i)} onClick={() => onChoose(i)}>
                <IconFile size={13} />
                <span className="mono pick-name">{p.slice(slash + 1)}</span>
                <span className="mono pick-desc">{slash > 0 ? p.slice(0, slash) : ""}</span>
              </button>
            );
          })}
      {empty && (
        <div className="pick-empty">
          {picker.kind === "mention" && picker.loading ? "Searching…" : picker.kind === "slash" ? "No matching commands" : "No matching files"}
        </div>
      )}
    </div>
  );
}

// ------------------------------------------------------------------ queue

function QueuePanel({ h, session }: { h: HostState; session: Session }) {
  return (
    <div className="queue">
      <div className="queue-head">
        <span>Queued</span>
        <span className="badge">{session.queue.length}</span>
        <span className="dim">sent in order when the current turn ends</span>
      </div>
      {session.queue.map((q, i) => (
        <QueueRow key={q.id} h={h} sid={session.id} q={q} index={i} />
      ))}
    </div>
  );
}

function QueueRow({ h, sid, q, index }: { h: HostState; sid: string; q: QueueItem; index: number }) {
  const [editing, setEditing] = useState(false);
  const [val, setVal] = useState(q.text);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    if (!editing) setVal(q.text);
  }, [q.text, editing]);

  const run = async (what: string, fn: () => Promise<unknown>) => {
    setBusy(what);
    setErr(null);
    try {
      await fn();
      return true;
    } catch (e) {
      setErr((e as Error).message);
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="qrow">
      <span className="qnum">{index + 1}</span>
      {editing ? (
        <div className="qedit">
          <textarea
            value={val}
            autoFocus
            rows={Math.min(6, Math.max(2, val.split("\n").length))}
            onChange={(e) => setVal(e.target.value)}
            onKeyDown={async (e) => {
              if (e.key === "Escape") setEditing(false);
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey || !e.shiftKey) && !isTouch()) {
                e.preventDefault();
                if (await run("save", () => h.api.queueEdit(sid, q.id, val))) setEditing(false);
              }
            }}
          />
          <div className="qedit-actions">
            <button className="btn btn-ghost btn-sm" onClick={() => setEditing(false)}>
              Cancel
            </button>
            <button
              className="btn btn-primary btn-sm"
              disabled={!val.trim() || busy !== null}
              onClick={async () => {
                if (await run("save", () => h.api.queueEdit(sid, q.id, val))) setEditing(false);
              }}
            >
              {busy === "save" ? <span className="spinner" /> : <IconCheck size={12} />}
              Save
            </button>
          </div>
        </div>
      ) : (
        <button className="qtext" onClick={() => setEditing(true)} title="Edit">
          {q.text || <span className="dim">(attachments only)</span>}
          {q.attachments?.length > 0 && <span className="tag">{q.attachments.length} attachment{q.attachments.length === 1 ? "" : "s"}</span>}
        </button>
      )}
      {!editing && (
        <div className="qactions">
          <button className="icon-btn tiny" title="Edit" aria-label="Edit queued prompt" onClick={() => setEditing(true)}>
            <IconEdit size={12} />
          </button>
          <button
            className="icon-btn tiny"
            title="Send now (interrupts the running turn)"
            aria-label="Send now"
            disabled={busy !== null}
            onClick={() => run("now", () => h.api.queueSendNow(sid, q.id))}
          >
            {busy === "now" ? <span className="spinner" /> : <IconZap size={12} />}
          </button>
          <button className="icon-btn tiny" title="Remove" aria-label="Remove queued prompt" disabled={busy !== null} onClick={() => run("del", () => h.api.queueDelete(sid, q.id))}>
            {busy === "del" ? <span className="spinner" /> : <IconTrash size={12} />}
          </button>
        </div>
      )}
      {err && <div className="qerr">{err}</div>}
    </div>
  );
}

// ------------------------------------------------------------------ config bar

function optLabel(o: ConfigOption) {
  const cur = o.options?.find((x) => x.value === o.currentValue);
  return cur?.name ?? String(o.currentValue ?? "—");
}

function ConfigBar({ h, session }: { h: HostState; session: Session }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const opts = session.config_options ?? [];
  const hasModeOpt = opts.some((o) => o.category === "mode");
  // Agents without config options can still expose ACP modes: fall back to POST /mode.
  const modeFallback = !hasModeOpt && session.modes?.length > 0;
  if (!opts.length && !modeFallback) return null;

  const ordered = [...opts].sort((a, b) => rank(a) - rank(b));

  const set = async (id: string, fn: () => Promise<unknown>) => {
    setBusy(id);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="configbar">
      {modeFallback && (
        <label className={cx("cfg", busy === "__mode" && "busy")} title="Mode">
          <span className="cfg-label">Mode</span>
          <select value={session.mode ?? ""} disabled={busy !== null} onChange={(e) => set("__mode", () => h.api.setMode(session.id, e.target.value))}>
            {!session.mode && <option value="">—</option>}
            {session.modes.map((m) => (
              <option key={m.id} value={m.id} title={m.description}>
                {m.name}
              </option>
            ))}
          </select>
        </label>
      )}
      {ordered.map((o) =>
        o.type === "select" && o.options?.length ? (
          <label key={o.id} className={cx("cfg", busy === o.id && "busy", o.category === "mode" && "cfg-mode")} title={o.description ?? o.name}>
            <span className="cfg-label">{o.name}</span>
            <select
              value={JSON.stringify(o.currentValue)}
              disabled={busy !== null}
              onChange={(e) => {
                const value = JSON.parse(e.target.value);
                void set(o.id, () => h.api.setConfig(session.id, o.id, value));
              }}
            >
              {!o.options.some((x) => x.value === o.currentValue) && <option value={JSON.stringify(o.currentValue)}>{String(o.currentValue)}</option>}
              {o.options.map((x) => (
                <option key={JSON.stringify(x.value)} value={JSON.stringify(x.value)} title={x.description}>
                  {x.name}
                </option>
              ))}
            </select>
          </label>
        ) : (
          <span key={o.id} className="cfg readonly" title={o.description ?? o.name}>
            <span className="cfg-label">{o.name}</span>
            <span>{optLabel(o)}</span>
          </span>
        ),
      )}
      {err && <span className="cfg-err">{err}</span>}
    </div>
  );
}

const RANK: Record<string, number> = { mode: 0, model: 1, thought_level: 2 };
function rank(o: ConfigOption) {
  return RANK[o.category ?? ""] ?? 5;
}
