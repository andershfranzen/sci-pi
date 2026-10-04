# outpost protocol (v0)

Two processes speak this:

- **outpostd** (`outpost serve`) runs on each remote machine, binds `127.0.0.1:7433`,
  owns agent sessions and serves the web UI.
- **hub** (`outpost ui`) runs on your laptop at `127.0.0.1:7430`, manages SSH tunnels to
  every configured host and serves the same web UI in multi-host mode.

## Auth & transports

A daemon is reachable two ways:

- **Loopback** (`127.0.0.1:7433`), reached from a laptop through an SSH tunnel the hub manages.
  Requires `Authorization: Bearer <token>` (WebSocket: `?token=`).
- **Tailnet** (native Tailscale): when `tailscaled` runs on the host, the daemon also listens on
  its Tailscale IPs, port 7433 – HTTPS with a Tailscale cert when the host may fetch one,
  else plain HTTP (WireGuard already encrypts it). Callers are identified with tailscaled's
  `whois`; a caller whose tailnet login is allowed needs **no token** (a valid token also works).
  This is what lets a phone on the tailnet open `https://homelab.<tailnet>.ts.net:7433` directly.

Clients must therefore treat the token as optional: send `Authorization` only when they have a
non-empty token, and on `401` ask for one. CORS is open (no cookies), so a UI served from one
origin can talk to any number of daemons.

`GET /api/ping` is unauthenticated: `{ outpost: true, version, host, tailnet_url: string | null }`.

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
  branch: string | null;          // worktree branch, e.g. "outpost/3f2a9c1b"
  base_commit: string | null;     // commit the worktree was branched from
  status: SessionStatus;
  status_message: string | null;  // error text etc.
  mode: string | null;            // current ACP mode id
  modes: { id: string; name: string; description?: string }[];
  usage: { used: number; size: number; cost?: { amount: number; currency: string } } | null;
  queued: number;                 // = queue.length
  queue: { id: string; text: string; attachments: Attachment[] }[]; // persisted; survives restarts
  config_options: ConfigOption[]; // ACP SessionConfigOption verbatim – model, effort, mode, …
  commands: { name: string; description: string; hint: string | null }[]; // agent slash commands
  prompt_caps: { image?: boolean; embeddedContext?: boolean } | null;
  turns: number;                  // number of turns started; turn numbers are 1-based
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
  type: string;                   // "select" (render as a dropdown); other types: show read-only
  currentValue: any;
  options?: { value: any; name: string; description?: string }[];
}

type Attachment =
  | { type: "image"; name: string; mime_type: string } // GET /api/attachments/:name (?token= works for <img>)
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
| `user_prompt`         | `{ text, attachments: Attachment[], turn: number, checkpoint: string \| null }` |
| `update`              | an ACP `SessionUpdate` object verbatim (discriminator `sessionUpdate`) |
| `permission_request`  | `{ request_id: string, tool_call: ToolCall, options: { optionId, name, kind }[] }` |
| `permission_resolved` | `{ request_id: string, outcome: "selected" \| "cancelled", option_id: string \| null }` |
| `turn_end`            | `{ stop_reason: string, usage?: object, turn: number, checkpoint: string \| null }` |
| `reverted`            | `{ turn, checkpoint }` – working tree restored to before `turn` |
| `git`                 | `{ action: "commit", sha, message }` or `{ action: "push", output }` |
| `pr_created`          | `{ url }` |
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
- `usage_update` and `available_commands_update` are **not** stored as events; they are folded
  into the `Session` object instead.

## REST (daemon)

| method & path | body | returns |
|---|---|---|
| `GET /api/info` | | `{ host, version, home, agents: {id,name}[], last_event_id, tailnet_url: string \| null, viewer: string \| null }` – `viewer` is the caller's tailnet login, if any |
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
| `GET /api/sessions/:id/files?q=` | | `string[]` – fuzzy file search for `@` mentions (paths relative to cwd, max 30) |
| `GET /api/sessions/:id/git` | | `{ git, branch, dirty, remote, upstream, ahead, behind, commits_since_base, pr_url, gh }` |
| `POST /api/sessions/:id/git/commit` | `{ message }` | `{ sha }` – `git add -A && git commit` |
| `POST /api/sessions/:id/git/push` | | `{ output }` |
| `POST /api/sessions/:id/git/pr` | `{ title?, body?, draft? }` | `{ url }` – pushes, then `gh pr create` on the host |
| `GET /api/sessions/:id/terminal` | | WebSocket, see below |
| `DELETE /api/sessions/:id/terminal` | | `{}` – kill the shell |
| `GET /api/attachments/:name` | | the uploaded file |
| `POST /api/sessions/:id/cancel` | | `{}` – cancels the running turn and clears the queue |
| `POST /api/sessions/:id/permission` | `{ request_id, option_id: string \| null }` | `{}` – `null` = cancel/deny |
| `POST /api/sessions/:id/mode` | `{ mode }` | `{}` |
| `POST /api/sessions/:id/stop` | | `{}` – kills the adapter; next prompt resumes it |
| `GET /api/sessions/:id/diff?turn=N` | | `{ diff: string, files: { path, status }[] }` – without `turn`: everything vs `base_commit` (or `HEAD`) incl. untracked; with `turn`: just that turn's changes. `status` is git's letter (A/M/D/R…) |
| `GET /api/inbox` | | `{ session_id, session_title, request_id, tool_call, options, ts }[]` |
| `GET /api/fs/list?path=~/code` | | `{ path, parent: string \| null, is_git, entries: { name, path, is_git }[] }` (directories only) |

Errors: non-2xx with `{ error: string }`.

## WebSocket (daemon)

`GET /api/ws?token=T&after=N` – the server first replays every event with `id > N`, then streams
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

`GET /api/sessions/:id/terminal?token=T` attaches to the session's persistent login shell (spawned
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
