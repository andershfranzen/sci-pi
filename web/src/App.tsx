import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { boot, currentHost, getHost, selectHost, sortedSessions, useStore } from "./store";
import { navigate, sessionHash, useRoute, type SessionTab } from "./router";
import { isMac } from "./util";
import { Sidebar } from "./components/Sidebar";
import { SessionView } from "./components/SessionView";
import { Inbox } from "./components/Inbox";
import { NewSessionDialog } from "./components/NewSession";
import { TokenScreen } from "./components/TokenScreen";
import { Home } from "./components/Home";
import { Palette, ShortcutsOverlay } from "./components/Palette";
import { IconMenu } from "./components/Icons";

function isTyping(t: EventTarget | null) {
  const el = t as HTMLElement | null;
  return !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT" || el.isContentEditable || !!el.closest(".xterm"));
}

export function App() {
  const store = useStore();
  const route = useRoute();
  const [drawer, setDrawer] = useState(false);
  const [newOpen, setNewOpen] = useState(false);
  const [palette, setPalette] = useState(false);
  const [help, setHelp] = useState(false);
  const routeRef = useRef(route);
  routeRef.current = route;

  useEffect(() => {
    void boot();
  }, []);

  useEffect(() => {
    if (route.name === "session" && getHost(route.host)) selectHost(route.host);
    setDrawer(false);
  }, [route]);

  useEffect(() => {
    let gPending = 0;
    // Capture phase, so these work even while xterm has focus. Exception: on Linux/Windows,
    // Ctrl+K inside the terminal stays readline's kill-line (use the sidebar search button).
    const onCapture = (e: KeyboardEvent) => {
      const inTerm = !!(e.target as HTMLElement | null)?.closest?.(".xterm");
      if ((isMac ? e.metaKey : e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k" && !(inTerm && !isMac)) {
        e.preventDefault();
        e.stopPropagation();
        setPalette((p) => !p);
        return;
      }
      const r = routeRef.current;
      if (e.altKey && !e.metaKey && !e.ctrlKey && r.name === "session") {
        const tabs: Record<string, SessionTab> = { Digit1: "chat", Digit2: "diff", Digit3: "terminal" };
        if (tabs[e.code]) {
          e.preventDefault();
          e.stopPropagation();
          navigate(sessionHash(r.host, r.id, tabs[e.code]));
        }
      }
    };
    window.addEventListener("keydown", onCapture, true);
    const on = (e: KeyboardEvent) => {
      const r = routeRef.current;
      if (e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
        const h = currentHost();
        if (!h) return;
        const list = sortedSessions(h);
        const i = r.name === "session" ? list.findIndex((s) => s.id === r.id) : -1;
        const next = list[e.key === "ArrowUp" ? Math.max(0, i - 1) : Math.min(list.length - 1, i + 1)];
        if (next) {
          e.preventDefault();
          navigate(sessionHash(h.key, next.id));
        }
        return;
      }
      if (isTyping(e.target) || e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.querySelector(".modal-backdrop")) return;
      if (e.key === "?") {
        e.preventDefault();
        setHelp(true);
      } else if (e.key === "n" && currentHost()?.info) {
        e.preventDefault();
        setNewOpen(true);
      } else if (e.key === "g") {
        gPending = Date.now();
      } else if (e.key === "i" && Date.now() - gPending < 1000) {
        navigate("#/inbox");
      }
    };
    window.addEventListener("keydown", on);
    return () => {
      window.removeEventListener("keydown", on);
      window.removeEventListener("keydown", onCapture, true);
    };
  }, []);

  const closePalette = useCallback(() => setPalette(false), []);
  const openNew = useCallback(() => setNewOpen(true), []);
  const openHelp = useCallback(() => setHelp(true), []);

  if (store.mode === "loading") return <div className="splash" />;
  if (store.mode === "need_token") return <TokenScreen />;

  const openMenu = () => setDrawer(true);
  const host = currentHost();
  let main: ReactNode;
  if (route.name === "inbox") {
    main = <Inbox onMenu={openMenu} />;
  } else if (route.name === "session") {
    const h = getHost(route.host);
    const s = h?.sessions.get(route.id);
    if (h && s) main = <SessionView key={`${h.key}:${s.id}`} h={h} session={s} tab={route.tab} focusEvent={route.focusEvent} onMenu={openMenu} />;
    else
      main = (
        <div className="page">
          <header className="page-header">
            <button className="icon-btn menu-btn" onClick={openMenu} aria-label="Open sidebar">
              <IconMenu />
            </button>
          </header>
          <div className="empty-state">
            <div className="empty-title">{!h || !h.sessionsLoaded ? "Connecting…" : "Session not found"}</div>
            {h?.sessionsLoaded && (
              <a href="#/" className="linkish">
                Back
              </a>
            )}
          </div>
        </div>
      );
  } else {
    main = <Home onMenu={openMenu} onNew={openNew} />;
  }

  return (
    <div className="app">
      <Sidebar
        route={route}
        open={drawer}
        onClose={() => setDrawer(false)}
        onNew={() => {
          setDrawer(false);
          setNewOpen(true);
        }}
        onPalette={() => {
          setDrawer(false);
          setPalette(true);
        }}
        onHelp={() => {
          setDrawer(false);
          setHelp(true);
        }}
      />
      <main className="main">{main}</main>
      {newOpen && host && <NewSessionDialog initialHost={host} onClose={() => setNewOpen(false)} />}
      {palette && <Palette route={route} onClose={closePalette} onNew={openNew} onHelp={openHelp} />}
      {help && <ShortcutsOverlay onClose={() => setHelp(false)} />}
    </div>
  );
}
