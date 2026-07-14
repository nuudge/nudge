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
        "NAME", "ID", "BRANCH", "SIZE"
    );
    for s in &sessions {
        let when = chrono::DateTime::<chrono::Local>::from(s.modified).format("%Y-%m-%d %H:%M");
        println!(
            "{:<28}  {:<36}  {:<16}  {:>8}  {}",
            s.name.as_deref().unwrap_or("(unnamed)"),
            s.id,
            s.branch.as_deref().unwrap_or("-"),
            human_size(s.size),
            when,
        );
    }
    println!("\nResume with: nudge --resume <name-or-id>");
    Ok(())
}

// Format a byte count as a short human-readable string (e.g. `1.2K`, `3.4M`) for the
// `--list` SIZE column. Bytes under 1K stay as a plain count so tiny transcripts read
// exactly rather than rounding to `0.0K`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}
