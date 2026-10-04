# Inspiration: T3 Code and OMP vs sci-pi

Researched 2026-10-04. Requirement: sci-pi must be **at least as good as T3 Code and OMP**.

Snapshots examined:

- **T3 Code**: `pingdotgg/t3code` at `b3b6ae2` (main, 2026-10-04; latest stable **v0.0.45**, 2026-10-02). 24.9k stars, MIT, repo created 2026-02-08.
- **OMP (oh-my-pi)**: `can1357/oh-my-pi` main, plus the local install at
  `~/.local/share/mise/installs/github-can1357-oh-my-pi/18.6.0/omp` (**v18.6.0**, 2026-10-03). 34.2k stars, MIT.

Citation shorthand:

- `t3:<path>` = `https://github.com/pingdotgg/t3code/blob/main/<path>`, and `t3#N` = `https://github.com/pingdotgg/t3code/issues/N`.
- `omp:<path>` = `https://github.com/can1357/oh-my-pi/blob/main/<path>`, and `omp#N` = `https://github.com/can1357/oh-my-pi/issues/N`.
- `T3 rel vX` = the GitHub release notes for that tag (`gh release view vX -R pingdotgg/t3code`).

Legend for the matrix: ✅ = has it, ◐ = partial, ❌ = missing, n/a = doesn't apply to that kind of tool.
Anything marked **unverified** was not confirmed in source or docs.

---

## 1. Summaries

### T3 Code: a multi-client control surface for vendor coding agents

T3 Code describes itself as an "agent harness control surface" (`t3:README.md`). It **does not run its own agent loop.** It
drives the vendor agents through their official SDKs and protocols:

- Claude through `@anthropic-ai/claude-agent-sdk` (`t3:apps/server/package.json`).
- Codex through `codex app-server`, with generated bindings (`t3:packages/effect-codex-app-server`).
- Cursor through its SDK.
- Grok and the whole **ACP Registry** through ACP v1 and the v2 preview (`t3:packages/effect-acp`, `t3:docs/user/providers-acp.md`).
- OpenCode 1.x and 2.x through its own server.
- Pi through `pi --mode rpc`.
- Antigravity.

Every provider sits behind one adapter contract (`t3:apps/server/src/orchestration-v2/Adapters/`) with a capability system (`t3:docs/orchestration-v2/provider-capability-system.md`).

**Architecture**

- **Server.** A Node 22+ TypeScript server, the `t3` CLI written with Effect, owns everything: provider processes, PTYs (node-pty), git, files and the database.
- **Clients.** Three clients share one `packages/client-runtime`:
  - a React web app, also hosted at app.t3.codes, which connects straight to your server;
  - an Electron desktop app, which bundles a server and can turn the local one off;
  - native iOS and Android apps built with Expo.
- **RPC.** They talk over an authenticated WebSocket RPC (`t3:packages/contracts/src/rpc.ts`). Capabilities are negotiated, because clients and servers are versioned independently (`t3:docs/internals/overview.md`).
- **Persistence.** Persistence is event-sourced SQLite (`statev2.sqlite`), and the "Orchestrator V2" decides events without doing I/O:
  - `EventSink` commits events, projections, the command receipt and outbox effects in one transaction.
  - `EffectWorker` then performs the side effects (`t3:docs/internals/overview.md`).
- **Checkpoints.** Checkpoints are hidden git refs captured around each turn (`t3:apps/server/src/checkpointing/CheckpointStore.ts`).
- **MCP injection.** T3 injects its own MCP server, `t3-code`, into every provider session with a scoped, expiring credential. Through it, agents get these tools (`t3:docs/orchestration-v2/orchestrator-mcp-server.md`, `t3:apps/server/src/mcp/`):
  - orchestration: spawning sub-threads on any provider, waiting, steering and listing;
  - browser preview tools (`preview_*`);
  - device tools (`device_*`);
  - PR-link tools.

**Remote access** (`t3:docs/user/remote-access.md`, `t3:docs/internals/remote.md`)

- **Direct pairing.** Pair over a LAN or tailnet with one-time links or QR codes (`t3 pair`).
- **Tailscale Serve.** `t3 serve --tailscale-serve` exposes the server over Tailscale HTTPS.
- **Desktop-managed SSH.** The desktop app downloads the server to `~/.t3/runtime` on the remote host and forwards the port.
- **T3 Connect.** An account-based relay (Clerk accounts plus Cloudflare tunnels) that also carries mobile push.
- **Background service.** `t3 service install` installs a systemd user service with lingering, or a launchd agent on macOS.

**Pace and maturity**

- The changelog is huge: about 400 `feat:` entries across stable releases (`gh release list -R pingdotgg/t3code`).
- It ships nightly builds.
- It still calls itself "very very early", and outside contributions are mostly closed (`t3:README.md`).

