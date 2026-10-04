import { useEffect, useState } from "react";

export type SessionTab = "chat" | "diff" | "terminal";

export type Route =
  | { name: "home" }
  | { name: "inbox" }
  | { name: "session"; host: string; id: string; tab: SessionTab; focusEvent: number | null };

const TABS: SessionTab[] = ["chat", "diff", "terminal"];

export function parseHash(hash: string): Route {
  const [path, query = ""] = hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  if (parts[0] === "inbox") return { name: "inbox" };
  if (parts[0] === "s" && parts.length >= 3) {
    const tab = TABS.includes(parts[3] as SessionTab) ? (parts[3] as SessionTab) : "chat";
    const e = new URLSearchParams(query).get("e");
    return { name: "session", host: parts[1], id: parts[2], tab, focusEvent: e ? Number(e) : null };
  }
  return { name: "home" };
}

export function sessionHash(host: string, id: string, tab: SessionTab = "chat", focusEvent?: number) {
  const base = `#/s/${encodeURIComponent(host)}/${encodeURIComponent(id)}`;
  const t = tab === "chat" ? "" : `/${tab}`;
  return `${base}${t}${focusEvent ? `?e=${focusEvent}` : ""}`;
}

export function navigate(hash: string) {
  if (location.hash !== hash) location.hash = hash;
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseHash(location.hash));
  useEffect(() => {
    const on = () => setRoute(parseHash(location.hash));
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  return route;
}
