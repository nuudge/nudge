use anyhow::{Context, Result};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};

pub const RUN_SEPARATOR: &str = "--- started ";
const DEFAULT_FILTER: &str = "nudge=info,warn";
const MB: u64 = 1024 * 1024;
// Trimming well below the cap leaves several MB of logging before the next trim.
const TRIM_ABOVE: u64 = 10 * MB;
const TRIM_TO: u64 = 5 * MB;
const WARN_TOTAL_ABOVE: u64 = 200 * MB;
pub const DEFAULT_PRUNE_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Clone, Copy)]
pub enum Mode {
    Tui,
    Headless,
}

#[derive(Debug, PartialEq)]
enum Output {
    File,
    FileAndStderr,
    // A host started by `start_host`: its parent opened the log and made it stderr.
    Stderr,
}

fn output(mode: Mode, stderr_is_log: bool) -> Output {
    match (mode, stderr_is_log) {
        (_, true) => Output::Stderr,
        (Mode::Tui, false) => Output::File,
        (Mode::Headless, false) => Output::FileAndStderr,
    }
}

// Returns the mode's name for the startup line.
pub fn init(log_path: &Path, mode: Mode) -> Result<&'static str> {
    let filter = match std::env::var("NUDGE_LOG") {
        Ok(spec) => {
            EnvFilter::try_new(&spec).with_context(|| format!("invalid NUDGE_LOG {spec:?}"))?
        }
        Err(_) => EnvFilter::new(DEFAULT_FILTER),
    };
    let out = output(mode, stderr_is(log_path));
    let name = match out {
        Output::File => "tui",
        Output::FileAndStderr => "daemon",
        Output::Stderr => "background",
    };
    let writer = match out {
        Output::Stderr => BoxMakeWriter::new(std::io::stderr),
        Output::File => {
            BoxMakeWriter::new(Mutex::new(open_run_log(log_path, chrono::Local::now())?))
        }
        Output::FileAndStderr => BoxMakeWriter::new(
            Mutex::new(open_run_log(log_path, chrono::Local::now())?).and(std::io::stderr),
        ),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("installing the logger: {e}"))?;

    // Verbatim: a str-slicing panic quotes the string it sliced, which may be a message
    // body. Accepted, since the payload is usually the only clue to the crash.
    let previous = std::panic::take_hook();
    let chain = out != Output::Stderr;
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        // The default hook writes to stderr, which in the background is the log itself.
        if chain {
            previous(info);
        }
    }));
    Ok(name)
}

fn stderr_is(path: &Path) -> bool {
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .is_ok_and(|fd| same_file(&File::from(fd), path))
}

fn same_file(file: &File, path: &Path) -> bool {
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

// Appended, so a revived session's log still shows how its previous run ended.
pub fn open_run_log(path: &Path, now: chrono::DateTime<chrono::Local>) -> Result<File> {
    trim(path, TRIM_ABOVE, TRIM_TO)?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(
        log,
        "{RUN_SEPARATOR}{} ---",
        now.to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(log)
}

// In place, so a background host's inherited stderr keeps pointing at the same inode.
fn trim(path: &Path, above: u64, to: u64) -> Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file.metadata()?.len();
    if len <= above {
        return Ok(());
    }
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(len - to))?;
    file.read_to_end(&mut tail)?;
    let start = tail
        .iter()
        .position(|&b| b == b'\n')
        .map_or(tail.len(), |i| i + 1);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&tail[start..])?;
    file.set_len((tail.len() - start) as u64)
        .with_context(|| format!("trimming {}", path.display()))
}

// Only the current run: earlier runs' lines would misreport why this one failed.
pub fn read_current_run(path: &Path) -> std::io::Result<Vec<String>> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| l.starts_with(RUN_SEPARATOR))
        .map_or(0, |i| i + 1);
    Ok(lines[start..].iter().map(|l| l.to_string()).collect())
}

