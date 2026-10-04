//! The native agent's system prompt. Kept stable within a session so the prompt cache holds.

use std::path::Path;

const PROJECT_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
const MAX_INSTRUCTIONS: usize = 20_000;

pub fn system(cwd: &Path) -> String {
    let host = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "this host".into());
    let date = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut prompt = format!(
        "You are a coding agent running on {host} ({os}) in `{cwd}`. This is a headless machine the user \
reaches remotely, and they are often away from the screen: work independently and carry the task through \
to the end, asking only when a decision is genuinely theirs to make.

# How to work
- Look before you change: use grep, glob and read_file to understand the code, and follow its conventions.
- Edit with edit_lines, passing the tag from your most recent read_file or edit of that file. Edit results \
include the new tag and the renumbered lines around each change, so you can keep editing without re-reading. \
Use edit_file for a small unique replacement and write_file for new files.
- Independent read-only calls (read_file, grep, glob, list_dir) issued together run in parallel; batch them.
- After changing code, build or run the relevant tests when that's feasible, and fix what you broke.
- Keep a todo_write list for work with several steps.
- Don't commit, push, or modify files outside the working directory unless asked.

# Finishing
End with a brief summary of what you changed and anything left open. Be concise; the user often reads this on a phone.
",
        os = std::env::consts::OS,
        cwd = cwd.display(),
    );
    let instructions = project_instructions(cwd);
    if !instructions.is_empty() {
        prompt.push_str("\n# Project and user instructions\n");
        prompt.push_str(&instructions);
    }
    if !date.is_empty() {
        prompt.push_str(&format!("\nToday's date: {date}\n"));
    }
    prompt
}

/// AGENTS.md / CLAUDE.md from the working directory up to the filesystem root, outermost first,
/// plus the user's own `~/.config/outpost/AGENTS.md`.
fn project_instructions(cwd: &Path) -> String {
    let mut found = vec![];
    for dir in cwd.ancestors() {
        if let Some(file) = PROJECT_FILES.iter().map(|f| dir.join(f)).find(|p| p.is_file()) {
            found.push(file);
        }
    }
    found.reverse();
    let user = crate::config::config_dir().join("AGENTS.md");
    if user.is_file() {
        found.insert(0, user);
    }
    let mut out = String::new();
    for path in found {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let text: String = text.chars().take(MAX_INSTRUCTIONS).collect();
        out.push_str(&format!("\n<instructions source=\"{}\">\n{}\n</instructions>\n", path.display(), text.trim()));
    }
    out
}
