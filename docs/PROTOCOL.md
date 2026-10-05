# sci-pi protocol (v0)

Two processes speak this:

- **sci-pi daemon** (`sci-pi serve`) runs on each remote machine, binds `127.0.0.1:7433`,
  owns agent sessions and serves the web UI.
- **hub** (`sci-pi ui`) runs on your laptop at `127.0.0.1:7430`, manages SSH tunnels to
  every configured host and serves the same web UI in multi-host mode.

## Auth & transports

A daemon is reachable two ways:

- **Loopback** (`127.0.0.1:7433`), reached from a laptop through an SSH tunnel the hub manages.
  Requires `Authorization: Bearer <token>`. Browser WebSockets send protocols
  `["sci-pi", "bearer." + base64url(UTF8(token))]` (without padding); the server selects `sci-pi`.
- **Tailnet** (native Tailscale): when `tailscaled` runs on the host, the daemon also listens on
  its Tailscale IPs, port 7433 – HTTPS with a Tailscale cert when the host may fetch one,
  else plain HTTP (WireGuard already encrypts it). Callers are identified with tailscaled's
  `whois`; a caller whose tailnet login is allowed needs **no token** (a valid token also works).
  This is what lets a phone on the tailnet open `https://homelab.<tailnet>.ts.net:7433` directly.

Clients must therefore treat the token as optional: send `Authorization` only when they have a
non-empty token, and on `401` ask for one. CORS is open (no cookies), so a UI served from one
origin can talk to any number of daemons.
`?token=` credentials are rejected, including on otherwise allowed tailnet connections.
An explicitly supplied invalid bearer does not fall back to tailnet identity. Attachments
must be fetched with Authorization and rendered as Blob-backed `<img>` elements; do not
navigate to an unsandboxed Blob document (SVG/HTML can execute in the UI origin).

Local pairing uses an admin-issued, single-use, 256-bit code expiring after 120 seconds.
The pairing URL carries `#pair=CODE`, not a bearer; consume/remove that fragment before
fetching. Both issuance and redemption require an actual loopback socket and loopback Host;
redemption additionally requires an Origin exactly matching that Host. Forwarded headers are
not trusted. Credential responses use `Cache-Control: no-store`. Device bearers are hash-only
on disk and revocation closes their existing API/terminal sockets. Every device still has full
host-account execution through tools/terminals; these are not mutually untrusted principals.


`GET /api/ping` is unauthenticated: `{ scipi: true, version, build: BuildInfo, host, tailnet_url: string | null }`.

## Types

```ts
type SessionStatus =
  | "starting"            // adapter process spawning / handshaking
  | "idle"                // waiting for a prompt
  | "running"             // a turn is in progress
  | "awaiting_permission" // a turn is blocked on a permission request
  | "detached"            // daemon restarted; resumes on next prompt
  | "stopped"             // adapter shut down by user
  | "error";

interface Session {
  id: string;
  title: string;
  agent: string;                  // key into Info.agents
  project: string;                // directory the user picked
  cwd: string;                    // where the agent runs (worktree path if worktree)
  branch: string | null;          // worktree branch, e.g. "sci-pi/3f2a9c1b"
  base_commit: string | null;     // commit the worktree was branched from
  status: SessionStatus;
  status_message: string | null;  // error text etc.
  mode: string | null;            // current ACP mode id
  modes: { id: string; name: string; description?: string }[];
  usage: { used: number; size?: number; cost?: { amount: number; currency: string } } | null;
  queued: number;                 // = queue.length
  queue: { id: string; text: string; attachments: Attachment[]; retry_of?: number }[]; // persisted; marker stays with the item on reorder
  config_options: ConfigOption[]; // ACP options with optional native metadata extension
  commands: { name: string; description: string; hint: string | null }[]; // agent slash commands
  prompt_caps: { image?: boolean; embeddedContext?: boolean } | null;
  turns: number;                  // number of turns started; turn numbers are 1-based
  recovery: TurnRecovery | null;  // last turn's outcome; cleared when a new attempt starts
  queue_paused: boolean;          // retained prompts require explicit recovery
  retry_pending: boolean;        // a queued retry already exists; do not duplicate it
  request_diagnostics: RequestDiagnostic[]; // latest 50 records for current/last turn
  pinned: boolean;
  archived: boolean;
  pr_url: string | null;
  pending_permissions: number;
  created_at: number;             // unix ms
  updated_at: number;
}

interface ConfigOption {          // e.g. { id: "model", name: "Model", category: "model", type: "select",
  id: string; name: string;       //        currentValue: "opus", options: [{ value, name, description }] }
  description?: string;
  category?: string;              // "mode" | "model" | "thought_level" | …
  type: string;                   // "select" (dropdown), "boolean" (toggle); others: read-only
  currentValue: any;
  options?: { value: any; name: string; description?: string }[];
  metadata?: ModelMetadata;       // native selected-model provenance
}

type Attachment =
  | { type: "image"; name: string; mime_type: string } // bearer-authenticated GET /api/attachments/:name
  | { type: "file"; path: string };                     // path relative to the session cwd (@-mention)

interface Event {
  id: number;          // global, monotonically increasing across all sessions on a host
  session_id: string;
  ts: number;          // unix ms
  kind: EventKind;
  data: any;           // see below
}
```

