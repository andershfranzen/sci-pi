// Line-level diffing (for ACP diff content) and unified-diff parsing (for the Diff tab).

export type LineType = "eq" | "add" | "del";

export interface DiffLine {
  type: LineType;
  text: string;
  /** 1-based line numbers in old / new text */
  a?: number;
  b?: number;
}

export type DiffRow = DiffLine | { type: "gap"; count: number };

function splitLines(s: string | null): string[] {
  if (!s) return [];
  const lines = s.split("\n");
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** Myers O((N+M)D) line diff with prefix/suffix trimming and a size guard. */
export function lineDiff(oldText: string | null, newText: string): DiffLine[] {
  const a = splitLines(oldText);
  const b = splitLines(newText);
  let pre = 0;
  while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;

  const out: DiffLine[] = [];
  for (let i = 0; i < pre; i++) out.push({ type: "eq", text: a[i], a: i + 1, b: i + 1 });
  const am = a.slice(pre, a.length - suf);
  const bm = b.slice(pre, b.length - suf);
  for (const l of myers(am, bm)) {
    out.push({
      type: l.type,
      text: l.text,
      a: l.a !== undefined ? l.a + pre + 1 : undefined,
      b: l.b !== undefined ? l.b + pre + 1 : undefined,
    });
  }
  for (let i = suf; i > 0; i--) {
    const ai = a.length - i;
    const bi = b.length - i;
    out.push({ type: "eq", text: a[ai], a: ai + 1, b: bi + 1 });
  }
  return out;
}

interface RawLine {
  type: LineType;
  text: string;
  a?: number;
  b?: number;
}

const MAX_D = 1500;

function myers(a: string[], b: string[]): RawLine[] {
  const n = a.length;
  const m = b.length;
  if (n === 0) return b.map((text, i) => ({ type: "add" as const, text, b: i }));
  if (m === 0) return a.map((text, i) => ({ type: "del" as const, text, a: i }));

  const max = n + m;
  const off = max + 1;
  const v = new Int32Array(2 * max + 3);
  const snaps: Int32Array[] = [];
  let found = -1;
  outer: for (let d = 0; d <= Math.min(max, MAX_D); d++) {
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[off + k - 1] < v[off + k + 1]) ? v[off + k + 1] : v[off + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) {
        x++;
        y++;
      }
      v[off + k] = x;
      if (x >= n && y >= m) {
        snaps.push(v.slice(off - d, off + d + 1));
        found = d;
        break outer;
      }
    }
    snaps.push(v.slice(off - d, off + d + 1));
  }
  if (found < 0) {
    // Too different: show as a full replacement.
    return [
      ...a.map((text, i) => ({ type: "del" as const, text, a: i })),
      ...b.map((text, i) => ({ type: "add" as const, text, b: i })),
    ];
  }

  const rev: RawLine[] = [];
  let x = n;
  let y = m;
  for (let d = found; d > 0; d--) {
    const prev = snaps[d - 1];
    const at = (k: number) => prev[k + (d - 1)];
    const k = x - y;
    const down = k === -d || (k !== d && at(k - 1) < at(k + 1));
    const prevK = down ? k + 1 : k - 1;
    const prevX = at(prevK);
    const prevY = prevX - prevK;
    while (x > prevX && y > prevY) {
      x--;
      y--;
      rev.push({ type: "eq", text: a[x], a: x, b: y });
    }
    if (down) {
      y--;
      rev.push({ type: "add", text: b[y], b: y });
    } else {
      x--;
      rev.push({ type: "del", text: a[x], a: x });
    }
    x = prevX;
    y = prevY;
  }
  while (x > 0 && y > 0) {
    x--;
    y--;
    rev.push({ type: "eq", text: a[x], a: x, b: y });
  }
  return rev.reverse();
}

