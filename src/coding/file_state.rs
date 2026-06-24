use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// Session-scoped Read-before-Edit tracker. Records the mtime of each file at
// the moment Read returned successfully. Edit consults this to enforce two
// invariants:
//
//   1. The model has Read the file in this session (otherwise it's editing on
//      assumptions, which silently corrupts files).
//   2. The file hasn't been modified externally between that Read and the
//      pending Edit (otherwise the `old_string` may match a different line
//      than the model intended, or the model's mental model of the file is
//      stale).
//
// Paths are canonicalized before insertion/lookup so `./foo`, `/abs/foo`, and
// symlink-equivalent paths all resolve to the same key. mtime equality is the
// staleness check — cheap, and sufficient for the local-dev case (the file
// would have to be rewritten at sub-second precision *and* land on the exact
// same mtime to slip through, which doesn't happen in practice).
//
// In-memory only. Not persisted to the JSONL session log — on `--resume`, the
// tracker starts empty and the model must Read again before editing. This is
// the safe default: a resumed session is a fresh process with no guarantee
// that the files match the pre-resume state.
#[derive(Default)]
pub struct FileState {
    read_mtimes: HashMap<PathBuf, SystemTime>,
}

impl FileState {
    pub fn record_read(&mut self, path: &Path) -> Result<()> {
        let canon = canonicalize(path)?;
        let mtime = mtime_of(&canon)?;
        self.read_mtimes.insert(canon, mtime);
        Ok(())
    }

    // Verify the Read-before-Edit invariant for `path`. Returns Ok(()) if the
    // edit may proceed. Call `record_write` after the edit lands to refresh
    // the stored mtime so chained edits in the same turn don't false-reject.
    pub fn check_edit(&self, path: &Path) -> Result<()> {
        let canon = canonicalize(path)?;
        let Some(stored) = self.read_mtimes.get(&canon) else {
            bail!(
                "must Read {} in this session before editing it; Read it first to confirm current contents",
                canon.display()
            );
        };
        let current = mtime_of(&canon)?;
        if &current != stored {
            bail!(
                "{} has been modified externally since the last Read; Read it again before editing",
                canon.display()
            );
        }
        Ok(())
    }

    pub fn record_write(&mut self, path: &Path) -> Result<()> {
        let canon = canonicalize(path)?;
        let mtime = mtime_of(&canon)?;
        self.read_mtimes.insert(canon, mtime);
        Ok(())
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn mtime_of(path: &Path) -> Result<SystemTime> {
    std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .modified()
        .with_context(|| format!("filesystem does not report mtime for {}", path.display()))
}