### Event kinds

| kind                  | data |
|-----------------------|------|
| `user_prompt`         | `{ text, attachments: Attachment[], turn: number, checkpoint: string \| null, retry_of: number \| null }` |
| `update`              | an ACP `SessionUpdate`, with native allowlisted `request_diagnostic` extension |
| `permission_request`  | `{ request_id: string, tool_call: ToolCall, options: { optionId, name, kind }[] }` |
| `permission_resolved` | `{ request_id: string, outcome: "selected" \| "cancelled", option_id: string \| null }` |
| `turn_end`            | `{ stop_reason: string, usage?: object, turn: number, checkpoint: string \| null, recovery: TurnRecovery }` |
| `reverted`            | `{ turn, checkpoint }` – working tree restored to before `turn` |
| `git`                 | `{ action: "commit", sha, message }` or `{ action: "push", output }` |
| `pr_created`          | `{ url }` |
| `forked`              | `{ from, from_title, turn }` – first event of a forked session |
| `retry_requested`     | `{ turn, message }` – new attempt queued; earlier effects remain |
| `status`              | `{ status: SessionStatus, message?: string }` |
| `error`               | `{ message: string }` |

ACP `update` objects the UI should render (others can be ignored):

- `agent_message_chunk` / `agent_thought_chunk` / `user_message_chunk`:
  `{ content: { type: "text", text }, messageId? }`. Concatenate consecutive chunks with the same
  `messageId` (or consecutive chunks of the same type when `messageId` is absent).
- `tool_call`: `{ toolCallId, title, kind, status, rawInput, content: ToolCallContent[], locations }`.
- `tool_call_update`: same fields, all optional; merge into the tool call with the same
  `toolCallId` (shallow merge; `content` replaces when present).
- `ToolCallContent` is `{type:"content", content:{type:"text",text}}`,
  `{type:"diff", path, oldText: string|null, newText}`, or `{type:"terminal", terminalId}`.
- `plan`: `{ entries: { content, priority, status: "pending"|"in_progress"|"completed" }[] }`
  (replaces the previous plan).
- `current_mode_update`: `{ currentModeId }`.
- `session_info_update`: `{ title? }`.
- `config_option_update`: `{ configOptions }` replaces the options. Model-dependent Effort
  selects and `{ id: "fast", type: "boolean", currentValue: boolean }` controls may appear or
  disappear on model changes. Send the actual boolean, not a string, when setting Fast.
- `usage_update`: `{ used, size?, cost? }`. `size` is omitted when the context window is
  unknown; clients must not infer a limit. `cost` is present only for known billed API usage,
  not inferred subscription charges.
- `usage_update` and `available_commands_update` are **not** stored as events; they are folded
  into the `Session` object instead.
- `request_diagnostic`: `{ diagnostic: RequestDiagnostic }`, emitted only by the native
  adapter. Upsert by `diagnostic.id`; pending/response updates are one HTTP attempt, not two.
  These allowlisted updates are stored as events; old turns remain available in history.
  A pending record after cancellation means no HTTP response was recorded, not an active turn.

### Native diagnostics, provenance, and recovery

```ts
interface BuildInfo {
  id: string; commit: string | null; dirty: boolean;
  built_at: number; // Unix seconds; dirty describes compiled source inputs
  version: string;
}
type MetadataSource = "provider" | "models_dev" | "user_override" | "learned_overflow";
interface ModelMetadata {
  endpoint: string | null; endpoint_id: string | null; pay_per_token: boolean;
  info: Record<string, unknown> & { provenance: Record<string, MetadataSource> };
}
interface TurnRecovery {
  turn: number; outcome: "completed" | "failed" | "cancelled" | "interrupted";
  completed_tools: number; started_tools: number; resumable: boolean; message: string;
}
interface RequestDiagnostic {
  id: string; at_ms?: number; attempt?: number;
  route?: "anthropic_key" | "anthropic_oauth" | "anthropic_proxy" | "openai";
  endpoint?: string | null; model?: string | null; phase?: "inference" | "compaction";
  state: "pending" | "response" | "network_error";
  http_status?: number | null; request_id?: string | null;
  effort?: string | null; fast?: string | null; thinking?: boolean;
  server_compaction?: boolean; compaction?: boolean;
  cache_owner?: "client" | "proxy" | "none"; explicit_cache_points?: number;
  automatic_cache?: boolean; context_window?: number | null; max_output?: number | null;
  rejected_fields?: string[];
}
```

