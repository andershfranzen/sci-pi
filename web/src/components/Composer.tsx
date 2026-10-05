import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { HostState } from "../store";
import { getTimeline } from "../store";
import { promptHistory } from "../timeline";
import type { AttachmentIn, ConfigOption, QueueItem, Session, SlashCommand } from "../types";
import { basename, cx, fuzzyScore, isTouch, readFileBase64 } from "../util";
import { IconArrowUp, IconAt, IconCheck, IconEdit, IconFile, IconGauge, IconImage, IconShield, IconSparkle, IconTrash, IconX, IconZap } from "./Icons";
import { Select, type SelectOption } from "./Select";
import { modelChoices, modelLabel, prettyValueName, toggleInfo, toggleLabel } from "../models";
import { useCompositionGuard } from "../keyboard";
import { useComposerDraft, type DraftImage } from "../composerDraft";
import "./Composer.css";

function ImagePreview({ image }: { image: DraftImage }) {
  const [url, setUrl] = useState("");
  useEffect(() => {
    const next = URL.createObjectURL(image.blob);
    setUrl(next);
    return () => URL.revokeObjectURL(next);
  }, [image.blob]);
  return <img src={url || undefined} alt={image.name} />;
}

type Picker =
  | { kind: "slash"; query: string; items: SlashCommand[] }
  | { kind: "mention"; query: string; start: number; items: string[]; loading: boolean };

const MAX_IMAGE = 10 * 1024 * 1024;

