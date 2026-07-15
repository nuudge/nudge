//! Skills (Phase 9) — progressive-disclosure capability packaging.
//!
//! A Skill is a folder: `SKILL.md` (YAML frontmatter + markdown body) plus
//! optional bundled reference files and scripts. Skills let the agent load
//! packaged expertise **on demand** instead of carrying it in the prompt:
//!
//! - **Level 1 (metadata)** — each skill's `name` + `description` are baked into
//!   the [`use_skill`](USE_SKILL_NAME) meta-tool's description (the `load_tool`
//!   pattern), so they sit in the cached tool prefix for ~free.
//! - **Level 2 (body)** — when the model calls `use_skill{skill}`, the SKILL.md
//!   body is returned as the tool_result, entering conversation history (not the
//!   system prompt, which would bust the system cache prefix).
//! - **Level 3 (bundled files/scripts)** — the body points at sibling files; the
//!   model reads them with `Read` and runs scripts with `Bash`. No new machinery.
//!
//! Unlike [`mcp`](super::mcp) there is no connection, subprocess, or lifecycle —
//! a skill's capability is just text, so the registry is immutable after
//! startup discovery and "loading" is just reading a file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// Name of the foundational meta-tool the model calls to load a skill's body.
pub const USE_SKILL_NAME: &str = "use_skill";

/// Directory under each root that holds skills, one subfolder per skill.
const SKILLS_SUBDIR: &str = ".nudge/skills";

struct Skill {
    name: String,
    description: String,
    /// The skill's folder — given to the model so it can resolve bundled-file
    /// paths (Level 3) against an absolute base.
    dir: PathBuf,
    /// `<dir>/SKILL.md`, re-read on load so edits are picked up mid-session.
    body_path: PathBuf,
}

/// Required SKILL.md frontmatter. Extra fields (e.g. `allowed-tools`) are
/// ignored, matching Anthropic's skill format.
#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

pub struct SkillRegistry {
    /// skill name -> skill. `BTreeMap` keeps the catalog in sorted order so the
    /// `use_skill` description is byte-stable across the session (a cache hit).
    skills: BTreeMap<String, Skill>,
    /// Human-readable discovery outcomes, printed by the caller before the TUI
    /// takes the screen — so a malformed SKILL.md fails loudly, not silently.
    pub discovery_log: Vec<String>,
}

impl SkillRegistry {
    /// Discover skills under the personal (`~/.nudge/skills/`) and project
    /// (`<cwd>/.nudge/skills/`) roots. A missing root is not an error. On a
    /// name collision, project wins over personal (closer to the work, matching
    /// AGENTS.md precedence). Invalid skills are skipped and logged.
    pub fn discover(cwd: &Path, home: Option<&Path>) -> Self {
        let mut reg = Self {
            skills: BTreeMap::new(),
            discovery_log: Vec::new(),
        };
        // Personal first, then project — project's `insert` overwrites, so it wins.
        if let Some(home) = home {
            reg.scan_root(&home.join(SKILLS_SUBDIR), "personal");
        }
        reg.scan_root(&cwd.join(SKILLS_SUBDIR), "project");
        reg
    }

    fn scan_root(&mut self, root: &Path, origin: &str) {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return, // root absent — no skills of this origin
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let body_path = dir.join("SKILL.md");
            let text = match std::fs::read_to_string(&body_path) {
                Ok(t) => t,
                Err(_) => continue, // a subdir without SKILL.md just isn't a skill
            };
            match parse_skill(&dir, &body_path, &text) {
                Ok(skill) => {
                    self.discovery_log
                        .push(format!("[skills] discovered: {} ({origin})", skill.name));
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => self
                    .discovery_log
                    .push(format!("[skills] skipped {}: {e:#}", body_path.display())),
            }
        }
    }

