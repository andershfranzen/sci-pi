// In-memory mock of the sci-pi daemon API (docs/PROTOCOL.md) for UI development.
//
//   bun run build && bun run mock      → http://127.0.0.1:7499/#token=dev
//
// Env: PORT (default 7499), MOCK_REQUIRE_TOKEN=1 (reject token-less requests instead of
// simulating tailnet access), MOCK_HUB=1 (also answer /hub/hosts with two hosts → hub mode).
import { join, normalize } from "node:path";
import type { ServerWebSocket } from "bun";

const PORT = Number(process.env.PORT ?? 7499);
const TOKEN = "dev";
const REQUIRE_TOKEN = process.env.MOCK_REQUIRE_TOKEN === "1";
const HUB = process.env.MOCK_HUB === "1";
const DIST = join(import.meta.dir, "..", "dist");
const HOME = "/home/dev";

type Status = "starting" | "idle" | "running" | "awaiting_permission" | "detached" | "stopped" | "error";

interface Session {
  id: string;
  title: string;
  agent: string;
  project: string;
  cwd: string;
  branch: string | null;
  base_commit: string | null;
  status: Status;
  status_message: string | null;
  mode: string | null;
  modes: { id: string; name: string; description?: string }[];
  usage: { used: number; size: number; cost?: { amount: number; currency: string } } | null;
  queued: number;
  queue: { id: string; text: string; attachments: any[] }[];
  config_options: any[];
  commands: { name: string; description: string; hint: string | null }[];
  prompt_caps: { image?: boolean; embeddedContext?: boolean } | null;
  turns: number;
  pinned: boolean;
  archived: boolean;
  pr_url: string | null;
  pending_permissions: number;
  created_at: number;
  updated_at: number;
}

interface Ev {
  id: number;
  session_id: string;
  ts: number;
  kind: string;
  data: any;
}

interface Pending {
  session_id: string;
  request_id: string;
  tool_call: any;
  options: { optionId: string; name: string; kind: string }[];
  ts: number;
  onResolve: (optionId: string | null) => void;
}

// ------------------------------------------------------------------ state

let nextId = 1;
const events: Ev[] = [];
const sessions = new Map<string, Session>();
const pending = new Map<string, Pending>();
const turnDiffs = new Map<string, Map<number, { diff: string; files: { path: string; status: string }[] }>>();
const attachments = new Map<string, { mime: string; bytes: Uint8Array }>();
interface GitState {
  dirty: number;
  commits: number;
  upstream: boolean;
  ahead: number;
  behind: number;
  remote: string | null;
}
const gitState = new Map<string, GitState>();
const cancelled = new Set<string>();
let server: ReturnType<typeof Bun.serve>;

const MIN = 60_000;
const sha = () => crypto.randomUUID().replace(/-/g, "") + "00000000";

function startTurn(sid: string, text: string, atts: any[] = [], ts = now()) {
  const s = sessions.get(sid)!;
  s.turns++;
  emit(sid, "user_prompt", { text, attachments: atts, turn: s.turns, checkpoint: sha() }, ts);
  touch(sid, { turns: s.turns }, ts);
}

function finishTurn(sid: string, stop_reason = "end_turn", ts = now()) {
  const s = sessions.get(sid)!;
  emit(sid, "turn_end", { stop_reason, turn: s.turns, checkpoint: sha(), usage: { inputTokens: 1200, outputTokens: 340 } }, ts);
}

function setTurnDiff(sid: string, turn: number, diff: string, files: { path: string; status: string }[]) {
  let m = turnDiffs.get(sid);
  if (!m) turnDiffs.set(sid, (m = new Map()));
  m.set(turn, { diff, files });
}

const now = () => Date.now();

function broadcast(msg: unknown) {
  server?.publish("all", JSON.stringify(msg));
}

function touch(sid: string, patch: Partial<Session> = {}, ts = now()) {
  const s = sessions.get(sid);
  if (!s) return;
  Object.assign(s, patch, { updated_at: Math.max(s.updated_at, ts) });
  broadcast({ type: "session", session: s });
}

function emit(sid: string, kind: string, data: any, ts = now()): Ev {
  const e: Ev = { id: nextId++, session_id: sid, ts, kind, data };
  events.push(e);
  broadcast({ type: "event", event: e });
  touch(sid, {}, ts);
  return e;
}

const upd = (sid: string, data: any, ts?: number) => emit(sid, "update", data, ts);
const say = (sid: string, text: string, messageId?: string, ts?: number) =>
  upd(sid, { sessionUpdate: "agent_message_chunk", content: { type: "text", text }, ...(messageId ? { messageId } : {}) }, ts);
const think = (sid: string, text: string, ts?: number) =>
  upd(sid, { sessionUpdate: "agent_thought_chunk", content: { type: "text", text } }, ts);
const setStatus = (sid: string, status: Status, ts?: number, message?: string) => {
  emit(sid, "status", message ? { status, message } : { status }, ts);
  touch(sid, { status, status_message: message ?? null }, ts);
};

function requestPermission(
  sid: string,
  tool_call: any,
  options: Pending["options"],
  onResolve: Pending["onResolve"],
  ts = now(),
) {
  const request_id = `req_${Math.random().toString(36).slice(2, 10)}`;
  pending.set(request_id, { session_id: sid, request_id, tool_call, options, ts, onResolve });
  emit(sid, "permission_request", { request_id, tool_call, options }, ts);
  const s = sessions.get(sid)!;
  setStatus(sid, "awaiting_permission", ts);
  touch(sid, { pending_permissions: s.pending_permissions + 1 }, ts);
  return request_id;
}

