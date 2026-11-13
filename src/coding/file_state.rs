use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// Edit requires a prior Read this session and an unchanged mtime since, so
// `old_string` can't match against a stale view. Not persisted: a resumed
// session must Read again, since files may have changed in between.
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

    // Callers must `record_write` after the edit so chained edits don't false-reject.
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