Model facts include context/output limits, effort choices/default, thinking, Fast,
compaction, refusal fallback and per-million-token rates. Provider negatives block catalog
fallback; unknown facts stay unknown. `provenance` maps fact names (including `cost.input`,
`cost.output`, `cost.cache_read`, `cost.cache_write`) to their effective source. User context
overrides are capped by smaller learned limits. Learned limits persist by normalized endpoint
and bare model; legacy unscoped files are ignored. Subscription endpoints omit API-price costs.

Diagnostic endpoints strip userinfo/query/fragment and redact the known provider credential.
Records exclude headers other than safe request IDs, prompts, tool contents, thinking/signatures,
credentials and upstream error bodies. HTTP 200/`response` means headers received, not successful
inference. Incomplete Anthropic/OpenAI streams fail without automatic retransmission or
execution of unfinished tools.

Failed/interrupted turns pause retained queues; cancellation clears the queue. Restarting
during an active turn emits one durable interrupted `turn_end`, unless a durable ending already
exists. Resume never repeats the prior prompt. Retry retains history and sends the original
prompt/attachments as a new numbered attempt, so effects may be repeated, never rolled back.
Preserve non-normal `stop_reason` labels (`max_tokens`, `refusal`, `max_turn_requests`) even
when the RPC outcome is `completed`.


## REST (daemon)

| method & path | body | returns |
|---|---|---|
| `GET /api/info` | | `{ host, version, build: BuildInfo, home, agents: {id,name}[], last_event_id, tailnet_url: string \| null, viewer: string \| null }` – `viewer` is the caller's tailnet login, if any |
| `GET /api/auth/me` | | `{ admin: boolean, device_id: string \| null }` |
| `DELETE /api/auth/me` | | `{}` – revoke this device's bearer |
| `GET /api/auth/devices` | | `{ id, name, created_at }[]` (Unix seconds); administrator only |
| `DELETE /api/auth/devices/:id` | | `{}` – administrator only; invalidates bearer and closes its sockets |
| `POST /api/auth/pair` | `{ name?: string }` | `{ url, expires_at }` (Unix seconds); administrator + loopback only |
| `POST /api/auth/redeem` | `{ code }` | `{ token, device: { id, name, created_at } }`; no prior bearer, strict local Origin required |
| `GET /api/sessions` | | `Session[]` (newest first) |
| `POST /api/sessions` | `{ agent, project, worktree: bool, title?, mode?, prompt?, attachments? }` | `Session` |
| `PATCH /api/sessions/:id` | `{ title?, pinned?, archived? }` | `Session` |
| `GET /api/sessions/:id` | | `Session` |
| `DELETE /api/sessions/:id` | | `{}` – stops the agent, deletes events; `?remove_worktree=1` also removes the worktree + branch |
| `GET /api/sessions/:id/events?after=N` | | `Event[]` with `id > N`, ascending |
| `POST /api/sessions/:id/prompt` | `{ text, attachments?: AttachmentIn[] }` | `{ queued: bool }` – always goes through the queue |
| `DELETE /api/sessions/:id/queue/:qid` | | `{}` |
| `PATCH /api/sessions/:id/queue/:qid` | `{ text }` | `{}` |
| `POST /api/sessions/:id/queue/:qid/send_now` | | `{}` – moves it to the front and interrupts the running turn |
| `POST /api/sessions/:id/config` | `{ config_id, value }` | `{}` – model/effort/etc.; result arrives as a `session` message |
| `POST /api/sessions/:id/revert` | `{ turn }` | `{}` – restore files to the checkpoint before `turn` (not while running). The agent is told on its next prompt. |
| `POST /api/sessions/:id/fork` | `{ turn?, agent? }` | `Session` – new session continuing from the end of `turn` (default: latest): files from that turn's checkpoint in a fresh worktree, conversation handed over from the log, so it works across agents ("try this with Codex instead") |
| `GET /api/search?q=` | | `{ session_id, session_title, event_id, role: "user"\|"agent", snippet }[]` – full-text, prefix matching; matches wrapped in `<<…>>` |
| `GET /api/sessions/:id/files?q=` | | `string[]` – fuzzy file search for `@` mentions (paths relative to cwd, max 30) |
| `GET /api/sessions/:id/git` | | `{ git, branch, dirty, remote, upstream, ahead, behind, commits_since_base, pr_url, gh }` |
| `POST /api/sessions/:id/git/commit` | `{ message }` | `{ sha }` – `git add -A && git commit` |
| `POST /api/sessions/:id/git/push` | | `{ output }` |
| `POST /api/sessions/:id/git/pr` | `{ title?, body?, draft? }` | `{ url }` – pushes, then `gh pr create` on the host |
| `GET /api/sessions/:id/terminal` | | WebSocket, see below |
| `DELETE /api/sessions/:id/terminal` | | `{}` – kill the shell |
| `GET /api/attachments/:name` | | the uploaded file |
| `POST /api/sessions/:id/cancel` | | `{}` – cancels the running turn and clears the queue |
| `POST /api/sessions/:id/retry` | | `Session` – new attempt of last unsuccessful turn; refuses active/approval/pending-retry state |
| `POST /api/sessions/:id/resume` | | `Session` – restart adapter and release queued work without repeating prior prompt |
| `POST /api/sessions/:id/permission` | `{ request_id, option_id: string \| null }` | `{}` – `null` = cancel/deny |
| `POST /api/sessions/:id/mode` | `{ mode }` | `{}` |
| `POST /api/sessions/:id/stop` | | `{}` – kills adapter; active turn becomes interrupted/paused, idle Stop resumes on next prompt |
| `GET /api/sessions/:id/diff?turn=N` | | `{ diff: string, files: { path, status }[] }` – without `turn`: everything vs `base_commit` (or `HEAD`) incl. untracked; with `turn`: just that turn's changes. `status` is git's letter (A/M/D/R…) |
| `GET /api/inbox` | | `{ session_id, session_title, request_id, tool_call, options, ts }[]` |
| `GET /api/fs/list?path=~/code` | | `{ path, parent: string \| null, is_git, entries: { name, path, is_git }[] }` (directories only) |

