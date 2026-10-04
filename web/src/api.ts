// Thin client over the daemon REST API (docs/PROTOCOL.md). One instance per host.
import type {
  CreateSessionBody,
  AttachmentIn,
  DiffResult,
  FsList,
  GitStatus,
  SearchHit,
  HubHost,
  InboxItem,
  Info,
  OEvent,
  Session,
} from "./types";

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
  get isAuth() {
    return this.status === 401 || this.status === 403;
  }
}

export interface ApiTarget {
  url: string;
  token: string;
}

function trimSlash(u: string) {
  return u.replace(/\/+$/, "");
}

export class Api {
  readonly url: string;
  readonly token: string;

  constructor({ url, token }: ApiTarget) {
    this.url = trimSlash(url);
    this.token = token;
  }

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    let res: Response;
    try {
      res = await fetch(`${this.url}/api${path}`, {
        method,
        headers: {
          // The token is optional: tailnet callers are authenticated by tailscaled whois.
          ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
          ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
        },
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
    } catch (e) {
      throw new ApiError(0, `network error: ${(e as Error).message}`);
    }
    const text = await res.text();
    let json: unknown = undefined;
    if (text) {
      try {
        json = JSON.parse(text);
      } catch {
        if (res.ok) throw new ApiError(res.status, "invalid JSON from server");
      }
    }
    if (!res.ok) {
      const msg =
        json && typeof json === "object" && "error" in json
          ? String((json as { error: unknown }).error)
          : `${res.status} ${res.statusText}`;
      throw new ApiError(res.status, msg);
    }
    return json as T;
  }

  info() {
    return this.req<Info>("GET", "/info");
  }
  sessions() {
    return this.req<Session[]>("GET", "/sessions");
  }
  session(id: string) {
    return this.req<Session>("GET", `/sessions/${enc(id)}`);
  }
  createSession(body: CreateSessionBody) {
    return this.req<Session>("POST", "/sessions", body);
  }
  deleteSession(id: string, removeWorktree: boolean) {
    return this.req<object>("DELETE", `/sessions/${enc(id)}${removeWorktree ? "?remove_worktree=1" : ""}`);
  }
  events(id: string, after = 0) {
    return this.req<OEvent[]>("GET", `/sessions/${enc(id)}/events?after=${after}`);
  }
  patchSession(id: string, patch: { title?: string; pinned?: boolean; archived?: boolean }) {
    return this.req<Session>("PATCH", `/sessions/${enc(id)}`, patch);
  }
  prompt(id: string, text: string, attachments?: AttachmentIn[]) {
    return this.req<{ queued: boolean }>("POST", `/sessions/${enc(id)}/prompt`, {
      text,
      ...(attachments?.length ? { attachments } : {}),
    });
  }
  queueDelete(id: string, qid: string) {
    return this.req<object>("DELETE", `/sessions/${enc(id)}/queue/${enc(qid)}`);
  }
  queueEdit(id: string, qid: string, text: string) {
    return this.req<object>("PATCH", `/sessions/${enc(id)}/queue/${enc(qid)}`, { text });
  }
  queueSendNow(id: string, qid: string) {
    return this.req<object>("POST", `/sessions/${enc(id)}/queue/${enc(qid)}/send_now`);
  }
  setConfig(id: string, config_id: string, value: unknown) {
    return this.req<object>("POST", `/sessions/${enc(id)}/config`, { config_id, value });
  }
  revert(id: string, turn: number) {
    return this.req<object>("POST", `/sessions/${enc(id)}/revert`, { turn });
  }
  files(id: string, q: string) {
    return this.req<string[]>("GET", `/sessions/${enc(id)}/files?q=${encodeURIComponent(q)}`);
  }
  git(id: string) {
    return this.req<GitStatus>("GET", `/sessions/${enc(id)}/git`);
  }
  commit(id: string, message: string) {
    return this.req<{ sha: string }>("POST", `/sessions/${enc(id)}/git/commit`, { message });
  }
  push(id: string) {
    return this.req<{ output: string }>("POST", `/sessions/${enc(id)}/git/push`);
  }
  createPr(id: string, body: { title?: string; body?: string; draft?: boolean }) {
    return this.req<{ url: string }>("POST", `/sessions/${enc(id)}/git/pr`, body);
  }
  fork(id: string, body: { turn?: number; agent?: string }) {
    return this.req<Session>("POST", `/sessions/${enc(id)}/fork`, body);
  }
  search(q: string) {
    return this.req<SearchHit[]>("GET", `/search?q=${encodeURIComponent(q)}`);
  }
  killTerminal(id: string) {
    return this.req<object>("DELETE", `/sessions/${enc(id)}/terminal`);
  }
  cancel(id: string) {
    return this.req<object>("POST", `/sessions/${enc(id)}/cancel`);
  }
  permission(id: string, request_id: string, option_id: string | null) {
    return this.req<object>("POST", `/sessions/${enc(id)}/permission`, { request_id, option_id });
  }
  setMode(id: string, mode: string) {
    return this.req<object>("POST", `/sessions/${enc(id)}/mode`, { mode });
  }
  stop(id: string) {
    return this.req<object>("POST", `/sessions/${enc(id)}/stop`);
  }
  diff(id: string, turn?: number) {
    return this.req<DiffResult>("GET", `/sessions/${enc(id)}/diff${turn ? `?turn=${turn}` : ""}`);
  }
  inbox() {
    return this.req<InboxItem[]>("GET", "/inbox");
  }
  fsList(path?: string) {
    return this.req<FsList>("GET", `/fs/list${path ? `?path=${encodeURIComponent(path)}` : ""}`);
  }

  private tokenParam(first: boolean) {
    if (!this.token) return "";
    return `${first ? "?" : "&"}token=${encodeURIComponent(this.token)}`;
  }

  wsUrl(after: number) {
    const base = this.url.replace(/^http/, "ws");
    return `${base}/api/ws?after=${after}${this.tokenParam(false)}`;
  }

  terminalUrl(id: string) {
    const base = this.url.replace(/^http/, "ws");
    return `${base}/api/sessions/${enc(id)}/terminal${this.tokenParam(true)}`;
  }

  /** URL for an uploaded attachment, usable as an <img src> (token as a query param). */
  attachmentUrl(name: string) {
    return `${this.url}/api/attachments/${enc(name)}${this.tokenParam(true)}`;
  }
}

function enc(s: string) {
  return encodeURIComponent(s);
}

/** Hub detection: returns the host list if this page is served by `sci-pi ui`, else null. */
export async function fetchHubHosts(): Promise<HubHost[] | null> {
  try {
    const res = await fetch(`${location.origin}/hub/hosts`, { headers: { Accept: "application/json" } });
    if (!res.ok) return null;
    const json = await res.json();
    return Array.isArray(json) ? (json as HubHost[]) : null;
  } catch {
    return null;
  }
}

export async function hubConnect(name: string): Promise<void> {
  await fetch(`${location.origin}/hub/hosts/${encodeURIComponent(name)}/connect`, { method: "POST" });
}
