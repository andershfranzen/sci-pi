//! The native agent's tools. All in-process Rust except `bash`.
//!
//! Edits are line-addressed (the "hashline" idea from oh-my-pi): `read_file` prints numbered
//! lines under a `[path#TAG]` header, where TAG is a 4-hex hash of the whole file. `edit_lines`
//! names line ranges plus that tag, so the model never re-types old text, and a stale tag
//! (the file changed since it was read) is rejected instead of corrupting the file. Every edit
//! returns the new tag and the renumbered region, so edits can be chained without re-reading.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const MAX_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_OUTPUT_CHARS: usize = 30_000;
const MAX_MATCHES: usize = 250;

#[derive(Debug)]
pub struct Output {
    /// What the model sees.
    pub text: String,
    pub is_error: bool,
    /// ACP tool-call content for the UI (diffs, text).
    pub content: Vec<Value>,
}

impl Output {
    fn ok(text: String) -> Self {
        Output { text, is_error: false, content: vec![] }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Output { text: text.into(), is_error: true, content: vec![] }
    }
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_file",
            "description": "Read a text file. Output starts with a `[path#TAG]` header (TAG identifies this exact file version; pass it to edit_lines) followed by `N:content` lines. Reads up to 2000 lines; use offset/limit for more.",
            "input_schema": { "type": "object", "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "1-based first line" },
                "limit": { "type": "integer" }
            }, "required": ["path"] }
        }),
        json!({
            "name": "edit_lines",
            "description": "Edit a file by line number, against the version you read (`tag` from the read_file/edit header). Each edit either replaces lines start..=end with `lines` (empty `lines` deletes them) or inserts `lines` after line `after` (0 = top of file). Line numbers refer to the version identified by `tag`, so several edits in one call never shift each other. Returns the new tag and the edited regions renumbered, so you can keep editing without re-reading.",
            "input_schema": { "type": "object", "properties": {
                "path": { "type": "string" },
                "tag": { "type": "string" },
                "edits": { "type": "array", "items": { "type": "object", "properties": {
                    "start": { "type": "integer" },
                    "end": { "type": "integer" },
                    "after": { "type": "integer" },
                    "lines": { "type": "array", "items": { "type": "string" } }
                }, "required": ["lines"] } }
            }, "required": ["path", "tag", "edits"] }
        }),
        json!({
            "name": "edit_file",
            "description": "Replace an exact, unique string in a file (use replace_all for every occurrence). Handy for small one-line changes; prefer edit_lines for anything larger.",
            "input_schema": { "type": "object", "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            }, "required": ["path", "old_string", "new_string"] }
        }),
        json!({
            "name": "write_file",
            "description": "Create or overwrite a whole file. Parent directories are created.",
            "input_schema": { "type": "object", "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            }, "required": ["path", "content"] }
        }),
        json!({
            "name": "bash",
            "description": "Run a shell command (bash) in the working directory. stdout and stderr are combined; long output keeps the start and end. Default timeout 120s, max 600s.",
            "input_schema": { "type": "object", "properties": {
                "command": { "type": "string" },
                "timeout_secs": { "type": "integer" }
            }, "required": ["command"] }
        }),
        json!({
            "name": "grep",
            "description": "Regex search over files (respects .gitignore). Returns `path:line:text` matches, or just paths with files_only.",
            "input_schema": { "type": "object", "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "File or directory (default: working directory)" },
                "glob": { "type": "string", "description": "Only files matching this glob, e.g. **/*.rs" },
                "case_insensitive": { "type": "boolean" },
                "files_only": { "type": "boolean" }
            }, "required": ["pattern"] }
        }),
        json!({
            "name": "glob",
            "description": "Find files by glob pattern (respects .gitignore), most recently modified first.",
            "input_schema": { "type": "object", "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" }
            }, "required": ["pattern"] }
        }),
        json!({
            "name": "list_dir",
            "description": "List a directory (directories end with /).",
            "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": [] }
        }),
        json!({
            "name": "todo_write",
            "description": "Replace the task list shown to the user. Use it for multi-step work and keep statuses current.",
            "input_schema": { "type": "object", "properties": {
                "todos": { "type": "array", "items": { "type": "object", "properties": {
                    "content": { "type": "string" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                }, "required": ["content", "status"] } }
            }, "required": ["todos"] }
        }),
    ]
}