function resolvePermission(request_id: string, option_id: string | null) {
  const p = pending.get(request_id);
  if (!p) return false;
  pending.delete(request_id);
  emit(p.session_id, "permission_resolved", {
    request_id,
    outcome: option_id === null ? "cancelled" : "selected",
    option_id,
  });
  const s = sessions.get(p.session_id)!;
  touch(p.session_id, { pending_permissions: Math.max(0, s.pending_permissions - 1) });
  p.onResolve(option_id);
  return true;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function stream(sid: string, text: string, messageId: string, delay = 60) {
  const words = text.split(/(?<=\s)/);
  for (let i = 0; i < words.length; i += 3) {
    if (cancelled.has(sid)) return;
    say(sid, words.slice(i, i + 3).join(""), messageId);
    await sleep(delay);
  }
}

async function endTurn(sid: string, stop_reason = "end_turn") {
  finishTurn(sid, stop_reason);
  setStatus(sid, "idle");
  await dequeue(sid);
}

async function dequeue(sid: string) {
  const s = sessions.get(sid);
  if (!s) return;
  const next = s.queue.shift();
  touch(sid, { queue: s.queue, queued: s.queue.length });
  if (next) {
    await sleep(300);
    void runTurn(sid, next.text, next.attachments);
  }
}

const PERM_OPTS = [
  { optionId: "allow", name: "Allow", kind: "allow_once" },
  { optionId: "allow_always", name: "Always allow", kind: "allow_always" },
  { optionId: "reject", name: "Reject", kind: "reject_once" },
];

/** Generic fake turn for prompts typed into the mock. */
async function runTurn(sid: string, text: string, atts: any[] = []) {
  cancelled.delete(sid);
  startTurn(sid, text, atts);
  setStatus(sid, "running");
  await sleep(400);
  think(sid, `The user wants: "${text.slice(0, 80)}". I should look at the relevant files first.`);
  await sleep(500);
  const tc = `tc_${nextId}`;
  upd(sid, {
    sessionUpdate: "tool_call",
    toolCallId: tc,
    title: "Read README.md",
    kind: "read",
    status: "in_progress",
    rawInput: { path: "README.md" },
    content: [],
    locations: [{ path: "README.md" }],
  });
  await sleep(700);
  upd(sid, {
    sessionUpdate: "tool_call_update",
    toolCallId: tc,
    status: "completed",
    content: [{ type: "content", content: { type: "text", text: "# sci-pi\n\nRemote-first coding-agent harness.\n" } }],
  });
  if (cancelled.has(sid)) return;
  const turn = sessions.get(sid)!.turns;
  setTurnDiff(
    sid,
    turn,
    `diff --git a/NOTES.md b/NOTES.md\nindex 1111111..2222222 100644\n--- a/NOTES.md\n+++ b/NOTES.md\n@@ -1,2 +1,3 @@\n # Notes\n \n+- ${text.replace(/\n/g, " ").slice(0, 60)}\n`,
    [{ path: "NOTES.md", status: "M" }],
  );
  const g = gitState.get(sid);
  if (g) g.dirty++;
  const mid = `m_${nextId}`;
  await stream(
    sid,
    `Sure — here's what I found.\n\n- The README is short and up to date.\n- Nothing else needs to change for **${text.slice(0, 40)}**.\n\n\`\`\`sh\ncargo build --release\n\`\`\`\n\nLet me know if you want me to go further.`,
    mid,
  );
  if (cancelled.has(sid)) return;
  await endTurn(sid);
}

// ------------------------------------------------------------------ fixtures

const MODES = [
  { id: "default", name: "Default", description: "Ask before edits and commands" },
  { id: "acceptEdits", name: "Accept edits", description: "Auto-approve file edits" },
  { id: "plan", name: "Plan", description: "Read-only planning" },
];

function mkSession(p: Partial<Session> & Pick<Session, "id" | "title" | "agent" | "project">, created: number): Session {
  const s: Session = {
    cwd: p.project,
    branch: null,
    base_commit: null,
    status: "idle",
    status_message: null,
    mode: null,
    modes: [],
    usage: null,
    queued: 0,
    queue: [],
    config_options: [],
    commands: [],
    prompt_caps: { image: true, embeddedContext: true },
    turns: 0,
    pinned: false,
    archived: false,
    pr_url: null,
    pending_permissions: 0,
    created_at: created,
    updated_at: created,
    ...p,
  };
  sessions.set(s.id, s);
  if (s.branch) gitState.set(s.id, { dirty: 0, commits: 0, upstream: false, ahead: 0, behind: 0, remote: "git@github.com:dev/" + s.project.split("/").pop() + ".git" });
  return s;
}

function claudeConfig(mode = "default", model = "opus", effort = "high") {
  return [
    {
      id: "mode",
      name: "Mode",
      category: "mode",
      type: "select",
      currentValue: mode,
      description: "Session permission mode",
      options: [
        { value: "default", name: "Manual", description: "Always ask before making changes" },
        { value: "acceptEdits", name: "Accept edits", description: "Automatically accept all file edits" },
        { value: "plan", name: "Plan", description: "Create a plan before making changes" },
        { value: "bypassPermissions", name: "Bypass permissions", description: "Accepts all permissions" },
      ],
    },
    {
      id: "model",
      name: "Model",
      category: "model",
      type: "select",
      currentValue: model,
      description: "AI model to use",
      options: [
        { value: "opus", name: "Opus 5.5" },
        { value: "sonnet", name: "Sonnet 5.5" },
        { value: "haiku", name: "Haiku 4.5" },
      ],
    },
    {
      id: "effort",
      name: "Effort",
      category: "thought_level",
      type: "select",
      currentValue: effort,
      options: ["low", "medium", "high", "max"].map((v) => ({ value: v, name: v[0].toUpperCase() + v.slice(1) })),
    },
    { id: "fast", name: "Fast mode", category: "model_config", type: "select", currentValue: "off", options: [{ value: "on", name: "On" }, { value: "off", name: "Off" }] },
  ];
}

function codexConfig() {
  return [
    { id: "model", name: "Model", category: "model", type: "select", currentValue: "gpt-5.5-codex", options: [{ value: "gpt-5.5-codex", name: "gpt-5.5-codex" }, { value: "gpt-5.5", name: "gpt-5.5" }] },
    { id: "reasoning", name: "Reasoning", category: "thought_level", type: "select", currentValue: "medium", options: ["minimal", "low", "medium", "high"].map((v) => ({ value: v, name: v })) },
    { id: "sandbox", name: "Sandbox", category: "other", type: "text", currentValue: "workspace-write" },
  ];
}

const CLAUDE_COMMANDS = [
  { name: "compact", description: "Free up context by summarizing the conversation so far", hint: "<optional custom summarization instructions>" },
  { name: "review", description: "Review the current diff for correctness bugs", hint: "[low|medium|high] [<pr#>|<branch>]" },
  { name: "init", description: "Initialize a CLAUDE.md file with codebase documentation", hint: null },
  { name: "simplify", description: "Review the changed code for reuse, simplification and efficiency, then apply fixes", hint: "[<target>]" },
  { name: "security-review", description: "Complete a security review of the pending changes on the current branch", hint: null },
  { name: "loop", description: "Run a prompt or slash command on a recurring interval", hint: "[interval] [prompt]" },
  { name: "debug", description: "Enable debug logging for this session and help diagnose issues", hint: "[issue description]" },
];

const AUTH_OLD = `import { createHmac, timingSafeEqual } from "node:crypto";
import type { Request, Response, NextFunction } from "express";

const SECRET = process.env.AUTH_SECRET!;

export function verifyToken(token: string): string | null {
  const [payload, sig] = token.split(".");
  if (!payload || !sig) return null;
  const expected = createHmac("sha256", SECRET).update(payload).digest("base64url");
  if (!timingSafeEqual(Buffer.from(sig), Buffer.from(expected))) return null;
  const { sub, exp } = JSON.parse(Buffer.from(payload, "base64url").toString());
  if (exp < Date.now() / 1000) return null;
  return sub;
}

export function requireAuth(req: Request, res: Response, next: NextFunction) {
  const header = req.headers.authorization ?? "";
  const token = header.replace(/^Bearer /, "");
  const user = verifyToken(token);
  if (!user) return res.status(401).json({ error: "unauthorized" });
  req.user = user;
  next();
}
`;

const AUTH_NEW = `import { jwtVerify, type JWTPayload } from "jose";
import type { Request, Response, NextFunction } from "express";

const SECRET = new TextEncoder().encode(process.env.AUTH_SECRET!);

export async function verifyToken(token: string): Promise<string | null> {
  try {
    const { payload } = await jwtVerify<JWTPayload>(token, SECRET, {
      algorithms: ["HS256"],
      clockTolerance: 5,
    });
    return payload.sub ?? null;
  } catch {
    return null;
  }
}

export async function requireAuth(req: Request, res: Response, next: NextFunction) {
  const header = req.headers.authorization ?? "";
  const token = header.replace(/^Bearer /, "");
  const user = await verifyToken(token);
  if (!user) return res.status(401).json({ error: "unauthorized" });
  req.user = user;
  next();
}
`;

async function seed() {
  const t0 = now();

  // --- A: awaiting permission on an edit (diff + plan) -----------------------------------
  const A = "s_auth";
  let t = t0 - 26 * MIN;
  mkSession(
    {
      id: A,
      title: "Refactor auth middleware to verify JWTs with jose",
      agent: "claude",
      project: `${HOME}/code/webapp`,
      cwd: `${HOME}/.sci-pi/worktrees/webapp-3f2a9c1b`,
      branch: "sci-pi/3f2a9c1b",
      base_commit: "9c1e4b7d2a5f8e3b6c0d1a2f4e5b6c7d8e9f0a1b",
      mode: "default",
      modes: MODES,
      config_options: claudeConfig(),
      commands: CLAUDE_COMMANDS,
      pinned: true,
      usage: { used: 48_210, size: 200_000, cost: { amount: 0.42, currency: "USD" } },
    },
    t,
  );
  emit(A, "status", { status: "running" }, t);
  startTurn(
    A,
    "Refactor the auth middleware in src/middleware/auth.ts to verify JWTs with `jose` instead of the hand-rolled HMAC check. Keep the existing error responses and make sure the tests still pass.",
    [{ type: "file", path: "src/middleware/auth.ts" }],
    (t += 1000),
  );
  think(A, "Let me look at the current middleware and see how tokens are issued. ", (t += 2000));
  think(A, "If the issuer already produces standard HS256 JWTs, jose's jwtVerify is a drop-in replacement.", (t += 500));
  say(A, "I'll start by reading the current middleware ", "m1", (t += 1500));
  say(A, "and finding every caller of `verifyToken`.", "m1", (t += 200));
  upd(
    A,
    {
      sessionUpdate: "tool_call",
      toolCallId: "t1",
      title: "Read src/middleware/auth.ts",
      kind: "read",
      status: "completed",
      rawInput: { file_path: "src/middleware/auth.ts" },
      content: [{ type: "content", content: { type: "text", text: AUTH_OLD } }],
      locations: [{ path: "src/middleware/auth.ts" }],
    },
    (t += 1500),
  );
  upd(
    A,
    {
      sessionUpdate: "tool_call",
      toolCallId: "t2",
      title: 'grep -rn "verifyToken" src',
      kind: "search",
      status: "completed",
      rawInput: { pattern: "verifyToken", path: "src" },
      content: [
        {
          type: "content",
          content: {
            type: "text",
            text: "src/middleware/auth.ts:6:export function verifyToken(token: string): string | null {\nsrc/middleware/auth.ts:19:  const user = verifyToken(token);\nsrc/routes/ws.ts:42:    const user = verifyToken(url.searchParams.get(\"token\") ?? \"\");\ntest/auth.test.ts:12:  expect(verifyToken(signed)).toBe(\"user_1\");",
          },
        },
      ],
    },
    (t += 2500),
  );
  upd(
    A,
    {
      sessionUpdate: "plan",
      entries: [
        { content: "Read the current middleware and its callers", priority: "medium", status: "completed" },
        { content: "Add jose and rewrite verifyToken with jwtVerify", priority: "high", status: "in_progress" },
        { content: "Make callers await the now-async verifyToken (routes/ws.ts)", priority: "medium", status: "pending" },
        { content: "Run the auth test suite", priority: "medium", status: "pending" },
      ],
    },
    (t += 1500),
  );
  say(
    A,
    "The token format is already standard **HS256 JWT** (`header.payload.sig`), so `jose` can verify it directly. Two things change:\n\n1. `verifyToken` becomes `async` — `jwtVerify` returns a promise.\n2. `routes/ws.ts` must `await` it.\n\nHere's the middleware rewrite:",
    "m2",
    (t += 3000),
  );
  const editCall = {
    toolCallId: "t3",
    title: "Edit src/middleware/auth.ts",
    kind: "edit",
    status: "pending",
    rawInput: { file_path: "src/middleware/auth.ts" },
    content: [{ type: "diff", path: "src/middleware/auth.ts", oldText: AUTH_OLD, newText: AUTH_NEW }],
    locations: [{ path: "src/middleware/auth.ts" }],
  };
  upd(A, { sessionUpdate: "tool_call", ...editCall }, (t += 2000));
  requestPermission(
    A,
    editCall,
    [
      { optionId: "allow", name: "Allow", kind: "allow_once" },
      { optionId: "allow_always", name: "Always allow edits", kind: "allow_always" },
      { optionId: "reject", name: "Reject", kind: "reject_once" },
    ],
    async (opt) => {
      if (opt === "allow" || opt === "allow_always") {
        setStatus(A, "running");
        upd(A, { sessionUpdate: "tool_call_update", toolCallId: "t3", status: "completed" });
        await sleep(600);
        upd(A, {
          sessionUpdate: "plan",
          entries: [
            { content: "Read the current middleware and its callers", priority: "medium", status: "completed" },
            { content: "Add jose and rewrite verifyToken with jwtVerify", priority: "high", status: "completed" },
            { content: "Make callers await the now-async verifyToken (routes/ws.ts)", priority: "medium", status: "in_progress" },
            { content: "Run the auth test suite", priority: "medium", status: "pending" },
          ],
        });
        await stream(A, "Edit applied. Next I'll update `routes/ws.ts` to await the new async verifier.", "m3");
        await endTurn(A);
      } else {
        upd(A, { sessionUpdate: "tool_call_update", toolCallId: "t3", status: "failed" });
        setStatus(A, "running");
        await stream(A, "Understood — I won't touch the middleware. Want me to propose a smaller change instead?", "m3");
        await endTurn(A, opt === null ? "cancelled" : "end_turn");
      }
    },
    t,
  );

  // --- B: running, streams live forever ------------------------------------------------
  const B = "s_ratelimit";
  t = t0 - 9 * MIN;
  mkSession(
    {
      id: B,
      title: "Add rate limiting to the public API",
      agent: "codex",
      project: `${HOME}/code/sci-pi`,
      cwd: `${HOME}/.sci-pi/worktrees/sci-pi-a81c07e2`,
      branch: "sci-pi/a81c07e2",
      base_commit: "1f2e3d4c5b6a79880716253443526170",
      usage: { used: 121_400, size: 272_000 },
      config_options: codexConfig(),
    },
    t,
  );
  startTurn(B, "Sketch where rate limiting should live in this codebase. Don't change anything yet.", [], (t += 1000));
  setStatus(B, "running", t);
  say(B, "The cleanest place is an axum `Layer` in `src/server.rs`, keyed by the bearer token. I added a stub module so the shape is visible.", "b0", (t += 20_000));
  setTurnDiff(
    B,
    1,
    "diff --git a/src/ratelimit.rs b/src/ratelimit.rs\nnew file mode 100644\nindex 0000000..8a1f2c3\n--- /dev/null\n+++ b/src/ratelimit.rs\n@@ -0,0 +1,4 @@\n+//! Per-token rate limiting (token bucket).\n+pub struct RateLimiter;\n+\n+impl RateLimiter {}\n",
    [{ path: "src/ratelimit.rs", status: "A" }],
  );
  finishTurn(B, "end_turn", (t += 1000));
  startTurn(B, "Add per-token rate limiting to the public API (token bucket, 60 req/min, configurable). Return 429 with Retry-After.", [], (t += 30_000));
  gitState.get(B)!.dirty = 2;
  gitState.get(B)!.commits = 1;
  sessions.get(B)!.queue = [
    { id: "q1", text: "Also add a `rate_limit_burst` option, default 10.", attachments: [] },
    { id: "q2", text: "Then update the README's configuration section.", attachments: [] },
  ];
  sessions.get(B)!.queued = 2;
  say(B, "Plan: implement a small token-bucket in `src/ratelimit.rs`, wire it as an axum layer, add config + tests.", "b1", (t += 4000));
  upd(
    B,
    {
      sessionUpdate: "plan",
      entries: [
        { content: "Token bucket implementation", status: "completed", priority: "high" },
        { content: "axum middleware layer + 429 response", status: "in_progress", priority: "high" },
        { content: "Config option rate_limit_per_minute", status: "pending", priority: "medium" },
        { content: "Tests", status: "pending", priority: "medium" },
      ],
    },
    (t += 2000),
  );
  upd(
    B,
    {
      sessionUpdate: "tool_call",
      toolCallId: "b_t1",
      title: "cargo test -p sci-pi ratelimit",
      kind: "execute",
      status: "completed",
      rawInput: { command: ["cargo", "test", "-p", "sci-pi", "ratelimit"] },
      content: [
        {
          type: "content",
          content: {
            type: "text",
            text: "   Compiling sci-pi v0.1.0 (/home/dev/code/sci-pi)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.21s\n     Running unittests src/main.rs\n\nrunning 3 tests\ntest ratelimit::tests::refills_over_time ... ok\ntest ratelimit::tests::rejects_when_empty ... ok\ntest ratelimit::tests::separate_buckets_per_token ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored",
          },
        },
      ],
    },
    (t += 5000),
  );

  // --- C: idle, finished turn with a resolved permission ---------------------------------
  const C = "s_flaky";
  t = t0 - 3 * 60 * MIN;
  mkSession(
    {
      id: C,
      title: "Fix flaky websocket reconnect test",
      agent: "opencode",
      project: `${HOME}/code/sci-pi`,
      cwd: `${HOME}/.sci-pi/worktrees/sci-pi-c7d1e0a2`,
      branch: "sci-pi/c7d1e0a2",
      base_commit: "77aa11bb22cc33dd44ee55ff66778899aabbccdd",
      pr_url: "https://github.com/dev/sci-pi/pull/42",
      modes: [
        { id: "build", name: "Build" },
        { id: "plan", name: "Plan" },
      ],
      mode: "build",
      usage: { used: 22_800, size: 128_000 },
    },
    t,
  );
  attachments.set("ci-failure.png", { mime: "image/png", bytes: new Uint8Array(await Bun.file(join(import.meta.dir, "..", "public", "icons", "icon-512.png")).arrayBuffer()) });
  startTurn(
    C,
    "test_ws_reconnect fails ~1 in 10 runs on CI. Find out why and fix it. Screenshot of the CI run attached.",
    [
      { type: "image", name: "ci-failure.png", mime_type: "image/png" },
      { type: "file", path: "tests/ws_reconnect.rs" },
    ],
    (t += 1000),
  );
  setStatus(C, "running", t);
  const runCall = {
    toolCallId: "c_t1",
    title: "Run tests 20 times",
    kind: "execute",
    status: "pending",
    rawInput: { command: "for i in $(seq 20); do cargo test test_ws_reconnect -q || break; done" },
    content: [],
  };
  upd(C, { sessionUpdate: "tool_call", ...runCall }, (t += 3000));
  const rid = `req_c1`;
  emit(C, "permission_request", { request_id: rid, tool_call: runCall, options: PERM_OPTS }, (t += 500));
  emit(C, "permission_resolved", { request_id: rid, outcome: "selected", option_id: "allow" }, (t += 40_000));
  upd(
    C,
    {
      sessionUpdate: "tool_call_update",
      toolCallId: "c_t1",
      status: "completed",
      content: [{ type: "content", content: { type: "text", text: "test result: FAILED. 0 passed; 1 failed (run 7)\nassertion failed: events.len() == 3 (got 2)" } }],
    },
    (t += 30_000),
  );
  say(
    C,
    "Found it: the test asserts on the event count immediately after reconnecting, but the replay is delivered asynchronously. I changed it to wait for `after` to catch up (with a 2s timeout) instead of sleeping 50ms.\n\n| | before | after |\n|---|---|---|\n| failures / 200 runs | 19 | 0 |\n",
    "c2",
    (t += 10_000),
  );
  setTurnDiff(
    C,
    1,
    "diff --git a/tests/ws_reconnect.rs b/tests/ws_reconnect.rs\nindex 4c1d2e0..9b8a7f6 100644\n--- a/tests/ws_reconnect.rs\n+++ b/tests/ws_reconnect.rs\n@@ -40,6 +40,7 @@ async fn test_ws_reconnect() {\n     client.disconnect().await;\n     server.emit_events(3).await;\n     client.reconnect().await;\n-    tokio::time::sleep(Duration::from_millis(50)).await;\n+    // Replay is async: wait for the cursor instead of sleeping.\n+    client.wait_for_after(server.last_event_id(), Duration::from_secs(2)).await?;\n     assert_eq!(client.events().len(), 3);\n }\n",
    [{ path: "tests/ws_reconnect.rs", status: "M" }],
  );
  finishTurn(C, "end_turn", (t += 500));
  startTurn(C, "Commit that and open a PR.", [], (t += 60_000));
  emit(C, "git", { action: "commit", sha: "4f9e2a1c7b3d", message: "test: wait for replay cursor in ws_reconnect" }, (t += 4000));
  emit(C, "git", { action: "push", output: "To github.com:dev/sci-pi.git\n * [new branch] sci-pi/c7d1e0a2 -> sci-pi/c7d1e0a2" }, (t += 3000));
  emit(C, "pr_created", { url: "https://github.com/dev/sci-pi/pull/42" }, (t += 3000));
  say(C, "Done — PR #42 is open.", "c3", (t += 1000));
  finishTurn(C, "end_turn", (t += 500));
  setStatus(C, "idle", t);

  // --- D: second pending permission (a command) for the inbox --------------------------------
  const D = "s_docker";
  t = t0 - 52 * MIN;
  mkSession(
    {
      id: D,
      title: "Free disk space on homelab",
      agent: "claude",
      project: `${HOME}/srv`,
      mode: "default",
      modes: MODES,
      config_options: claudeConfig("default", "sonnet", "medium"),
      commands: CLAUDE_COMMANDS,
      usage: { used: 9_100, size: 200_000, cost: { amount: 0.06, currency: "USD" } },
    },
    t,
  );
  startTurn(D, "The disk is 94% full. Find what's using space and clean up safely.", [], (t += 1000));
  setStatus(D, "running", t);
  say(D, "Docker is the main culprit: **38 GB** of dangling volumes from old compose projects. None are attached to running containers.", "d1", (t += 9000));
  const pruneCall = {
    toolCallId: "d_t1",
    title: "docker volume prune -f",
    kind: "execute",
    status: "pending",
    rawInput: { command: "docker volume prune -f" },
    content: [],
  };
  upd(D, { sessionUpdate: "tool_call", ...pruneCall }, (t += 1000));
  requestPermission(D, pruneCall, PERM_OPTS, async (opt) => {
    setStatus(D, "running");
    upd(D, { sessionUpdate: "tool_call_update", toolCallId: "d_t1", status: opt?.startsWith("allow") ? "completed" : "failed" });
    await stream(D, opt?.startsWith("allow") ? "Reclaimed 38.2 GB. Disk is now at 61%." : "Skipped. Nothing was deleted.", "d2");
    await endTurn(D);
  }, t);

  // --- E: error / F: detached ----------------------------------------------------------------
  t = t0 - 2 * 24 * 60 * MIN;
  mkSession(
    { id: "s_react", title: "Upgrade to React 19", agent: "claude", project: `${HOME}/code/webapp`, status: "error", status_message: "adapter exited with status 1: ANTHROPIC_API_KEY not set" },
    t,
  );
  startTurn("s_react", "Upgrade the app to React 19 and fix any type errors.", [], t + 1000);
  emit("s_react", "error", { message: "adapter exited with status 1: ANTHROPIC_API_KEY not set" }, t + 3000);
  emit("s_react", "status", { status: "error", message: "adapter exited with status 1" }, t + 3000);

  t = t0 - 5 * 60 * MIN;
  mkSession(
    { id: "s_leak", title: "Investigate memory growth in the indexer", agent: "codex", project: `${HOME}/code/indexer`, status: "detached", status_message: "Daemon restarted; send a prompt to resume." },
    t,
  );
  startTurn("s_leak", "RSS grows ~200MB/day. Profile and find the leak.", [], t + 1000);
  say("s_leak", "Heap profile shows `LruCache` entries are never evicted because `capacity` is read before config is loaded (it's 0 → unbounded).", undefined, t + 60_000);
  finishTurn("s_leak", "end_turn", t + 61_000);
  touch("s_leak", { archived: true }, t + 61_000);
  emit("s_leak", "status", { status: "detached" }, t + 2 * 60 * MIN);
  touch("s_leak", {}, t + 2 * 60 * MIN);

  void liveLoop(B);
}

/** Session B keeps streaming so live updates are visible. */
async function liveLoop(B: string) {
  let n = 0;
  await sleep(1500);
  for (;;) {
    const s = sessions.get(B);
    if (!s) return;
    if (s.status !== "running") {
      await sleep(2000);
      continue;
    }
    n++;
    cancelled.delete(B);
    think(B, "Need to make sure the bucket refill uses a monotonic clock, not SystemTime.");
    await sleep(1200);
    const id = `b_live_${n}`;
    upd(B, {
      sessionUpdate: "tool_call",
      toolCallId: id,
      title: "Edit src/ratelimit.rs",
      kind: "edit",
      status: "in_progress",
      rawInput: { file_path: "src/ratelimit.rs" },
      content: [],
    });
    await sleep(900);
    upd(B, {
      sessionUpdate: "tool_call_update",
      toolCallId: id,
      status: "completed",
      content: [
        {
          type: "diff",
          path: "src/ratelimit.rs",
          oldText: "fn refill(&mut self) {\n    let now = SystemTime::now();\n    let dt = now.duration_since(self.last).unwrap();\n    self.tokens = (self.tokens + dt.as_secs_f64() * self.rate).min(self.burst);\n    self.last = now;\n}\n",
          newText: `fn refill(&mut self) {\n    let now = Instant::now();\n    let dt = now - self.last;\n    self.tokens = (self.tokens + dt.as_secs_f64() * self.rate).min(self.burst);\n    self.last = now;\n}\n// pass ${n}\n`,
        },
      ],
    });
    if (cancelled.has(B) || sessions.get(B)?.status !== "running") continue;
    await stream(B, `Switched the bucket to \`Instant\` (pass ${n}). Now wiring the layer into the router and returning \`429 Too Many Requests\` with a \`Retry-After\` header. `, `b_msg_${n}`, 120);
    await sleep(2500);
    if (cancelled.has(B) || sessions.get(B)?.status !== "running") continue;
    touch(B, { usage: { used: Math.min(272_000, (s.usage?.used ?? 0) + 3_100), size: 272_000 } });
    await sleep(4000);
  }
}

// ------------------------------------------------------------------ fake filesystem

const TREE: Record<string, { dirs: string[]; git?: boolean }> = {
  "/": { dirs: ["home", "srv", "tmp"] },
  "/home": { dirs: ["dev"] },
  [HOME]: { dirs: [".config", ".sci-pi", "code", "notes", "srv"] },
  [`${HOME}/.config`]: { dirs: [] },
  [`${HOME}/.sci-pi`]: { dirs: ["worktrees"] },
  [`${HOME}/.sci-pi/worktrees`]: { dirs: [] },
  [`${HOME}/code`]: { dirs: ["dotfiles", "indexer", "sci-pi", "scratch", "webapp"] },
  [`${HOME}/code/dotfiles`]: { dirs: [".git", "nvim", "zsh"], git: true },
  [`${HOME}/code/indexer`]: { dirs: [".git", "src", "benches"], git: true },
  [`${HOME}/code/sci-pi`]: { dirs: [".git", "docs", "src", "web"], git: true },
  [`${HOME}/code/sci-pi/web`]: { dirs: ["mock", "src"] },
  [`${HOME}/code/scratch`]: { dirs: [] },
  [`${HOME}/code/webapp`]: { dirs: [".git", "public", "src", "test"], git: true },
  [`${HOME}/notes`]: { dirs: [] },
  [`${HOME}/srv`]: { dirs: ["caddy", "immich", "actual"] },
  "/srv": { dirs: [] },
  "/tmp": { dirs: [] },
};

function fsList(raw: string | null) {
  let p = raw?.trim() || HOME;
  if (p === "~" || p.startsWith("~/")) p = HOME + p.slice(1);
  p = normalize(p).replace(/\/+$/, "") || "/";
  const node = TREE[p] ?? (p.startsWith(HOME) ? { dirs: [] } : undefined);
  if (!node) return null;
  const parent = p === "/" ? null : p.slice(0, p.lastIndexOf("/")) || "/";
  return {
    path: p,
    parent,
    is_git: !!node.git,
    entries: node.dirs.map((name) => {
      const path = p === "/" ? `/${name}` : `${p}/${name}`;
      return { name, path, is_git: !!TREE[path]?.git };
    }),
  };
}

const DIFF = `diff --git a/src/middleware/auth.ts b/src/middleware/auth.ts
index 3b18e51..a9f02c4 100644
--- a/src/middleware/auth.ts
+++ b/src/middleware/auth.ts
@@ -1,22 +1,26 @@
-import { createHmac, timingSafeEqual } from "node:crypto";
+import { jwtVerify, type JWTPayload } from "jose";
 import type { Request, Response, NextFunction } from "express";

-const SECRET = process.env.AUTH_SECRET!;
+const SECRET = new TextEncoder().encode(process.env.AUTH_SECRET!);

-export function verifyToken(token: string): string | null {
-  const [payload, sig] = token.split(".");
-  if (!payload || !sig) return null;
-  const expected = createHmac("sha256", SECRET).update(payload).digest("base64url");
-  if (!timingSafeEqual(Buffer.from(sig), Buffer.from(expected))) return null;
-  const { sub, exp } = JSON.parse(Buffer.from(payload, "base64url").toString());
-  if (exp < Date.now() / 1000) return null;
-  return sub;
+export async function verifyToken(token: string): Promise<string | null> {
+  try {
+    const { payload } = await jwtVerify<JWTPayload>(token, SECRET, {
+      algorithms: ["HS256"],
+      clockTolerance: 5,
+    });
+    return payload.sub ?? null;
+  } catch {
+    return null;
+  }
 }

-export function requireAuth(req: Request, res: Response, next: NextFunction) {
+export async function requireAuth(req: Request, res: Response, next: NextFunction) {
   const header = req.headers.authorization ?? "";
   const token = header.replace(/^Bearer /, "");
-  const user = verifyToken(token);
+  const user = await verifyToken(token);
   if (!user) return res.status(401).json({ error: "unauthorized" });
   req.user = user;
   next();
diff --git a/package.json b/package.json
index 77ab1c0..2c9d3e1 100644
--- a/package.json
+++ b/package.json
@@ -12,6 +12,7 @@
   "dependencies": {
     "express": "^4.19.2",
+    "jose": "^5.9.6",
     "pino": "^9.4.0",
     "zod": "^3.23.8"
   },
diff --git a/test/jwt.test.ts b/test/jwt.test.ts
new file mode 100644
index 0000000..5e1d2f0
--- /dev/null
+++ b/test/jwt.test.ts
@@ -0,0 +1,9 @@
+import { SignJWT } from "jose";
+import { verifyToken } from "../src/middleware/auth";
+
+test("accepts a valid HS256 token", async () => {
+  const secret = new TextEncoder().encode(process.env.AUTH_SECRET!);
+  const jwt = await new SignJWT({}).setProtectedHeader({ alg: "HS256" }).setSubject("user_1").setExpirationTime("1h").sign(secret);
+  expect(await verifyToken(jwt)).toBe("user_1");
+});
+
`;

// ------------------------------------------------------------------ http

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Headers": "Authorization, Content-Type",
  "Access-Control-Allow-Methods": "GET, POST, DELETE, OPTIONS",
};

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json", ...CORS } });
const err = (status: number, error: string) => json({ error }, status);

