import { memo, useMemo, useState } from "react";
import { diffStats, lineDiff, withContext, type DiffRow } from "../diff";
import { IconFile } from "./Icons";

/** Red/green line diff for an ACP `diff` tool-call content item. */
export const DiffBlock = memo(function DiffBlock({
  path,
  oldText,
  newText,
  maxHeight,
}: {
  path: string;
  oldText: string | null;
  newText: string;
  maxHeight?: number;
}) {
  const [full, setFull] = useState(false);
  const lines = useMemo(() => lineDiff(oldText, newText), [oldText, newText]);
  const rows: DiffRow[] = useMemo(() => (full ? lines : withContext(lines, 3)), [lines, full]);
  const st = useMemo(() => diffStats(lines), [lines]);
  return (
    <div className="diffblock">
      <div className="diffblock-head">
        <IconFile size={13} />
        <span className="mono path" title={path}>
          {path}
        </span>
        {oldText === null && <span className="tag tag-green">new</span>}
        <span className="dstat">
          <span className="add">+{st.add}</span> <span className="del">−{st.del}</span>
        </span>
      </div>
      <div className="codebox" style={maxHeight ? { maxHeight } : undefined}>
        <table className="difftable">
          <tbody>
            {rows.map((r, i) =>
              r.type === "gap" ? (
                <tr key={i} className="dl-gap" onClick={() => setFull(true)}>
                  <td colSpan={3}>⋯ {r.count} unchanged line{r.count === 1 ? "" : "s"}</td>
                </tr>
              ) : (
                <tr key={i} className={`dl-${r.type}`}>
                  <td className="ln">{r.type === "add" ? "" : r.a}</td>
                  <td className="ln">{r.type === "del" ? "" : r.b}</td>
                  <td className="code">
                    <span className="sign">{r.type === "add" ? "+" : r.type === "del" ? "−" : " "}</span>
                    {r.text}
                  </td>
                </tr>
              ),
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
});
