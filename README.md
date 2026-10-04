# outpost

Remote-first coding agents. Install a headless daemon on the machines you own, then drive
Claude Code, Codex or OpenCode on them from your laptop, a browser or your phone. Close the
laptop; the agents keep going, and the approvals and results wait for you.

```
 laptop / phone                               your machines
 ┌──────────────────┐   tailnet (direct)    ┌──────────────────────────────────┐
 │ web UI           │ ────────────────────► │ outpost serve  (systemd --user)  │
 │  outpost ui (hub)│   or SSH tunnel       │  ├─ session actors ─► ACP agents │
 └──────────────────┘ ────────────────────► │  ├─ SQLite event log             │
          ▲  ntfy push: "approval needed"   │  ├─ git worktree per session     │
          └─────────────────────────────────┤  └─ diff / inbox / fs API        │
                                            └──────────────────────────────────┘
```

## How it works

- **Sessions are event-sourced.** Every chunk, tool call, approval and turn is appended to
  SQLite. UIs are viewers: they replay from a cursor and stream live, so a laptop that slept
  for six hours catches up exactly, and several devices can watch one session.
- **Agents speak ACP** ([Agent Client Protocol](https://agentclientprotocol.com)). outpost
  doesn't ship its own agent loop; any ACP agent works (`config.toml` → `[agents.*]`).
- **Sessions outlive everything.** Client disconnects don't matter; the daemon restarting
  marks sessions `detached` and the next prompt resumes the agent's own context
  (`session/resume`, falling back to `session/load`).
- **Approvals are asynchronous.** Permission requests land in an inbox across sessions and hosts
  and are pushed via ntfy; answer from any device.
- **Worktree per session** (optional) so parallel agents never collide; the diff view compares
  against the commit the worktree branched from, untracked files included.

## Tailscale (native)

When `tailscaled` runs on a host, the daemon also listens on its tailnet IPs (`:7433`):

- **HTTPS** with a Tailscale-issued cert when the daemon may fetch one
  (`sudo tailscale set --operator=$USER` on that host), else plain HTTP over WireGuard.
- **No tokens on the tailnet**: callers are identified with tailscaled's `whois`; logins in
  `tailscale.allow` (default: the node's owner; `outpost add` adds yours) get in directly.
  Open `https://<host>.<tailnet>.ts.net:7433` on your phone and you're in.
- **Discovery**: `outpost ui` probes online tailnet peers and lists every outpost it finds.

## Usage

```sh
cargo build --release            # embeds web/dist (cd web && bun install && bun run build first)

outpost add homelab              # scp itself over, install systemd user unit, enable linger
outpost ui                       # hub on http://127.0.0.1:7430 with every host
outpost doctor                   # agents on PATH? tailscale? https?
```

On the remote host, agents need their own runtime and login (e.g. `npx` and a logged-in
`claude`). The install captures your login shell's `PATH` into `~/.config/outpost/env`, so
whatever works in your shell works for the daemon.

Config lives in `~/.config/outpost/config.toml`:

```toml
bind = "127.0.0.1:7433"
ntfy_url = "https://ntfy.sh/your-secret-topic"   # optional push

[tailscale]
enabled = true
port = 7433
allow = ["you@example.com"]

[agents.claude]
name = "Claude Code"
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
```

See [docs/PROTOCOL.md](docs/PROTOCOL.md) for the API.
