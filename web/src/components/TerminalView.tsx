// Integrated terminal: xterm.js on the daemon's persistent per-session shell (lazy-loaded chunk).
import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import type { HostState } from "../store";
import type { Session } from "../types";
import { cx, isTouch } from "../util";
import { IconRefresh, IconTrash } from "./Icons";

type Conn = "connecting" | "open" | "closed" | "exited";

function cssVar(name: string, fallback: string) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function themeFromTokens(): ITheme {
  const dark = matchMedia("(prefers-color-scheme: dark)").matches;
  return {
    background: cssVar("--code-bg", dark ? "#111215" : "#f6f7f9"),
    foreground: cssVar("--text", dark ? "#e4e5e8" : "#1b1c1f"),
    cursor: cssVar("--accent", "#8090ff"),
    cursorAccent: cssVar("--code-bg", "#111215"),
    selectionBackground: dark ? "rgba(128,144,255,0.35)" : "rgba(88,101,242,0.25)",
    black: dark ? "#1c1f24" : "#3b3f46",
    red: cssVar("--red", "#f2716b"),
    green: cssVar("--green", "#3ecf8e"),
    yellow: cssVar("--amber", "#f0b44c"),
    blue: cssVar("--blue", "#5eb0ef"),
    magenta: dark ? "#c792ea" : "#9f4fd1",
    cyan: dark ? "#5ed3e0" : "#0f8f9c",
    white: dark ? "#c9ccd2" : "#6b6f78",
    brightBlack: cssVar("--text-3", "#6b6f78"),
    brightRed: dark ? "#ff8f88" : "#c5352e",
    brightGreen: dark ? "#6ee7b7" : "#0f8a5a",
    brightYellow: dark ? "#ffd27a" : "#a66a08",
    brightBlue: dark ? "#8cc8ff" : "#1a6fb8",
    brightMagenta: dark ? "#e0aaff" : "#8a3fbf",
    brightCyan: dark ? "#8ce9f2" : "#0b7a86",
    brightWhite: dark ? "#ffffff" : "#1b1c1f",
  };
}

const KEYS: { label: string; data: string; title?: string }[] = [
  { label: "esc", data: "\x1b" },
  { label: "tab", data: "\t" },
  { label: "^C", data: "\x03", title: "Ctrl-C" },
  { label: "^D", data: "\x04", title: "Ctrl-D" },
  { label: "^L", data: "\x0c", title: "Ctrl-L (clear)" },
  { label: "↑", data: "\x1b[A" },
  { label: "↓", data: "\x1b[B" },
  { label: "←", data: "\x1b[D" },
  { label: "→", data: "\x1b[C" },
  { label: "|", data: "|" },
  { label: "~", data: "~" },
  { label: "/", data: "/" },
  { label: "-", data: "-" },
];