/** Collapse long runs of unchanged lines, keeping `ctx` lines of context around changes. */
export function withContext(lines: DiffLine[], ctx = 3): DiffRow[] {
  const keep = new Uint8Array(lines.length);
  lines.forEach((l, i) => {
    if (l.type !== "eq") {
      for (let j = Math.max(0, i - ctx); j <= Math.min(lines.length - 1, i + ctx); j++) keep[j] = 1;
    }
  });
  const out: DiffRow[] = [];
  let gap = 0;
  lines.forEach((l, i) => {
    if (keep[i]) {
      if (gap) out.push({ type: "gap", count: gap });
      gap = 0;
      out.push(l);
    } else gap++;
  });
  if (gap) out.push({ type: "gap", count: gap });
  return out;
}

export function diffStats(lines: DiffLine[]) {
  let add = 0;
  let del = 0;
  for (const l of lines) {
    if (l.type === "add") add++;
    else if (l.type === "del") del++;
  }
  return { add, del };
}

// ---------------------------------------------------------------- unified diff

export type UniLineType = "add" | "del" | "ctx" | "hunk" | "meta";

export interface UniLine {
  type: UniLineType;
  text: string;
  a?: number;
  b?: number;
}

export interface FileDiff {
  path: string;
  oldPath: string | null;
  lines: UniLine[];
  add: number;
  del: number;
  binary: boolean;
}

function stripPrefix(p: string) {
  if (p === "/dev/null") return p;
  return p.replace(/^[ab]\//, "");
}

export function parseUnifiedDiff(text: string): FileDiff[] {
  const files: FileDiff[] = [];
  let cur = null as FileDiff | null;
  let a = 0;
  let b = 0;
  let inHunk = false;
  let aLeft = 0;
  let bLeft = 0;
  const start = (path: string): FileDiff => {
    const f: FileDiff = { path, oldPath: null, lines: [], add: 0, del: 0, binary: false };
    files.push(f);
    inHunk = false;
    return f;
  };
  for (const line of text.split("\n")) {
    if (line.startsWith("diff --git ")) {
      const m = /^diff --git a\/(.+?) b\/(.+)$/.exec(line);
      cur = start(m ? m[2] : line.slice(11));
      continue;
    }
    if (!inHunk && line.startsWith("--- ")) {
      const old = stripPrefix(line.slice(4).trim());
      if (!cur) cur = start(old);
      cur.oldPath = old;
      continue;
    }
    if (!inHunk && line.startsWith("+++ ")) {
      const p = stripPrefix(line.slice(4).trim());
      if (cur && p !== "/dev/null") cur.path = p;
      continue;
    }
    if (!cur) continue;
    const f: FileDiff = cur;
    if (line.startsWith("@@")) {
      const m = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
      a = m ? +m[1] : 0;
      b = m ? +m[3] : 0;
      aLeft = m ? (m[2] === undefined ? 1 : +m[2]) : Infinity;
      bLeft = m ? (m[4] === undefined ? 1 : +m[4]) : Infinity;
      inHunk = true;
      f.lines.push({ type: "hunk", text: line });
      continue;
    }
    if (!inHunk) {
      if (line.startsWith("Binary files")) f.binary = true;
      if (line.startsWith("new file") || line.startsWith("deleted file") || line.startsWith("rename") || line.startsWith("Binary"))
        f.lines.push({ type: "meta", text: line });
      continue;
    }
    if (line.startsWith("\\")) {
      f.lines.push({ type: "meta", text: line });
    } else if (line.startsWith("+")) {
      f.lines.push({ type: "add", text: line.slice(1), b: b++ });
      f.add++;
      bLeft--;
    } else if (line.startsWith("-")) {
      f.lines.push({ type: "del", text: line.slice(1), a: a++ });
      f.del++;
      aLeft--;
    } else {
      // " text", or "" when a tool stripped the trailing space of an empty context line
      f.lines.push({ type: "ctx", text: line.slice(1), a: a++, b: b++ });
      aLeft--;
      bLeft--;
    }
    if (aLeft <= 0 && bLeft <= 0) inHunk = false;
  }
  return files;
}
