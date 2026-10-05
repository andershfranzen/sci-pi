// Reconstructs the conversation timeline from a session's event log.
// The builder is incremental: appended events are folded in without touching unchanged
// items, so React.memo on item components keeps re-renders cheap while streaming.
import type { Attachment, OEvent, PermissionOption, PlanEntry, ToolCall, TurnRecovery } from "./types";

export interface Resolution {
  outcome: "selected" | "cancelled" | string;
  optionId: string | null;
}

export type TItem =
  | { t: "user"; key: string; ts: number; text: string; attachments: Attachment[]; turn: number | null }
  | { t: "msg"; key: string; ts: number; role: "agent" | "thought" | "user"; text: string; messageId?: string }
  | { t: "tool"; key: string; ts: number; call: ToolCall }
  | { t: "plan"; key: string; ts: number; entries: PlanEntry[] }
  | {
      t: "perm";
      key: string;
      ts: number;
      requestId: string;
      toolCall: ToolCall;
      options: PermissionOption[];
      resolved: Resolution | null;
    }
  | { t: "turn"; key: string; ts: number; stopReason: string; turn: number | null; reverted: boolean; recovery: TurnRecovery | null }
  | { t: "forked"; key: string; ts: number; from: string; fromTitle: string; turn: number | null }
  | { t: "sys"; key: string; ts: number; text: string; level: "info" | "error" | "ok"; href?: string };

const CHUNK_ROLE: Record<string, "agent" | "thought" | "user"> = {
  agent_message_chunk: "agent",
  agent_thought_chunk: "thought",
  user_message_chunk: "user",
};

const QUIET_STATUSES = new Set(["running", "idle", "awaiting_permission", "starting"]);

function chunkText(content: unknown): string {
  if (!content || typeof content !== "object") return "";
  const c = content as { type?: string; text?: string; uri?: string; name?: string };
  if (c.type === "text" || typeof c.text === "string") return c.text ?? "";
  if (c.type === "resource_link") return `[${c.name ?? c.uri ?? "resource"}]`;
  if (c.type === "image") return "[image]";
  if (c.type === "audio") return "[audio]";
  return "";
}

export function mergeToolCall(base: ToolCall | undefined, upd: Partial<ToolCall> & { toolCallId: string }): ToolCall {
  const out: ToolCall = { ...(base ?? { toolCallId: upd.toolCallId }) };
  for (const [k, v] of Object.entries(upd)) {
    if (v === undefined || k === "sessionUpdate") continue;
    if (k === "content" && v === null) continue;
    (out as unknown as Record<string, unknown>)[k] = v;
  }
  return out;
}

export class TimelineBuilder {
  items: TItem[] = [];
  /** id of the last event folded in */
  lastId = 0;
  private toolIdx = new Map<string, number>();
  private permIdx = new Map<string, number>();
  private earlyResolutions = new Map<string, Resolution>();
  private planIdx: number | null = null;
  private turnIdx = new Map<number, number>();
  private dirty = false;

  /** Fold events (ascending, all with id > lastId) into the timeline. Returns the items array
   *  (a new array identity iff anything changed). */
  add(events: OEvent[]): TItem[] {
    this.dirty = false;
    const items = this.items.slice();
    for (const e of events) {
      if (e.id <= this.lastId) continue;
      this.lastId = e.id;
      this.fold(items, e);
    }
    if (this.dirty) this.items = items;
    return this.items;
  }

  private set(items: TItem[], i: number, it: TItem) {
    items[i] = it;
    this.dirty = true;
  }
  private push(items: TItem[], it: TItem) {
    items.push(it);
    this.dirty = true;
  }

