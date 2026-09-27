use anyhow::Result;

use crate::coding;

// `--list`: print the current project's saved sessions, most-recent-first, so the
// user can pick one to `--resume` by name instead of squinting at uuids. Nameless
// sessions still appear (their id is the only handle). Read-only; no API key needed.
pub fn print_sessions() -> Result<()> {
    let sessions = coding::list_sessions()?;
    if sessions.is_empty() {
        println!("No saved sessions for this directory.");
        return Ok(());
    }
    println!(
        "{:<28}  {:<36}  {:<16}  {:>8}  LAST USED",
        "NAME", "ID", "BRANCH", "TURNS"
    );
    for s in &sessions {
        let when = s
            .last_activity
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M");
        println!(
            "{:<28}  {:<36}  {:<16}  {:>8}  {}",
            s.name.as_deref().unwrap_or("(unnamed)"),
            s.id,
            s.branch.as_deref().unwrap_or("-"),
            s.turns,
            when,
        );
    }
    println!("\nResume with: nudge --resume <name-or-id>");
    Ok(())
}
