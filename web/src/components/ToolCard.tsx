import { memo, useState, type ReactNode } from "react";
import type { PermissionOption, PlanEntry, ToolCall, ToolCallContent } from "../types";
import type { Resolution } from "../timeline";
import { commandOf, cx } from "../util";
import { DiffBlock } from "./DiffBlock";
import { IconCheck, IconChevron, IconShield, IconTerminal, IconX, ToolKindIcon } from "./Icons";

const KIND_LABEL: Record<string, string> = {
  read: "Read",
  edit: "Edit",
  delete: "Delete",
  move: "Move",
  search: "Search",
  execute: "Run",
  think: "Think",
  fetch: "Fetch",
  switch_mode: "Mode",
  other: "Tool",
};

export function kindLabel(kind?: string) {
  return KIND_LABEL[kind ?? "other"] ?? kind ?? "Tool";
}

export function ToolStatusIcon({ status }: { status?: string }) {
  switch (status) {
    case "completed":
      return (
        <span className="tstatus ok" title="Completed">
          <IconCheck size={13} />
        </span>
      );
    case "failed":
      return (
        <span className="tstatus bad" title="Failed">
          <IconX size={13} />
        </span>
      );
    case "in_progress":
      return <span className="tstatus spin" title="In progress" />;
    default:
      return <span className="tstatus pending" title="Pending" />;
  }
}

function hasValue(v: unknown) {
  if (v === undefined || v === null) return false;
  if (typeof v === "object" && Object.keys(v as object).length === 0) return false;
  return true;
}

function contentText(c: ToolCallContent): string | null {
  if (c.type !== "content") return null;
  const inner = c.content;
  if (!inner) return null;
  if (typeof inner.text === "string") return inner.text;
  return `[${inner.type}]`;
}

/** Expanded body of a tool call: diffs, text output, terminals, else rawInput JSON. */
export function ToolContent({ call, maxHeight, skipCommand }: { call: ToolCall; maxHeight?: number; skipCommand?: boolean }) {
  const content = call.content ?? [];
  const rawCmd = commandOf(call.rawInput);
  const cmd = skipCommand ? null : rawCmd;
  const blocks = content.map((c, i) => {
    if (c.type === "diff") return <DiffBlock key={i} path={c.path} oldText={c.oldText} newText={c.newText} maxHeight={maxHeight} />;
    if (c.type === "terminal")
      return (
        <div key={i} className="terminal-ref">
          <IconTerminal size={13} /> Terminal <span className="mono">{c.terminalId}</span>
        </div>
      );
    const t = contentText(c);
    if (t === null || t === "") return null;
    return (
      <pre key={i} className="codebox out" style={maxHeight ? { maxHeight } : undefined}>
        {t}
      </pre>
    );
  });
  const any = blocks.some(Boolean);
  return (
    <div className="tool-content">
      {cmd && (
        <pre className="codebox cmd">
          <span className="prompt">$ </span>
          {cmd}
        </pre>
      )}
      {call.locations && call.locations.length > 0 && !content.some((c) => c.type === "diff") && (
        <div className="locations">
          {call.locations.map((l, i) => (
            <span key={i} className="mono loc">
              {l.path}
              {l.line ? `:${l.line}` : ""}
            </span>
          ))}
        </div>
      )}
      {blocks}
      {!any && !rawCmd && hasValue(call.rawInput) && (
        <pre className="codebox json" style={maxHeight ? { maxHeight } : undefined}>
          {JSON.stringify(call.rawInput, null, 2)}
        </pre>
      )}
    </div>
  );
}

function diffSummary(call: ToolCall) {
  const diffs = (call.content ?? []).filter((c) => c.type === "diff");
  return diffs.length;
}

export const ToolCard = memo(function ToolCard({ call }: { call: ToolCall }) {
  const [open, setOpen] = useState(false);
  const cmd = commandOf(call.rawInput);
  const kind = call.kind ?? "other";
  const nd = diffSummary(call);
  const label = kindLabel(kind);
  // "Edit Edit foo.ts" reads badly: drop the label when the title already starts with it.
  const redundant = !!call.title && call.title.toLowerCase().startsWith(label.toLowerCase());
  return (
    <div className={cx("tool", open && "open", call.status === "failed" && "failed")}>
      <button className="tool-head" onClick={() => setOpen(!open)} aria-expanded={open}>
        <IconChevron size={12} className="chev" />
        <span className={`tool-kind k-${kind}`}>
          <ToolKindIcon kind={kind} size={13} />
          {!redundant && <span className="tool-kind-label">{label}</span>}
        </span>
        <span className="tool-title">{call.title || cmd || call.toolCallId}</span>
        {nd > 0 && <span className="tag">{nd} file{nd === 1 ? "" : "s"}</span>}
        <ToolStatusIcon status={call.status} />
      </button>
      {open && (
        <div className="tool-body">
          <ToolContent call={call} maxHeight={420} skipCommand={!!call.title && call.title === cmd} />
        </div>
      )}
    </div>
  );
});