Errors: non-2xx with `{ error: string }`.

## WebSocket (daemon)

`GET /api/ws?after=N` with the bearer subprotocols above – the server first replays every event with `id > N`, then streams
live. Server → client messages:

```ts
{ type: "event", event: Event }
{ type: "session", session: Session }   // any change to a session row (status, usage, title…)
{ type: "session_deleted", id: string }
{ type: "ping" }                         // every 25s; reconnect if none for ~60s
```

After the replay the server sends one `session` message per session, so status changes missed
while disconnected are picked up without a separate fetch. Unknown `/api/*` and `/hub/*` paths
return JSON 404 (never `index.html`).

`AttachmentIn` (upload form): `{ type: "image", mime_type, data /* base64 */ }` or `{ type: "file", path }`.

### Terminal WebSocket

`GET /api/sessions/:id/terminal` with the same bearer subprotocols attaches to the session's persistent login shell (spawned
in the session cwd on first attach, kept alive across disconnects like tmux). The server first
sends the scrollback (up to 512 KiB) as one binary frame, then live output as binary frames, and
`{"type":"exit"}` (text) when the shell exits. The client sends text frames
`{"type":"input","data":"ls\r"}` and `{"type":"resize","cols":120,"rows":40}`.

No client → server messages; actions go through REST. Clients track the highest event id seen
and pass it as `after` when reconnecting, so nothing is lost while a laptop sleeps.

## Hub (laptop)

| method & path | returns |
|---|---|
| `GET /hub/hosts` | `{ name, url, token, transport: "ssh"\|"tailscale"\|"local", discovered: bool, status: "connected"\|"connecting"\|"error", error? }[]` |
| `POST /hub/hosts/:name/connect` | `{}` – (re)start the tunnel |

`url` is the base URL the browser should use: the SSH tunnel's local end
(`http://127.0.0.1:7501`), or the host's tailnet URL (`https://homelab.<tailnet>.ts.net:7433`).
`token` may be `""` for tailnet hosts. `discovered: true` means the hub found the daemon by
probing tailnet peers rather than from `hosts.toml`.

The web UI picks its mode at load: if `GET /hub/hosts` succeeds it is in **hub mode**
(multi-host). Otherwise it is in **direct mode**: one host at `location.origin`; token from
`location.hash` (`#token=…`) or `localStorage` if present, and if `/api/info` still answers `401`,
a "paste token" screen. Over the tailnet no token is needed.
