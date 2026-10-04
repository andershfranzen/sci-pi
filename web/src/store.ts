// Global client state: hosts, their sessions and event logs, live WebSocket connections.
// A single mutable store + version counter, exposed to React via useSyncExternalStore.
import { useSyncExternalStore } from "react";
import { Api, ApiError, fetchHubHosts, hubConnect } from "./api";
import { TimelineBuilder, type TItem } from "./timeline";
import type { HubHost, InboxItem, Info, OEvent, Session, WsMessage } from "./types";
import { maybeNotify } from "./notify";

export type ConnStatus = "connecting" | "open" | "reconnecting" | "auth";

export interface SessionLog {
  events: OEvent[];
  ids: Set<number>;
  rev: number;
  loaded: boolean;
  loading: boolean;
  error: string | null;
  builder: TimelineBuilder | null;
  builderRev: number;
  needRebuild: boolean;
}

export interface HostState {
  key: string;
  name: string;
  api: Api;
  hub: { status: HubHost["status"]; error?: string; transport: HubHost["transport"]; discovered: boolean } | null;
  conn: ConnStatus;
  connError: string | null;
  info: Info | null;
  sessions: Map<string, Session>;
  sessionsLoaded: boolean;
  logs: Map<string, SessionLog>;
  inbox: InboxItem[];
  inboxLoaded: boolean;
  /** highest event id received over the WebSocket (the `after` cursor) */
  wsCursor: number | null;
}

export type AppMode = "loading" | "hub" | "direct" | "need_token";

const TOKEN_KEY = "outpost.token";
const HOST_KEY = "outpost.selectedHost";
const NOTIFY_KEY = "outpost.notify";

function lsGet(k: string): string | null {
  try {
    return localStorage.getItem(k);
  } catch {
    return null;
  }
}
function lsSet(k: string, v: string | null) {
  try {
    if (v === null) localStorage.removeItem(k);
    else localStorage.setItem(k, v);
  } catch {
    /* ignore */
  }
}

export const store = {
  mode: "loading" as AppMode,
  tokenError: null as string | null,
  hosts: [] as HostState[],
  selectedHost: lsGet(HOST_KEY),
  notify: lsGet(NOTIFY_KEY) === "1",
  version: 0,
};

// ------------------------------------------------------------------ subscription

const listeners = new Set<() => void>();
let scheduled = false;

export function emit() {
  if (scheduled) return;
  scheduled = true;
  const flush = () => {
    if (!scheduled) return;
    scheduled = false;
    store.version++;
    listeners.forEach((l) => l());
  };
  // rAF coalesces bursts (WS replay, streaming chunks); the timeout covers hidden tabs.
  requestAnimationFrame(flush);
  setTimeout(flush, 100);
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => listeners.delete(l);
}

export function useStore() {
  useSyncExternalStore(subscribe, () => store.version);
  return store;
}

// ------------------------------------------------------------------ selectors

export function getHost(key: string | null | undefined): HostState | undefined {
  if (!key) return undefined;
  return store.hosts.find((h) => h.key === key);
}

export function currentHost(): HostState | undefined {
  return getHost(store.selectedHost) ?? store.hosts[0];
}

export function selectHost(key: string) {
  if (store.selectedHost === key) return;
  store.selectedHost = key;
  lsSet(HOST_KEY, key);
  emit();
}

/** Active sessions: pinned first, then most recently updated. */
export function sortedSessions(h: HostState): Session[] {
  return [...h.sessions.values()]
    .filter((s) => !s.archived)
    .sort((a, b) => Number(!!b.pinned) - Number(!!a.pinned) || b.updated_at - a.updated_at);
}

export function archivedSessions(h: HostState): Session[] {
  return [...h.sessions.values()].filter((s) => s.archived).sort((a, b) => b.updated_at - a.updated_at);
}

/** PATCH a session (title / pinned / archived) and apply the returned row immediately. */
export async function patchSession(h: HostState, id: string, patch: { title?: string; pinned?: boolean; archived?: boolean }) {
  const cur = h.sessions.get(id);
  if (cur) {
    h.sessions.set(id, { ...cur, ...patch });
    emit();
  }
  try {
    const s = await h.api.patchSession(id, patch);
    if (s && s.id) h.sessions.set(id, s);
  } catch (e) {
    if (cur) h.sessions.set(id, cur);
    throw e;
  } finally {
    emit();
  }
}