// ------------------------------------------------------------------ permission

function optionClass(kind: string) {
  if (kind === "allow_once") return "btn btn-primary";
  if (kind === "allow_always") return "btn btn-primary-soft";
  if (kind === "reject_always") return "btn btn-danger";
  if (kind.startsWith("reject")) return "btn btn-danger-soft";
  return "btn";
}

export function PermissionCard({
  toolCall,
  options,
  resolved,
  onChoose,
  compact,
  header,
}: {
  toolCall: ToolCall;
  options: PermissionOption[];
  resolved: Resolution | null;
  onChoose: (optionId: string | null) => Promise<void>;
  compact?: boolean;
  header?: ReactNode;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const cmd = commandOf(toolCall.rawInput);
  const kind = toolCall.kind ?? "other";
  const chosen = resolved?.optionId ? options.find((o) => o.optionId === resolved.optionId) : undefined;
  const allowed = chosen?.kind.startsWith("allow");

  const choose = async (id: string | null) => {
    setBusy(id ?? "__cancel");
    setErr(null);
    try {
      await onChoose(id);
    } catch (e) {
      setErr((e as Error).message);
      setBusy(null);
    }
  };

  return (
    <div className={cx("perm", resolved && "resolved", compact && "compact")}>
      {header}
      <div className="perm-head">
        <IconShield size={14} />
        <span>{resolved ? "Permission" : "Permission required"}</span>
        <span className={`tool-kind k-${kind}`}>
          <ToolKindIcon kind={kind} size={12} />
          <span className="tool-kind-label">{kindLabel(kind)}</span>
        </span>
      </div>
      <div className="perm-title">{toolCall.title || cmd || "Tool call"}</div>
      {!resolved && <ToolContent call={toolCall} maxHeight={compact ? 240 : 360} skipCommand={toolCall.title === cmd} />}
      {resolved ? (
        <div className={cx("perm-outcome", resolved.outcome === "cancelled" ? "muted" : allowed ? "ok" : "bad")}>
          {resolved.outcome === "cancelled" ? (
            <>
              <IconX size={13} /> Cancelled
            </>
          ) : allowed ? (
            <>
              <IconCheck size={13} /> {chosen?.name ?? "Allowed"}
            </>
          ) : (
            <>
              <IconX size={13} /> {chosen?.name ?? resolved.optionId ?? "Rejected"}
            </>
          )}
        </div>
      ) : (
        <div className="perm-actions">
          {options.map((o) => (
            <button
              key={o.optionId}
              className={optionClass(o.kind)}
              disabled={busy !== null}
              onClick={() => choose(o.optionId)}
            >
              {busy === o.optionId && <span className="spinner" />}
              {o.name}
            </button>
          ))}
          {options.length === 0 && (
            <button className="btn btn-danger-soft" disabled={busy !== null} onClick={() => choose(null)}>
              Deny
            </button>
          )}
        </div>
      )}
      {err && <div className="perm-err">{err}</div>}
    </div>
  );
}

// ------------------------------------------------------------------ plan

export const PlanCard = memo(function PlanCard({ entries }: { entries: PlanEntry[] }) {
  const done = entries.filter((e) => e.status === "completed").length;
  return (
    <div className="plan">
      <div className="plan-head">
        <span>Plan</span>
        <span className="plan-count">
          {done}/{entries.length}
        </span>
        <span className="plan-bar">
          <span style={{ width: `${entries.length ? (done / entries.length) * 100 : 0}%` }} />
        </span>
      </div>
      <ul>
        {entries.map((e, i) => (
          <li key={i} className={`pe-${e.status}`}>
            <span className="pe-box">{e.status === "completed" ? <IconCheck size={11} /> : null}</span>
            <span className="pe-text">{e.content}</span>
            {e.priority === "high" && e.status !== "completed" && <span className="tag tag-warn">high</span>}
          </li>
        ))}
      </ul>
    </div>
  );
});
