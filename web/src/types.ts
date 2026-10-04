// Types mirroring docs/PROTOCOL.md (v0).

export type SessionStatus =
  | "starting"
  | "idle"
  | "running"
  | "awaiting_permission"
  | "detached"
  | "stopped"
  | "error";

export interface SessionMode {
  id: string;
  name: string;
  description?: string;
}

export interface Usage {
  used: number;
  /** context window; missing/null when the model's window is unknown */
  size?: number | null;
  cost?: { amount: number; currency: string };
}

export interface ConfigOption {
  id: string;
  name: string;
  description?: string;
  /** "mode" | "model" | "thought_level" | … */
  category?: string;
  /** "select" renders as a dropdown; other types are shown read-only */
  type: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  currentValue: any;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  options?: { value: any; name: string; description?: string }[];
}

export type Attachment =
  | { type: "image"; name: string; mime_type: string }
  | { type: "file"; path: string };

export type AttachmentIn =
  | { type: "image"; mime_type: string; data: string }
  | { type: "file"; path: string };

export interface QueueItem {
  id: string;
  text: string;
  attachments: Attachment[];
}

export interface SlashCommand {
  name: string;
  description: string;
  hint: string | null;
}

export interface Session {
  id: string;
  title: string;
  agent: string;
  project: string;
  cwd: string;
  branch: string | null;
  base_commit: string | null;
  status: SessionStatus;
  status_message: string | null;
  mode: string | null;
  modes: SessionMode[];
  usage: Usage | null;
  queued: number;
  queue: QueueItem[];
  config_options: ConfigOption[];
  commands: SlashCommand[];
  prompt_caps: { image?: boolean; embeddedContext?: boolean } | null;
  turns: number;
  pinned: boolean;
  archived: boolean;
  pr_url: string | null;
  pending_permissions: number;
  created_at: number;
  updated_at: number;
}

export type EventKind =
  | "user_prompt"
  | "update"
  | "permission_request"
  | "permission_resolved"
  | "turn_end"
  | "reverted"
  | "git"
  | "pr_created"
  | "forked"
  | "status"
  | "error";

export interface OEvent {
  id: number;
  session_id: string;
  ts: number;
  kind: EventKind;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  data: any;
}

export type ToolKind =
  | "read"
  | "edit"
  | "delete"
  | "move"
  | "search"
  | "execute"
  | "think"
  | "fetch"
  | "switch_mode"
  | "other";

export type ToolStatus = "pending" | "in_progress" | "completed" | "failed";

export type ToolCallContent =
  | { type: "content"; content: { type: string; text?: string } }
  | { type: "diff"; path: string; oldText: string | null; newText: string }
  | { type: "terminal"; terminalId: string };

export interface ToolCall {
  toolCallId: string;
  title?: string;
  kind?: ToolKind | string;
  status?: ToolStatus | string;
  rawInput?: unknown;
  rawOutput?: unknown;
  content?: ToolCallContent[] | null;
  locations?: { path: string; line?: number | null }[] | null;
}

export type PermissionOptionKind = "allow_once" | "allow_always" | "reject_once" | "reject_always" | string;

export interface PermissionOption {
  optionId: string;
  name: string;
  kind: PermissionOptionKind;
}

export interface PlanEntry {
  content: string;
  priority?: string;
  status: "pending" | "in_progress" | "completed" | string;
}

export interface AgentInfo {
  id: string;
  name: string;
}

export interface Info {
  host: string;
  version: string;
  home: string;
  agents: AgentInfo[];
  last_event_id: number;
  tailnet_url: string | null;
  /** caller's tailnet login, when reached over Tailscale */
  viewer: string | null;
}

export interface InboxItem {
  session_id: string;
  session_title: string;
  request_id: string;
  tool_call: ToolCall;
  options: PermissionOption[];
  ts: number;
}

export interface DiffResult {
  diff: string;
  files: { path: string; status: string }[];
}

export interface GitStatus {
  git: boolean;
  branch: string | null;
  dirty: number;
  remote: string | null;
  upstream: boolean | string | null;
  ahead: number | null;
  behind: number | null;
  commits_since_base: number;
  pr_url: string | null;
  gh: boolean;
}

export interface FsList {
  path: string;
  parent: string | null;
  is_git: boolean;
  entries: { name: string; path: string; is_git: boolean }[];
}

export interface HubHost {
  name: string;
  url: string;
  /** may be "" for tailnet hosts */
  token: string;
  transport: "ssh" | "tailscale" | "local";
  /** found by probing tailnet peers rather than from hosts.toml */
  discovered: boolean;
  status: "connected" | "connecting" | "error";
  error?: string;
}

export type WsMessage =
  | { type: "event"; event: OEvent }
  | { type: "session"; session: Session }
  | { type: "session_deleted"; id: string }
  | { type: "ping" };

export interface CreateSessionBody {
  agent: string;
  project: string;
  worktree: boolean;
  title?: string;
  mode?: string;
  prompt?: string;
  attachments?: AttachmentIn[];
}

export interface SearchHit {
  session_id: string;
  session_title: string;
  event_id: number;
  role: "user" | "agent";
  /** matches wrapped in <<…>> */
  snippet: string;
}