export function totalPending(): number {
  let n = 0;
  for (const h of store.hosts) for (const s of h.sessions.values()) n += s.pending_permissions || 0;
  return n;
}

function newLog(): SessionLog {
  return {
    events: [],
    ids: new Set(),
    rev: 0,
    loaded: false,
    loading: false,
    error: null,
    builder: null,
    builderRev: -1,
    needRebuild: false,
  };
}

function getLog(h: HostState, sid: string): SessionLog {
  let log = h.logs.get(sid);
  if (!log) {
    log = newLog();
    h.logs.set(sid, log);
  }
  return log;
}

export function peekLog(h: HostState, sid: string): SessionLog | undefined {
  return h.logs.get(sid);
}

/** Timeline items for a session, rebuilt incrementally as events arrive. */
export function getTimeline(h: HostState, sid: string): TItem[] {
  const log = getLog(h, sid);
  if (log.builder && log.builderRev === log.rev) return log.builder.items;
  if (!log.builder || log.needRebuild) {
    log.builder = new TimelineBuilder();
    log.builder.add(log.events);
    log.needRebuild = false;
  } else {
    const last = log.builder.lastId;
    let i = log.events.length;
    while (i > 0 && log.events[i - 1].id > last) i--;
    log.builder.add(log.events.slice(i));
  }
  log.builderRev = log.rev;
  return log.builder.items;
}

function insertEvents(log: SessionLog, events: OEvent[]) {
  let changed = false;
  for (const e of events) {
    if (log.ids.has(e.id)) continue;
    log.ids.add(e.id);
    const tail = log.events[log.events.length - 1];
    if (!tail || e.id > tail.id) {
      log.events.push(e);
    } else {
      // out of order (history merge): insert and force a full timeline rebuild
      let i = log.events.length;
      while (i > 0 && log.events[i - 1].id > e.id) i--;
      log.events.splice(i, 0, e);
      log.needRebuild = true;
    }
    changed = true;
  }
  if (changed) log.rev++;
  return changed;
}

/** Lazily fetch a session's full history and merge it with any live events. */
export async function loadHistory(h: HostState, sid: string, force = false) {
  const log = getLog(h, sid);
  if (log.loading || (log.loaded && !force)) return;
  log.loading = true;
  log.error = null;
  emit();
  try {
    const events = await h.api.events(sid, 0);
    insertEvents(log, events);
    log.loaded = true;
  } catch (e) {
    log.error = (e as Error).message;
  } finally {
    log.loading = false;
    emit();
  }
}

// ------------------------------------------------------------------ inbox

const inboxTimers = new Map<string, ReturnType<typeof setTimeout>>();

export function refreshInbox(h: HostState, delay = 200) {
  clearTimeout(inboxTimers.get(h.key));
  inboxTimers.set(
    h.key,
    setTimeout(async () => {
      try {
        h.inbox = await h.api.inbox();
        h.inboxLoaded = true;
        emit();
      } catch {
        /* connection loop will surface errors */
      }
    }, delay),
  );
}

export function refreshAllInboxes() {
  for (const h of store.hosts) refreshInbox(h, 0);
}

/** Optimistically drop an inbox item after answering it. */
export function dropInboxItem(h: HostState, requestId: string) {
  h.inbox = h.inbox.filter((i) => i.request_id !== requestId);
  emit();
}

// ------------------------------------------------------------------ live connection

function handleMessage(h: HostState, msg: WsMessage) {
  switch (msg.type) {
    case "event": {
      const e = msg.event;
      if (h.wsCursor === null || e.id > h.wsCursor) h.wsCursor = e.id;
      const log = getLog(h, e.session_id);
      if (insertEvents(log, [e])) {
        if (e.kind === "permission_request" || e.kind === "permission_resolved") refreshInbox(h);
        if (e.kind === "permission_request" || e.kind === "turn_end") {
          maybeNotify(h, h.sessions.get(e.session_id), e);
        }
        emit();
      }
      return;
    }
    case "session": {
      const prev = h.sessions.get(msg.session.id);
      h.sessions.set(msg.session.id, msg.session);
      if (!prev || prev.pending_permissions !== msg.session.pending_permissions) refreshInbox(h);
      emit();
      return;
    }
    case "session_deleted": {
      h.sessions.delete(msg.id);
      h.logs.delete(msg.id);
      h.inbox = h.inbox.filter((i) => i.session_id !== msg.id);
      emit();
      return;
    }
    case "ping":
      return;
  }
}

