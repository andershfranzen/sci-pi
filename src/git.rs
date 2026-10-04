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

/// Creates `<data>/worktrees/<repo>-<id8>` on a fresh `sci-pi/<id8>` branch off HEAD.
/// If `project` is a subdirectory of the repo, the returned path points at the same subdirectory.
pub async fn create_worktree(project: &Path, worktrees_dir: &Path, session_id: &str) -> Result<Worktree> {
    let Some(top) = toplevel(project).await else { bail!("{} is not a git repository", project.display()) };
    let short = &session_id[..8];
    let repo_name = top.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let wt = worktrees_dir.join(format!("{repo_name}-{short}"));
    let branch = format!("sci-pi/{short}");
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
    let (diff, names) = with_temp_index(&top, async |env| {
        git_env(&top, &["add", "-A"], env).await?;
        let diff = git_env(&top, &["diff", "--cached", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", &base], env).await?;
        let names = git_env(&top, &["diff", "--cached", "--name-status", &base], env).await?;
        Ok((diff, names))
    })
    .await?;
    Ok(json!({ "diff": diff, "files": name_status(&names) }))
}

/// Runs `f` with `GIT_INDEX_FILE` pointing at a throwaway copy of the repo's index.
async fn with_temp_index<T>(top: &Path, f: impl AsyncFnOnce(&[(&str, &Path)]) -> Result<T>) -> Result<T> {
    let real = PathBuf::from(git(top, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).await?.trim());
    let tmp = std::env::temp_dir().join(format!("sci-pi-index-{}", uuid::Uuid::new_v4().simple()));
    if real.exists() {
        std::fs::copy(&real, &tmp)?;
    }
    let result = f(&[("GIT_INDEX_FILE", tmp.as_path())]).await;
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Commits the full working-tree state (untracked included, ignored excluded) without touching
/// the index, HEAD or any branch, and pins it at `ref_name` if given. None outside git repos.
pub async fn snapshot(cwd: &Path, ref_name: Option<&str>) -> Result<Option<String>> {
    let Some(top) = toplevel(cwd).await else { return Ok(None) };
    let head = git(&top, &["rev-parse", "HEAD"]).await.ok().map(|h| h.trim().to_string());
    let commit = with_temp_index(&top, async |env| {
        git_env(&top, &["add", "-A"], env).await?;
        let tree = git_env(&top, &["write-tree"], env).await?;
        let mut args = vec!["-c", "user.name=sci-pi", "-c", "user.email=sci-pi@localhost", "commit-tree", tree.trim(), "-m", "sci-pi checkpoint"];
        if let Some(h) = &head {
            args.extend(["-p", h]);
        }
        Ok(git(&top, &args).await?.trim().to_string())
    })
    .await?;
    if let Some(r) = ref_name {
        git(&top, &["update-ref", r, &commit]).await?;
    }
    Ok(Some(commit))
}

/// Puts the working tree back to a snapshot: rewrites every file in it and deletes files
/// created since. The real index and HEAD are left alone.
pub async fn restore(cwd: &Path, commit: &str) -> Result<()> {
    let Some(top) = toplevel(cwd).await else { bail!("not a git repository") };
    let current = snapshot(cwd, None).await?.unwrap_or_default();
    let added = git(&top, &["diff", "--name-only", "--no-renames", "--diff-filter=A", commit, &current]).await?;
    with_temp_index(&top, async |env| {
        git_env(&top, &["read-tree", commit], env).await?;
        git_env(&top, &["checkout-index", "-a", "-f"], env).await?;
        Ok(())
    })
    .await?;
    for path in added.lines().filter(|l| !l.is_empty()) {
        let _ = std::fs::remove_file(top.join(path));
    }
    Ok(())
}

pub async fn diff_between(cwd: &Path, from: &str, to: &str) -> Result<Value> {
    let Some(top) = toplevel(cwd).await else { bail!("not a git repository") };
    let diff = git(&top, &["diff", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", from, to]).await?;
    let names = git(&top, &["diff", "--name-status", from, to]).await?;
    Ok(json!({ "diff": diff, "files": name_status(&names) }))
}

fn name_status(names: &str) -> Vec<Value> {
    names
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let status = parts.next()?;
            let path = parts.last()?;
            Some(json!({ "path": path, "status": status }))
        })
        .collect()
}

pub async fn delete_refs(cwd: &Path, prefix: &str) {
    let Some(top) = toplevel(cwd).await else { return };
    if let Ok(refs) = git(&top, &["for-each-ref", "--format=%(refname)", prefix]).await {
        for r in refs.lines() {
            let _ = git(&top, &["update-ref", "-d", r]).await;
        }
    }
}

/// Tracked + untracked (non-ignored) files, relative to `cwd`.
pub async fn list_files(cwd: &Path) -> Result<Vec<String>> {
    let out = git(cwd, &["ls-files", "-co", "--exclude-standard"]).await?;
    Ok(out.lines().map(str::to_string).collect())
}

pub async fn status(cwd: &Path, base: Option<&str>) -> Result<Value> {
    let Some(top) = toplevel(cwd).await else { return Ok(json!({ "git": false })) };
    let branch = git(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).await.unwrap_or_default().trim().to_string();
    let dirty = git(&top, &["status", "--porcelain"]).await.unwrap_or_default().lines().count();
    let remote = git(&top, &["remote", "get-url", "origin"]).await.ok().map(|r| r.trim().to_string());
    let (behind, ahead, upstream) = match git(&top, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]).await {
        Ok(out) => {
            let mut it = out.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
            (it.next(), it.next(), true)
        }
        Err(_) => (None, None, false),
    };
    let commits = match base {
        Some(b) => git(&top, &["rev-list", "--count", &format!("{b}..HEAD")]).await.ok().and_then(|n| n.trim().parse::<u64>().ok()),
        None => None,
    };
    Ok(json!({
        "git": true, "branch": branch, "dirty": dirty, "remote": remote, "upstream": upstream,
        "ahead": ahead, "behind": behind, "commits_since_base": commits,
    }))
}

pub async fn commit_all(cwd: &Path, message: &str) -> Result<String> {
    let Some(top) = toplevel(cwd).await else { bail!("not a git repository") };
    git(&top, &["add", "-A"]).await?;
    git(&top, &["commit", "-m", message]).await?;
    Ok(git(&top, &["rev-parse", "HEAD"]).await?.trim().to_string())
}

pub async fn push(cwd: &Path) -> Result<String> {
    let Some(top) = toplevel(cwd).await else { bail!("not a git repository") };
    let out = Command::new("git").arg("-C").arg(&top).args(["push", "-u", "origin", "HEAD"]).output().await?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("git push: {}", text.trim());
    }
    Ok(text.trim().to_string())
}

/// `gh pr create` for the current branch; returns the PR URL.
pub async fn create_pr(cwd: &Path, title: &str, body: &str, draft: bool) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(cwd).args(["pr", "create", "--title", title, "--body", body]);
    if draft {
        cmd.arg("--draft");
    }
    let out = cmd.output().await.map_err(|e| anyhow::anyhow!("gh: {e} (is the GitHub CLI installed on this host?)"))?;
    if !out.status.success() {
        bail!("gh pr create: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout.lines().rev().find(|l| l.starts_with("http")).unwrap_or(stdout.trim()).to_string())
}