/** null = authorized; also reports whether this looks like a token-less tailnet caller. */
function authorize(req: Request, url: URL): { ok: boolean; viaTailnet: boolean } {
  const header = req.headers.get("authorization");
  const tok = header?.replace(/^Bearer\s+/i, "") ?? url.searchParams.get("token");
  if (tok) return { ok: tok === TOKEN, viaTailnet: false };
  return { ok: !REQUIRE_TOKEN, viaTailnet: !REQUIRE_TOKEN };
}

function info(viaTailnet: boolean) {
  return {
    host: "devbox",
    version: "0.1.0-mock",
    home: HOME,
    agents: [
      { id: "claude", name: "Claude Code" },
      { id: "codex", name: "Codex" },
      { id: "opencode", name: "OpenCode" },
    ],
    last_event_id: nextId - 1,
    tailnet_url: "https://devbox.tail1a2b3.ts.net:7433",
    viewer: viaTailnet ? "dev@example.com" : null,
  };
}

async function body(req: Request): Promise<any> {
  try {
    return await req.json();
  } catch {
    return {};
  }
}

const FILES = [
  "README.md",
  "NOTES.md",
  "package.json",
  "tsconfig.json",
  "src/index.ts",
  "src/server.ts",
  "src/middleware/auth.ts",
  "src/middleware/cors.ts",
  "src/routes/ws.ts",
  "src/routes/sessions.ts",
  "src/routes/inbox.ts",
  "src/ratelimit.rs",
  "src/store/events.ts",
  "src/store/sessions.ts",
  "test/auth.test.ts",
  "test/jwt.test.ts",
  "tests/ws_reconnect.rs",
  "docs/PROTOCOL.md",
  "web/src/App.tsx",
  "web/src/components/Composer.tsx",
];