class HostConn {
  private ws: WebSocket | null = null;
  private attempt = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private gen = 0;
  private stopped = false;
  private h: HostState;
  private lastMsg = 0;
  private watchdog: ReturnType<typeof setInterval> | undefined;

  constructor(h: HostState) {
    this.h = h;
    // The daemon pings every 25s; a silent socket for 60s is dead (e.g. after laptop sleep).
    this.watchdog = setInterval(() => {
      if (this.h.conn === "open" && this.lastMsg && Date.now() - this.lastMsg > 60_000) this.kick(true);
    }, 10_000);
  }

  start() {
    void this.connect();
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    clearInterval(this.watchdog);
    this.gen++;
    const ws = this.ws;
    this.ws = null;
    ws?.close();
  }

  /** Reconnect now unless a healthy connection is up (used on wake / online / hub reconnect). */
  kick(force = false) {
    if (this.stopped) return;
    if (!force && (this.h.conn === "open" || this.h.conn === "connecting")) return;
    this.attempt = 0;
    const ws = this.ws;
    this.ws = null;
    ws?.close();
    void this.connect();
  }

  private schedule() {
    if (this.stopped) return;
    clearTimeout(this.timer);
    const base = Math.min(30_000, 500 * 2 ** this.attempt);
    const delay = base * (0.8 + Math.random() * 0.4);
    this.attempt++;
    this.timer = setTimeout(() => void this.connect(), delay);
  }

  private async connect() {
    clearTimeout(this.timer);
    if (this.stopped) return;
    const h = this.h;
    const gen = ++this.gen;
    h.conn = this.attempt === 0 && !h.info ? "connecting" : "reconnecting";
    emit();
    try {
      const info = await h.api.info();
      if (gen !== this.gen) return;
      if (h.wsCursor !== null && info.last_event_id < h.wsCursor) {
        // The daemon's log was reset (fresh database): drop cached state.
        h.logs.clear();
        h.wsCursor = null;
      }
      h.info = info;
      if (h.wsCursor === null) h.wsCursor = info.last_event_id;

      const sessions = await h.api.sessions();
      if (gen !== this.gen) return;
      h.sessions = new Map(sessions.map((s) => [s.id, s]));
      h.sessionsLoaded = true;
      for (const id of [...h.logs.keys()]) if (!h.sessions.has(id)) h.logs.delete(id);
      refreshInbox(h, 0);
      emit();
      this.openWs(gen);
    } catch (e) {
      if (gen !== this.gen) return;
      if (e instanceof ApiError && e.isAuth) {
        h.conn = "auth";
        h.connError = "token rejected";
        emit();
        onAuthError(h);
        return;
      }
      h.conn = "reconnecting";
      h.connError = (e as Error).message;
      emit();
      this.schedule();
    }
  }

  private openWs(gen: number) {
    const h = this.h;
    const ws = new WebSocket(h.api.wsUrl(h.wsCursor ?? 0));
    this.ws = ws;
    ws.onopen = () => {
      if (gen !== this.gen) return;
      this.attempt = 0;
      this.lastMsg = Date.now();
      h.conn = "open";
      h.connError = null;
      emit();
    };
    ws.onmessage = (m) => {
      if (gen !== this.gen) return;
      this.lastMsg = Date.now();
      try {
        handleMessage(h, JSON.parse(m.data as string) as WsMessage);
      } catch (err) {
        console.warn("bad ws message", err);
      }
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      if (this.stopped || gen !== this.gen) return;
      h.conn = "reconnecting";
      emit();
      this.schedule();
    };
  }
}

const conns = new Map<string, HostConn>();

function addHost(key: string, name: string, url: string, token: string, hub: HostState["hub"]) {
  const h: HostState = {
    key,
    name,
    api: new Api({ url, token }),
    hub,
    conn: "connecting",
    connError: null,
    info: null,
    sessions: new Map(),
    sessionsLoaded: false,
    logs: new Map(),
    inbox: [],
    inboxLoaded: false,
    wsCursor: null,
  };
  store.hosts.push(h);
  const c = new HostConn(h);
  conns.set(key, c);
  c.start();
  return h;
}

