import { useMemo, useState } from "react";
import type { MetadataSource, ModelMetadata, OEvent, RequestDiagnostic, Session } from "../types";
import { fmtTokens } from "../util";
import { Modal } from "./Modal";
import { Select } from "./Select";
import "./Diagnostics.css";

const sources: Record<MetadataSource, string> = {
  provider: "Provider response", models_dev: "models.dev fallback",
  user_override: "User override", learned_overflow: "Learned overflow limit",
};
const known = (value: unknown) => value === null || value === undefined ? "Unknown" : typeof value === "boolean" ? (value ? "Yes" : "No") : String(value);
const tokens = (value: number | null | undefined) => value == null ? "Unknown" : `${fmtTokens(value)} tokens`;

export function DiagnosticsButton({ session, events }: { session: Session; events: OEvent[] }) {
  const [open, setOpen] = useState(false);
  if (session.agent !== "sci-pi" && !session.request_diagnostics?.length) return null;
  return <>
    <button className="btn btn-ghost diagnostics-trigger" onClick={() => setOpen(true)}>Diagnostics</button>
    {open && <Diagnostics session={session} events={events} onClose={() => setOpen(false)} />}
  </>;
}

function Diagnostics({ session, events, onClose }: { session: Session; events: OEvent[]; onClose: () => void }) {
  const [tab, setTab] = useState<"requests" | "metadata">("requests");
  const [selected, setSelected] = useState<number | null>(null);
  const groups = useMemo(() => {
    const turns = new Map<number, { records: Map<string, RequestDiagnostic>; outcome?: string }>();
    let turn = 0;
    for (const event of events) {
      if (event.kind === "user_prompt" && typeof event.data.turn === "number") turn = event.data.turn;
      if (event.kind === "turn_end" && typeof event.data.turn === "number") {
        const group = turns.get(event.data.turn);
        if (group) group.outcome = event.data.recovery?.outcome ?? event.data.stop_reason;
      }
      if (event.kind !== "update" || event.data.sessionUpdate !== "request_diagnostic") continue;
      const record = event.data.diagnostic as RequestDiagnostic | undefined;
      if (!record?.id) continue;
      let group = turns.get(turn);
      if (!group) { group = { records: new Map() }; turns.set(turn, group); }
      group.records.set(record.id, record);
    }
    if (session.request_diagnostics?.length) {
      let group = turns.get(session.turns);
      if (!group) { group = { records: new Map() }; turns.set(session.turns, group); }
      for (const record of session.request_diagnostics) group.records.set(record.id, record);
      if (session.recovery?.turn === session.turns) group.outcome = session.recovery.outcome;
    }
    return [...turns].sort(([a], [b]) => b - a);
  }, [events, events.length, session.request_diagnostics, session.recovery, session.turns]);
  const chosen = groups.some(([turn]) => turn === selected) ? selected : groups[0]?.[0];
  const records = groups.find(([turn]) => turn === chosen)?.[1].records;
  const model = session.config_options.find((option) => option.id === "model");
  const metadata = model?.metadata;
  return <Modal title="Request diagnostics" onClose={onClose} wide>
    <p className="dim diagnostics-note">Request shape only. Prompts, tool contents, thinking, credentials, and upstream error bodies are not included.</p>
    <div className="diagnostics-tabs" role="tablist" aria-label="Diagnostic view">
      <button className="btn btn-ghost" role="tab" aria-selected={tab === "requests"} onClick={() => setTab("requests")}>HTTP requests</button>
      <button className="btn btn-ghost" role="tab" aria-selected={tab === "metadata"} onClick={() => setTab("metadata")}>Model metadata</button>
    </div>
    {tab === "metadata" ? <Metadata metadata={metadata} model={known(model?.currentValue)} /> : <>
      {groups.length > 0 && <Select label="Diagnostic turn" value={chosen} variant="field" searchPlaceholder="Find a turn"
        options={groups.map(([turn, group]) => ({ value: turn, label: turn ? `Turn ${turn}${group.outcome ? ` · ${group.outcome}` : ""}` : "Before a turn",
          description: `${group.records.size} HTTP attempt${group.records.size === 1 ? "" : "s"}` }))}
        onChange={(value) => setSelected(Number(value))} />}
      {!records?.size && <p className="dim">No HTTP request diagnostics recorded for this session yet. Older turns created before diagnostics were enabled have no request records.</p>}
      {records && [...records.values()].map((record, index) => <RequestDetails key={record.id} record={record} index={index} />)}
    </>}
  </Modal>;
}