function fuzzy(q: string, t: string) {
  q = q.toLowerCase();
  t = t.toLowerCase();
  let i = 0;
  for (const ch of t) if (ch === q[i]) i++;
  return i === q.length;
}

/** Tiny stand-in for the daemon's FTS: user prompts and merged agent messages. */
function search(q: string) {
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  const docs: { session_id: string; event_id: number; role: "user" | "agent"; text: string }[] = [];
  let cur: (typeof docs)[number] | null = null;
  for (const e of events) {
    if (e.kind === "user_prompt") {
      docs.push({ session_id: e.session_id, event_id: e.id, role: "user", text: e.data.text });
      cur = null;
    } else if (e.kind === "update" && e.data.sessionUpdate === "agent_message_chunk") {
      if (cur && cur.session_id === e.session_id) cur.text += e.data.content?.text ?? "";
      else docs.push((cur = { session_id: e.session_id, event_id: e.id, role: "agent", text: e.data.content?.text ?? "" }));
    } else if (e.kind !== "update") cur = null;
  }
  const out = [];
  for (const d of docs) {
    const lower = d.text.toLowerCase();
    if (!terms.every((t) => new RegExp(`\\b${t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`).test(lower))) continue;
    const first = lower.indexOf(terms[0]);
    const start = Math.max(0, first - 50);
    let snip = (start > 0 ? "…" : "") + d.text.slice(start, first + 90) + (first + 90 < d.text.length ? "…" : "");
    for (const t of terms) snip = snip.replace(new RegExp(`\\b(${t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\w*)`, "gi"), "<<$1>>");
    out.push({ session_id: d.session_id, session_title: sessions.get(d.session_id)?.title ?? "", event_id: d.event_id, role: d.role, snippet: snip.replace(/\s+/g, " ") });
  }
  return out.slice(0, 50);
}

