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

- **Providers, over raw HTTP:** the Anthropic Messages API (streaming, adaptive thinking,
  effort, prompt caching, refusal fallbacks), any OpenAI-compatible endpoint (OpenAI,
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
sci-pi doctor             # agents, Tailscale, HTTPS
sci-pi update             # push this build to every host (refuses while agents are mid-turn)
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
survives restarts in `data/agent/learned-windows.json`. Optional-field rejections are remembered
per endpoint and model for the running agent. Anthropic requests require a known output
limit; missing metadata produces an error rather than a guessed token cap.

USD usage costs are computed only from reported rates on pay-per-token endpoints.
CLIProxyAPI does not get API-price estimates; other subscriptions can set
`pay_per_token = false`. Missing cache prices are not substituted with guessed rates.

A note on subscriptions: ChatGPT plans can be used by third-party tools. Claude Pro/Max
subscriptions may only be used through Anthropic's own Claude Code – run Claude Code as the
session's agent for that, or use an API key with sci-pi's harness.

See [docs/PROTOCOL.md](docs/PROTOCOL.md) for the daemon API.

## Acknowledgements

sci-pi stands on [pi](https://github.com/earendil-works/pi) by Mario Zechner and
[oh-my-pi](https://github.com/can1357/oh-my-pi) by can1357 (both MIT): the line-addressed
"hashline" editing approach and much of the thinking about what makes a harness fast come from
them – hence the name. The code here is an independent Rust implementation.
[T3 Code](https://github.com/pingdotgg/t3code) shaped a lot of the UI.