function RequestDetails({ record, index }: { record: RequestDiagnostic; index: number }) {
  const status = record.state === "response" ? `HTTP ${known(record.http_status)} received`
    : record.state === "network_error" ? "No HTTP response (request failed)" : "No HTTP response recorded";
  return <details className="diagnostic-request" open={index === 0}>
    <summary>Request {index + 1} · {known(record.phase)} · {status}</summary>
    <dl className="diagnostic-grid">
      <dt>Route</dt><dd>{known(record.route)}</dd>
      <dt>Endpoint</dt><dd className="mono">{known(record.endpoint)}</dd>
      <dt>Model</dt><dd>{known(record.model)}</dd>
      <dt>HTTP attempt</dt><dd>{known(record.attempt)}</dd>
      <dt>Upstream request ID</dt><dd className="mono">{known(record.request_id)}</dd>
      <dt>Effort</dt><dd>{record.effort ?? "Not sent"}</dd>
      <dt>Fast</dt><dd>{record.fast ?? "Not sent"}</dd>
      <dt>Thinking enabled</dt><dd>{known(record.thinking)}</dd>
      <dt>Server compaction supported</dt><dd>{known(record.server_compaction)}</dd>
      <dt>Compaction request</dt><dd>{known(record.compaction)}</dd>
      <dt>Context limit</dt><dd>{tokens(record.context_window)}</dd>
      <dt>Output limit</dt><dd>{tokens(record.max_output)}</dd>
      <dt>Cache placement owner</dt><dd>{known(record.cache_owner)}</dd>
      <dt>Client explicit cache points</dt><dd>{known(record.explicit_cache_points)}{record.cache_owner === "proxy" && " (before proxy transformation)"}</dd>
      <dt>Client automatic caching</dt><dd>{known(record.automatic_cache)}</dd>
      <dt>Remembered rejections</dt><dd>{record.rejected_fields?.join(", ") || "None"}</dd>
    </dl>
    <p className="dim diagnostics-note">Receiving HTTP headers does not mean the turn completed. Streaming failures and interruption outcomes are shown separately in the timeline.</p>
  </details>;
}

function Metadata({ metadata, model }: { metadata?: ModelMetadata; model: string }) {
  if (!metadata) return <p className="dim">This adapter has not reported metadata provenance. No capability or limit is inferred.</p>;
  const { info } = metadata;
  const rows: [string, string, string][] = [
    ["Context limit", tokens(info.window), "window"],
    ["Output limit", tokens(info.max_output), "max_output"],
    ["Effort levels", info.efforts_known ? (info.efforts.map(([name]) => name).join(", ") || "None reported") : "Unknown", "efforts"],
    ["Default effort", known(info.default_effort), "default_effort"],
    ["Adaptive thinking", known(info.adaptive_thinking), "adaptive_thinking"],
    ["Reasoning", known(info.reasoning), "reasoning"],
    ["Fast", info.fast ? (info.fast.kind === "service_tier" ? `Service tier: ${info.fast.tier}` : "Anthropic speed") : info.fast_known ? "Not supported" : "Unknown", "fast"],
    ["Thinking display", known(info.thinking_display), "thinking_display"],
    ["Refusal fallbacks", known(info.fallbacks), "fallbacks"],
    ["Server compaction", known(info.server_compaction), "server_compaction"],
  ];
  if (metadata.pay_per_token) {
    for (const [field, title] of [["input", "Input"], ["output", "Output"], ["cache_read", "Cache read"], ["cache_write", "Cache write"]] as const) {
      const rate = info.cost?.[field];
      rows.push([`${title} rate`, rate == null ? "Unknown" : `$${rate} per million tokens`, `cost.${field}`]);
    }
  }
  return <section className="model-provenance" aria-label="Model metadata provenance">
    <p><strong>{model}</strong></p>
    <p className="mono dim diagnostics-note">{metadata.endpoint ?? "Endpoint unknown"}</p>
    <div className="provenance-table" role="table" aria-label="Effective model facts">
      <div className="provenance-row heading" role="row"><span role="columnheader">Fact</span><span role="columnheader">Effective value</span><span role="columnheader">Source</span></div>
      {rows.map(([title, value, field]) => <div className="provenance-row" role="row" key={field}>
        <span role="cell">{title}</span><span role="cell">{value}</span>
        <span role="cell" className="dim">{info.provenance?.[field] ? sources[info.provenance[field]] : "Unknown / not reported"}</span>
      </div>)}
    </div>
    {!metadata.pay_per_token && <p className="dim">Subscription endpoint: API-price estimates are not applied.</p>}
    <p className="dim diagnostics-note">Provider facts win over catalog fallbacks. User context overrides are capped by a smaller learned limit. Learned limits are isolated by endpoint and model; unknown values stay unknown.</p>
  </section>;
}