pub fn log_tail(path: &Path, n: usize) -> String {
    let run = read_current_run(path).unwrap_or_default();
    run[run.len().saturating_sub(n)..].join("\n")
}

fn logs(dir: &Path) -> Result<Vec<(PathBuf, Metadata)>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    Ok(entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?)))
        .collect())
}

pub fn size_warning(dir: &Path) -> Result<Option<String>> {
    let total: u64 = logs(dir)?.iter().map(|(_, m)| m.len()).sum();
    Ok((total > WARN_TOTAL_ABOVE).then(|| {
        format!(
            "nudge logs use {} MB; free space with nudge --prune-logs",
            total / MB
        )
    }))
}

pub struct Pruned {
    pub files: usize,
    pub bytes: u64,
}

// `live` probes a session's socket, so a running session's log is never deleted.
pub fn prune(
    dir: &Path,
    older_than: Duration,
    now: SystemTime,
    live: impl Fn(&Path) -> bool,
) -> Result<Pruned> {
    let mut pruned = Pruned { files: 0, bytes: 0 };
    for (path, meta) in logs(dir)? {
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if age >= older_than && !live(&path.with_extension("sock")) {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            pruned.files += 1;
            pruned.bytes += meta.len();
        }
    }
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-log-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn the_output_follows_the_mode_unless_stderr_is_the_log() {
        assert_eq!(output(Mode::Tui, false), Output::File);
        assert_eq!(output(Mode::Headless, false), Output::FileAndStderr);
        assert_eq!(output(Mode::Headless, true), Output::Stderr);
    }

    #[test]
    fn stderr_is_the_log_only_when_it_is_the_same_file() {
        let dir = temp_dir();
        let log = dir.join("s.log");
        let inherited = open_run_log(&log, chrono::Local::now()).unwrap();
        assert!(same_file(&inherited, &log));
        let other = File::create(dir.join("other.log")).unwrap();
        assert!(!same_file(&other, &log));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trim_keeps_a_tail_cut_at_a_line_on_the_same_inode() {
        let dir = temp_dir();
        let path = dir.join("s.log");
        let lines: String = (0..100).map(|i| format!("line {i:03}\n")).collect();
        std::fs::write(&path, &lines).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();

        trim(&path, 2000, 1000).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            lines,
            "under the cap"
        );

        trim(&path, 500, 100).unwrap();
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.len() <= 100 && !kept.is_empty(), "{kept:?}");
        assert!(lines.ends_with(&kept), "{kept:?}");
        assert!(
            kept.starts_with("line "),
            "cut at a line boundary: {kept:?}"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_restart_appends_to_the_log_and_the_tail_is_the_current_run() {
        let dir = temp_dir();
        let path = dir.join("s.log");
        let mut first = open_run_log(&path, chrono::Local::now()).unwrap();
        writeln!(first, "stopping").unwrap();
        let mut second = open_run_log(&path, chrono::Local::now()).unwrap();
        writeln!(second, "error: bad config").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("stopping"), "the earlier run is kept: {text}");
        assert_eq!(text.matches(RUN_SEPARATOR).count(), 2, "{text}");
        assert_eq!(log_tail(&path, 10), "error: bad config");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_respects_the_cutoff_and_skips_live_sessions() {
        let dir = temp_dir();
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        for (name, modified) in [
            ("old.log", old),
            ("live.log", old),
            ("new.log", now),
            ("old.sock", old),
        ] {
            let mut file = File::create(dir.join(name)).unwrap();
            file.write_all(b"x").unwrap();
            file.set_modified(modified).unwrap();
        }

        let live = |sock: &Path| sock.file_stem().is_some_and(|s| s == "live");
        let pruned = prune(&dir, Duration::from_secs(60), now, live).unwrap();
        assert_eq!((pruned.files, pruned.bytes), (1, 1));
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["live.log", "new.log", "old.sock"]);

        let pruned = prune(&dir, Duration::ZERO, now, live).unwrap();
        assert_eq!(pruned.files, 1, "a zero cutoff takes every log not running");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