/// ACP tool kind, used for icons and permission policy.
pub fn kind(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" => "read",
        "edit_lines" | "edit_file" | "write_file" => "edit",
        "bash" => "execute",
        "grep" | "glob" => "search",
        "todo_write" => "think",
        _ => "other",
    }
}

/// Safe to run concurrently with other read-only calls.
pub fn read_only(name: &str) -> bool {
    matches!(kind(name), "read" | "search" | "think")
}

pub fn title(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    match name {
        "read_file" => format!("Read {}", s("path")),
        "edit_lines" | "edit_file" => format!("Edit {}", s("path")),
        "write_file" => format!("Write {}", s("path")),
        "bash" => s("command"),
        "grep" => format!("grep {}", s("pattern")),
        "glob" => format!("glob {}", s("pattern")),
        "list_dir" => format!("List {}", input["path"].as_str().unwrap_or(".")),
        "todo_write" => "Update plan".into(),
        other => other.to_string(),
    }
}

pub async fn run(name: &str, input: &Value, cwd: &Path, cancel: &CancellationToken) -> Output {
    let result = match name {
        "read_file" => read_file(input, cwd),
        "edit_lines" => edit_lines(input, cwd),
        "edit_file" => edit_file(input, cwd),
        "write_file" => write_file(input, cwd),
        "bash" => bash(input, cwd, cancel).await,
        "grep" => {
            let (input, cwd) = (input.clone(), cwd.to_path_buf());
            tokio::task::spawn_blocking(move || grep(&input, &cwd)).await.map_err(|e| anyhow!(e)).and_then(|r| r)
        }
        "glob" => {
            let (input, cwd) = (input.clone(), cwd.to_path_buf());
            tokio::task::spawn_blocking(move || glob(&input, &cwd)).await.map_err(|e| anyhow!(e)).and_then(|r| r)
        }
        "list_dir" => list_dir(input, cwd),
        "todo_write" => Ok(Output::ok("Plan updated.".into())),
        other => Err(anyhow!("unknown tool `{other}`")),
    };
    result.unwrap_or_else(|e| Output::err(format!("{e:#}")))
}

fn arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = crate::config::expand_tilde(path);
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// 4-hex snapshot tag of a file's exact content (FNV-1a, top 16 bits).
pub fn tag(content: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in content.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:04X}", (h >> 48) as u16)
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        bail!("{} looks like a binary file", path.display());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn numbered(lines: &[&str], first: usize) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        let l = if l.chars().count() > MAX_LINE_CHARS {
            format!("{}… [line truncated]", l.chars().take(MAX_LINE_CHARS).collect::<String>())
        } else {
            l.to_string()
        };
        out.push_str(&format!("{}:{}\n", first + i, l));
    }
    out
}

/// The read_file view of a whole (small) file, for inlining @-mentioned files into prompts.
pub fn read_view(path: &str, cwd: &Path) -> Result<String> {
    Ok(read_file(&json!({ "path": path }), cwd)?.text)
}

fn read_file(input: &Value, cwd: &Path) -> Result<Output> {
    let path = resolve(cwd, arg(input, "path")?);
    if path.is_dir() {
        bail!("{} is a directory; use list_dir", path.display());
    }
    let text = read_text(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64).clamp(1, MAX_READ_LINES as u64) as usize;
    let start = (offset - 1).min(lines.len());
    let end = (start + limit).min(lines.len());
    let mut out = format!("[{}#{}]", display(cwd, &path), tag(&text));
    if start > 0 || end < lines.len() {
        out.push_str(&format!(" lines {}-{} of {}", start + 1, end, lines.len()));
    }
    out.push('\n');
    if lines.is_empty() {
        out.push_str("(empty file)\n");
    }
    out.push_str(&numbered(&lines[start..end], start + 1));
    Ok(Output::ok(out))
}