function storeAttachments(list: any[] | undefined) {
  return (list ?? []).map((a) => {
    if (a.type === "image") {
      const name = `${crypto.randomUUID().slice(0, 8)}.${(a.mime_type ?? "image/png").split("/")[1] ?? "png"}`;
      attachments.set(name, { mime: a.mime_type, bytes: Uint8Array.from(atob(a.data), (c) => c.charCodeAt(0)) });
      return { type: "image", name, mime_type: a.mime_type };
    }
    return { type: "file", path: a.path };
  });
}

function sessionDiff(s: Session) {
  const turns = turnDiffs.get(s.id);
  if (s.id === "s_auth" || !turns) return { diff: s.branch ? DIFF : "", files: s.branch ? [{ path: "src/middleware/auth.ts", status: "M" }, { path: "package.json", status: "M" }, { path: "test/jwt.test.ts", status: "A" }] : [] };
  const all = [...turns.values()];
  return { diff: all.map((t) => t.diff).join(""), files: all.flatMap((t) => t.files) };
}

async function api(req: Request, url: URL): Promise<Response> {
  const path = url.pathname.slice(4); // strip /api
  const m = req.method;
  if (path === "/ping") return json({ scipi: true, version: "0.2.0-mock", host: "devbox", tailnet_url: info(false).tailnet_url });

  const auth = authorize(req, url);
  if (!auth.ok) return err(401, "unauthorized");

  if (path === "/info" && m === "GET") return json(info(auth.viaTailnet));
  if (path === "/sessions" && m === "GET") return json([...sessions.values()].sort((a, b) => b.created_at - a.created_at));
  if (path === "/inbox" && m === "GET")
    return json(
      [...pending.values()].map((p) => ({
        session_id: p.session_id,
        session_title: sessions.get(p.session_id)?.title ?? "",
        request_id: p.request_id,
        tool_call: p.tool_call,
        options: p.options,
        ts: p.ts,
      })),
    );
  if (path === "/search" && m === "GET") return json(search(url.searchParams.get("q") ?? ""));
  if (path.startsWith("/attachments/") && m === "GET") {
    const a = attachments.get(decodeURIComponent(path.slice(13)));
    return a ? new Response(a.bytes as unknown as BodyInit, { headers: { "Content-Type": a.mime, ...CORS } }) : err(404, "no such attachment");
  }
  if (path === "/fs/list" && m === "GET") {
    const l = fsList(url.searchParams.get("path"));
    return l ? json(l) : err(404, "no such directory");
  }
  if (path === "/sessions" && m === "POST") {
    const b = await body(req);
    if (!b.agent || !b.project) return err(400, "agent and project are required");
    const id = `s_${Math.random().toString(36).slice(2, 10)}`;
    const short = id.slice(2, 10);
    const name = String(b.project).split("/").pop();
    const s = mkSession(
      {
        id,
        title: b.title || (b.prompt ? String(b.prompt).split("\n")[0].slice(0, 60) : `New session in ${name}`),
        agent: b.agent,
        project: b.project,
        cwd: b.worktree ? `${HOME}/.sci-pi/worktrees/${name}-${short}` : b.project,
        branch: b.worktree ? `sci-pi/${short}` : null,
        base_commit: b.worktree ? "0123456789abcdef0123456789abcdef01234567" : null,
        status: "starting",
        mode: b.mode || "default",
        modes: MODES,
        config_options: b.agent === "claude" ? claudeConfig(b.mode || "default") : b.agent === "codex" ? codexConfig() : [],
        commands: b.agent === "claude" ? CLAUDE_COMMANDS : [],
        usage: { used: 0, size: 200_000 },
      },
      now(),
    );
    broadcast({ type: "session", session: s });
    setTimeout(() => {
      setStatus(id, "idle");
      if (b.prompt) void runTurn(id, String(b.prompt), storeAttachments(b.attachments));
    }, 700);
    return json(s);
  }

  const sm = /^\/sessions\/([^/]+)(\/[a-z_]+)?(?:\/([^/]+))?(\/[a-z_]+)?$/.exec(path);
  if (!sm) return err(404, "not found");
  const sid = decodeURIComponent(sm[1]);
  const action = sm[2] ?? "";
  const sub = sm[3] ? decodeURIComponent(sm[3]) : null;
  const subAction = sm[4] ?? "";
  const s = sessions.get(sid);
  if (!s) return err(404, "session not found");
  const busy = () => s.status === "running" || s.status === "awaiting_permission" || s.status === "starting";

  if (action === "" && m === "GET") return json(s);
  if (action === "" && m === "PATCH") {
    const b = await body(req);
    const patch: Partial<Session> = {};
    if (typeof b.title === "string" && b.title.trim()) patch.title = b.title.trim();
    if (typeof b.pinned === "boolean") patch.pinned = b.pinned;
    if (typeof b.archived === "boolean") patch.archived = b.archived;
    touch(sid, patch);
    return json(s);
  }
  if (action === "" && m === "DELETE") {
    sessions.delete(sid);
    for (const [k, p] of pending) if (p.session_id === sid) pending.delete(k);
    for (let i = events.length - 1; i >= 0; i--) if (events[i].session_id === sid) events.splice(i, 1);
    broadcast({ type: "session_deleted", id: sid });
    return json({});
  }
  if (action === "/events" && m === "GET") {
    const after = Number(url.searchParams.get("after") ?? 0);
    return json(events.filter((e) => e.session_id === sid && e.id > after));
  }
  if (action === "/prompt" && m === "POST") {
    const b = await body(req);
    if (!b.text && !b.attachments?.length) return err(400, "text is required");
    const atts = storeAttachments(b.attachments);
    if (busy()) {
      s.queue.push({ id: `q_${Math.random().toString(36).slice(2, 8)}`, text: b.text ?? "", attachments: atts });
      touch(sid, { queue: s.queue, queued: s.queue.length });
      return json({ queued: true });
    }
    void runTurn(sid, b.text ?? "", atts);
    return json({ queued: false });
  }
  if (action === "/queue" && sub) {
    const i = s.queue.findIndex((q) => q.id === sub);
    if (i < 0) return err(404, "no such queued prompt");
    if (m === "DELETE" && !subAction) {
      s.queue.splice(i, 1);
    } else if (m === "PATCH" && !subAction) {
      const { text } = await body(req);
      if (!text) return err(400, "text is required");
      s.queue[i] = { ...s.queue[i], text };
    } else if (m === "POST" && subAction === "/send_now") {
      const [item] = s.queue.splice(i, 1);
      s.queue.unshift(item);
      touch(sid, { queue: s.queue, queued: s.queue.length });
      if (busy()) {
        cancelled.add(sid);
        for (const p of [...pending.values()]) if (p.session_id === sid) resolveCancelled(sid, p.request_id);
        await endTurn(sid, "cancelled");
      } else await dequeue(sid);
      return json({});
    } else return err(405, "method not allowed");
    touch(sid, { queue: s.queue, queued: s.queue.length });
    return json({});
  }
  if (action === "/cancel" && m === "POST") {
    cancelled.add(sid);
    s.queue = [];
    touch(sid, { queue: [], queued: 0 });
    for (const p of [...pending.values()]) if (p.session_id === sid) resolveCancelled(sid, p.request_id);
    if (busy()) {
      finishTurn(sid, "cancelled");
      setStatus(sid, "idle");
    }
    return json({});
  }
  if (action === "/permission" && m === "POST") {
    const b = await body(req);
    const p = pending.get(b.request_id);
    if (!p || p.session_id !== sid) return err(404, "no such permission request");
    resolvePermission(b.request_id, b.option_id ?? null);
    return json({});
  }
  if (action === "/mode" && m === "POST") {
    const { mode } = await body(req);
    if (!s.modes.some((x) => x.id === mode)) return err(400, "unknown mode");
    upd(sid, { sessionUpdate: "current_mode_update", currentModeId: mode });
    touch(sid, { mode });
    return json({});
  }
  if (action === "/config" && m === "POST") {
    const { config_id, value } = await body(req);
    const opt = s.config_options.find((o) => o.id === config_id);
    if (!opt) return err(400, "unknown config option");
    if (opt.options && !opt.options.some((o: any) => o.value === value)) return err(400, "invalid value");
    await sleep(250);
    opt.currentValue = value;
    if (opt.category === "mode") {
      upd(sid, { sessionUpdate: "current_mode_update", currentModeId: value });
      touch(sid, { mode: value, config_options: s.config_options });
    } else touch(sid, { config_options: s.config_options });
    return json({});
  }
  if (action === "/revert" && m === "POST") {
    const { turn } = await body(req);
    if (busy()) return err(409, "can't revert while a turn is running");
    if (!turn || turn < 1 || turn > s.turns) return err(400, "no such turn");
    emit(sid, "reverted", { turn, checkpoint: sha() });
    const g = gitState.get(sid);
    if (g) g.dirty = 0;
    return json({});
  }
  if (action === "/fork" && m === "POST") {
    const b = await body(req);
    const turn = b.turn ?? s.turns;
    const id = `s_${Math.random().toString(36).slice(2, 10)}`;
    const short = id.slice(2, 10);
    const agent = b.agent ?? s.agent;
    const f = mkSession(
      {
        id,
        title: `${s.title} (fork)`,
        agent,
        project: s.project,
        cwd: `${HOME}/.sci-pi/worktrees/${s.project.split("/").pop()}-${short}`,
        branch: `sci-pi/${short}`,
        base_commit: s.base_commit,
        status: "idle",
        modes: agent === "claude" ? MODES : [],
        mode: agent === "claude" ? "default" : null,
        config_options: agent === "claude" ? claudeConfig() : agent === "codex" ? codexConfig() : [],
        commands: agent === "claude" ? CLAUDE_COMMANDS : [],
        usage: { used: 0, size: 200_000 },
      },
      now(),
    );
    emit(id, "forked", { from: sid, from_title: s.title, turn });
    broadcast({ type: "session", session: f });
    return json(f);
  }
  if (action === "/files" && m === "GET") {
    const q = url.searchParams.get("q") ?? "";
    return json(FILES.filter((f) => !q || fuzzy(q, f)).slice(0, 30));
  }
  if (action === "/stop" && m === "POST") {
    cancelled.add(sid);
    setStatus(sid, "stopped");
    return json({});
  }
  if (action === "/diff" && m === "GET") {
    const turn = Number(url.searchParams.get("turn") ?? 0);
    if (turn) return json(turnDiffs.get(sid)?.get(turn) ?? { diff: "", files: [] });
    return json(sessionDiff(s));
  }
  if (action === "/git" && !sub) {
    const g = gitState.get(sid);
    if (m !== "GET") return err(405, "method not allowed");
    if (!g) return json({ git: false, branch: null, dirty: 0, remote: null, upstream: false, ahead: null, behind: null, commits_since_base: 0, pr_url: null, gh: false });
    return json({
      git: true,
      branch: s.branch,
      dirty: g.dirty,
      remote: g.remote,
      upstream: g.upstream,
      ahead: g.upstream ? g.ahead : null,
      behind: g.upstream ? g.behind : null,
      commits_since_base: g.commits,
      pr_url: s.pr_url,
      gh: true,
    });
  }
  if (action === "/git" && sub && m === "POST") {
    const g = gitState.get(sid);
    if (!g) return err(400, "not a git repository");
    await sleep(500);
    if (sub === "commit") {
      const { message } = await body(req);
      if (!message) return err(400, "message is required");
      if (!g.dirty) return err(400, "nothing to commit");
      g.dirty = 0;
      g.commits++;
      g.ahead++;
      const h = sha().slice(0, 12);
      emit(sid, "git", { action: "commit", sha: h, message });
      return json({ sha: h });
    }
    if (sub === "push") {
      const output = `To ${g.remote}\n * [new branch] ${s.branch} -> ${s.branch}`;
      g.upstream = true;
      g.ahead = 0;
      emit(sid, "git", { action: "push", output });
      return json({ output });
    }
    if (sub === "pr") {
      const b = await body(req);
      g.upstream = true;
      g.ahead = 0;
      const url2 = `https://github.com/dev/${s.project.split("/").pop()}/pull/${100 + Math.floor(Math.random() * 900)}`;
      emit(sid, "pr_created", { url: url2, title: b.title, draft: !!b.draft });
      touch(sid, { pr_url: url2 });
      return json({ url: url2 });
    }
    return err(404, "not found");
  }
  if (action === "/terminal" && m === "DELETE") {
    killShell(sid);
    return json({});
  }
  return err(404, "not found");
}

