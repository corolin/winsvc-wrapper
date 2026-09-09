//! Rolling log engine: append / reset / none / roll / roll-by-size /
//! roll-by-time / roll-by-size-time, with retention (`keep_files`).
//!
//! Child stdout/stderr is pumped line-by-line through reader threads (the child
//! never holds log file handles directly), which sidesteps the classic
//! "locked log file" problem of file-inheriting wrappers.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, NaiveDate, NaiveTime};

use crate::config::LogMode;

// ---------------------------------------------------------------------------
// Clock (injectable for tests)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Clock {
    System,
    #[allow(dead_code)] // constructed only in tests
    Fake(Arc<Mutex<DateTime<Local>>>),
}

impl Clock {
    pub fn system() -> Clock {
        Clock::System
    }

    #[cfg(test)]
    pub fn fake(at: DateTime<Local>) -> (Clock, Arc<Mutex<DateTime<Local>>>) {
        let cell = Arc::new(Mutex::new(at));
        (Clock::Fake(cell.clone()), cell)
    }

    pub fn now(&self) -> DateTime<Local> {
        match self {
            Clock::System => Local::now(),
            Clock::Fake(cell) => *cell.lock().unwrap(),
        }
    }
}

// ---------------------------------------------------------------------------
// Rotating file
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Out,
    Err,
}

pub struct RotatingFile {
    mode: LogMode,
    dir: PathBuf,
    /// e.g. `myapp.out` — live file is `myapp.out.log`.
    stem: String,
    threshold_bytes: u64,
    chrono_pattern: String,
    auto_roll_at: Option<NaiveTime>,
    keep: i64,
    clock: Clock,

    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    size: u64,
    /// Current period name for time-based modes.
    period: Option<String>,
    last_auto_roll: Option<NaiveDate>,
    /// One-shot flags applied the first time the file is opened by this process.
    opened_once: bool,
}

