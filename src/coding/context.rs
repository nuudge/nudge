use std::path::Path;
use std::process::Command;

// Project instructions from CLAUDE.md files in cwd and its ancestors.
// Outermost file first, so the file nearest cwd lands last and wins on
// conflict — instructions later in the prompt take precedence, and the most
// specific file should be the one that does. Returns None when no non-empty
// CLAUDE.md exists, so the request omits the block instead of sending an
// empty system entry. Framing mirrors Claude Code's claudeMd context shape.
pub fn claude_md_block(cwd: &Path) -> Option<String> {
    let mut sections: Vec<String> = cwd
        .ancestors()
        .filter_map(|dir| {
            let path = dir.join("CLAUDE.md");
            let content = std::fs::read_to_string(&path).ok()?;
            if content.trim().is_empty() {
                return None;
            }
            Some(format!(
                "Contents of {} (project instructions, checked into the codebase):\n\n{}",
                path.display(),
                content.trim_end()
            ))
        })
        .collect();
    if sections.is_empty() {
        return None;
    }
    sections.reverse();
    Some(format!(
        "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written. When instructions conflict, the file nearest the working directory wins.\n\n{}",
        sections.join("\n\n")
    ))
}

pub fn stable_env_block(cwd: &Path) -> String {
    let platform = std::env::consts::OS;
    let mut out = format!(
        "<environment-stable>\nWorking directory: {}\nPlatform: {platform}",
        cwd.display()
    );
    // Repo identity only — branch/upstream rarely change mid-session, so they
    // belong in the cached prefix. Mutable state (HEAD, status, commits) lives
    // in the volatile block so it stays fresh as the agent works.
    match collect_git_info(cwd) {
        Some(g) => {
            out.push_str(&format!("\nGit repo: yes\nBranch: {}", g.branch));
            if let Some(up) = &g.upstream {
                out.push_str(&format!("\nUpstream: {up}"));
            }
        }
        None => out.push_str("\nGit repo: no"),
    }
    out.push_str("\n</environment-stable>");
    out
}

pub fn volatile_env_block(cwd: &Path) -> String {
    let listing = list_top_level(cwd);
    let mut out = format!("<environment-volatile>\nTop-level entries:\n{listing}");
    if let Some(g) = collect_git_info(cwd) {
        out.push_str(&format!("\nGit HEAD: {}", g.head));
        if !g.recent_commits.is_empty() {
            out.push_str("\nRecent commits:");
            for commit in &g.recent_commits {
                out.push_str(&format!("\n  {commit}"));
            }
        }
    }
    out.push_str("\n</environment-volatile>");
    out
}

fn list_top_level(cwd: &Path) -> String {
    const MAX_ENTRIES: usize = 50;
    let read_dir = match std::fs::read_dir(cwd) {
        Ok(rd) => rd,
        Err(_) => return "  (could not read directory)".into(),
    };
    let mut entries: Vec<(String, bool)> = read_dir
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (name, is_dir)
        })
        .collect();
    // Sort is load-bearing for cache stability — read_dir order is filesystem-
    // dependent, so we sort to keep the output bytes deterministic across runs.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let total = entries.len();
    let mut out: Vec<String> = entries
        .iter()
        .take(MAX_ENTRIES)
        .map(|(name, is_dir)| {
            let suffix = if *is_dir { "/" } else { "" };
            format!("  {name}{suffix}")
        })
        .collect();
    if total > MAX_ENTRIES {
        out.push(format!("  ... and {} more", total - MAX_ENTRIES));
    }
    out.join("\n")
}

// Snapshot of the repository state, gathered by shelling out to `git`. Returns
// None when cwd isn't inside a work tree (or git is absent). Consumers pick the
// fields they need — the TUI shows only `branch`. Working-tree status is
// deliberately excluded: it changes almost every turn the agent edits, and the
// env block sits ahead of the chat history in the cache prefix, so including it
// would bust the history cache each turn. Read it on demand via `git status`.
pub struct GitInfo {
    pub branch: String,
    pub head: String,
    pub upstream: Option<String>,
    pub recent_commits: Vec<String>,
}

pub fn collect_git_info(cwd: &Path) -> Option<GitInfo> {
    // Authoritative repo probe; also short-circuits when git isn't installed.
    if git(cwd, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
        return None;
    }
    let head = git(cwd, &["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // symbolic-ref resolves the branch even on an unborn HEAD (fresh repo, no
    // commits); it fails on a detached HEAD, which we render via the sha.
    let branch = match git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Some(b) if !b.trim().is_empty() => b.trim().to_string(),
        _ => format!("(detached at {head})"),
    };
    let upstream = git(
        cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let recent_commits = git(cwd, &["log", "-5", "--oneline", "--no-decorate"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    Some(GitInfo {
        branch,
        head,
        upstream,
        recent_commits,
    })
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