export default function TerminalView({ h, session }: { h: HostState; session: Session }) {
  const host = useRef<HTMLDivElement>(null);
  const term = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const ws = useRef<WebSocket | null>(null);
  const ctrl = useRef(false);
  const retry = useRef(0);
  const [conn, setConn] = useState<Conn>("connecting");
  const [ctrlOn, setCtrlOn] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const touch = isTouch();

  const send = useCallback((msg: object) => {
    const s = ws.current;
    if (s && s.readyState === WebSocket.OPEN) s.send(JSON.stringify(msg));
  }, []);

  const input = useCallback(
    (data: string) => {
      if (ctrl.current && data.length === 1 && /[a-z@[\\\]^_ ]/i.test(data)) {
        data = String.fromCharCode(data.toUpperCase().charCodeAt(0) & 31);
        ctrl.current = false;
        setCtrlOn(false);
      }
      send({ type: "input", data });
    },
    [send],
  );

  const connect = useCallback(() => {
    const t = term.current;
    if (!t) return;
    ws.current?.close();
    setConn("connecting");
    setErr(null);
    const s = new WebSocket(h.api.terminalUrl(session.id));
    s.binaryType = "arraybuffer";
    ws.current = s;
    let first = true;
    let exited = false;
    s.onopen = () => {
      retry.current = 0;
      setConn("open");
      fit.current?.fit();
      send({ type: "resize", cols: t.cols, rows: t.rows });
    };
    s.onmessage = (m) => {
      if (typeof m.data === "string") {
        try {
          const msg = JSON.parse(m.data);
          if (msg.type === "exit") {
            exited = true;
            setConn("exited");
          }
        } catch {
          t.write(m.data);
        }
        return;
      }
      const bytes = new Uint8Array(m.data as ArrayBuffer);
      if (first) {
        // First binary frame is the scrollback: start from a clean screen so reconnects don't duplicate.
        first = false;
        t.reset();
      }
      t.write(bytes);
    };
    s.onclose = (ev) => {
      if (ws.current !== s) return;
      ws.current = null;
      if (exited) return;
      setConn("closed");
      if (ev.code === 1008 || ev.code === 4401) {
        setErr("Unauthorized");
        return;
      }
      // Transient drop (sleep, network): retry a few times with backoff.
      if (retry.current < 5) {
        const delay = 500 * 2 ** retry.current++;
        setTimeout(() => {
          if (!ws.current && term.current) connect();
        }, delay);
      }
    };
  }, [h, session.id, send]);

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const t = new Terminal({
      fontFamily: cssVar("--mono", "ui-monospace, monospace"),
      fontSize: touch ? 12 : 13,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 10000,
      theme: themeFromTokens(),
      allowProposedApi: false,
      macOptionIsMeta: true,
    });
    const f = new FitAddon();
    t.loadAddon(f);
    t.open(el);
    term.current = t;
    fit.current = f;
    requestAnimationFrame(() => f.fit());
    const d1 = t.onData(input);
    const d2 = t.onResize(({ cols, rows }) => send({ type: "resize", cols, rows }));
    const ro = new ResizeObserver(() => {
      try {
        f.fit();
      } catch {
        /* element hidden */
      }
    });
    ro.observe(el);
    const mq = matchMedia("(prefers-color-scheme: dark)");
    const onScheme = () => (t.options.theme = themeFromTokens());
    mq.addEventListener("change", onScheme);
    connect();
    if (!touch) t.focus();
    return () => {
      mq.removeEventListener("change", onScheme);
      ro.disconnect();
      d1.dispose();
      d2.dispose();
      const s = ws.current;
      ws.current = null;
      s?.close();
      t.dispose();
      term.current = null;
    };
  }, [connect, input, send, touch]);

  const kill = async () => {
    if (!confirm("Kill the shell? Running processes in it will be terminated.")) return;
    try {
      await h.api.killTerminal(session.id);
    } catch (e) {
      setErr((e as Error).message);
    }
  };

  return (
    <div className="termview">
      <div className="term-bar">
        <span className={cx("cdot", conn === "open" ? "ok" : conn === "connecting" ? "warn" : "bad")} />
        <span className="dim mono term-cwd" title={session.cwd}>
          {conn === "open" ? session.cwd : conn === "connecting" ? "connecting…" : conn === "exited" ? "shell exited" : "disconnected"}
        </span>
        <span className="spacer" />
        {(conn === "closed" || conn === "exited") && (
          <button
            className="btn btn-sm"
            onClick={() => {
              retry.current = 0;
              connect();
            }}
          >
            <IconRefresh size={12} /> Reconnect
          </button>
        )}
        <button className="btn btn-ghost btn-sm" onClick={kill} disabled={conn === "exited"} title="Kill the shell (a new one starts on reconnect)">
          <IconTrash size={12} /> <span className="hide-narrow">Kill shell</span>
        </button>
      </div>
      <div className="term-wrap">
        <div className="term-host" ref={host} onClick={() => term.current?.focus()} />
        {conn === "exited" && (
          <div className="term-overlay">
            <div>Shell exited</div>
            <button
              className="btn btn-primary btn-sm"
              onClick={() => {
                retry.current = 0;
                connect();
              }}
            >
              Reconnect
            </button>
          </div>
        )}
        {err && <div className="term-err">{err}</div>}
      </div>
      {touch && (
        <div className="term-keys" onMouseDown={(e) => e.preventDefault()}>
          <button
            className={cx("tkey", ctrlOn && "on")}
            onClick={() => {
              ctrl.current = !ctrl.current;
              setCtrlOn(ctrl.current);
              term.current?.focus();
            }}
          >
            ctrl
          </button>
          {KEYS.map((k) => (
            <button
              key={k.label}
              className="tkey"
              title={k.title}
              onClick={() => {
                send({ type: "input", data: k.data });
                term.current?.focus();
              }}
            >
              {k.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