function resolveCancelled(sid: string, request_id: string) {
  const s = sessions.get(sid)!;
  pending.delete(request_id);
  emit(sid, "permission_resolved", { request_id, outcome: "cancelled", option_id: null });
  touch(sid, { pending_permissions: Math.max(0, s.pending_permissions - 1) });
}

// ------------------------------------------------------------------ fake terminal

type WsData = { kind: "events"; after: number } | { kind: "term"; sid: string };

interface Shell {
  buf: string;
  line: string;
  clients: Set<ServerWebSocket<WsData>>;
}
const shells = new Map<string, Shell>();
const enc = new TextEncoder();

function shellFor(sid: string): Shell {
  let sh = shells.get(sid);
  if (!sh) {
    const s = sessions.get(sid);
    sh = { buf: "", line: "", clients: new Set() };
    shells.set(sid, sh);
    out(sid, `\x1b[2mmock shell for ${s?.title ?? sid}\x1b[0m\r\n`);
    out(sid, prompt(sid));
  }
  return sh;
}

function prompt(sid: string) {
  const s = sessions.get(sid);
  const dir = (s?.cwd ?? HOME).replace(HOME, "~").replace(/\/$/, "");
  return `\x1b[32mdev@devbox\x1b[0m:\x1b[34m${dir}\x1b[0m$ `;
}

