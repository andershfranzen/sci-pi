import { useEffect, useMemo, useState } from "react";
import type { HostState } from "../store";
import { emit, store } from "../store";
import type { FsList } from "../types";
import { navigate, sessionHash } from "../router";
import { cx, tildify } from "../util";
import { useCompositionGuard } from "../keyboard";
import { Modal } from "./Modal";
import { Select } from "./Select";
import { IconChevron, IconFolder, IconGit } from "./Icons";

const LAST_KEY = "sci-pi.lastNew";

const AGENT_DESC: Record<string, string> = {
  "sci-pi": "Built-in harness · any model via CLIProxyAPI",
  claude: "Anthropic's Claude Code over ACP",
  codex: "OpenAI Codex over ACP",
  opencode: "OpenCode over ACP",
  omp: "oh-my-pi over ACP",
};

interface Last {
  agent?: string;
  project?: string;
}

function readLast(hostKey: string): Last {
  try {
    return JSON.parse(localStorage.getItem(`${LAST_KEY}.${hostKey}`) ?? "{}") as Last;
  } catch {
    return {};
  }
}

export function NewSessionDialog({ initialHost, onClose }: { initialHost: HostState; onClose: () => void }) {
  const composing = useCompositionGuard();
  const [hostKey, setHostKey] = useState(initialHost.key);
  const h = store.hosts.find((x) => x.key === hostKey) ?? initialHost;
  const last = useMemo(() => readLast(h.key), [h.key]);
  const agents = h.info?.agents ?? [];

  const [agent, setAgent] = useState(last.agent && agents.some((a) => a.id === last.agent) ? last.agent : (agents[0]?.id ?? ""));
  const [title, setTitle] = useState("");
  const [mode, setMode] = useState("");
  const [prompt, setPrompt] = useState("");
  const [worktree, setWorktree] = useState(true);
  const [worktreeTouched, setWorktreeTouched] = useState(false);
  const [picked, setPicked] = useState<{ path: string; isGit: boolean | null } | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!agents.some((a) => a.id === agent) && agents[0]) setAgent(agents[0].id);
  }, [agents, agent]);

  useEffect(() => {
    if (!worktreeTouched && picked) setWorktree(picked.isGit !== false);
  }, [picked, worktreeTouched]);

  const submit = async () => {
    if (!picked || !agent) return;
    setBusy(true);
    setErr(null);
    try {
      const s = await h.api.createSession({
        agent,
        project: picked.path,
        worktree,
        ...(title.trim() ? { title: title.trim() } : {}),
        ...(mode.trim() ? { mode: mode.trim() } : {}),
        ...(prompt.trim() ? { prompt: prompt.trim() } : {}),
      });
      try {
        localStorage.setItem(`${LAST_KEY}.${h.key}`, JSON.stringify({ agent, project: picked.path }));
      } catch {
        /* ignore */
      }
      if (!h.sessions.has(s.id)) h.sessions.set(s.id, s);
      emit();
      onClose();
      navigate(sessionHash(h.key, s.id));
    } catch (e) {
      setErr((e as Error).message);
      setBusy(false);
    }
  };

  const hosts = store.hosts.filter((x) => x.info);

  return (
    <Modal title="New session" onClose={onClose}>
      <form
        className="form"
        onKeyDown={(e) => { if (e.key === "Enter" && composing(e.nativeEvent)) e.preventDefault(); }}
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div className="form-row two">
          {store.mode === "hub" && hosts.length > 1 && (
            <div className="field">
              <span className="label">Host</span>
              <Select
                variant="field"
                label="Host"
                value={hostKey}
                onChange={(v) => setHostKey(String(v))}
                options={hosts.map((x) => ({
                  value: x.key,
                  label: x.name,
                  description: x.hub ? `${x.hub.transport}${x.hub.discovered ? " · discovered" : ""}` : undefined,
                }))}
              />
            </div>
          )}
          <div className="field">
            <span className="label">Agent</span>
            <Select
              variant="field"
              label="Agent"
              value={agent}
              onChange={(v) => setAgent(String(v))}
              options={agents.map((a) => ({ value: a.id, label: a.name, description: AGENT_DESC[a.id] }))}
            />
          </div>
          <label className="field">
            <span className="label">
              Mode <span className="dim">optional</span>
            </span>
            <input value={mode} onChange={(e) => setMode(e.target.value)} placeholder="e.g. plan" className="mono" />
          </label>
        </div>

        <div className="field">
          <span className="label">Project directory</span>
          <DirPicker key={h.key} h={h} start={last.project ?? h.info?.home ?? "~"} picked={picked} onPick={setPicked} />
        </div>

        <label className={cx("check", picked?.isGit === false && "dim")}>
          <input
            type="checkbox"
            checked={worktree}
            onChange={(e) => {
              setWorktree(e.target.checked);
              setWorktreeTouched(true);
            }}
          />
          <span>
            Run in an isolated git worktree
            <span className="hint">
              {picked?.isGit === false ? "Not a git repository." : "New branch off HEAD; your checkout stays untouched."}
            </span>
          </span>
        </label>

        <label className="field">
          <span className="label">
            Title <span className="dim">optional</span>
          </span>
          <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Derived from the prompt if empty" />
        </label>

        <label className="field">
          <span className="label">Prompt</span>
          <textarea
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            rows={5}
            placeholder="What should the agent do?"
            onKeyDown={(e) => {
              if (composing(e.nativeEvent)) return;
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void submit();
              }
            }}
          />
        </label>

        {err && <div className="form-error">{err}</div>}
        <div className="modal-actions">
          <span className="hint grow">{picked ? <span className="mono">{tildify(picked.path, h.info?.home)}</span> : "Pick a directory"}</span>
          <button type="button" className="btn btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!picked || !agent || busy}>
            {busy && <span className="spinner" />}
            Start session
          </button>
        </div>
      </form>
    </Modal>
  );
}

