//! Worktree-per-session and diffs, by shelling out to git.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::process::Command;

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[]).await
}

async fn git_env(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().await?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).await.ok().map(|s| PathBuf::from(s.trim()))
}

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

/// Creates `<data>/worktrees/<repo>-<id8>` on a fresh `outpost/<id8>` branch off HEAD.
/// If `project` is a subdirectory of the repo, the returned path points at the same subdirectory.
pub async fn create_worktree(project: &Path, worktrees_dir: &Path, session_id: &str) -> Result<Worktree> {
    let Some(top) = toplevel(project).await else { bail!("{} is not a git repository", project.display()) };
    let short = &session_id[..8];
    let repo_name = top.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let wt = worktrees_dir.join(format!("{repo_name}-{short}"));
    let branch = format!("outpost/{short}");
    let base = git(&top, &["rev-parse", "HEAD"]).await?.trim().to_string();
    std::fs::create_dir_all(worktrees_dir)?;
    git(&top, &["worktree", "add", "-b", &branch, &wt.to_string_lossy(), &base]).await?;
    let path = match project.strip_prefix(&top) {
        Ok(sub) if !sub.as_os_str().is_empty() => wt.join(sub),
        _ => wt,
    };
    Ok(Worktree { path, branch, base_commit: base })
}

pub async fn remove_worktree(project: &Path, cwd: &Path, branch: &str) -> Result<()> {
    let top = toplevel(cwd).await.unwrap_or_else(|| cwd.to_path_buf());
    let repo = toplevel(project).await.unwrap_or_else(|| project.to_path_buf());
    git(&repo, &["worktree", "remove", "--force", &top.to_string_lossy()]).await?;
    git(&repo, &["branch", "-D", branch]).await?;
    Ok(())
}

/// Everything that differs from `base` (staged, unstaged and untracked), computed against a
/// throwaway index so the user's real index is never touched.
pub async fn diff(cwd: &Path, base: Option<&str>) -> Result<Value> {
    let Some(top) = toplevel(cwd).await else {
        return Ok(json!({ "diff": "", "files": [], "error": "not a git repository" }));
    };
    let base = match base {
        Some(b) => b.to_string(),
        None => match git(&top, &["rev-parse", "HEAD"]).await {
            Ok(h) => h.trim().to_string(),
            Err(_) => return Ok(json!({ "diff": "", "files": [] })), // no commits yet
        },
    };
    let real_index = PathBuf::from(git(&top, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).await?.trim());
    let tmp_index = std::env::temp_dir().join(format!("outpost-index-{}", uuid::Uuid::new_v4().simple()));
    if real_index.exists() {
        std::fs::copy(&real_index, &tmp_index)?;
    }
    let env = [("GIT_INDEX_FILE", tmp_index.as_path())];
    let result = async {
        git_env(&top, &["add", "-A"], &env).await?;
        let diff = git_env(&top, &["diff", "--cached", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", &base], &env).await?;
        let names = git_env(&top, &["diff", "--cached", "--name-status", &base], &env).await?;
        anyhow::Ok((diff, names))
    }
    .await;
    let _ = std::fs::remove_file(&tmp_index);
    let (diff, names) = result?;
    let files: Vec<Value> = names
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let status = parts.next()?;
            let path = parts.last()?;
            Some(json!({ "path": path, "status": status }))
        })
        .collect();
    Ok(json!({ "diff": diff, "files": files }))
}
