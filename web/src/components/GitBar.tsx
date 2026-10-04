import { useCallback, useEffect, useState } from "react";
import type { HostState } from "../store";
import type { GitStatus, Session } from "../types";
import { cx } from "../util";
import { IconBranch, IconCommit, IconExternal, IconPR, IconUpload } from "./Icons";
import { Modal } from "./Modal";

/** Git status + commit / push / PR actions for a session's worktree (Diff tab header). */
export function GitBar({ h, session, onChanged }: { h: HostState; session: Session; onChanged: () => void }) {
  const [git, setGit] = useState<GitStatus | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [committing, setCommitting] = useState(false);
  const [msg, setMsg] = useState("");
  const [prOpen, setPrOpen] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setGit(await h.api.git(session.id));
    } catch (e) {
      setErr((e as Error).message);
    }
  }, [h, session.id]);

  useEffect(() => {
    void load();
  }, [load, session.turns, session.status]);

  if (!git) return err ? <div className="gitbar error">{err}</div> : <div className="gitbar dim">Loading git status…</div>;
  if (!git.git) return <div className="gitbar dim">Not a git repository.</div>;

  const run = async (what: string, fn: () => Promise<string | void>) => {
    setBusy(what);
    setErr(null);
    setNote(null);
    try {
      const n = await fn();
      if (n) setNote(n);
      await load();
      onChanged();
      return true;
    } catch (e) {
      setErr((e as Error).message);
      return false;
    } finally {
      setBusy(null);
    }
  };

  const prUrl = session.pr_url ?? git.pr_url;
  const canPush = git.commits_since_base > 0 && (!git.upstream || (git.ahead ?? 1) > 0);

  return (
    <div className="gitbar">
      <div className="gitbar-row">
        <span className="git-branch mono" title={git.remote ? `remote: ${git.remote}` : "no remote"}>
          <IconBranch size={13} />
          {git.branch ?? "detached"}
        </span>
        <span className={cx("git-stat", git.dirty > 0 && "warn")}>{git.dirty > 0 ? `${git.dirty} uncommitted` : "clean"}</span>
        {git.commits_since_base > 0 && (
          <span className="git-stat">
            {git.commits_since_base} commit{git.commits_since_base === 1 ? "" : "s"}
          </span>
        )}
        {git.upstream ? (
          <span className="git-stat mono" title="ahead / behind upstream">
            ↑{git.ahead ?? 0} ↓{git.behind ?? 0}
          </span>
        ) : (
          git.remote && <span className="git-stat dim">not pushed</span>
        )}
        <span className="spacer" />
        {git.dirty > 0 && !committing && (
          <button className="btn btn-sm" onClick={() => setCommitting(true)}>
            <IconCommit size={13} /> Commit
          </button>
        )}
        {canPush && git.remote && (
          <button
            className="btn btn-sm"
            disabled={busy !== null}
            onClick={() => run("push", async () => (await h.api.push(session.id)).output?.trim().split("\n").pop() || "Pushed")}
          >
            {busy === "push" ? <span className="spinner" /> : <IconUpload size={13} />} Push
          </button>
        )}
        {prUrl ? (
          <a className="btn btn-sm btn-primary-soft" href={prUrl} target="_blank" rel="noopener noreferrer">
            <IconPR size={13} /> View PR <IconExternal size={11} />
          </a>
        ) : (
          git.gh &&
          git.remote &&
          (git.commits_since_base > 0 || git.dirty > 0) && (
            <button className="btn btn-sm btn-primary" disabled={busy !== null} onClick={() => setPrOpen(true)}>
              <IconPR size={13} /> Create PR
            </button>
          )
        )}
      </div>
      {committing && (
        <form
          className="commit-row"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!msg.trim()) return;
            const ok = await run("commit", async () => `Committed ${(await h.api.commit(session.id, msg.trim())).sha.slice(0, 7)}`);
            if (ok) {
              setMsg("");
              setCommitting(false);
            }
          }}
        >
          <input autoFocus value={msg} onChange={(e) => setMsg(e.target.value)} placeholder="Commit message" aria-label="Commit message" onKeyDown={(e) => e.key === "Escape" && setCommitting(false)} />
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => setCommitting(false)}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary btn-sm" disabled={!msg.trim() || busy !== null}>
            {busy === "commit" && <span className="spinner" />}
            Commit {git.dirty} file{git.dirty === 1 ? "" : "s"}
          </button>
        </form>
      )}
      {(err || note) && <div className={cx("git-note", err && "error")}>{err ?? note}</div>}
      {prOpen && (
        <PrDialog
          session={session}
          dirty={git.dirty}
          onClose={() => setPrOpen(false)}
          onSubmit={async (body) => {
            const ok = await run("pr", async () => {
              const r = await h.api.createPr(session.id, body);
              return `Opened ${r.url}`;
            });
            if (ok) setPrOpen(false);
            return ok;
          }}
        />
      )}
    </div>
  );
}

function PrDialog({
  session,
  dirty,
  onClose,
  onSubmit,
}: {
  session: Session;
  dirty: number;
  onClose: () => void;
  onSubmit: (b: { title?: string; body?: string; draft?: boolean }) => Promise<boolean>;
}) {
  const [title, setTitle] = useState(session.title);
  const [body, setBody] = useState("");
  const [draft, setDraft] = useState(false);
  const [busy, setBusy] = useState(false);
  return (
    <Modal title="Create pull request" onClose={onClose}>
      <form
        className="form"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          const ok = await onSubmit({ title: title.trim() || undefined, body: body.trim() || undefined, draft });
          if (!ok) setBusy(false);
        }}
      >
        {dirty > 0 && <div className="form-warn">{dirty} uncommitted file{dirty === 1 ? "" : "s"} won't be in the PR. Commit first to include them.</div>}
        <label className="field">
          <span className="label">Title</span>
          <input value={title} onChange={(e) => setTitle(e.target.value)} autoFocus />
        </label>
        <label className="field">
          <span className="label">
            Description <span className="dim">optional</span>
          </span>
          <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={6} placeholder="What changed and why" />
        </label>
        <label className="check">
          <input type="checkbox" checked={draft} onChange={(e) => setDraft(e.target.checked)} />
          <span>Open as draft</span>
        </label>
        <div className="modal-actions">
          <span className="hint grow">Pushes the branch, then runs gh pr create on the host.</span>
          <button type="button" className="btn btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={busy}>
            {busy && <span className="spinner" />}
            Create PR
          </button>
        </div>
      </form>
    </Modal>
  );
}