impl RotatingFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: LogMode,
        dir: PathBuf,
        stem: String,
        threshold_bytes: u64,
        chrono_pattern: String,
        auto_roll_at: Option<NaiveTime>,
        keep: i64,
        clock: Clock,
    ) -> RotatingFile {
        RotatingFile {
            mode,
            dir,
            stem,
            threshold_bytes,
            chrono_pattern,
            auto_roll_at,
            keep,
            clock,
            inner: Mutex::new(Inner {
                file: None,
                size: 0,
                period: None,
                last_auto_roll: None,
                opened_once: false,
            }),
        }
    }

    fn live_path(&self, period: Option<&str>) -> PathBuf {
        match period {
            Some(p) => self.dir.join(format!("{}.{}.log", self.stem, p)),
            None => self.dir.join(format!("{}.log", self.stem)),
        }
    }

    /// Writes one line (a trailing newline is appended). Best effort: errors
    /// are reported to stderr so a broken log config never kills the child.
    pub fn write_line(&self, line: &str) {
        if self.mode == LogMode::None {
            return;
        }
        let bytes = line.as_bytes();
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(e) = self.write_locked(&mut guard, bytes) {
            eprintln!(
                "rsw: log write failed for {}: {e}",
                self.live_path(guard.period.as_deref()).display()
            );
        }
    }

    fn write_locked(&self, inner: &mut Inner, bytes: &[u8]) -> io::Result<()> {
        let now = self.clock.now();
        match self.mode {
            LogMode::None => Ok(()),
            LogMode::Append | LogMode::Reset | LogMode::Roll => {
                self.ensure_plain_open(inner)?;
                self.append(inner, bytes)
            }
            LogMode::RollBySize => {
                self.ensure_plain_open(inner)?;
                if inner.size + bytes.len() as u64 + 1 > self.threshold_bytes {
                    self.rotate_numbered(inner, None)?;
                }
                self.append(inner, bytes)
            }
            LogMode::RollByTime => {
                let period = now.format(&self.chrono_pattern).to_string();
                self.ensure_period_file(inner, &period)?;
                self.append(inner, bytes)
            }
            LogMode::RollBySizeTime => {
                let period = now.format(&self.chrono_pattern).to_string();
                self.ensure_period_file(inner, &period)?;
                let auto_roll_due = self.auto_roll_due(inner, &now);
                if auto_roll_due || inner.size + bytes.len() as u64 + 1 > self.threshold_bytes {
                    self.rotate_numbered(inner, Some(&period))?;
                    if auto_roll_due {
                        inner.last_auto_roll = Some(now.date_naive());
                    }
                }
                self.append(inner, bytes)
            }
        }
    }

    fn auto_roll_due(&self, inner: &Inner, now: &DateTime<Local>) -> bool {
        let Some(at) = self.auto_roll_at else {
            return false;
        };
        if now.time() < at {
            return false;
        }
        match inner.last_auto_roll {
            Some(last) => now.date_naive() > last,
            // First check after startup: roll only if the existing file predates today.
            None => inner
                .file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .and_then(|m| m.modified().ok())
                .map(|modified| {
                    let modified: DateTime<Local> = modified.into();
                    modified.date_naive() < now.date_naive()
                })
                .unwrap_or(false),
        }
    }

    /// Opens the plain `<stem>.log`, applying reset/roll-once semantics.
    fn ensure_plain_open(&self, inner: &mut Inner) -> io::Result<()> {
        if inner.file.is_some() {
            return Ok(());
        }
        let path = self.live_path(None);
        match self.mode {
            LogMode::Reset if !inner.opened_once => {
                let f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)?;
                inner.file = Some(f);
                inner.size = 0;
                inner.opened_once = true;
                return Ok(());
            }
            LogMode::Roll if !inner.opened_once => {
                if path.exists() {
                    let old = self.dir.join(format!("{}.old", self.stem));
                    let _ = std::fs::remove_file(&old);
                    std::fs::rename(&path, &old)?;
                }
                let f = OpenOptions::new().create(true).append(true).open(&path)?;
                inner.size = f.metadata()?.len();
                inner.opened_once = true;
                inner.file = Some(f);
                return Ok(());
            }
            _ => {}
        }
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        inner.size = f.metadata()?.len();
        inner.opened_once = true;
        inner.file = Some(f);
        Ok(())
    }

    /// Opens (or switches to) the dated `<stem>.<period>.log` for time modes.
    fn ensure_period_file(&self, inner: &mut Inner, period: &str) -> io::Result<()> {
        if inner.file.is_some() && inner.period.as_deref() == Some(period) {
            return Ok(());
        }
        if let Some(f) = inner.file.take() {
            drop(f);
        }
        let path = self.live_path(Some(period));
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        inner.size = f.metadata()?.len();
        inner.file = Some(f);
        inner.period = Some(period.to_string());
        self.prune_period_files();
        Ok(())
    }

    fn append(&self, inner: &mut Inner, bytes: &[u8]) -> io::Result<()> {
        let Some(f) = inner.file.as_mut() else {
            return Ok(());
        };
        f.write_all(bytes)?;
        f.write_all(b"\n")?;
        f.flush()?;
        inner.size += bytes.len() as u64 + 1;
        Ok(())
    }

    /// Size-mode rotation: rename chain `<live>.N` -> `<live>.N+1`, live -> `.1`.
    fn rotate_numbered(&self, inner: &mut Inner, period: Option<&str>) -> io::Result<()> {
        if let Some(f) = inner.file.take() {
            drop(f);
        }
        let live = self.live_path(period);
        let max_idx = if self.keep >= 0 {
            self.keep.max(1)
        } else {
            max_existing_index(&self.dir, &live)
        };
        // Drop the oldest slot, shift the rest up by one.
        if self.keep >= 0 {
            let _ = std::fs::remove_file(with_suffix(&live, max_idx));
        }
        for idx in (1..max_idx).rev() {
            let from = with_suffix(&live, idx);
            let to = with_suffix(&live, idx + 1);
            if from.exists() {
                if to.exists() {
                    let _ = std::fs::remove_file(&to);
                }
                std::fs::rename(&from, &to)?;
            }
        }
        if live.exists() {
            std::fs::rename(&live, with_suffix(&live, 1))?;
        }
        let f = OpenOptions::new().create(true).append(true).open(&live)?;
        inner.size = 0;
        inner.file = Some(f);
        Ok(())
    }

    /// Retention for the time-based modes: keeps the newest `keep` periods.
    /// A period is `<stem>.<period>.log` plus, in roll-by-size-time, its
    /// numbered `.log.N` siblings — those are pruned with their period, so
    /// old days' size-rotated files cannot pile up forever.
    fn prune_period_files(&self) {
        if self.keep < 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let prefix = format!("{}.", self.stem);
        // Period names sort chronologically for the supported tokens.
        let mut by_period: std::collections::BTreeMap<String, Vec<PathBuf>> = Default::default();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some((period, tail)) = rest.split_once(".log") else {
                continue;
            };
            let numbered = tail
                .strip_prefix('.')
                .is_some_and(|n| n.parse::<u64>().is_ok());
            if period.is_empty() || !(tail.is_empty() || numbered) {
                continue;
            }
            by_period
                .entry(period.to_string())
                .or_default()
                .push(entry.path());
        }
        let excess = by_period.len().saturating_sub(self.keep as usize);
        for (_, files) in by_period.into_iter().take(excess) {
            for f in files {
                let _ = std::fs::remove_file(f);
            }
        }
    }
}