    /// Schema for the `use_skill` meta-tool. The catalog (name + description) is
    /// baked into the description so the model knows what's available without a
    /// discovery round-trip — the same shape as `McpRegistry::load_tool_schema`.
    /// Byte-stable across the session (discovery is startup-only), so it stays a
    /// foundational, cached tool. `None` when no skills were found.
    pub fn use_skill_schema(&self) -> Option<Value> {
        if self.skills.is_empty() {
            return None;
        }
        let mut listing = String::new();
        for (name, skill) in &self.skills {
            listing.push_str(&format!("\n- {name}: {}", skill.description));
        }
        Some(json!({
            "name": USE_SKILL_NAME,
            "description": format!(
                "Load a Skill — packaged expertise (instructions, and optionally \
                 reference files and scripts) for a specific kind of task. Call \
                 this when the task matches one of the skills below: its \
                 instructions are returned to you, and you then follow them, \
                 reading any referenced files with Read and running any \
                 referenced scripts with Bash. Loading is cheap and keeps a \
                 skill's full instructions out of context until needed. \
                 Available skills:{listing}"
            ),
            "input_schema": {
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Name of a skill listed in this tool's description."
                    }
                },
                "required": ["skill"]
            }
        }))
    }

    /// Level-2 load: read the named skill's SKILL.md body (frontmatter stripped)
    /// and return it for the tool_result, prefixed with the skill's absolute
    /// directory so the model can resolve bundled-file paths.
    pub fn load_body(&self, name: &str) -> Result<String> {
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| anyhow!("no skill named '{name}'"))?;
        let text = std::fs::read_to_string(&skill.body_path)
            .with_context(|| format!("reading {}", skill.body_path.display()))?;
        let body = split_frontmatter(&text).map_or(text.as_str(), |(_, b)| b);
        Ok(format!(
            "Skill '{name}' loaded. Its files are in {}. Reference files and \
             scripts mentioned below are relative to that directory — read them \
             with Read and run them with Bash, using absolute paths.\n\n{}",
            skill.dir.display(),
            body.trim()
        ))
    }
}

