// Browser notifications for permission requests / finished turns while the tab is hidden.
import type { HostState } from "./store";
import { store } from "./store";
import type { OEvent, Session } from "./types";
import { sessionHash } from "./router";

export function notificationsSupported() {
  return typeof window !== "undefined" && "Notification" in window;
}

export async function requestNotifications(): Promise<boolean> {
  if (!notificationsSupported()) return false;
  if (Notification.permission === "granted") return true;
  if (Notification.permission === "denied") return false;
  return (await Notification.requestPermission()) === "granted";
}

export function maybeNotify(h: HostState, s: Session | undefined, e: OEvent) {
  if (!store.notify || !document.hidden || !notificationsSupported()) return;
  if (Notification.permission !== "granted") return;
  // Skip events replayed after a long disconnect; they are history, not news.
  if (Date.now() - e.ts > 60_000) return;
  const title = s?.title || "Session";
  const where = store.hosts.length > 1 ? ` · ${h.name}` : "";
  let body: string;
  if (e.kind === "permission_request") {
    const tc = e.data?.tool_call ?? {};
    body = `Permission needed: ${tc.title ?? "tool call"}`;
  } else {
    const reason = String(e.data?.stop_reason ?? "end_turn");
    body = reason === "end_turn" ? "Turn finished" : `Turn ended: ${reason.replace(/_/g, " ")}`;
  }
  try {
    const n = new Notification(`${title}${where}`, {
      body,
      tag: `outpost:${h.key}:${e.session_id}`,
    });
    n.onclick = () => {
      window.focus();
      location.hash = sessionHash(h.key, e.session_id);
      n.close();
    };
  } catch {
    /* some mobile browsers only allow notifications from a service worker */
  }
}