function out(sid: string, text: string) {
  const sh = shells.get(sid)!;
  sh.buf = (sh.buf + text).slice(-512 * 1024);
  for (const c of sh.clients) c.send(enc.encode(text));
}

function killShell(sid: string) {
  const sh = shells.get(sid);
  if (!sh) return;
  for (const c of sh.clients) c.send(JSON.stringify({ type: "exit" }));
  shells.delete(sid);
}

function runLine(sid: string, line: string) {
  const s = sessions.get(sid);
  const [cmd, ...args] = line.trim().split(/\s+/);
  switch (cmd) {
    case undefined:
    case "":
      break;
    case "ls":
      out(sid, ["README.md", "NOTES.md", "package.json", "\x1b[34msrc\x1b[0m", "\x1b[34mtest\x1b[0m", "\x1b[34mdocs\x1b[0m"].join("  ") + "\r\n");
      break;
    case "pwd":
      out(sid, `${s?.cwd ?? HOME}\r\n`);
      break;
    case "echo":
      out(sid, args.join(" ") + "\r\n");
      break;
    case "whoami":
      out(sid, "dev\r\n");
      break;
    case "date":
      out(sid, new Date().toString() + "\r\n");
      break;
    case "git":
      out(sid, `On branch ${s?.branch ?? "main"}\r\nChanges not staged for commit:\r\n  \x1b[31mmodified:   src/middleware/auth.ts\x1b[0m\r\n`);
      break;
    case "clear":
      out(sid, "\x1b[2J\x1b[H");
      break;
    case "exit":
      out(sid, "logout\r\n");
      killShell(sid);
      return;
    default:
      out(sid, `mock-shell: ${cmd}: command not found\r\n`);
  }
  out(sid, prompt(sid));
}

