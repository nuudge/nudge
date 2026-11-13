use std::path::Path;
use std::process::Command;

// Outermost file first, so the file nearest cwd lands last and wins on conflict.
pub fn agents_md_block(cwd: &Path) -> Option<String> {
    let mut sections: Vec<String> = cwd
        .ancestors()
        .filter_map(|dir| {
            let path = dir.join("AGENTS.md");
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
    // Only rarely-changing repo identity here; mutable git state goes in the volatile block.
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
    // read_dir order is filesystem-dependent; sorting keeps the cached bytes deterministic.
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

// Working-tree status is excluded: it changes almost every turn and would bust
// the history cache that sits after the env block.
pub struct GitInfo {
    pub branch: String,
    pub head: String,
    pub upstream: Option<String>,
    pub recent_commits: Vec<String>,
}

pub fn collect_git_info(cwd: &Path) -> Option<GitInfo> {
    if git(cwd, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
        return None;
    }
    let head = git(cwd, &["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // symbolic-ref works on an unborn HEAD but fails when detached.
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