fn diff_content(path: &Path, old: Option<&str>, new: &str) -> Vec<Value> {
    vec![json!({ "type": "diff", "path": path.display().to_string(), "oldText": old, "newText": new })]
}

/// Joins lines back, keeping the file's trailing-newline convention.
fn join(lines: &[String], trailing_newline: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        s.push('\n');
    }
    s
}

/// A computed edit: the file, its current and new content, and where each change landed.
struct Planned {
    path: PathBuf,
    old: Option<String>,
    new: String,
    landed: Vec<(usize, usize)>,
}

/// The diff an edit would make, without touching the file (for approval prompts).
pub fn preview(name: &str, input: &Value, cwd: &Path) -> Vec<Value> {
    let planned = match name {
        "edit_lines" => plan_edit_lines(input, cwd),
        "edit_file" => plan_edit_file(input, cwd).map(|(p, _)| p),
        "write_file" => plan_write_file(input, cwd),
        _ => return vec![],
    };
    match planned {
        Ok(p) => diff_content(&p.path, p.old.as_deref(), &p.new),
        Err(_) => vec![],
    }
}

fn edit_lines(input: &Value, cwd: &Path) -> Result<Output> {
    let Planned { path, old, new: updated, landed } = plan_edit_lines(input, cwd)?;
    std::fs::write(&path, &updated)?;
    let refs: Vec<&str> = updated.lines().collect();
    let mut out = format!("[{}#{}] edited, now {} lines\n", display(cwd, &path), tag(&updated), refs.len());
    for (start, len) in landed {
        let from = start.saturating_sub(2);
        let to = (start + len + 2).min(refs.len());
        out.push_str("…\n");
        out.push_str(&numbered(&refs[from..to], from + 1));
    }
    Ok(Output { text: out, is_error: false, content: diff_content(&path, old.as_deref(), &updated) })
}

fn plan_edit_lines(input: &Value, cwd: &Path) -> Result<Planned> {
    let path = resolve(cwd, arg(input, "path")?);
    let want = arg(input, "tag")?.trim().trim_start_matches('#').to_uppercase();
    let old = read_text(&path)?;
    let have = tag(&old);
    if want != have {
        bail!(
            "stale tag: {} is now #{have}, not #{want} (it changed since you read it). Re-read the lines you need and retry.",
            display(cwd, &path)
        );
    }
    let mut lines: Vec<String> = old.lines().map(str::to_string).collect();
    let n = lines.len();

    // Normalise every edit to (start, remove_count, new_lines), in original-file coordinates.
    let mut edits: Vec<(usize, usize, Vec<String>)> = vec![];
    for e in input["edits"].as_array().ok_or_else(|| anyhow!("`edits` must be an array"))? {
        let new: Vec<String> = e["lines"]
            .as_array()
            .ok_or_else(|| anyhow!("each edit needs `lines` (an array of strings)"))?
            .iter()
            .map(|l| l.as_str().unwrap_or_default().trim_end_matches(['\r', '\n']).to_string())
            .collect();
        if let Some(after) = e["after"].as_u64() {
            let after = after as usize;
            if after > n {
                bail!("insert after line {after}, but the file has {n} lines");
            }
            edits.push((after, 0, new));
        } else {
            let (Some(start), Some(end)) = (e["start"].as_u64(), e["end"].as_u64()) else {
                bail!("each edit needs either `after`, or `start` and `end`");
            };
            let (start, end) = (start as usize, end as usize);
            if start < 1 || end < start || end > n {
                bail!("range {start}..={end} is outside the file's 1..={n} lines");
            }
            edits.push((start - 1, end - start + 1, new));
        }
    }
    edits.sort_by_key(|e| (e.0, e.1));
    for w in edits.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 && !(w[0].1 == 0 && w[1].1 == 0) {
            bail!("edits overlap around line {}; merge them into one", w[1].0 + 1);
        }
    }

    // Apply bottom-up so original coordinates stay valid; remember where each lands.
    let mut landed = vec![];
    let mut shift: isize = 0;
    for (start, remove, new) in &edits {
        landed.push(((*start as isize + shift) as usize, new.len()));
        shift += new.len() as isize - *remove as isize;
    }
    for (start, remove, new) in edits.iter().rev() {
        lines.splice(*start..*start + *remove, new.iter().cloned());
    }
    let updated = join(&lines, old.ends_with('\n') || old.is_empty());
    Ok(Planned { path, old: Some(old), new: updated, landed })
}