fn with_suffix(live: &Path, idx: i64) -> PathBuf {
    let mut name = live.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{idx}"));
    live.with_file_name(name)
}

/// Finds the highest existing `.<N>` suffix for a live path (unbounded chains).
fn max_existing_index(dir: &Path, live: &Path) -> i64 {
    let stem = match live.file_name().and_then(|n| n.to_str()) {
        Some(s) => format!("{s}."),
        None => return 1,
    };
    let mut max = 1i64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let Some(name) = e.file_name().into_string().ok() else {
                continue;
            };
            if let Some(rest) = name.strip_prefix(&stem)
                && let Ok(n) = rest.parse::<i64>()
            {
                max = max.max(n + 1);
            }
        }
    }
    max.clamp(1, 10_000)
}

// ---------------------------------------------------------------------------
// Sink: child stdout/stderr + wrapper event log
// ---------------------------------------------------------------------------

pub struct LogSink {
    out: RotatingFile,
    err: Option<RotatingFile>, // None when merged into `out`
    wrapper: Option<RotatingFile>,
    /// Mirror child lines to the console (foreground `rsw run` mode).
    tee: bool,
    /// Timestamps for wrapper events (same injectable clock as rotation).
    clock: Clock,
}

impl LogSink {
    pub fn open(
        logging: &crate::config::LoggingConfig,
        dir: &Path,
        basename: &str,
        tee: bool,
        clock: Clock,
    ) -> io::Result<LogSink> {
        std::fs::create_dir_all(dir)?;
        let chrono_pattern = crate::config::translate_time_pattern(&logging.time_pattern)
            .unwrap_or_else(|| "%Y%m%d".into());
        let auto_roll_at = logging
            .auto_roll_at
            .as_deref()
            .and_then(|s| NaiveTime::parse_from_str(s, "%H:%M:%S").ok());
        let make = |suffix: &str, mode: LogMode| {
            RotatingFile::new(
                mode,
                dir.to_path_buf(),
                format!("{basename}.{suffix}"),
                logging.size_threshold_mb.saturating_mul(1024 * 1024).max(1),
                chrono_pattern.clone(),
                auto_roll_at,
                logging.keep_files,
                clock.clone(),
            )
        };
        let out = make("out", logging.mode);
        let err = if logging.merge_stderr {
            None
        } else {
            Some(make("err", logging.mode))
        };
        let wrapper = if logging.wrapper_log {
            Some(make("wrapper", LogMode::Append))
        } else {
            None
        };
        Ok(LogSink {
            out,
            err,
            wrapper,
            tee,
            clock,
        })
    }