function termInput(sid: string, data: string) {
  const sh = shellFor(sid);
  for (const ch of data) {
    if (ch === "\r") {
      out(sid, "\r\n");
      const line = sh.line;
      sh.line = "";
      runLine(sid, line);
      if (!shells.has(sid)) return;
    } else if (ch === "\x7f") {
      if (sh.line) {
        sh.line = sh.line.slice(0, -1);
        out(sid, "\b \b");
      }
    } else if (ch === "\x03") {
      sh.line = "";
      out(sid, "^C\r\n" + prompt(sid));
    } else if (ch === "\x0c") {
      out(sid, "\x1b[2J\x1b[H" + prompt(sid) + sh.line);
    } else if (ch === "\x04") {
      if (!sh.line) {
        out(sid, "logout\r\n");
        killShell(sid);
        return;
      }
    } else if (ch >= " " || ch === "\t") {
      sh.line += ch;
      out(sid, ch);
    }
  }
}

async function serveStatic(url: URL): Promise<Response> {
  let p = decodeURIComponent(url.pathname);
  if (p === "/" || p === "") p = "/index.html";
  const full = normalize(join(DIST, p));
  if (!full.startsWith(DIST)) return new Response("forbidden", { status: 403 });
  const f = Bun.file(full);
  if (await f.exists()) return new Response(f);
  if (p === "/index.html") return new Response("dist/ not built — run `bun run build` first", { status: 500 });
  return new Response("not found", { status: 404 });
}

await seed();

server = Bun.serve<WsData>({
  hostname: "0.0.0.0",
  port: PORT,
  async fetch(req, srv) {
    const url = new URL(req.url);
    if (req.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });
    if (url.pathname === "/api/ws") {
      if (!authorize(req, url).ok) return err(401, "unauthorized");
      const after = Number(url.searchParams.get("after") ?? 0);
      if (srv.upgrade(req, { data: { kind: "events", after } })) return undefined;
      return err(400, "expected websocket");
    }
    const tm = /^\/api\/sessions\/([^/]+)\/terminal$/.exec(url.pathname);
    if (tm && req.method === "GET") {
      if (!authorize(req, url).ok) return err(401, "unauthorized");
      const sid = decodeURIComponent(tm[1]);
      if (!sessions.has(sid)) return err(404, "session not found");
      if (srv.upgrade(req, { data: { kind: "term", sid } })) return undefined;
      return err(400, "expected websocket");
    }
    if (url.pathname.startsWith("/api/")) return api(req, url);
    if (url.pathname === "/hub/hosts") {
      if (!HUB) return err(404, "not a hub");
      const origin = `http://127.0.0.1:${PORT}`;
      return json([
        { name: "devbox", url: origin, token: TOKEN, transport: "ssh", discovered: false, status: "connected" },
        { name: "homelab", url: origin, token: "", transport: "tailscale", discovered: true, status: "connected" },
        { name: "gpu-box", url: "http://127.0.0.1:1", token: "x", transport: "ssh", discovered: false, status: "error", error: "ssh: connect to host gpu-box port 22: No route to host" },
      ]);
    }
    if (url.pathname.startsWith("/hub/")) {
      if (HUB && url.pathname.startsWith("/hub/hosts/") && req.method === "POST") return json({});
      return err(404, "not found");
    }
    return serveStatic(url);
  },
  websocket: {
    open(ws) {
      if (ws.data.kind === "term") {
        const sh = shellFor(ws.data.sid);
        ws.send(enc.encode(sh.buf));
        sh.clients.add(ws);
        return;
      }
      for (const e of events) if (e.id > ws.data.after) ws.send(JSON.stringify({ type: "event", event: e }));
      for (const s of sessions.values()) ws.send(JSON.stringify({ type: "session", session: s }));
      ws.subscribe("all");
    },
    message(ws, msg) {
      if (ws.data.kind !== "term") return;
      try {
        const m = JSON.parse(String(msg));
        if (m.type === "input") termInput(ws.data.sid, String(m.data));
      } catch {
        /* ignore */
      }
    },
    close(ws) {
      if (ws.data.kind === "term") shells.get(ws.data.sid)?.clients.delete(ws);
    },
  },
});

setInterval(() => broadcast({ type: "ping" }), 25_000);

console.log(`sci-pi mock on http://127.0.0.1:${PORT}/#token=${TOKEN}${HUB ? "  (hub mode)" : ""}`);