fn edit_file(input: &Value, cwd: &Path) -> Result<Output> {
    let (Planned { path, old, new: updated, .. }, count) = plan_edit_file(input, cwd)?;
    std::fs::write(&path, &updated)?;
    let text = format!("[{}#{}] replaced {count} occurrence(s)", display(cwd, &path), tag(&updated));
    Ok(Output { text, is_error: false, content: diff_content(&path, old.as_deref(), &updated) })
}

fn plan_edit_file(input: &Value, cwd: &Path) -> Result<(Planned, usize)> {
    let path = resolve(cwd, arg(input, "path")?);
    let (from, to) = (arg(input, "old_string")?, arg(input, "new_string")?);
    if from.is_empty() {
        bail!("old_string is empty; use write_file or edit_lines to insert");
    }
    let old = read_text(&path)?;
    let count = old.matches(from).count();
    let all = input["replace_all"].as_bool().unwrap_or(false);
    match count {
        0 => bail!("old_string not found in {} (check whitespace and indentation, or use edit_lines)", display(cwd, &path)),
        n if n > 1 && !all => bail!("old_string occurs {n} times; add surrounding context or set replace_all"),
        _ => {}
    }
    let updated = if all { old.replace(from, to) } else { old.replacen(from, to, 1) };
    let n = if all { count } else { 1 };
    Ok((Planned { path, old: Some(old), new: updated, landed: vec![] }, n))
}

fn plan_write_file(input: &Value, cwd: &Path) -> Result<Planned> {
    let path = resolve(cwd, arg(input, "path")?);
    let new = arg(input, "content")?.to_string();
    Ok(Planned { old: std::fs::read_to_string(&path).ok(), path, new, landed: vec![] })
}

fn write_file(input: &Value, cwd: &Path) -> Result<Output> {
    let path = resolve(cwd, arg(input, "path")?);
    let content = arg(input, "content")?;
    let old = std::fs::read_to_string(&path).ok();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    let verb = if old.is_some() { "overwrote" } else { "created" };
    let text = format!("[{}#{}] {verb}, {} lines", display(cwd, &path), tag(content), content.lines().count());
    Ok(Output { text, is_error: false, content: diff_content(&path, old.as_deref(), content) })
}

/// Keeps the head and tail of long output.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut head = max * 2 / 3;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - (max - head);
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n… [{} bytes omitted] …\n{}", &s[..head], tail - head, &s[tail..])
}