    /// Writes one raw child output line.
    pub fn child_line(&self, channel: Channel, line: &str) {
        debug_assert!(matches!(channel, Channel::Out | Channel::Err));
        let separate_err = channel == Channel::Err && self.err.is_some();
        let target = if separate_err {
            self.err.as_ref().expect("checked above")
        } else {
            &self.out
        };
        target.write_line(line);
        if self.tee {
            if separate_err {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }

    /// Records a wrapper lifecycle event with timestamp and level.
    pub fn event(&self, level: &str, message: &str) {
        let ts = self.clock.now().format("%Y-%m-%dT%H:%M:%S%:z");
        let line = format!("[{ts} {level:<5}] {message}");
        if let Some(w) = &self.wrapper {
            w.write_line(&line);
        }
        if self.tee {
            eprintln!("rsw: {message}");
        }
    }

    pub fn info(&self, message: &str) {
        self.event("INFO", message);
    }

    pub fn warn(&self, message: &str) {
        self.event("WARN", message);
    }

    pub fn error(&self, message: &str) {
        self.event("ERROR", message);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoggingConfig;

    fn sink_config(mode: LogMode, threshold_mb: u64, keep: i64) -> LoggingConfig {
        LoggingConfig {
            mode,
            size_threshold_mb: threshold_mb,
            keep_files: keep,
            ..LoggingConfig::default()
        }
    }

    fn write_many(f: &RotatingFile, lines: usize, prefix: &str) {
        for i in 0..lines {
            f.write_line(&format!("{prefix} line {i}"));
        }
    }

    #[test]
    fn roll_by_size_rotates_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, cell) = Clock::fake(Local::now());
        // 1 MB threshold floor is per-config; bypass by constructing directly.
        let f = RotatingFile::new(
            LogMode::RollBySize,
            dir.path().to_path_buf(),
            "app.out".into(),
            40, // bytes
            "%Y%m%d".into(),
            None,
            2,
            clock,
        );
        write_many(&f, 12, "hello");
        drop(cell);
        let live = dir.path().join("app.out.log");
        assert!(live.is_file());
        assert!(dir.path().join("app.out.log.1").is_file());
        assert!(dir.path().join("app.out.log.2").is_file());
        assert!(!dir.path().join("app.out.log.3").exists()); // pruned (keep=2)
        let live_len = std::fs::metadata(&live).unwrap().len();
        assert!(
            live_len <= 40,
            "live file must stay under threshold, got {live_len}"
        );
    }

    #[test]
    fn roll_by_time_switches_period() {
        let dir = tempfile::tempdir().unwrap();
        let start: DateTime<Local> = Local::now()
            .date_naive()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        let (clock, cell) = Clock::fake(start);
        let f = RotatingFile::new(
            LogMode::RollByTime,
            dir.path().to_path_buf(),
            "app.out".into(),
            1024,
            "%Y%m%d".into(),
            None,
            -1,
            clock,
        );
        f.write_line("day one");
        *cell.lock().unwrap() = (start + chrono::Duration::days(1)).with_timezone(&Local);
        f.write_line("day two");
        let d1 = start.format("%Y%m%d").to_string();
        let d2 = (start + chrono::Duration::days(1))
            .format("%Y%m%d")
            .to_string();
        assert!(dir.path().join(format!("app.out.{d1}.log")).is_file());
        assert!(dir.path().join(format!("app.out.{d2}.log")).is_file());
    }

    #[test]
    fn roll_by_time_prunes_by_keep() {
        let dir = tempfile::tempdir().unwrap();
        let start: DateTime<Local> = Local::now()
            .date_naive()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        let (clock, cell) = Clock::fake(start);
        let f = RotatingFile::new(
            LogMode::RollByTime,
            dir.path().to_path_buf(),
            "app.out".into(),
            1024,
            "%Y%m%d".into(),
            None,
            2,
            clock,
        );
        for day in 0..5 {
            f.write_line(&format!("day {day}"));
            *cell.lock().unwrap() = (start + chrono::Duration::days(1)).with_timezone(&Local);
        }
        let dated: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("app.out."))
            .collect();
        assert_eq!(dated.len(), 2, "keep=2 must retain exactly two files");
    }

