import { useCallback, useEffect, useMemo, useState } from "react";
import type { HostState } from "../store";
import type { DiffResult, Session } from "../types";
import { parseUnifiedDiff, type FileDiff } from "../diff";
import { cx } from "../util";
import { IconChevron, IconRefresh } from "./Icons";
import { GitBar } from "./GitBar";

function statusClass(s: string) {
  const c = s.slice(0, 1).toUpperCase();
  if (c === "A" || c === "?" || c === "U") return "add";
  if (c === "D") return "del";
  return "mod";
}

function statusLetter(s: string) {
  return s === "??" ? "U" : s.slice(0, 1).toUpperCase();
}

export function useDiff(h: HostState, sid: string, turn?: number) {
  const [data, setData] = useState<DiffResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const load = useCallback(async () => {
    setLoading(true);
    setErr(null);
    try {
      setData(await h.api.diff(sid, turn));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [h, sid, turn]);
  useEffect(() => {
    void load();
  }, [load]);
  return { data, loading, err, load };
}

export function DiffView({ h, session }: { h: HostState; session: Session }) {
  const { data, loading, err, load } = useDiff(h, session.id);
  return (
    <div className="diffview">
      <GitBar h={h} session={session} onChanged={load} />
      <div className="diffview-bar">
        <span className="dim">
          {session.base_commit ? (
            <>
              vs <span className="mono">{session.base_commit.slice(0, 8)}</span>
            </>
          ) : (
            "vs HEAD"
          )}
        </span>
        <DiffTotals data={data} />
        <button className="btn btn-ghost btn-sm" onClick={load} disabled={loading}>
          <IconRefresh size={13} className={loading ? "spin-icon" : undefined} />
          Refresh
        </button>
      </div>
      <div className="diffview-scroll">
        <DiffFiles data={data} err={err} />
      </div>
    </div>
  );
}

export function DiffTotals({ data }: { data: DiffResult | null }) {
  const totals = useMemo(() => {
    const files = data ? parseUnifiedDiff(data.diff) : [];
    return files.reduce((a, f) => ({ add: a.add + f.add, del: a.del + f.del }), { add: 0, del: 0 });
  }, [data]);
  if (!data) return null;
  return (
    <span className="dstat">
      {data.files.length} file{data.files.length === 1 ? "" : "s"} <span className="add">+{totals.add}</span> <span className="del">−{totals.del}</span>
    </span>
  );
}

/** File list + per-file collapsible unified diffs. */
export function DiffFiles({ data, err }: { data: DiffResult | null; err: string | null }) {
  const files = useMemo(() => (data ? parseUnifiedDiff(data.diff) : []), [data]);
  const statusOf = useMemo(() => new Map((data?.files ?? []).map((f) => [f.path, f.status])), [data]);
  return (
    <>
      {err && <div className="tl-sys error">{err}</div>}
      {data && data.files.length === 0 && !data.diff.trim() && <div className="tl-empty">No changes.</div>}
      {data && data.files.length > 1 && (
        <ul className="filelist">
          {data.files.map((f) => (
            <li key={f.path}>
              <a
                href={`#file-${encodeURIComponent(f.path)}`}
                onClick={(e) => {
                  e.preventDefault();
                  document.getElementById(`file-${f.path}`)?.scrollIntoView({ behavior: "smooth", block: "start" });
                }}
              >
                <span className={cx("fstatus", statusClass(f.status))}>{statusLetter(f.status)}</span>
                <span className="mono path">{f.path}</span>
              </a>
            </li>
          ))}
        </ul>
      )}
      {files.map((f) => (
        <FileSection key={f.path} f={f} status={statusOf.get(f.path)} />
      ))}
    </>
  );
}

function FileSection({ f, status }: { f: FileDiff; status?: string }) {
  const [open, setOpen] = useState(f.lines.length < 1500);
  return (
    <section className={cx("dfile", open && "open")} id={`file-${f.path}`}>
      <button className="dfile-head" onClick={() => setOpen(!open)} aria-expanded={open}>
        <IconChevron size={12} className="chev" />
        {status && <span className={cx("fstatus", statusClass(status))}>{statusLetter(status)}</span>}
        <span className="mono path">{f.path}</span>
        <span className="dstat">
          <span className="add">+{f.add}</span> <span className="del">−{f.del}</span>
        </span>
      </button>
      {open && (
        <div className="codebox">
          {f.binary && f.lines.length === 0 && <div className="dl-meta">Binary file</div>}
          <table className="difftable">
            <tbody>
              {f.lines.map((l, i) => (
                <tr key={i} className={`dl-${l.type === "ctx" ? "eq" : l.type}`}>
                  {l.type === "hunk" || l.type === "meta" ? (
                    <td colSpan={3} className="code">
                      {l.text}
                    </td>
                  ) : (
                    <>
                      <td className="ln">{l.a ?? ""}</td>
                      <td className="ln">{l.b ?? ""}</td>
                      <td className="code">
                        <span className="sign">{l.type === "add" ? "+" : l.type === "del" ? "−" : " "}</span>
                        {l.text}
                      </td>
                    </>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
