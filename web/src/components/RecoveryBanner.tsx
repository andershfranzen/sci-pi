import { useEffect, useState } from "react";
import { emit, type HostState } from "../store";
import type { Session } from "../types";
import { Modal } from "./Modal";
import "./RecoveryBanner.css";

export function RecoveryBanner({ h, session }: { h: HostState; session: Session }) {
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const recovery = session.recovery;
  useEffect(() => {
    setConfirm(false);
    setBusy(false);
    setError(null);
  }, [session.id, recovery?.turn]);
  if (!recovery || recovery.outcome === "completed") return null;
  const active = session.status === "running" || session.status === "awaiting_permission" || session.status === "starting";
  const disabled = busy || active || session.pending_permissions > 0;
  const canResume = session.queue_paused || session.status === "detached" || session.status === "stopped" || session.status === "error";
  async function act(action: "retry" | "resume") {
    setBusy(true);
    setError(null);
    try {
      const updated = await h.api.call<Session>("POST", `/sessions/${encodeURIComponent(session.id)}/${action}`);
      h.sessions.set(updated.id, updated);
      emit();
      setConfirm(false);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <section className="recovery-banner" aria-label="Turn recovery">
      <strong>Turn {recovery.turn} {recovery.outcome}</strong>
      <p>{recovery.message}</p>
      <p>{recovery.completed_tools} of {recovery.started_tools} tool calls completed{recovery.started_tools > recovery.completed_tools ? "; remaining tool effects may be incomplete or unknown" : ""}. Nothing has been rolled back.</p>
      {session.queue_paused && <p>{session.queue.length} queued prompt{session.queue.length === 1 ? " is" : "s are"} paused until you choose how to continue.</p>}
      {session.retry_pending && <p>A new retry attempt is queued. Resume to start it; another retry would duplicate the prompt.</p>}
      {error && <div className="form-error" role="alert">{error}</div>}
      {recovery.resumable && (
        <div className="recovery-actions">
          {canResume && <button className="btn btn-ghost" disabled={disabled} onClick={() => void act("resume")}>{session.queue.length ? "Resume queued prompts" : "Resume agent"}</button>}
          <button className="btn btn-primary" disabled={disabled || session.retry_pending} onClick={() => setConfirm(true)}>Retry as new attempt…</button>
          <span className="dim">Resume does not repeat the interrupted prompt. You can also send a new instruction.</span>
        </div>
      )}
      {confirm && (
        <Modal title={`Retry turn ${recovery.turn} as a new attempt?`} onClose={() => !busy && setConfirm(false)} small>
          <p className="modal-text">This sends the original prompt and attachments again as a new numbered turn, ahead of queued prompts. Earlier partial output remains in the timeline. Files, commands, network requests, and other tool effects are not reverted and may be repeated. Review the working tree and external effects before retrying.</p>
          {error && <div className="form-error" role="alert">{error}</div>}
          <div className="modal-actions">
            <button className="btn btn-ghost" disabled={busy} onClick={() => setConfirm(false)}>Keep current state</button>
            <button className="btn btn-primary" disabled={disabled || session.retry_pending} onClick={() => void act("retry")}>{busy ? "Starting…" : "Start new attempt"}</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
