// Session-isolation concept: DeepSeek Harness packages/client/ui-user-questions/src/client/draft-store.ts.
// Native IndexedDB implementation is local; upstream notice: /web/public/DEEPSEEK-LICENSE.
import { useEffect, useSyncExternalStore } from "react";

export interface DraftImage { id: string; mime_type: string; blob: Blob; name: string; size: number }
interface Draft { text: string; files: string[]; fileTokens: Record<string, string>; textVersion: number; images: DraftImage[]; unavailableImages: string[]; hydrated: boolean; warning: string | null; saving: boolean; sending: boolean }
interface StoredDraft { text: string; files: string[]; images: string[] }

function storedDraft(value: unknown): StoredDraft | undefined {
  if (!value || typeof value !== "object" || !("text" in value) || typeof value.text !== "string" || !("files" in value) || !Array.isArray(value.files) || !("images" in value) || !Array.isArray(value.images)) return;
  const files: unknown[] = value.files;
  const images: unknown[] = value.images;
  if (!files.every((path): path is string => typeof path === "string") || !images.every((id): id is string => typeof id === "string")) return;
  return { text: value.text, files, images };
}
function storedImage(value: unknown): DraftImage | undefined {
  if (!value || typeof value !== "object" || !("id" in value) || typeof value.id !== "string" || !("mime_type" in value) || typeof value.mime_type !== "string" || !("blob" in value) || !(value.blob instanceof Blob) || !("name" in value) || typeof value.name !== "string" || !("size" in value) || typeof value.size !== "number") return;
  return { id: value.id, mime_type: value.mime_type, blob: value.blob, name: value.name, size: value.size };
}
interface Entry { value: Draft; legacyKey: string; listeners: Set<() => void>; loading: Promise<void>; timer?: number; pending: number; holds: number; chain: Promise<void> }
const entries = new Map<string, Entry>();
let database: Promise<IDBDatabase> | undefined;
function db() {
  if (database) return database;
  // ES2023 browser target: Promise.withResolvers is not available.
  database = new Promise<IDBDatabase>((resolve, reject) => {
    try {
      const request = indexedDB.open("sci-pi-composer", 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore("drafts");
        request.result.createObjectStore("images", { keyPath: "id" });
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
      request.onblocked = () => reject(new Error("Draft database is blocked by another tab"));
    } catch (error) { reject(error); }
  });
  return database;
}
function result<T>(request: IDBRequest<T>) {
  return new Promise<T>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
function complete(tx: IDBTransaction) {
  return new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onabort = tx.onerror = () => reject(tx.error ?? new Error("Draft transaction failed"));
  });
}
const journalKey = (key: string) => `sci-pi.draft-meta.${key}`;
const metadata = (value: Draft): StoredDraft => ({ text: value.text, files: value.files, images: [...value.images.map(i => i.id), ...value.unavailableImages] });
function notify(entry: Entry, value: Draft) { entry.value = value; entry.listeners.forEach(fn => fn()); }
function warning(entry: Entry, error: unknown) {
  notify(entry, { ...entry.value, warning: `Draft storage unavailable: ${error instanceof Error ? error.message : String(error)}. Keep this page open or copy your draft; attachments may not survive reload.` });
}
function journal(key: string, entry: Entry) {
  try {
    const record = metadata(entry.value);
    if (record.text || record.files.length || record.images.length) localStorage.setItem(journalKey(key), JSON.stringify(record));
    else localStorage.removeItem(journalKey(key));
    return true;
  } catch (error) {
    // An older journal must not shadow a newer successful IndexedDB commit.
    try { localStorage.removeItem(journalKey(key)); } catch { /* Unavailable storage cannot supply a journal on reload either. */ }
    warning(entry, error);
    return false;
  }
}
function persist(key: string, entry: Entry) {
  clearTimeout(entry.timer);
  const snapshot = entry.value;
  if (!snapshot.hydrated) return;
  const record = metadata(snapshot);
  entry.pending++;
  notify(entry, { ...entry.value, saving: true });
  entry.chain = entry.chain.then(async () => {
    const database = await db();
    const tx = database.transaction(["drafts", "images"], "readwrite");
    const done = complete(tx);
    await Promise.all([done, (async () => {
      const store = tx.objectStore("images");
      const previous = storedDraft(await result(tx.objectStore("drafts").get(key)));
      // Image IDs are globally unique; existing payloads are never rewritten on text changes.
      for (const image of snapshot.images) {
        if (!await result(store.getKey(image.id))) store.put(image);
      }
      for (const id of previous?.images ?? []) if (!record.images.includes(id)) store.delete(id);
      if (record.text || record.files.length || record.images.length) tx.objectStore("drafts").put(record, key);
      else tx.objectStore("drafts").delete(key);
    })()]);
    try { sessionStorage.removeItem(entry.legacyKey); } catch { /* Legacy storage may be disabled. */ }
    if (journal(key, entry)) notify(entry, { ...entry.value, warning: null });
  }).catch(error => warning(entry, error)).finally(() => {
    entry.pending--;
    notify(entry, { ...entry.value, saving: entry.pending > 0 });
  });
}
function getEntry(key: string, legacyKey: string) {
  const cached = entries.get(key);
  if (cached) return cached;
  let legacy = "";
  try { legacy = sessionStorage.getItem(legacyKey) ?? ""; } catch { /* IndexedDB may still work. */ }
  let journalRecord: StoredDraft | undefined;
  let journalError: unknown;
  try { const raw = localStorage.getItem(journalKey(key)); if (raw) journalRecord = storedDraft(JSON.parse(raw)); }
  catch (error) { journalError = error; }
  const seededFromJournal = !!journalRecord && (!legacy || legacy === journalRecord.text);
  const initialText = legacy || journalRecord?.text || "";
  const entry: Entry = { value: { text: initialText, files: [], fileTokens: {}, textVersion: 0, images: [], unavailableImages: [], hydrated: false, warning: null, saving: false, sending: false }, legacyKey, listeners: new Set(), pending: 0, holds: 0, chain: Promise.resolve(), loading: Promise.resolve() };
  entries.set(key, entry);
  if (journalError) warning(entry, journalError);
  entry.loading = (async () => {
    let stored = journalRecord;
    let images: (DraftImage | undefined)[] = [];
    try {
      const database = await db();
      const tx = database.transaction(["drafts", "images"], "readonly");
      const saved = storedDraft(await result(tx.objectStore("drafts").get(key)));
      stored ??= saved;
      // Issue all reads while the transaction is active.
      images = stored ? await Promise.all(stored.images.map(async id => storedImage(await result(tx.objectStore("images").get(id))))) : [];
    } catch (error) { warning(entry, error); }
    const current = entry.value;
    const files = [...new Set([...(stored?.files ?? []), ...current.files])];
    const fileTokens = Object.fromEntries(files.map(path => [path, current.fileTokens[path] ?? crypto.randomUUID()]));
    const restored = images.filter((i): i is DraftImage => !!i);
    const unavailableImages = (stored?.images ?? []).filter(id => !restored.some(image => image.id === id));
    const savedText = stored?.text ?? "";
    // Journal text is visible before the async read. Without a journal, preserve
    // both a recovered saved draft and edits made while restoration was pending.
    const text = seededFromJournal ? current.text : savedText && current.text && savedText !== current.text ? `${savedText}\n\n${current.text}` : savedText || current.text;
    notify(entry, { ...current, text, files, fileTokens, images: [...restored, ...current.images.filter(i => !stored?.images.includes(i.id))], unavailableImages, hydrated: true });
    journal(key, entry);
    persist(key, entry);
  })();
  return entry;
}
export function useComposerDraft(hostKey: string, sessionId: string) {
  const key = JSON.stringify([hostKey, sessionId]);
  const entry = getEntry(key, `sci-pi.draft.${hostKey}.${sessionId}`);
  const value = useSyncExternalStore(fn => { entry.listeners.add(fn); return () => { entry.listeners.delete(fn); }; }, () => entry.value);
  useEffect(() => {
    const flush = () => { if (entry.value.hydrated) { journal(key, entry); persist(key, entry); } };
    const beforeUnload = (event: BeforeUnloadEvent) => {
      const unsafe = entry.pending > 0 || !!entry.value.warning || !entry.value.hydrated;
      flush();
      if (unsafe) { event.preventDefault(); event.returnValue = ""; }
    };
    window.addEventListener("pagehide", flush);
    window.addEventListener("beforeunload", beforeUnload);
    return () => {
      window.removeEventListener("pagehide", flush);
      window.removeEventListener("beforeunload", beforeUnload);
      flush();
      // Release payload memory only after writes settle; StrictMode remounts keep their entry.
      void entry.loading.then(() => entry.chain).then(() => {
        if (!entry.listeners.size && !entry.holds && !entry.value.warning && !entry.pending && entries.get(key) === entry) entries.delete(key);
      });
    };
  }, [key, entry]);
  const change = <K extends "text" | "files" | "images" | "unavailableImages">(field: K, update: Draft[K] | ((previous: Draft[K]) => Draft[K])) => {
    const next = typeof update === "function" ? update(entry.value[field]) : update;
    const fileTokens = field === "files" ? Object.fromEntries((next as string[]).map(path => [path, entry.value.fileTokens[path] ?? crypto.randomUUID()])) : entry.value.fileTokens;
    notify(entry, { ...entry.value, [field]: next, fileTokens, textVersion: entry.value.textVersion + (field === "text" ? 1 : 0) });
    if (!entry.value.hydrated) return;
    journal(key, entry);
    if (field === "text") { clearTimeout(entry.timer); entry.timer = window.setTimeout(() => persist(key, entry), 200); }
    else persist(key, entry);
  };
  const releaseWhenIdle = () => {
    void entry.loading.then(() => entry.chain).then(() => {
      if (!entry.listeners.size && !entry.holds && !entry.value.warning && !entry.pending && entries.get(key) === entry) entries.delete(key);
    });
  };
  return {
    ...value,
    getSnapshot: () => entry.value,
    beginSend: () => {
      if (entry.value.sending || !entry.value.hydrated) return null;
      entry.holds++;
      notify(entry, { ...entry.value, sending: true });
      return () => {
        entry.holds--;
        notify(entry, { ...entry.value, sending: false });
        releaseWhenIdle();
      };
    },
    setText: (v: string | ((p: string) => string)) => change("text", v),
    setFiles: (v: string[] | ((p: string[]) => string[])) => change("files", v),
    setImages: (v: DraftImage[] | ((p: DraftImage[]) => DraftImage[])) => change("images", v),
    discardUnavailableImages: () => change("unavailableImages", []),
  };
}
