import type { SessionStatus } from "../types";
import type { ConnStatus } from "../store";

const LABEL: Record<SessionStatus, string> = {
  starting: "Starting",
  idle: "Idle",
  running: "Running",
  awaiting_permission: "Needs approval",
  detached: "Detached",
  stopped: "Stopped",
  error: "Error",
};

export function statusLabel(s: SessionStatus) {
  return LABEL[s] ?? s;
}

export function StatusDot({ status, title }: { status: SessionStatus; title?: string }) {
  return <span className={`sdot s-${status}`} title={title ?? statusLabel(status)} aria-label={statusLabel(status)} />;
}

export function StatusPill({ status }: { status: SessionStatus }) {
  return (
    <span className={`spill s-${status}`}>
      <StatusDot status={status} />
      {statusLabel(status)}
    </span>
  );
}

export function ConnDot({ conn, hub }: { conn: ConnStatus; hub?: string }) {
  const cls = conn === "open" ? "ok" : conn === "auth" || hub === "error" ? "bad" : "warn";
  const title =
    conn === "open" ? "Connected" : conn === "auth" ? "Token rejected" : hub === "error" ? "Tunnel error" : "Connecting…";
  return <span className={`cdot ${cls}`} title={title} aria-label={title} />;
}