function removeHost(key: string) {
  conns.get(key)?.stop();
  conns.delete(key);
  store.hosts = store.hosts.filter((h) => h.key !== key);
}

function onAuthError(h: HostState) {
  if (store.mode === "direct") {
    const hadToken = !!h.api.token;
    removeHost(h.key);
    lsSet(TOKEN_KEY, null);
    store.mode = "need_token";
    store.tokenError = hadToken ? "The daemon rejected that token." : null;
    emit();
  }
}

export function reconnectHost(key: string) {
  const h = getHost(key);
  if (h?.hub && h.hub.status !== "connected") void hubConnect(h.name).then(pollHub);
  conns.get(key)?.kick(true);
}

// ------------------------------------------------------------------ hub mode

function hubMeta(hh: HubHost): NonNullable<HostState["hub"]> {
  return { status: hh.status, error: hh.error, transport: hh.transport ?? "ssh", discovered: !!hh.discovered };
}

function syncHub(list: HubHost[]) {
  const seen = new Set<string>();
  for (const hh of list) {
    seen.add(hh.name);
    const h = getHost(hh.name);
    if (!h) {
      addHost(hh.name, hh.name, hh.url, hh.token ?? "", hubMeta(hh));
      continue;
    }
    const wasConnected = h.hub?.status === "connected";
    h.hub = hubMeta(hh);
    if (h.api.url !== hh.url.replace(/\/+$/, "") || h.api.token !== (hh.token ?? "")) {
      h.api = new Api({ url: hh.url, token: hh.token ?? "" });
      conns.get(h.key)?.kick(true);
    } else if (hh.status === "connected" && !wasConnected) {
      conns.get(h.key)?.kick();
    }
  }
  for (const h of [...store.hosts]) if (!seen.has(h.key)) removeHost(h.key);
  emit();
}

async function pollHub() {
  const list = await fetchHubHosts();
  if (list) syncHub(list);
}

// ------------------------------------------------------------------ boot

function readHashToken(): string | null {
  const m = /(?:^#|&)token=([^&]+)/.exec(location.hash);
  if (!m) return null;
  const token = decodeURIComponent(m[1]);
  history.replaceState(null, "", location.pathname + location.search);
  return token;
}

/** Direct mode: connect to the serving daemon. The token may be empty (tailnet access). */
function startDirect(token: string) {
  for (const h of [...store.hosts]) removeHost(h.key);
  store.mode = "direct";
  addHost("local", location.hostname || "local", location.origin, token, null);
  emit();
}

export function setToken(token: string) {
  token = token.trim();
  if (!token) return;
  lsSet(TOKEN_KEY, token);
  store.tokenError = null;
  startDirect(token);
}

export function forgetToken() {
  for (const h of [...store.hosts]) removeHost(h.key);
  lsSet(TOKEN_KEY, null);
  store.mode = "need_token";
  emit();
}

export function setNotify(on: boolean) {
  store.notify = on;
  lsSet(NOTIFY_KEY, on ? "1" : "0");
  emit();
}

let booted = false;

export async function boot() {
  if (booted) return;
  booted = true;
  const hashToken = readHashToken();
  const hub = await fetchHubHosts();
  if (hub) {
    store.mode = "hub";
    syncHub(hub);
    setInterval(pollHub, 5000);
  } else {
    // Try the hash/stored token, or none at all (tailnet callers need no token);
    // the paste-token screen only appears if /api/info answers 401.
    const token = hashToken ?? lsGet(TOKEN_KEY) ?? "";
    if (hashToken) lsSet(TOKEN_KEY, hashToken);
    startDirect(token);
  }
  emit();

  // Laptop wake / tab return: sockets may be silently dead, so force a resync.
  let hiddenAt = 0;
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      hiddenAt = Date.now();
      return;
    }
    const long = hiddenAt && Date.now() - hiddenAt > 20_000;
    for (const c of conns.values()) c.kick(Boolean(long));
    if (store.mode === "hub") void pollHub();
  });
  window.addEventListener("online", () => {
    for (const c of conns.values()) c.kick(true);
  });
}
