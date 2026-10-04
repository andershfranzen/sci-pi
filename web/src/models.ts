// Display helpers for agent config options: pretty model names, provider grouping, toggles.
import type { ConfigOption } from "./types";

const PROVIDERS: Record<string, string> = {
  cliproxy: "CLIProxyAPI",
  mock: "Mock",
  anthropic: "Anthropic",
  openai: "OpenAI",
  google: "Google",
  openrouter: "OpenRouter",
  ollama: "Ollama",
  local: "Local",
};

const FAMILIES: Record<string, string> = {
  claude: "Claude",
  gpt: "GPT",
  "gpt-image": "GPT Image",
  codex: "Codex",
  o1: "OpenAI o-series",
  o3: "OpenAI o-series",
  o4: "OpenAI o-series",
  gemini: "Gemini",
  grok: "Grok",
  llama: "Llama",
  qwen: "Qwen",
  deepseek: "DeepSeek",
  mistral: "Mistral",
};

const WORDS: Record<string, string> = { gpt: "GPT", xhigh: "XHigh", api: "API", ai: "AI", oss: "OSS" };

function cap(w: string) {
  if (WORDS[w]) return WORDS[w];
  return w.charAt(0).toUpperCase() + w.slice(1);
}

/** "cliproxy/gpt-6.1-sol" → { provider: "cliproxy", id: "gpt-6.1-sol" } */
export function splitModel(value: string) {
  const i = value.indexOf("/");
  return i > 0 ? { provider: value.slice(0, i), id: value.slice(i + 1) } : { provider: null, id: value };
}

/** A raw id such as "gpt-6.1-sol (cliproxy)" or "claude-opus-4-5-20251101" → human label. */
export function prettyModelId(id: string): { label: string; snapshot: string | null } {
  const tokens = id.toLowerCase().split(/[-_]/).filter(Boolean);
  let snapshot: string | null = null;
  const out: string[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i];
    if (/^\d{8}$/.test(t)) {
      snapshot = `${t.slice(0, 4)}-${t.slice(4, 6)}-${t.slice(6)}`;
      continue;
    }
    // claude-opus-4-5 → "Opus 4.5"; claude-3-5-haiku → "3.5 Haiku"
    if (/^\d$/.test(t) && /^\d$/.test(tokens[i + 1] ?? "")) {
      out.push(`${t}.${tokens[i + 1]}`);
      i++;
      continue;
    }
    out.push(/^[\d.]+$/.test(t) ? t : cap(t));
  }
  // "GPT 6.1 Sol" reads better as "GPT-6.1 Sol"
  let label = out.join(" ").replace(/^GPT (\d)/, "GPT-$1");
  if (!label) label = id;
  return { label, snapshot };
}

function looksRaw(name: string, id: string) {
  const n = name.replace(/\s*\([^)]*\)\s*$/, "").trim();
  return n === id || n === id.split("/").pop() || /^[a-z0-9][a-z0-9._-]*$/.test(n);
}

function family(id: string) {
  const t = id.toLowerCase();
  if (t.startsWith("gpt-image")) return "gpt-image";
  const first = t.split(/[-_.]/)[0];
  return first;
}

function version(id: string) {
  const { label } = prettyModelId(id);
  const m = /(\d+(?:\.\d+)?)/.exec(label);
  return m ? parseFloat(m[1]) : 0;
}

function vendorOf(fam: string) {
  if (fam === "claude") return "Anthropic";
  if (fam.startsWith("gpt") || /^o\d/.test(fam) || fam === "codex") return "OpenAI";
  if (fam === "gemini") return "Google";
  if (fam === "grok") return "xAI";
  return "Models";
}

export interface ModelChoice {
  value: unknown;
  label: string;
  description?: string;
  group: string;
}

/** Model options → pretty labels, grouped by provider (and by family within big providers), newest first. */
export function modelChoices(o: ConfigOption): ModelChoice[] {
  const opts = o.options ?? [];
  const items = opts.map((x, idx) => {
    const raw = String(x.value);
    const { provider, id } = splitModel(raw);
    const pretty = prettyModelId(id);
    const label = looksRaw(x.name, raw) ? pretty.label : x.name;
    const fam = family(id);
    const descParts = [x.description, pretty.snapshot ? `Snapshot ${pretty.snapshot}` : null].filter(Boolean);
    return { x, idx, provider, id, label, fam, description: descParts.join(" · ") || undefined };
  });
  const byProvider = new Map<string, typeof items>();
  for (const it of items) {
    const key = it.provider ?? "";
    byProvider.set(key, [...(byProvider.get(key) ?? []), it]);
  }
  const out: ModelChoice[] = [];
  for (const [prov, list] of byProvider) {
    const provLabel = prov ? (PROVIDERS[prov] ?? cap(prov)) : null;
    const famGroups = new Map<string, typeof items>();
    for (const it of list) famGroups.set(it.fam, [...(famGroups.get(it.fam) ?? []), it]);
    const split = list.length > 8 && famGroups.size > 1;
    const groups = split ? [...famGroups.entries()] : [["", list] as [string, typeof items]];
    for (const [fam, g] of groups) {
      const famLabel = fam ? (FAMILIES[fam] ?? cap(fam)) : "";
      // Plain ids ("claude-opus-5-5") talk to the vendor directly: name the vendor.
      const vendor = provLabel ?? vendorOf(fam || list[0]?.fam || "");
      const header = split ? `${famLabel} · ${vendor}` : vendor;
      const sorted = split ? [...g].sort((a, b) => version(b.id) - version(a.id) || a.idx - b.idx) : g;
      for (const it of sorted) out.push({ value: it.x.value, label: it.label, description: it.description, group: header });
    }
  }
  return out;
}

/** Label for the current value of a model option (for the trigger chip). */
export function modelLabel(o: ConfigOption, value: unknown): string {
  const hit = o.options?.find((x) => x.value === value);
  const raw = String(value ?? "");
  if (hit && !looksRaw(hit.name, raw)) return hit.name;
  return prettyModelId(splitModel(raw).id).label;
}

const ON = /^(on|true|enabled?|yes|1)$/i;
const OFF = /^(off|false|disabled?|no|0)$/i;

/** Boolean options, or 2-value selects whose values look like on/off. */
export function toggleInfo(o: ConfigOption): { on: boolean; onValue: unknown; offValue: unknown } | null {
  if (o.type === "boolean") return { on: o.currentValue === true, onValue: true, offValue: false };
  if (o.type !== "select" || o.options?.length !== 2) return null;
  const [a, b] = o.options;
  const sa = String(a.value);
  const sb = String(b.value);
  const onOpt = ON.test(sa) && OFF.test(sb) ? a : ON.test(sb) && OFF.test(sa) ? b : null;
  if (!onOpt) return null;
  const offOpt = onOpt === a ? b : a;
  return { on: o.currentValue === onOpt.value, onValue: onOpt.value, offValue: offOpt.value };
}

/** "Fast mode" → "Fast" */
export function toggleLabel(o: ConfigOption) {
  return o.name.replace(/\s+mode$/i, "");
}

export function prettyValueName(name: string) {
  if (/^[a-z]+$/.test(name)) return name === "xhigh" ? "Extra high" : cap(name);
  return name;
}
