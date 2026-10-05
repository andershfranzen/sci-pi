# sci-pi

A remote-first coding agent harness, written in Rust. Install one binary on the machines you
own; drive its agents from your laptop, a browser or your phone. Close the laptop – the agents
keep working, and approvals and results wait for you.

```
 laptop / phone                                your machines
 ┌───────────────────┐   tailnet (direct)    ┌───────────────────────────────────────┐
 │ web UI            │ ────────────────────► │ sci-pi serve   (systemd --user)       │
 │  sci-pi ui (hub)  │   or SSH tunnel       │  ├─ session actors ──► agents (ACP)   │
 └───────────────────┘ ────────────────────► │  │    ├─ sci-pi acp  (native harness)  │
          ▲  push: "approval needed"         │  │    └─ Claude Code, Codex, OpenCode… │
          └──────────────────────────────────┤  ├─ SQLite event log + checkpoints    │
                                             │  └─ worktrees, terminal, git, search  │
                                             └───────────────────────────────────────┘
```

## The harness

`sci-pi acp` is sci-pi's own agent loop, served over the
[Agent Client Protocol](https://agentclientprotocol.com) – the daemon runs it, and ACP editors
(Zed, …) can too.

- **Providers, over raw HTTP:** the Anthropic Messages API (API keys or OMP-style browser
  OAuth, streaming, adaptive thinking, effort, prompt caching, refusal fallbacks), any OpenAI-compatible endpoint (OpenAI,
  OpenRouter, llama.cpp, vLLM, Ollama), and [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)
  servers (models discovered automatically).
- **Runtime model metadata:** provider catalogs supply model names, context/output limits,
  effort choices and defaults, thinking, Fast mode, compaction support and prices.
  CLIProxyAPI's own catalog takes precedence over models.dev; unsupported capabilities stay
  unsupported, and unknown limits stay unknown.
- **Line-addressed edits.** `read_file` tags each file version with a short hash and numbers its
  lines; `edit_lines` replaces line ranges against that tag. The model never re-types old code,
  stale edits are rejected, and every edit returns the renumbered region so edits chain without
  re-reading.
- **Fast paths:** grep/glob in-process (ripgrep's crates), independent read-only tool calls in
  parallel, long output clipped to head and tail, a stable prompt prefix that stays cached,
  @-mentioned files inlined with their tag.
- **Subagents:** a `task` tool hands a self-contained job to a fresh context. Read-only
  "explore" subagents issued together run in parallel; "work" subagents can edit behind the
  same approvals. Only their reports enter the parent's context.
- **Automatic compaction:** past `native.compact_at_tokens`, or a configurable fraction of
  the known context window (default 80%), the conversation is summarized. Server compaction
  is used only when reported by the provider; otherwise the model writes the summary.
  Unknown windows rely on overflow recovery. `/compact` does it on demand.
- **Modes:** ask, accept edits, plan (read-only), autonomous – approvals show the diff and can
  be answered from any device.
- **Durable:** history is saved after every step; a restarted agent resumes the session and
  closes out tool calls that were interrupted.

Other ACP agents (Claude Code, Codex, OpenCode, oh-my-pi) can be used per session instead.

## The platform

- **Event-sourced sessions.** Every chunk, tool call and approval is appended to SQLite. UIs
  replay from a cursor and stream live, so a laptop that slept for hours catches up exactly.
- **Checkpoints** before and after every turn (git refs, untracked files included): per-turn
  diffs, revert, and fork-from-any-turn – even onto a different agent.
- **Worktree per session**, an editable prompt queue, a persistent terminal per session,
  commit/push/PR on the host, full-text search across sessions and hosts.
- **Native Tailscale:** the daemon listens on the tailnet with a Tailscale HTTPS cert and
  identifies callers with `whois` – no tokens on your tailnet. The hub discovers every sci-pi
  on it.

## Usage

```sh
cd web && bun install && bun run build && cd ..   # the UI is embedded in the binary
cargo build --release

sci-pi add homelab        # install on a host over SSH (systemd user unit + linger)
sci-pi ui                 # multi-host UI on http://127.0.0.1:7430
sci-pi auth set anthropic # store a provider key on this host (reads stdin)
sci-pi auth login anthropic # browser OAuth, including remote/headless hosts
sci-pi auth status          # credential source and saved OAuth account (no secrets)
sci-pi doctor             # agents, Tailscale, HTTPS
sci-pi update             # push this build to every host (refuses active or queued work)
sci-pi service install    # install/upgrade this laptop's daemon with the same restart guard
sci-pi pair --name Laptop # pair a local browser with its own revocable credential
```

`~/.config/sci-pi/config.toml`:

```toml
[native]
model = ""                    # first available model; or an explicit "<provider>/<model>"
effort = ""                   # the selected model's reported default
compact_ratio = 0.8            # when compact_at_tokens is unset and the window is known
# compact_at_tokens = 300000   # optional explicit threshold
# subagent_model = "..."       # optional model override; otherwise use the session's model

[native.providers.cliproxy]      # a CLIProxyAPI server
kind = "cliproxy"
base_url = "https://homelab.example.ts.net"

[native.providers.local]         # any OpenAI-compatible server
base_url = "http://gpu-box:8080/v1"
models = ["qwen3.6-35b"]
# pay_per_token = false        # set for subscription endpoints (automatic for CLIProxyAPI)

[tailscale]
allow = ["you@example.com"]

ntfy_url = "https://ntfy.sh/your-secret-topic"   # optional phone push
```

Keys come from `<PROVIDER>_API_KEY` env vars or `sci-pi auth set <provider>`.

The model picker includes provider descriptions and runtime context/output limits.
Effort and Fast controls appear only when reported for the selected model; changing models
selects that model's effort default and resets Fast. Explicit `native.context_windows`
overrides are supported, but a smaller provider-reported overflow limit always wins and
survives restarts in `data/agent/learned-windows.json`, scoped by normalized endpoint and model.
Legacy model-only learned limits are ignored because their endpoint cannot be established safely.
Optional-field rejections are remembered per endpoint and model for the running agent. Anthropic requests require a known output
limit; missing metadata produces an error rather than a guessed token cap.

Direct Anthropic inference uses automatic prompt caching. Anthropic-compatible proxies
own cache placement; sci-pi does not add top-level automatic caching on that route,
which would consume an extra slot beyond the proxy's four explicit cache breakpoints.

USD usage costs are computed only from reported rates on pay-per-token endpoints.
CLIProxyAPI and Anthropic OAuth do not get API-price estimates; other subscriptions can set
`pay_per_token = false`. Missing cache prices are not substituted with guessed rates.

For Anthropic browser OAuth, run `sci-pi auth login anthropic` **on the agent's host**, open
the printed URL on any device, and complete authorization. A local browser can return to
`http://localhost:54545/callback`; for remote hosts, paste the final callback URL or
`code#state` into the host's terminal. A bare authorization code is also accepted and bound
to that login's PKCE verifier.

The native agent uses [OMP's Anthropic OAuth approach](https://github.com/can1357/oh-my-pi/blob/main/packages/catalog/src/compat/rules/auth/anthropic.kdl):
Bearer authentication, Claude-compatible request identity, wire-only tool-name mapping,
and automatic refresh five minutes before token expiry. Signed thinking blocks remain intact
when tools are replayed or the conversation is compacted. OAuth tokens stay in
`~/.config/sci-pi/anthropic-oauth.json` (0600, atomically replaced; concurrent refreshes are
serialized). `SCIPI_HOME` relocates this alongside the other host configuration.

Credential priority is `ANTHROPIC_API_KEY`, then a saved Anthropic API key, then the saved
OAuth login. `sci-pi auth logout anthropic` removes only sci-pi's OAuth tokens; it leaves API
keys and other clients' credentials untouched. Configured Anthropic/CLIProxyAPI endpoints
continue to use their own provider credentials. Provider credentials are separate from the
daemon token used to connect the UI.

OAuth compatibility does not establish Anthropic permission to use a subscription from a
third-party client. Check the current subscription terms; API keys and the Claude Code
session adapter remain available alternatives.

## Diagnostics and recovery

The session header's **Diagnostics** opens HTTP attempts for the current or an earlier turn:
route, sanitized endpoint, model, effective effort/Fast/thinking/compaction flags, cache
placement ownership, limits, HTTP status, upstream request ID, and remembered optional-field
rejections. No prompts, tool contents, signed thinking, credentials, or upstream error bodies
are retained in these diagnostic records. HTTP headers received are not evidence of a completed
turn. The **Model metadata** tab shows each effective value's source: provider, models.dev
fallback, user override, or an endpoint-and-model-specific learned overflow limit.

Failed, cancelled, and interrupted turns keep partial output and tool effects. An unsuccessful
turn or daemon restart during a turn pauses queued work; idle Stop does not strand later prompts.
**Resume** starts the adapter and releases the retained queue without repeating the earlier
prompt. **Retry as new attempt** explicitly resends the original prompt and attachments as a
new numbered turn. Neither action rolls back files, commands, or external effects; a retry may
repeat them. Interrupted tool outcomes can remain unknown. Incomplete provider streams fail
without retransmission or execution of unfinished tool calls.

## Pairing and updates

Run `sci-pi pair --name Laptop` on the daemon's host. Open its one-use, 120-second pairing
link on a loopback address; `--no-open` prints it without launching a browser. The code lives
only in the URL fragment and the UI removes it before API requests. Issuance requires the
CLI/admin bearer; redemption requires a real loopback peer, a loopback Host, and matching
browser Origin. Remote/Tailscale origins cannot redeem it; an SSH loopback tunnel can.

The **Devices** dialog lists and independently revokes browser credentials, closing that
credential's API and terminal sockets without changing other devices or the CLI/admin token.
An administrator credential entered into this management dialog stays in memory until it
closes. Device credentials are persisted as hashes in `devices.json` (0600); the master token
remains a private local CLI/bootstrap credential. HTTP uses bearer headers, WebSockets use
base64url bearer subprotocols, and attachment previews use authenticated Blob images, never
active Blob documents. Token query URLs are no longer supported.

**Only pair trusted devices:** the app grants terminals and agent tools full execution as the
daemon's Unix account. Device-management authorization is not an OS privilege boundary.
Revocation invalidates that bearer and closes its sockets; it cannot undo commands, remove
installed access, invalidate copied master credentials, or revoke an independently allowed
Tailscale identity.

The sidebar and `sci-pi --version` identify the commit and unique source/build fingerprint.
Archives report an unknown revision rather than borrowing an unrelated checkout's HEAD.
Local and remote updates refuse active turns, approvals, queued work, and uncertain/auth-failed
inspection. `--force` explicitly bypasses the preflight guard. After restart, installation
verifies authenticated session inspection and the exact running build identity, not just the
package version. Reload an existing browser tab after upgrading to load the new embedded UI.

## Verification

`bun run --cwd web build` checks and builds the UI. `cargo test --locked` includes real-binary,
isolated provider boundary scenarios for Anthropic API-key/OAuth, CLIProxyAPI, and OpenAI:
multi-round tools, signed replay, compaction/continuation, bounded optional rejection, partial
stream errors and premature EOF, cancellation, and proxy cache-budget interaction.

See [docs/PROTOCOL.md](docs/PROTOCOL.md) for the daemon API.

## Acknowledgements

sci-pi stands on [pi](https://github.com/earendil-works/pi) by Mario Zechner and
[oh-my-pi](https://github.com/can1357/oh-my-pi) by can1357 (both MIT): the line-addressed
"hashline" editing approach and much of the thinking about what makes a harness fast come from
them – hence the name. The code here is an independent Rust implementation.
[T3 Code](https://github.com/pingdotgg/t3code) shaped a lot of the UI.
