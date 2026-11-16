use anyhow::{Context, Result};
use std::time::{Duration, SystemTime};

use crate::coding;
use crate::logging;

pub fn prune_logs(older_than: Duration) -> Result<()> {
    let pruned = logging::prune(
        &coding::run_dir()?,
        older_than,
        SystemTime::now(),
        coding::is_listening,
    )?;
    println!(
        "Removed {} log file(s), freeing {:.1} MB.",
        pruned.files,
        pruned.bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

pub fn print_log(reference: &str) -> Result<()> {
    let row = coding::find_session(reference)?;
    let path = coding::session_log_path(&row.id)?;
    let run = logging::read_current_run(&path)
        .with_context(|| format!("reading the log of session {}", row.id))?;
    for line in run {
        println!("{line}");
    }
    Ok(())
}

pub fn print_sessions() -> Result<()> {
    let sessions = coding::list_sessions()?;
    if sessions.is_empty() {
        println!("No saved sessions for this directory.");
        return Ok(());
    }
    println!(
        "{:<28}  {:<36}  {:<16}  {:<7}  {:>8}  LAST USED",
        "NAME", "ID", "BRANCH", "STATE", "TURNS"
    );
    for s in &sessions {
        let when = s
            .last_activity
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M");
        println!(
            "{:<28}  {:<36}  {:<16}  {:<7}  {:>8}  {}",
            s.name.as_deref().unwrap_or("(unnamed)"),
            s.id,
            s.branch.as_deref().unwrap_or("-"),
            if s.live { "live" } else { "ended" },
            s.turns,
            when,
        );
    }
    println!("\nResume with: nudge --resume <name-or-id>");
    Ok(())
}