function DirPicker({
  h,
  start,
  picked,
  onPick,
}: {
  h: HostState;
  start: string;
  picked: { path: string; isGit: boolean | null } | null;
  onPick: (p: { path: string; isGit: boolean | null }) => void;
}) {
  const composing = useCompositionGuard();
  const [list, setList] = useState<FsList | null>(null);
  const [input, setInput] = useState(start);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const go = async (path: string, fallback?: string) => {
    setLoading(true);
    setErr(null);
    try {
      const l = await h.api.fsList(path);
      setList(l);
      setInput(tildify(l.path, h.info?.home));
      setFilter("");
      onPick({ path: l.path, isGit: l.is_git });
    } catch (e) {
      if (fallback) return go(fallback);
      setErr((e as Error).message);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void go(start, h.info?.home);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const crumbs = useMemo(() => {
    if (!list) return [];
    const home = h.info?.home;
    const p = list.path;
    const out: { label: string; path: string }[] = [];
    let rest = p;
    let prefix = "";
    if (home && (p === home || p.startsWith(home + "/"))) {
      out.push({ label: "~", path: home });
      rest = p.slice(home.length);
      prefix = home;
    } else {
      out.push({ label: "/", path: "/" });
    }
    for (const seg of rest.split("/").filter(Boolean)) {
      prefix = prefix === "/" ? `/${seg}` : `${prefix}/${seg}`;
      out.push({ label: seg, path: prefix });
    }
    return out;
  }, [list, h.info?.home]);

  const entries = (list?.entries ?? []).filter(
    (e) => e.name !== ".git" && (!filter || e.name.toLowerCase().includes(filter.toLowerCase())),
  );

  return (
    <div className="dirpicker">
      <div className="dp-input">
        <IconFolder size={14} />
        <input
          className="mono"
          data-modal-autofocus
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (composing(e.nativeEvent)) return;
            if (e.key === "Enter") {
              e.preventDefault();
              void go(input);
            }
          }}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          aria-label="Directory path"
        />
        <button type="button" className="btn btn-sm btn-ghost" onClick={() => go(input)}>
          Go
        </button>
      </div>
      <div className="dp-crumbs">
        {crumbs.map((c, i) => (
          <span key={c.path} className="crumb">
            {i > 0 && <IconChevron size={10} className="dim" />}
            <button type="button" className="linkish mono" onClick={() => go(c.path)}>
              {c.label}
            </button>
          </span>
        ))}
        {picked?.isGit && <span className="tag tag-green">git</span>}
        <span className="spacer" />
        {(list?.entries.length ?? 0) > 12 && (
          <input className="dp-filter" placeholder="Filter" value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="Filter directories" />
        )}
      </div>
      <ul className={cx("dp-list", loading && "loading")}>
        {list?.parent && (
          <li>
            <button type="button" onClick={() => go(list.parent!)}>
              <IconFolder size={14} className="dim" />
              <span className="mono">..</span>
            </button>
          </li>
        )}
        {entries.map((e) => (
          <li key={e.path}>
            <button type="button" onClick={() => go(e.path)}>
              {e.is_git ? <IconGit size={14} className="git" /> : <IconFolder size={14} className="dim" />}
              <span className="mono name">{e.name}</span>
              {e.is_git && <span className="tag tag-green">git</span>}
            </button>
          </li>
        ))}
        {list && entries.length === 0 && <li className="dp-empty">No subdirectories</li>}
        {err && <li className="dp-empty error">{err}</li>}
      </ul>
    </div>
  );
}