export function Composer({ h, session, busy, quote, onQuoteApplied }: { h: HostState; session: Session; busy: boolean; quote: { id: number; text: string } | null; onQuoteApplied: (id: number) => void }) {
  const { text, files, images, unavailableImages, discardUnavailableImages, setText, setFiles, setImages, getSnapshot, beginSend, sending, hydrated, warning, saving } = useComposerDraft(h.key, session.id);
  const guard = useCompositionGuard();
  const appliedQuote = useRef<number | null>(null);
  const unsentText = useRef("");
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
    if (!hydrated || !quote || appliedQuote.current === quote.id) return;
    appliedQuote.current = quote.id;
    const block = quote.text.replace(/\r\n?/g, "\n").split("\n").map(line => `> ${line}`).join("\n");
    setText(current => `${current}${current ? "\n\n" : ""}${block}\n\n`);
    setHist(null);
    onQuoteApplied(quote.id);
    requestAnimationFrame(() => ta.current?.focus());
  }, [hydrated, quote, onQuoteApplied, setText]);

  useEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [text]);


  // ------------------------------------------------------------ pickers

  const updatePicker = (value: string, caret: number) => {
    mentionSeq.current++;
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
          if (seq !== mentionSeq.current) return;
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
      setImages((cur) => [
        ...cur,
        { id: crypto.randomUUID(), mime_type: f.type, blob: f, name: f.name || "pasted image", size: f.size },
      ]);
    }
  };

  const removeImage = (id: string) => setImages(cur => cur.filter(image => image.id !== id));

  // ------------------------------------------------------------ history

  const timeline = getTimeline(h, session.id);
  const history = useMemo(() => promptHistory(timeline), [timeline]);

  const recall = (dir: -1 | 1) => {
    if (!history.length) return false;
    let next: number | null;
    if (hist === null) {
      if (dir === 1) return false;
      next = history.length - 1;
      unsentText.current = text;
    } else {
      next = hist + dir;
      if (next < 0) next = 0;
      if (next >= history.length) next = null;
    }
    setHist(next);
    const v = next === null ? unsentText.current : history[next];
    setText(v);
    requestAnimationFrame(() => ta.current?.setSelectionRange(v.length, v.length));
    return true;
  };

  // ------------------------------------------------------------ send

  const canSend = hydrated && (text.trim() || files.length || images.length) && !sending;

  const send = async () => {
    if (!canSend) return;
    const release = beginSend();
    if (!release) return;
    setErr(null);
    const snapshot = getSnapshot();
    try {
      const attachments: AttachmentIn[] = [
        ...snapshot.files.map(path => ({ type: "file" as const, path })),
        ...await Promise.all(snapshot.images.map(async image => ({ type: "image" as const, mime_type: image.mime_type, data: await readFileBase64(new File([image.blob], image.name, { type: image.mime_type })) }))),
      ];
      await h.api.prompt(session.id, snapshot.text.trim(), attachments);
      if (getSnapshot().textVersion === snapshot.textVersion) {
        setText(current => current === snapshot.text ? "" : current);
        setHist(null);
        setPicker(null);
      }
      setFiles(current => current.filter(path => !snapshot.files.includes(path) || getSnapshot().fileTokens[path] !== snapshot.fileTokens[path]));
      setImages(current => current.filter(image => !snapshot.images.some(sent => sent.id === image.id)));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      release();
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
    if (guard(e.nativeEvent)) return;
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
    if (e.key === "ArrowUp" && !e.shiftKey && (ta.current?.selectionStart === 0 || (hist !== null && text === history[hist]))) {
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
      {warning && <div className="composer-draft-warning" role="status">{warning}</div>}
      {unavailableImages.length > 0 && <div className="composer-draft-warning" role="status">
        {unavailableImages.length} saved image attachment(s) could not be restored and will not be sent.
        <button type="button" className="btn btn-ghost btn-sm" onClick={discardUnavailableImages}>Remove unavailable images</button>
      </div>}
      {saving && <div className="composer-draft-saving" role="status">Saving draft…</div>}
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
                <ImagePreview image={i} />
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
            onBlur={() => setTimeout(() => {
              const element = ta.current;
              if (!element) return;
              const active = element.ownerDocument.activeElement;
              if (active !== element && !active?.closest(".picker")) setPicker(null);
            }, 150)}
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
        <span className="dim">{session.queue_paused ? "paused until you choose how to continue" : "sent in order when the current turn ends"}</span>
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
  const guard = useCompositionGuard();
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
              if (guard(e.nativeEvent)) return;
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

const MODE_TONE: Record<string, "accent" | "warn" | "info" | undefined> = {
  plan: "info",
  acceptEdits: "accent",
  bypassPermissions: "warn",
  auto: "warn",
  yolo: "warn",
  autonomous: "warn",
};

function categoryIcon(o: ConfigOption) {
  switch (o.category) {
    case "mode":
      return <IconShield size={13} />;
    case "model":
      return <IconSparkle size={13} />;
    case "thought_level":
      return <IconGauge size={13} />;
    default:
      return undefined;
  }
}

function choicesFor(o: ConfigOption): SelectOption[] {
  if (o.category === "model") return modelChoices(o);
  return (o.options ?? []).map((x) => ({ value: x.value, label: prettyValueName(x.name), description: x.description }));
}

function ConfigBar({ h, session }: { h: HostState; session: Session }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  // Optimistic values while a change is in flight; dropped when the session row catches up.
  const [pending, setPending] = useState<Record<string, unknown>>({});
  const opts = session.config_options ?? [];
  const sig = JSON.stringify(opts.map((o) => [o.id, o.currentValue]));
  useEffect(() => setPending({}), [sig, session.mode]);

  const hasModeOpt = opts.some((o) => o.category === "mode");
  // Agents without config options can still expose ACP modes: fall back to POST /mode.
  const modeFallback = !hasModeOpt && session.modes?.length > 0;
  if (!opts.length && !modeFallback) return null;

  const ordered = [...opts].sort((a, b) => rank(a) - rank(b));

  const set = async (id: string, value: unknown, fn: () => Promise<unknown>) => {
    setBusy(id);
    setErr(null);
    setPending((p) => ({ ...p, [id]: value }));
    try {
      await fn();
    } catch (e) {
      setErr((e as Error).message);
      setPending((p) => {
        const { [id]: _drop, ...rest } = p;
        void _drop;
        return rest;
      });
    } finally {
      setBusy(null);
    }
  };

  const val = (o: ConfigOption) => (o.id in pending ? pending[o.id] : o.currentValue);

  return (
    <div className="configbar" role="toolbar" aria-label="Agent settings">
      {modeFallback && (
        <Select
          label="Mode"
          icon={<IconShield size={13} />}
          value={"__mode" in pending ? pending.__mode : session.mode}
          options={session.modes.map((m) => ({ value: m.id, label: m.name, description: m.description }))}
          busy={busy === "__mode"}
          tone={MODE_TONE[String(session.mode)]}
          onChange={(v) => set("__mode", v, () => h.api.setMode(session.id, String(v)))}
        />
      )}
      {ordered.map((o) => {
        const tog = toggleInfo(o);
        if (tog) {
          const on = o.id in pending ? pending[o.id] === tog.onValue : tog.on;
          return (
            <button
              key={o.id}
              type="button"
              role="switch"
              aria-checked={on}
              className={cx("cfg-toggle", on && "on", busy === o.id && "busy")}
              title={`${o.name}: ${on ? "on" : "off"}${o.description ? ` — ${o.description}` : ""}`}
              disabled={busy === o.id}
              onClick={() => {
                const next = on ? tog.offValue : tog.onValue;
                void set(o.id, next, () => h.api.setConfig(session.id, o.id, next));
              }}
            >
              {/fast|turbo|speed/i.test(o.id + o.name) ? <IconZap size={12} /> : <span className="cfg-toggle-dot" />}
              <span>{toggleLabel(o)}</span>
            </button>
          );
        }
        if (o.type === "select" && o.options?.length) {
          const v = val(o);
          return (
            <Select
              key={o.id}
              label={o.name}
              icon={categoryIcon(o)}
              value={v}
              display={o.category === "model" ? modelLabel(o, v) : undefined}
              options={choicesFor(o)}
              busy={busy === o.id}
              tone={o.category === "mode" ? MODE_TONE[String(v)] : undefined}
              searchPlaceholder={o.category === "model" ? "Search models…" : undefined}
              onChange={(nv) => set(o.id, nv, () => h.api.setConfig(session.id, o.id, nv))}
            />
          );
        }
        return (
          <span key={o.id} className="sel-chip readonly" title={o.description ?? o.name}>
            <span className="sel-label">{o.name}</span>
            <span className="sel-value">{optLabel(o)}</span>
          </span>
        );
      })}
      {err && (
        <span className="cfg-err" title={err}>
          {err}
        </span>
      )}
    </div>
  );
}

function optLabel(o: ConfigOption) {
  const cur = o.options?.find((x) => x.value === o.currentValue);
  return cur?.name ?? String(o.currentValue ?? "—");
}

const RANK: Record<string, number> = { mode: 0, model: 1, thought_level: 2 };
function rank(o: ConfigOption) {
  const r = RANK[o.category ?? ""] ?? 5;
  return toggleInfo(o) ? r + 10 : r;
}
