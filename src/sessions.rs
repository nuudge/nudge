use anyhow::Result;

use crate::coding;

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
            s.state.as_str(),
            s.turns,
            when,
        );
    }
    println!("\nResume with: nudge --resume <name-or-id>");
    Ok(())
}