### OMP (oh-my-pi): a batteries-included terminal agent

OMP is **an agent, not a harness.** It is a fork of Mario Zechner's pi-mono with its own agent loop, its own LLM client for
60+ providers and its own tools.

**Runtime**

- TypeScript on Bun, plus about 80k lines of Rust in N-API addons (`omp:README.md`, `omp:crates/`):
  - an embedded bash (a brush fork with in-process coreutils);
  - ripgrep, tree-sitter and ast-grep, PTY handling, and desktop control.
- Its signature ideas:
  - hashline edits addressed by content hash (`omp:crates/pi-edit`, and the blog post https://blog.can.ac/2026/02/12/the-harness-problem/);
  - LSP and DAP tools;
  - first-class subagents in isolated worktrees or copy-on-write clones;
  - a second "advisor" model that reviews every turn;
  - "time-traveling stream rules" (TTSR);
  - memory backends;
  - a very deep slash-command and extension system.

**Entry points** (`omp:README.md` §"Four entry points")

- the TUI (the default);
- `-p` print mode;
- a Node SDK;
- `--mode rpc` and `rpc-ui`, which speak NDJSON over stdio;
- `omp acp`, an ACP server. It advertises `loadSession`, list, fork, resume and close, image and embedded-context prompts, plan mode and config options (`omp:packages/coding-agent/src/modes/acp/acp-agent.ts`, around lines 633-657 and 746-757).

**Persistence.** Sessions are append-only JSONL **trees**, made of `id`/`parentId` entries plus a leaf pointer, under `~/.omp/agent/sessions`, with a `history.db` index (`omp:docs/session.md`).

**Remote story.** OMP has **no daemon and no GUI.** "GUI on top of oh-my-pi" (`omp#5742`) and "remote control" (`omp#436`) are both top-voted requests. Instead it offers:

- `/collab`: a live, end-to-end-sealed relay share. Guests join from another `omp` or from a browser client at `my.omp.sh`, with view-only or control links and a QR code (`omp:docs/collab.md`).
- `/share`: encrypted transcript links.
- `omp stream`: a public live broadcast.
- `ssh://` paths and `omp ssh` hosts, so the agent's own tools can reach other machines.

**Relevance to sci-pi:** `omp acp` makes OMP a candidate **fourth agent** for sci-pi, and sci-pi would then be
the daemon and GUI that OMP users are asking for.

---

## 2. Feature matrix

sci-pi-MVP baseline, as given:

- an ACP adapter for Claude, Codex and OpenCode;
- durable sessions that survive client disconnect;
- resume after a daemon restart;
- a worktree per session;
- a diff view against the base;
- approvals with an inbox;
- ntfy push and browser notifications;
- prompt queueing;
- mode switching;
- SSH bootstrap of remote hosts;
- a multi-host hub;
- direct Tailscale access with whois auth.

Everything else is ❌ unless a note says otherwise. A few ◐ cells reflect what is already in the sci-pi tree:

- a context meter: `web/src/components/SessionView.tsx`;
- plan and thought rendering: `web/src/components/Timeline.tsx`;
- a directory picker: `GET /api/fs/list`.

### Runtime and architecture

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| How agents run | Wraps vendor SDKs, app-server, ACP and RPC per provider | Own agent loop that calls LLM APIs directly | Wraps agents via ACP (agent commands configurable in `src/config.rs`) |
| Agents and providers | Codex, Claude, Cursor, Grok, OpenCode 1/2, Pi, Antigravity, any ACP Registry agent | 60+ LLM providers, OAuth subscriptions, local models | Claude, Codex, OpenCode (ACP) |
| Several accounts per provider | ✅ provider "instances" (`t3:docs/internals/providers.md`) | ✅ round-robin credentials, `--profile` | ❌ |
| Agent-side durability: work continues with no client | ✅ server-owned | ❌ the agent lives in the TUI process (◐ with tmux or `/collab`) | ✅ |
| Resume after server or daemon restart | ✅ active threads continue; queues are held for **Resume** (T3 rel v0.0.39, `t3:docs/user/composer.md`) | ◐ `-c`/`-r` reload JSONL; nothing runs while no process is up (for vibe workers, `omp:docs/vibe-mode.md` says interrupted turns are not auto-resumed) | ✅ |
| Event-sourced log | ✅ SQLite, transactional outbox | ◐ JSONL session tree | ✅ |
| Background service install | ✅ `t3 service install`, systemd user service with linger, or launchd | n/a (`omp ps` supervises background jobs) | ❌ |
| Remote self-update | ✅ `t3 update`; remote server updates; "update providers on every machine" (T3 rel v0.0.45) | ✅ `omp update` (local only) | ❌ |
| Provider CLI install and update management | ✅ ownership-aware updaters (`t3:docs/internals/providers.md`) | n/a | ❌ |

### Sessions and threads

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Session list with live status | ✅ sidebar with Working, Pinned, Active, Snoozed and Settled sections (`t3:docs/user/thread-sidebar.md`) | ◐ `/resume` picker, `/pin` | ◐ sidebar plus status |
| Pin, reorder, archive, snooze, settle, undo | ✅ all; drag between sections; mod+z undo | ◐ pin and rename | ❌ |
| Auto-settle (archive) on PR merge or inactivity | ✅ server-side (`t3:docs/user/thread-sidebar.md`) | ❌ | ❌ |
| Auto titles and regenerate | ✅ | ✅ | ◐ (only from ACP `session_info_update`) |
| Full-text search across sessions | ✅ Cmd+K searches messages and final responses across environments | ✅ Ctrl+R prompt history; `archive` eval global | ❌ |
| Fork a conversation | ✅ native fork, else portable handoff (`t3:docs/orchestration-v2/feature-lifecycles.md`) | ✅ `/fork`, `/branch`, `/tree` (`omp:docs/tree.md`) | ❌ |
| Edit an earlier prompt or rewind | ✅ "Edit from here", "Revert and keep changes" or "Revert files too" | ✅ `/branch`, `/tree` (conversation only) | ❌ |
| Switch provider mid-thread with context handoff | ✅ budgeted portable handoff (`t3:docs/user/portable-handoffs.md`) | ✅ `/model` (same loop) | ❌ |
| Export or share a transcript | ❌ "no whole-thread export command" (`t3:docs/user/thread-migration.md`) | ✅ `/export` HTML, `/share` encrypted link, `/dump` | ❌ |
| Import native CLI sessions | ◐ Pi native sessions | ✅ `--from-claude`, `--from-codex` | ❌ |
| Projectless or scratch sessions | ✅ `~/.t3/scratch/<date>-<slug>` (T3 rel v0.0.45) | ✅ auto temp dir when started in `~` | ❌ |

### Composer and input

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Queue a follow-up | ✅ stored on the server; edit, reorder, promote to steer | ✅ `/queue`, Ctrl+Enter, Alt+Up dequeue | ✅ (count only, no edit or reorder) |
| Steer the running turn | ✅ when the adapter supports it | ✅ Enter while running | ❌ |
| `@` file mentions, fuzzy workspace search | ✅ inline chips | ✅ `@file` | ❌ |
| Images and file attachments | ✅ up to 100 files, 10 MiB per image, 50 MiB per file, HEIC→JPEG, drag/paste/share sheet | ✅ clipboard image paste, `@image.png` | ❌ |
| Large paste becomes an attachment | ✅ at 32 KiB or more | ✅ collapsed pastes | ❌ |
| Slash commands and skills picker | ✅ provider-native `/` plus T3 commands; `$` skills | ✅ about 80 built-ins, custom commands, skills | ❌ (daemon folds `available_commands_update`; no UI) |
| Prompt history recall and stash | ✅ ArrowUp recall; Cmd+S stash | ✅ Up, Ctrl+R, recover cleared draft | ❌ |
| Quote a response or attach a review comment as context | ✅ "Cite in composer", diff review comments, terminal excerpts, PR `#` and thread `@` chips | ✅ `/annotate` | ❌ |
| Voice input | ✅ on-device iOS transcription | ✅ push-to-talk STT, `/live` realtime voice | ❌ |
| Edit the prompt in `$EDITOR` | n/a | ✅ Ctrl+G | n/a |

### Agent control

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Permission modes | ✅ Supervised, Auto-accept edits, Auto (AI review), Full access; per-project default (`t3:docs/user/permission-modes.md`) | ✅ `always-ask`, `write`, `yolo` (**yolo is the default**), per-tool allow/deny/prompt policy (`omp:docs/approval-mode.md`) | ✅ ACP modes plus approvals |
| Cross-session approval inbox | ◐ per-thread panels, sidebar badges, push | ❌ | ✅ `/api/inbox` |
| Structured ask-user questions | ✅ persisted across restarts; answers can carry attachments (`t3:docs/user/question-attachments.md`) | ✅ `ask` tool with an auto-select timeout | ❌ |
| Plan mode | ✅ Plan/Build toggle, `/plan`, proposed-plan artifact | ✅ `/plan`, `/plan-review`, plan-model role, `--plan-yolo` | ◐ ACP plan mode where the agent offers it; plan entries rendered |
| Switch model mid-session | ✅ model picker (mod+shift+m), custom models, favorites | ✅ `/model`, Ctrl+P role cycling, presets, fallback chains | ❌ |
| Reasoning effort and fast mode | ✅ effort picker, Fast/Ultrafast | ✅ thinking levels, `/fast`, `ultrathink` | ❌ |
| One prompt fanned out to N models | ✅ shift-click models: one thread and worktree each (T3 rel v0.0.43) | ◐ `task` subagents with per-agent models | ❌ |
| Context meter | ✅ (opt-in) | ✅ footer %, `/context` breakdown | ◐ usage bar |
| Compaction | ✅ `/compact`, button on the context meter, automatic for old Claude threads | ✅ automatic and manual, `/shake`, `/handoff`, snapcompact (`omp:docs/compaction.md`) | ❌ |
| Retry or resume after error or rate limit | ✅ Resume, "Resume at reset", auto-resume, "Snooze until reset" | ✅ `/retry`, F5, fallback chains | ❌ |
| Loops, goals, schedules | ✅ recurring scheduled tasks (`t3:apps/server/src/scheduledTasks/`) | ✅ `/loop --until <cmd>`, `/goal`, `--max-time` | ❌ |
| Restart the agent session to load new skills or MCP | ✅ "Restart agent session" in Cmd+K | ✅ `/reload-plugins`, `/restart` | ◐ `POST /stop`, then the next prompt resumes |

### Multi-agent

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Many parallel sessions | ✅ | ◐ one per terminal | ✅ (and across hosts) |
| Subagent visibility | ✅ Agents panel; subagents are read-only child threads | ✅ Agent Hub (Alt+A): live roster, cost, transcript, steer, revive, kill (`omp:docs/agent-hub.md`) | ❌ |
| Agents spawning other agents or threads | ✅ `t3-code` MCP orchestrator on any provider | ✅ `task` fan-out with schema-typed results, IRC between agents, `/vibe` director mode | ❌ |
| Second-model reviewer | ❌ | ✅ advisor and `WATCHDOG.yml` (`omp:docs/advisor-watchdog.md`) | ❌ |
| Multi-agent code review | ◐ PR review UI (human) | ✅ `/review`: parallel reviewers, P0-P3 findings, a verdict | ❌ |

### Git, worktrees and review

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Worktree per session | ✅ per thread or project default; "New thread in this worktree" | ✅ per subagent; isolation backends: git worktree, APFS, btrfs, ZFS, reflink, overlayfs (`omp:packages/coding-agent/src/task/settings.ts`); `/wt` | ✅ |
| Branch naming | ✅ static prefix, AI semantic prefix, or custom instructions | ◐ | ◐ `sci-pi/<id>` |
| Worktree setup scripts | ✅ `t3.json` `runOnWorktreeCreate`, cancellable progress | ◐ runs `post-checkout` hooks | ❌ |
| Worktree cleanup policy | ✅ inactivity, merged or empty; per project (`t3:docs/user/project-settings.md`) | ✅ `omp worktree clear` | ◐ delete with `remove_worktree=1` |
| Diff view | ✅ scopes: turn, working tree, branch; file tree; hide whitespace; colour palettes; line comments sent to the agent | ✅ `omp git` full-screen split diff, staging, commit composer | ✅ diff against base |
| Per-turn diffs and checkpoints | ✅ hidden git refs | ❌ (no workspace checkpoint or revert found) | ❌ |
| Commit, push, create PR | ✅ with AI-written commit and PR text in a configurable style | ✅ `omp commit` (atomic splits); `github` tool | ❌ |
| PR review, merge, stacks, viewed marks | ✅ Pull Requests page across GitHub, GitLab, Bitbucket, Azure DevOps, Forgejo, Gitea | ◐ `pr://` reads, `/annotate code-review` | ❌ |
| PR auto-linking and babysitting | ✅ branch PR discovery; `watch_pull_request` wakes the agent on CI, reviews or conflicts | ◐ GitHub Actions run-watch | ❌ |
| Merge-conflict resolution aid | ❌ (not found) | ✅ `conflict://N` with `@ours`, `@theirs`, `@base` | ❌ |
| Clone, new project, publish repo | ✅ | n/a | ◐ directory picker only |

### Workspace tools

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Integrated terminal | ✅ per-thread drawer (libghostty-vt); server keeps 5,000 lines / 8 MiB of scrollback (`t3:docs/user/terminal.md`) | ✅ it *is* a terminal app; PTY bash tool | ❌ |
| File explorer and viewer | ✅ tree; HTML and PDF rendered; read-only for files outside the workspace | n/a | ❌ |
| Open in an editor | ✅ VS Code, Cursor, Zed, Kiro, IDEA, including remote over SSH (T3 rel v0.0.34) | n/a | ❌ |
| Project scripts and actions | ✅ `t3.json` scripts, keybindable as `script.<id>.run` | ◐ `omp ps` supervised processes | ❌ |
| Browser preview and agent browser control | ✅ desktop: preview panel, dev-port scanner, element picker, recording; `preview_*` MCP tools | ✅ `browser` tool (Puppeteer, or relay into your own Chrome) | ❌ |
| Simulators and devices | ✅ iOS Simulator and Android Emulator streams, also over SSH hosts (`t3:docs/user/devices.md`) | ❌ | ❌ |
| Screenshots and computer use | ✅ SnapShots window capture with accessibility data | ✅ `computer` tool | ❌ |
| LSP, DAP, AST tools | ❌ (provider-native only) | ✅ | ❌ (agent-side) |

### Notifications, remote and mobile

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Desktop and browser notifications | ✅ opt-in, sounds, badges | ✅ terminal notifications on completion, error and ask; OSC 9;4 progress (`omp:packages/coding-agent/src/modes/settings.ts`) | ✅ |
| Phone push | ✅ native APNs/FCM, Live Activities, widgets; **requires T3 Connect** (`t3:docs/user/mobile-notifications.md`) | ❌ | ✅ ntfy |
| Mobile client | ✅ native iOS and Android: offline outbox, share sheet, voice | ◐ browser guest via `/collab` | ◐ web UI from a phone browser over the tailnet |
| Remote over SSH | ✅ desktop launches the server and forwards the port | ◐ agent tools reach `ssh://` hosts (no remote control) | ✅ bootstrap plus hub tunnels |
| Tailscale | ✅ `tailscale serve` HTTPS plus pairing | ❌ | ✅ native tailnet listener, whois auth, no token |
| No-VPN relay | ✅ T3 Connect (account, Cloudflare tunnels) | ✅ collab relay, end-to-end sealed | ❌ |
| Several machines in one UI | ✅ environments, "All environments" bulk settings, load balancing | ❌ | ✅ hub |
| Pairing and revocation | ✅ one-time links and QR, revoke devices | ✅ link-is-capability | ◐ token or whois; no per-device revoke |
| Live sharing and multiplayer | ◐ the same user on several devices | ✅ `/collab` with view-only or control links | ◐ several whois-allowed users (no roles) |

### Settings, extensibility and usage

| Feature | T3 Code | OMP | sci-pi-MVP |
|---|---|---|---|
| Custom keybindings | ✅ `keybindings.json` with `when` clauses (`t3:docs/user/keybindings.md`) | ✅ `keybindings.yml`, vim mode | ❌ |
| Command palette | ✅ Cmd+K for commands, threads, PRs and settings | ◐ slash commands | ❌ |
| Themes and fonts | ✅ theme library, VS Code themes from Open VSX, OKLCH, contrast, fonts | ✅ TUI themes | ❌ |
| Settings UI with per-project overrides | ✅ environment, project and `t3.json` layers, bulk edit across environments | ✅ `/settings`, project `.omp/config.yml`, path-scoped models | ◐ `config.toml` |
| MCP management | ◐ uses each provider's MCP; injects its own | ✅ `/mcp add/list/test`, Smithery | ❌ |
| Plugins, extensions, hooks | ❌ | ✅ TypeScript extensions, hooks, marketplace, skills registry | ❌ |
| Memory and rules | ❌ (provider-native) | ✅ memory backends, TTSR rules, reads other tools' rule files | ❌ |
| Usage and cost dashboard | ✅ Usage page: tokens, cache, cost by model, custom prices, across environments | ✅ `omp stats` web dashboard | ◐ per-session cost field |
| Subscription limits | ✅ pooled across accounts and hosts; banked resets; `/usage-limits` | ✅ `omp usage`, `/usage` | ❌ |
| OpenTelemetry export | ✅ | ✅ | ❌ |

---

## 3. Prioritized gap list

Tags: **[H]** = harness-only work in sci-pi daemon, the hub or the web UI. **[A]** = needs agent or adapter support (ACP capability or
vendor adapter behaviour). **[H+A]** = harness work that degrades gracefully when the agent lacks the capability.

### P0: needed to credibly claim "at least as good"

| # | Gap | How sci-pi should do it | Tag |
|---|---|---|---|
| 1 | **Per-turn checkpoints and file revert** | At every `user_prompt` and `turn_end`, snapshot the worktree into hidden refs `refs/sci-pi/<session>/<turn>` (with a temporary index, like T3). Add `GET /diff?turn=N` and `POST /revert {turn, files_too}`. The snapshots live on the durable host, so they survive client and daemon restarts. | H |
| 2 | **Rewind and fork a conversation** | Use ACP `session/fork` (unstable) or `loadSession` where advertised. Otherwise, start a new ACP session seeded with a budgeted handoff built from **our own event log** (we own the transcript, so this works for every agent). Add "Edit from here" in the UI. | H+A |
| 3 | **Composer essentials** | `@` fuzzy file search served by the daemon (a gitignore-aware index per worktree); paste or drag images sent as ACP `image` blocks when `promptCapabilities.image` is set, else uploaded to `~/.sci-pi/attachments` and passed as a resource link; large pastes become file attachments; recall with ArrowUp. | H+A |
| 4 | **Slash-command and skill picker** | Render the `available_commands` the daemon already folds into `Session`. Commands go through as prompt text. Add sci-pi-native commands such as `/model`, `/plan`, `/compact`, `/fork` and `/revert`. | H |
| 5 | **Model, effort and config switching** | Wire ACP `session/set_config_option` and `set_model`: show the agent's model list and effort levels, and remember defaults per project and agent. | A |
| 6 | **Durable integrated terminal** | A PTY per session on the daemon with a ring-buffer scrollback, replayed on reconnect like events (`after=N`), rendered with xterm.js or ghostty-web. Use tmux semantics: it keeps running while the laptop is closed. | H |
| 7 | **Git actions: commit, push, PR** | A daemon endpoint that runs git and `gh`, `glab` or `tea` on the remote with the remote's credentials. Write the AI commit and PR text with a cheap one-shot ACP prompt. Link the PR to the session and show a status badge. | H |
| 8 | **Daemon as a service, plus fleet update** | `sci-pi service install` (systemd `--user` plus `loginctl enable-linger`, or launchd). The hub's "update all hosts" pushes the new binary over SSH, waits for idle turns, then restarts. | H |
| 9 | **Search and command palette** | SQLite FTS5 over prompts and agent messages on each daemon. The hub fans out queries in parallel across hosts. Cmd+K covers sessions, hosts, commands and actions. | H |
| 10 | **Session hygiene** | Rename, pin, archive or settle, unread and needs-you states, and auto-titles from a cheap model when the agent sends none. Optionally auto-settle on PR merge or inactivity, evaluated on the daemon. | H |
| 11 | **Queue editing and steer** | Make queued prompts editable, reorderable and deletable on the server (we already hold the queue). Steer is **[A]**: ACP has no mid-turn inject, so fall back to "cancel, then send" with the partial turn kept. | H+A |
| 12 | **Add OMP (and the ACP Registry) as agents** | Add the default agent `omp: ["omp","acp"]`. It already supports load, list, fork, resume, plan mode and images. Then add a registry-backed "add agent" flow modelled on `t3:docs/user/providers-acp.md`. | H |

### P1: parity on what users praise

| # | Gap | How sci-pi should do it | Tag |
|---|---|---|---|
| 13 | Structured ask-user questions | Render agent questions (ACP elicitation, or the Codex and Claude `AskUserQuestion` tool-call pattern) in the inbox next to approvals, persist them across restarts, and answer them from push notifications. | A |
| 14 | Retry, "resume at limit reset" and auto-resume | The daemon parses rate-limit stops and schedules continuation timers that survive restarts. Add a Snooze option and show it in the inbox. | H |
| 15 | Subagent visibility | Group tool calls by Claude `Task` and Codex subagent IDs into collapsible child timelines with their own cost and status; if `omp acp` exposes child sessions, show them there too. | H+A |
| 16 | Orchestration MCP (`sci-pi` MCP server) | Pass an HTTP MCP server in ACP `session/new` `mcpServers`, with a scoped token per session. Tools: `spawn_session(host, agent, prompt, worktree)`, `wait`, `read_session`, `send`, `link_pr`. It works **across hosts** via the hub (see §4). | H+A |
| 17 | Multi-agent fan-out | "Send to N": the same prompt to Claude, Codex and OpenCode (or OMP) in N worktrees, possibly on different hosts, then compare the diffs side by side and keep a winner. | H |
| 18 | Project scripts and worktree setup | Read `.sci-pi.toml`, **and also `t3.json`**, for `scripts[]` and `runOnWorktreeCreate`. Run them in the session PTY with progress events, so they can be bound to keys. | H |
| 19 | Dev-server preview through the daemon | Scan for listening ports in the worktree process tree and reverse-proxy them at `/preview/<session>/<port>` over the existing tunnel or tailnet, so it opens from a phone. Agent browser tools come later. | H |
| 20 | Usage and limits dashboard | Aggregate `turn_end.usage` and cost per host, agent and model in the hub. Read subscription limits where the CLIs expose them (Codex and Claude); one option is the CLIProxyAPI hub, as T3 does. | H |
| 21 | Compaction control | A "Compact" button on the context meter. It sends `/compact` when that is in `available_commands`; otherwise it does an sci-pi handoff into a fresh session. | A |
| 22 | Better approvals | "Allow always for this session" and per-tool rules applied by the daemon before forwarding; "Allow all edits" from the inbox; batch approve. | H |
| 23 | Open in editor | Generate `vscode://vscode-remote/ssh-remote+<host><path>`, Zed `ssh://` and Cursor links for the worktree and for each file in the diff. | H |
| 24 | Inline review comments from the diff | Select lines in the diff, write a comment, and it becomes a structured context chip in the next prompt (T3's `review_comment`). | H |
| 25 | Installable PWA and Web Push | A manifest and service worker, plus Web Push (VAPID) from the daemon so it works with no ntfy; an offline outbox for prompts. | H |
| 26 | Keybindings, themes, settings UI | A JSON keybinding map with `when` clauses; light, dark and accent themes; a settings page that edits `config.toml` per host and per project. | H |

### P2: differentiators and long tail

| # | Gap | How sci-pi should do it | Tag |
|---|---|---|---|
| 27 | Export and share | Static HTML export from the event log; a read-only share URL gated by tailnet ACL, or Tailscale Funnel with a capability token. | H |
| 28 | Live multiplayer with roles | Map whois identities to viewer, approver or driver roles, and attribute every prompt and approval to a person. | H |
| 29 | Advisor and review agents | A second ACP session on a different agent that receives each `turn_end` diff and posts notes; `/review` fans reviewers out over the session diff. | H |
| 30 | PR watch ("babysit") | The daemon polls `gh pr checks` and reviews, then wakes the session with a prompt when CI fails, a review comes in or a conflict appears. | H |
| 31 | MCP and skills management per host | List, add and test MCP servers per host, injected into new ACP sessions. | H+A |
| 32 | Import native sessions | Use ACP `session/list` plus `loadSession` to adopt existing Claude, Codex and OMP sessions found on a host. | A |
| 33 | Scheduled and recurring tasks | Cron on the daemon that creates sessions from templates, for example nightly dependency bumps. | H |
| 34 | Voice input | The browser Web Speech API, or whisper on the daemon or a GPU host. | H |
| 35 | Load-aware placement | The hub picks a host for new sessions from live CPU, memory, GPU and agent-auth data. | H |
| 36 | OpenTelemetry | OTLP traces for turns, tools and approvals. | H |
| 37 | Device simulators, computer use, LSP and DAP | Out of scope for the harness: the agent (OMP) provides these. | A |

---

## 4. Where remote-first lets sci-pi clearly beat both

1. **Approve from the lock screen.** ntfy and Web Push notifications can carry **action buttons** ("Allow once", "Deny",
   "Reply…") that POST straight to the daemon over the tailnet, where whois identifies the caller with no token. T3 needs T3
   Connect for push and opens the thread to act. OMP has no push at all.
2. **No vendor account or relay.** T3's phone push and no-VPN access depend on T3 Connect (a Clerk account and Cloudflare
   tunnels). OMP's sharing depends on a relay. sci-pi needs only SSH plus Tailscale (identity) plus ntfy, which can be
   self-hosted. Make this the headline story.
3. **Cross-host orchestration.** In T3, "a project and its threads belong to one environment" (`t3:docs/internals/remote.md`),
   and its orchestrator spawns threads on the same server. The sci-pi hub can let an agent on the laptop spawn work on the
   GPU box or the homelab through the `sci-pi` MCP server (#16), with results flowing into one inbox.
4. **Move a live session between hosts.** Push the worktree branch, replay a handoff from our event log on the target host,
   and continue there: start on the laptop, migrate to the homelab before closing the lid. T3 can only move a *draft*
   between machines.
5. **Your OMP, finally with a GUI.** `omp acp` already exposes load, list, fork and resume. sci-pi becomes the daemon and web
   UI that `omp#5742` and `omp#436` ask for, while keeping OMP's tools (hashline, LSP, subagents).
6. **Daemon-owned timers.** Resume at limit reset, PR babysitting, scheduled tasks and auto-settle all run in sci-pi daemon, so
   they keep working on a headless box with every client closed and survive daemon restarts (they are persisted as events).
7. **Per-session preview URLs on the tailnet.** Proxy each session's dev server at a stable tailnet URL, for example
   `https://<host>.<tailnet>.ts.net:7433/p/<session>/`, so you can check the agent's UI work from a phone. T3's live
   preview needs its Electron app. OMP has none.
8. **Identity-attributed multiplayer.** Tailscale whois gives real identities, so "who approved `rm -rf`" can be audited per
   person. OMP collab is link-possession only, and T3 is single-user.
9. **The event log as a product.** Time-travel replay of any session, FTS across every host, and read-only share links
   bounded by tailnet ACLs, all from the one append-only log we already keep.
10. **Durable terminals and flaky networks.** Treat PTYs and agent turns alike, as replayable streams with `after=N` cursors:
    a train-tunnel disconnect loses nothing, and the phone catches up in one request.

---

## Appendix A: T3 Code details, UX and weaknesses

**Distinctive UX that users praise**

- A single clean GUI over several agents, using the official SDKs rather than `-p` hacks (Better Stack guide, below).
- Fast for an Electron app ("proof electron apps don't have to suck", quoted on t3.codes).
- Linux treated as first-class, including AUR packages kept in the repo and an Omarchy/Hyprland screenshot helper (`t3:docs/user/snap-shot.md`).
- The git and PR integration.
- Tool calls kept visually separate from prose.

Notable polish:

- Inline context chips: files, terminal excerpts, PRs, threads, quotes (`t3:docs/user/composer.md`).
- Undo for sidebar actions.
- Hold-to-quit.
- Server-side prompt queue with "Resume" after a restart.
- Auto-settle of finished threads.
- Pooled subscription limits across accounts, with mobile widgets (`t3:docs/user/usage.md`).
- Agent PR babysitting through `watch_pull_request` (`t3:docs/user/source-control.md`).

**Weaknesses and complaints**

- **Slower and costlier than the native CLIs:**
  - `t3#695`: the same task took 15+ minutes in T3 against 4m35s in Codex (open).
  - `t3#7338`: Claude usage was 3-5x higher than in the native app.
- **Telemetry trust:** `t3#4123`, "promise no opt-out telemetry while telemetry has been on the whole time".
- **The V1→V2 orchestrator migration** (`t3#14871`):
  - It drops checkpoints, diffs, tool history and plans.
  - Store mobile apps cannot connect to V2 servers (TestFlight only).
  - Clients and servers must be upgraded in lockstep.
- **Stuck or hung threads:**
  - `t3#2234`: a thread cannot be stopped.
  - `t3#2778`: a session hung after spawning subagents.
  - `t3#4852`: threads stay "Working" forever when the environment is unavailable.
  - `t3#2644`.
- **Diff and checkpoint bugs:** `t3#4022` (diff scopes empty for in-place projects) and `t3#2017`.
- **Claude billing:** according to Theo's post, official Claude support bills API credits (https://x.com/theo/status/2054613327696056660). Not independently verified.
- **Complexity and openness:** the feature surface is enormous, contributions are mostly closed (`t3:README.md`), and there is no thread export.

## Appendix B: OMP details, UX and weaknesses

**Distinctive UX that users praise**

- Edit reliability with hashline: the project reports Grok Code Fast going from 6.7% to 68.3%. These are self-reported benchmarks (`omp:README.md`).
- LSP-aware renames and a real debugger.
- Agent Hub for watching and steering subagents.
- The advisor model.
- `/collab` live sharing with a browser guest client.
- Native speed: in-process grep and bash.
- Model roles (`smol`, `slow`, `plan`, `commit` and others) with fallback chains.
- It reads existing `.claude`, `.cursor` and `.codex` rules and MCP config with no migration.
- Magic keywords such as `ultrathink` and `orchestrate` (`omp:docs/magic-keywords.md`).
- `/btw` side questions and `/tan` background tangents (`omp:packages/coding-agent/src/slash-commands/builtin-session.ts`).

**Weaknesses and complaints**

- **No GUI, daemon or remote control:** `omp#5742` and `omp#436` (redirected to discussion #6460), and `omp#8077` asks for cross-session messaging.
- **TUI rendering and terminal issues:** `omp#9780` (streamed rows re-committed), `omp#10232` (asks for an alternate-screen transcript), `omp#5618` (tmux input deafness).
- **Provider quirks:** repeated Antigravity 429s (`omp#11689`, `omp#12655`, `omp#11699`), and some models struggle with hashline (`omp#9717`, `omp#3772`).
- **Compaction:** `omp#9235`, "freed too little context".
- **Release churn:** several releases a day (v18.x), with crash-on-upgrade incidents (`omp#651`, `omp#116`).
- **Unsafe default:** the default approval mode is `yolo` (`omp:docs/approval-mode.md`).
- **No workspace file checkpoints or revert** for the main session: `/branch` and `/tree` rewind the conversation only. This is from a source search and not exhaustively verified.

## Sources

- Repos: https://github.com/pingdotgg/t3code and https://github.com/can1357/oh-my-pi, cloned to `/tmp/research/`. Paths are cited inline.
- T3 release notes: `gh release view <tag> -R pingdotgg/t3code` (v0.0.2 through v0.0.45).
- OMP changelog: `omp:packages/coding-agent/CHANGELOG.md`, plus `omp --help` and the subcommand `--help` output from the local v18.6.0 binary.
- Reviews:
  - https://betterstack.com/community/guides/ai/t3-code/
  - https://flaviocopes.com/t3-code/
  - https://daily.dev/posts/t3-code-another-agentic-gui-that-is-good-but-not-usable--jorrdkvkz
  - https://leonardmartinis.com/blog/t3-code-review/
  - https://betterstack.com/community/guides/ai/oh-my-pi-ai-coding-agent/
- Home pages: https://t3.codes/ and https://omp.sh