  private fold(items: TItem[], e: OEvent) {
    const key = String(e.id);
    const d = e.data ?? {};
    switch (e.kind) {
      case "user_prompt":
        this.push(items, {
          t: "user",
          key,
          ts: e.ts,
          text: String(d.text ?? ""),
          attachments: Array.isArray(d.attachments) ? d.attachments : [],
          turn: typeof d.turn === "number" ? d.turn : null,
        });
        this.planIdx = null;
        this.toolIdx.clear();
        if (typeof d.retry_of === "number") {
          this.push(items, { t: "sys", key: `${key}-retry`, ts: e.ts, text: `New attempt of turn ${d.retry_of}. Earlier tool effects remain and may be repeated.`, level: "info" });
        }
        return;
      case "turn_end": {
        const turn = typeof d.turn === "number" ? d.turn : null;
        if (turn !== null) this.turnIdx.set(turn, items.length);
        this.push(items, { t: "turn", key, ts: e.ts, stopReason: String(d.stop_reason ?? "end_turn"), turn, reverted: false, recovery: d.recovery ?? null });
        this.planIdx = null;
        return;
      }
      case "reverted": {
        const turn = Number(d.turn);
        // Reverting to before turn N undoes the file changes of N and every later turn.
        for (const [t, i] of this.turnIdx) {
          const it = items[i];
          if (t >= turn && it.t === "turn" && !it.reverted) this.set(items, i, { ...it, reverted: true });
        }
        this.push(items, { t: "sys", key, ts: e.ts, text: `Files reverted to before turn ${turn}`, level: "info" });
        return;
      }
      case "git": {
        if (d.action === "commit") {
          const sha = String(d.sha ?? "").slice(0, 7);
          const msg = String(d.message ?? "").split("\n")[0];
          this.push(items, { t: "sys", key, ts: e.ts, text: `Committed ${sha}${msg ? ` · ${msg}` : ""}`, level: "ok" });
        } else if (d.action === "push") {
          this.push(items, { t: "sys", key, ts: e.ts, text: "Pushed to remote", level: "ok" });
        } else {
          this.push(items, { t: "sys", key, ts: e.ts, text: `git ${d.action ?? ""}`.trim(), level: "info" });
        }
        return;
      }
      case "forked":
        this.push(items, {
          t: "forked",
          key,
          ts: e.ts,
          from: String(d.from ?? ""),
          fromTitle: String(d.from_title ?? "another session"),
          turn: typeof d.turn === "number" ? d.turn : null,
        });
        return;
      case "pr_created":
        this.push(items, { t: "sys", key, ts: e.ts, text: "Pull request opened", level: "ok", href: String(d.url ?? "") });
        return;
      case "retry_requested":
        this.push(items, { t: "sys", key, ts: e.ts, text: String(d.message ?? "Retry requested. Previous tool effects remain."), level: "info" });
        return;
      case "error":
        this.push(items, { t: "sys", key, ts: e.ts, text: String(d.message ?? "error"), level: "error" });
        return;
      case "status": {
        const st = String(d.status ?? "");
        if (QUIET_STATUSES.has(st) && !d.message) return;
        const label = st.replace(/_/g, " ");
        this.push(items, {
          t: "sys",
          key,
          ts: e.ts,
          text: d.message ? `${label}: ${d.message}` : `Session ${label}`,
          level: st === "error" ? "error" : "info",
        });
        return;
      }
      case "permission_request": {
        const reqId = String(d.request_id);
        const tc: ToolCall = d.tool_call ?? { toolCallId: "" };
        const known = tc.toolCallId ? this.toolIdx.get(tc.toolCallId) : undefined;
        const base = known !== undefined ? (items[known] as Extract<TItem, { t: "tool" }>).call : undefined;
        const toolCall = base ? mergeToolCall(base, tc) : tc;
        this.permIdx.set(reqId, items.length);
        this.push(items, {
          t: "perm",
          key,
          ts: e.ts,
          requestId: reqId,
          toolCall,
          options: Array.isArray(d.options) ? d.options : [],
          resolved: this.earlyResolutions.get(reqId) ?? null,
        });
        return;
      }
      case "permission_resolved": {
        const reqId = String(d.request_id);
        const res: Resolution = { outcome: d.outcome, optionId: d.option_id ?? null };
        const i = this.permIdx.get(reqId);
        if (i === undefined) {
          this.earlyResolutions.set(reqId, res);
          return;
        }
        const it = items[i] as Extract<TItem, { t: "perm" }>;
        this.set(items, i, { ...it, resolved: res });
        return;
      }
      case "update":
        this.foldUpdate(items, e, key);
        return;
    }
  }

  private foldUpdate(items: TItem[], e: OEvent, key: string) {
    const u = e.data ?? {};
    const kind: string = u.sessionUpdate;
    const role = CHUNK_ROLE[kind];
    if (role) {
      const text = chunkText(u.content);
      const messageId: string | undefined = u.messageId ?? undefined;
      const last = items[items.length - 1];
      if (last && last.t === "msg" && last.role === role && last.messageId === messageId) {
        this.set(items, items.length - 1, { ...last, text: last.text + text });
      } else {
        this.push(items, { t: "msg", key, ts: e.ts, role, text, messageId });
      }
      return;
    }
    switch (kind) {
      case "tool_call": {
        const id: string = u.toolCallId;
        const existing = this.toolIdx.get(id);
        if (existing !== undefined) {
          const it = items[existing] as Extract<TItem, { t: "tool" }>;
          this.set(items, existing, { ...it, call: mergeToolCall(it.call, u) });
          return;
        }
        this.toolIdx.set(id, items.length);
        this.push(items, { t: "tool", key, ts: e.ts, call: mergeToolCall(undefined, u) });
        return;
      }
      case "tool_call_update": {
        const id: string = u.toolCallId;
        const existing = this.toolIdx.get(id);
        if (existing === undefined) {
          this.toolIdx.set(id, items.length);
          this.push(items, { t: "tool", key, ts: e.ts, call: mergeToolCall(undefined, u) });
          return;
        }
        const it = items[existing] as Extract<TItem, { t: "tool" }>;
        this.set(items, existing, { ...it, call: mergeToolCall(it.call, u) });
        return;
      }
      case "plan": {
        const entries: PlanEntry[] = Array.isArray(u.entries) ? u.entries : [];
        if (this.planIdx !== null) {
          const it = items[this.planIdx] as Extract<TItem, { t: "plan" }>;
          this.set(items, this.planIdx, { ...it, entries, ts: e.ts });
        } else {
          this.planIdx = items.length;
          this.push(items, { t: "plan", key, ts: e.ts, entries });
        }
        return;
      }
      case "current_mode_update":
        this.push(items, { t: "sys", key, ts: e.ts, text: `Mode → ${u.currentModeId}`, level: "info" });
        return;
      default:
        return;
    }
  }
}

/** The latest plan in the timeline, if any (used for the compact plan strip). */
export function latestPlan(items: TItem[]): PlanEntry[] | null {
  for (let i = items.length - 1; i >= 0; i--) {
    const it = items[i];
    if (it.t === "plan") return it.entries;
  }
  return null;
}

/** Prompts the user sent in this session, oldest first (for ArrowUp recall). */
export function promptHistory(items: TItem[]): string[] {
  const out: string[] = [];
  for (const it of items) if (it.t === "user" && it.text.trim()) out.push(it.text);
  return out;
}