fn parse_skill(dir: &Path, body_path: &Path, text: &str) -> Result<Skill> {
    let (frontmatter, _) =
        split_frontmatter(text).context("missing YAML frontmatter (expected a `---` block)")?;
    let fm: Frontmatter =
        serde_yaml_ng::from_str(frontmatter).context("parsing SKILL.md frontmatter")?;
    validate_name(&fm.name)?;
    validate_description(&fm.description)?;
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        dir: dir.to_path_buf(),
        body_path: body_path.to_path_buf(),
    })
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("name must be 1–64 characters (got {})", name.len());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("name '{name}' must be lowercase letters, digits, and hyphens only");
    }
    if name == "anthropic" || name == "claude" {
        bail!("name '{name}' is reserved");
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<()> {
    if description.trim().is_empty() {
        bail!("description must be non-empty");
    }
    if description.len() > 1024 {
        bail!(
            "description must be ≤1024 characters (got {})",
            description.len()
        );
    }
    Ok(())
}

/// Split a `---`-fenced YAML frontmatter header from the markdown body. Returns
/// `(frontmatter, body)` or `None` when the text doesn't open with a `---` line
/// closed by another `---` line. Both returned slices borrow from `text`, so the
/// original line endings are preserved.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    // Find a line that is exactly `---`, scanning newline-anchored so a `---`
    // mid-line (e.g. a horizontal rule with trailing text) doesn't false-match.
    let mut search_from = 0;
    while let Some(rel) = rest[search_from..].find("\n---") {
        let close = search_from + rel + 1; // index of the `---`
        let after = &rest[close + 3..];
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            let frontmatter = &rest[..close];
            let body = after
                .strip_prefix('\n')
                .or_else(|| after.strip_prefix("\r\n"))
                .unwrap_or(after);
            return Some((frontmatter, body));
        }
        search_from = close + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dirname: &str, content: &str) {
        let dir = root.join(SKILLS_SUBDIR).join(dirname);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    const VALID: &str = "---\nname: commit-msg\ndescription: Writes conventional commit messages from a staged diff.\n---\n# Commit message skill\n\nRun `git diff --staged` then summarize.\n";

    #[test]
    fn discovers_valid_project_skill() {
        let cwd = temp_root();
        write_skill(&cwd, "commit-msg", VALID);
        let reg = SkillRegistry::discover(&cwd, None);
        let skill = reg.skills.get("commit-msg").expect("skill discovered");
        assert_eq!(
            skill.description,
            "Writes conventional commit messages from a staged diff."
        );
    }

    #[test]
    fn skips_skill_without_frontmatter() {
        let cwd = temp_root();
        write_skill(&cwd, "bad", "# no frontmatter here\njust a body\n");
        let reg = SkillRegistry::discover(&cwd, None);
        assert!(reg.skills.is_empty());
        assert!(reg.discovery_log.iter().any(|l| l.contains("skipped")));
    }

    #[test]
    fn skips_skill_with_oversized_name() {
        let cwd = temp_root();
        let name = "a".repeat(65);
        write_skill(
            &cwd,
            "big",
            &format!("---\nname: {name}\ndescription: ok\n---\nbody\n"),
        );
        let reg = SkillRegistry::discover(&cwd, None);
        assert!(reg.skills.is_empty());
    }

    #[test]
    fn skips_skill_with_invalid_name_charset() {
        let cwd = temp_root();
        write_skill(
            &cwd,
            "shouty",
            "---\nname: Commit_Msg\ndescription: ok\n---\nbody\n",
        );
        let reg = SkillRegistry::discover(&cwd, None);
        assert!(reg.skills.is_empty());
    }

    #[test]
    fn skips_skill_with_oversized_description() {
        let cwd = temp_root();
        let desc = "x".repeat(1025);
        write_skill(
            &cwd,
            "verbose",
            &format!("---\nname: verbose\ndescription: {desc}\n---\nbody\n"),
        );
        let reg = SkillRegistry::discover(&cwd, None);
        assert!(reg.skills.is_empty());
    }

    #[test]
    fn project_skill_wins_over_personal_on_name_collision() {
        let home = temp_root();
        let cwd = temp_root();
        write_skill(
            &home,
            "shared",
            "---\nname: shared\ndescription: personal version\n---\nbody\n",
        );
        write_skill(
            &cwd,
            "shared",
            "---\nname: shared\ndescription: project version\n---\nbody\n",
        );
        let reg = SkillRegistry::discover(&cwd, Some(&home));
        assert_eq!(
            reg.skills.get("shared").unwrap().description,
            "project version"
        );
    }

    #[test]
    fn load_body_strips_frontmatter_and_prepends_dir() {
        let cwd = temp_root();
        write_skill(&cwd, "commit-msg", VALID);
        let reg = SkillRegistry::discover(&cwd, None);
        let body = reg.load_body("commit-msg").unwrap();
        assert!(!body.contains("name: commit-msg"));
        assert!(body.contains("# Commit message skill"));
        let dir = cwd.join(SKILLS_SUBDIR).join("commit-msg");
        assert!(body.contains(&dir.display().to_string()));
    }

    #[test]
    fn load_body_errors_on_unknown_skill() {
        let reg = SkillRegistry::discover(&temp_root(), None);
        assert!(reg.load_body("nope").is_err());
    }

    #[test]
    fn use_skill_schema_is_none_when_empty() {
        let reg = SkillRegistry::discover(&temp_root(), None);
        assert!(reg.use_skill_schema().is_none());
    }

    #[test]
    fn use_skill_schema_lists_the_catalog() {
        let cwd = temp_root();
        write_skill(&cwd, "commit-msg", VALID);
        let reg = SkillRegistry::discover(&cwd, None);
        let schema = reg.use_skill_schema().unwrap();
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("commit-msg"));
        assert!(desc.contains("Writes conventional commit messages"));
    }

    #[test]
    fn missing_root_is_not_an_error() {
        let reg = SkillRegistry::discover(Path::new("/nonexistent/path/xyz"), None);
        assert!(reg.skills.is_empty());
    }

    #[test]
    fn split_frontmatter_ignores_body_horizontal_rule() {
        let text = "---\nname: x\n---\nbefore\n---\nafter\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "name: x\n");
        assert_eq!(body, "before\n---\nafter\n");
    }
}