async fn bash(input: &Value, cwd: &Path, cancel: &CancellationToken) -> Result<Output> {
    let command = arg(input, "command")?;
    let timeout = Duration::from_secs(input["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 600));
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = Vec::new();
    let pid = child.id();
    let kill_group = || {
        if let Some(pid) = pid {
            let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
        }
    };
    let outcome = tokio::select! {
        r = async {
            stdout.read_to_end(&mut buf).await?;
            child.wait().await
        } => Some(r?),
        _ = tokio::time::sleep(timeout) => { kill_group(); None }
        _ = cancel.cancelled() => { kill_group(); return Ok(Output::err("cancelled by the user")) }
    };
    let text = clip(&String::from_utf8_lossy(&buf), MAX_OUTPUT_CHARS);
    let (text, is_error) = match outcome {
        None => (format!("{text}\n[timed out after {}s and was killed]", timeout.as_secs()), true),
        Some(status) if status.success() => (if text.is_empty() { "(no output)".into() } else { text }, false),
        Some(status) => (format!("{text}\n[exit code {}]", status.code().unwrap_or(-1)), true),
    };
    let content = vec![json!({ "type": "content", "content": { "type": "text", "text": format!("```\n{text}\n```") } })];
    Ok(Output { text, is_error, content })
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root).hidden(true).git_ignore(true).git_global(true).parents(true).build()
}

fn grep(input: &Value, cwd: &Path) -> Result<Output> {
    let pattern = arg(input, "pattern")?;
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(input["case_insensitive"].as_bool().unwrap_or(false))
        .build()?;
    let root = resolve(cwd, input["path"].as_str().unwrap_or("."));
    let filter = match input["glob"].as_str() {
        Some(g) => Some(globset::Glob::new(g)?.compile_matcher()),
        None => None,
    };
    let files_only = input["files_only"].as_bool().unwrap_or(false);
    let mut out = vec![];
    let mut truncated = false;
    'files: for entry in walker(&root).flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = display(cwd, entry.path());
        if let Some(f) = &filter {
            let rel_root = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if !f.is_match(rel_root) && !f.is_match(&rel) {
                continue;
            }
        }
        if entry.metadata().is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
            continue;
        }
        let Ok(text) = read_text(entry.path()) else { continue };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                if out.len() >= MAX_MATCHES {
                    truncated = true;
                    break 'files;
                }
                if files_only {
                    out.push(rel.clone());
                    continue 'files;
                }
                let line: String = line.trim_end().chars().take(300).collect();
                out.push(format!("{rel}:{}:{line}", i + 1));
            }
        }
    }
    let mut text = if out.is_empty() { "No matches.".to_string() } else { out.join("\n") };
    if truncated {
        text.push_str(&format!("\n[stopped at {MAX_MATCHES} matches; narrow the pattern or path]"));
    }
    Ok(Output::ok(text))
}

fn glob(input: &Value, cwd: &Path) -> Result<Output> {
    let matcher = globset::Glob::new(arg(input, "pattern")?)?.compile_matcher();
    let root = resolve(cwd, input["path"].as_str().unwrap_or("."));
    let mut hits: Vec<(std::time::SystemTime, String)> = walker(&root)
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter(|e| matcher.is_match(e.path().strip_prefix(&root).unwrap_or(e.path())))
        .map(|e| {
            let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            (mtime, display(cwd, e.path()))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    let total = hits.len();
    let mut text: String = hits.into_iter().take(MAX_MATCHES).map(|(_, p)| p).collect::<Vec<_>>().join("\n");
    if total == 0 {
        text = "No files matched.".into();
    } else if total > MAX_MATCHES {
        text.push_str(&format!("\n[{total} files; showing the {MAX_MATCHES} most recent]"));
    }
    Ok(Output::ok(text))
}

fn list_dir(input: &Value, cwd: &Path) -> Result<Output> {
    let path = resolve(cwd, input["path"].as_str().unwrap_or("."));
    let mut entries: Vec<String> = std::fs::read_dir(&path)
        .map_err(|e| anyhow!("{}: {e}", path.display()))?
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
        })
        .collect();
    entries.sort();
    let total = entries.len();
    entries.truncate(500);
    let mut text = entries.join("\n");
    if total > 500 {
        text.push_str(&format!("\n[{total} entries; showing 500]"));
    }
    Ok(Output::ok(if text.is_empty() { "(empty directory)".into() } else { text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_lines_applies_against_snapshot_coordinates() {
        let dir = std::env::temp_dir().join(format!("sci-pi-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let t = tag(&std::fs::read_to_string(&f).unwrap());
        let input = json!({ "path": "a.txt", "tag": t, "edits": [
            { "start": 2, "end": 2, "lines": ["TWO", "TWO-B"] },
            { "after": 0, "lines": ["zero"] },
            { "start": 4, "end": 5, "lines": [] },
        ]});
        let out = edit_lines(&input, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "zero\none\nTWO\nTWO-B\nthree\n");
        assert!(out.text.contains("3:TWO"), "{}", out.text);
        // The old tag is now stale.
        assert!(edit_lines(&input, &dir).unwrap_err().to_string().contains("stale tag"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clip_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let c = clip(&s, 60);
        assert!(c.starts_with("aaaa") && c.ends_with("bbbb") && c.contains("omitted"));
    }
}