    #[test]
    fn reset_truncates_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::fake(Local::now());
        let path = dir.path().join("app.out.log");
        std::fs::write(&path, "old content\n").unwrap();
        let f = RotatingFile::new(
            LogMode::Reset,
            dir.path().to_path_buf(),
            "app.out".into(),
            1024,
            "%Y%m%d".into(),
            None,
            -1,
            clock,
        );
        f.write_line("new line");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new line\n");
    }

    #[test]
    fn roll_mode_renames_to_old_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::fake(Local::now());
        let live = dir.path().join("app.out.log");
        std::fs::write(&live, "previous run\n").unwrap();
        let f = RotatingFile::new(
            LogMode::Roll,
            dir.path().to_path_buf(),
            "app.out".into(),
            1024,
            "%Y%m%d".into(),
            None,
            -1,
            clock,
        );
        f.write_line("fresh run");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.out.old")).unwrap(),
            "previous run\n"
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "fresh run\n");
    }

    #[test]
    fn append_mode_never_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::fake(Local::now());
        let f = RotatingFile::new(
            LogMode::Append,
            dir.path().to_path_buf(),
            "app.out".into(),
            10,
            "%Y%m%d".into(),
            None,
            2,
            clock,
        );
        write_many(&f, 30, "data");
        let entries = std::fs::read_dir(dir.path()).unwrap().flatten().count();
        assert_eq!(entries, 1, "append mode must produce a single file");
    }

    #[test]
    fn none_mode_discards() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::fake(Local::now());
        let f = RotatingFile::new(
            LogMode::None,
            dir.path().to_path_buf(),
            "app.out".into(),
            10,
            "%Y%m%d".into(),
            None,
            2,
            clock,
        );
        write_many(&f, 5, "x");
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten().count() == 0);
    }

    #[test]
    fn sink_merges_stderr_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = sink_config(LogMode::Append, 1, -1);
        let mut cfg = cfg;
        cfg.merge_stderr = true;
        let sink = LogSink::open(&cfg, dir.path(), "app", false, Clock::system()).unwrap();
        sink.child_line(Channel::Out, "to stdout");
        sink.child_line(Channel::Err, "to stderr");
        let content = std::fs::read_to_string(dir.path().join("app.out.log")).unwrap();
        assert_eq!(content, "to stdout\nto stderr\n");
        assert!(!dir.path().join("app.err.log").exists());
    }

    #[test]
    fn size_time_prunes_numbered_files_of_old_periods() {
        let dir = tempfile::tempdir().unwrap();
        let start: DateTime<Local> = Local::now()
            .date_naive()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        let (clock, cell) = Clock::fake(start);
        let f = RotatingFile::new(
            LogMode::RollBySizeTime,
            dir.path().to_path_buf(),
            "app.out".into(),
            30, // bytes: several size rotations per day
            "%Y%m%d".into(),
            None,
            2, // periods to keep
            clock,
        );
        for day in 0..4 {
            write_many(&f, 8, &format!("day{day}"));
            *cell.lock().unwrap() = (start + chrono::Duration::days(day + 1)).with_timezone(&Local);
        }
        let periods: std::collections::BTreeSet<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter_map(|n| {
                n.strip_prefix("app.out.")
                    .and_then(|r| r.split_once(".log"))
                    .map(|(p, _)| p.to_string())
            })
            .collect();
        assert_eq!(
            periods.len(),
            2,
            "only the newest 2 periods (live + numbered files) may remain: {periods:?}"
        );
        let newest_numbered = dir.path().join(format!(
            "app.out.{}.log.1",
            (start + chrono::Duration::days(3)).format("%Y%m%d")
        ));
        assert!(
            newest_numbered.is_file(),
            "numbered files of kept periods must survive"
        );
    }

    #[test]
    fn size_time_hybrid_rotates_on_size() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::fake(Local::now());
        let f = RotatingFile::new(
            LogMode::RollBySizeTime,
            dir.path().to_path_buf(),
            "app.out".into(),
            30,
            "%Y%m%d".into(),
            None,
            3,
            clock,
        );
        write_many(&f, 8, "hybrid");
        let today = Local::now().format("%Y%m%d").to_string();
        assert!(dir.path().join(format!("app.out.{today}.log")).is_file());
        assert!(dir.path().join(format!("app.out.{today}.log.1")).is_file());
    }
}
