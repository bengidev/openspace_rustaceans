use std::{
    collections::VecDeque,
    env, fs,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use regex::Regex;
use tracing_appender::rolling::RollingFileAppender;
use tracing_flame::{FlameLayer, FlushGuard};
use tracing_subscriber::{
    fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry,
};

pub const LOG_RETENTION_DAYS: i64 = 14;
pub const RECENT_LOG_LINE_CAPACITY: usize = 200;
pub const FLAME_ENV: &str = "OPENSPACE_FLAME";
pub const FLAME_FILENAME: &str = "flame.folded";
const LOG_PREFIX: &str = "openspace-";
const LOG_SUFFIX: &str = ".log";

type FlameFileLayer = FlameLayer<Registry, BufWriter<File>>;
type FlameFileGuard = FlushGuard<BufWriter<File>>;

#[derive(Debug)]
pub struct FileLoggingGuard {
    _writer: SharedRedactingWriter,
    _flame_guard: Option<FlameFileGuard>,
}

pub fn init_file_logging(data_dir: impl AsRef<Path>) -> io::Result<FileLoggingGuard> {
    let data_dir = data_dir.as_ref();
    let log_dir = data_dir.join("logs");
    prune_old_logs(&log_dir, Utc::now())?;

    // Use tracing-appender for daily file rotation. The RollingFileAppender
    // handles date-check, close-reopen, and filename templating — matching
    // the PRD-05 spec for "tracing-appender::rolling::daily".
    let appender =
        tracing_appender::rolling::daily(&log_dir, LOG_PREFIX);
    let writer = SharedRedactingWriter::new(appender);
    crate::crash_dump::CrashDumpWriter::new(data_dir, writer.clone()).install();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter()));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(writer.clone());
    let (flame_layer, flame_guard) = flame_layer(data_dir)?;

    Registry::default()
        .with(flame_layer)
        .with(filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|err| io::Error::new(io::ErrorKind::AlreadyExists, err))?;

    Ok(FileLoggingGuard {
        _writer: writer,
        _flame_guard: flame_guard,
    })
}

fn flame_layer(data_dir: &Path) -> io::Result<(Option<FlameFileLayer>, Option<FlameFileGuard>)> {
    if !flame_enabled() {
        return Ok((None, None));
    }

    let flame_path = data_dir.join(FLAME_FILENAME);
    let (layer, guard) =
        FlameLayer::with_file(&flame_path).map_err(|err| io::Error::other(err.to_string()))?;
    Ok((Some(layer), Some(guard)))
}

fn flame_enabled() -> bool {
    env::var_os(FLAME_ENV).is_some_and(|value| value == "1")
}

#[must_use]
pub const fn default_filter() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    }
}

pub fn prune_old_logs(log_dir: impl AsRef<Path>, now: DateTime<Utc>) -> io::Result<()> {
    let log_dir = log_dir.as_ref();
    if !log_dir.exists() {
        return Ok(());
    }

    let cutoff = now.date_naive() - Duration::days(LOG_RETENTION_DAYS);
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(date) = log_date(&path) else {
            continue;
        };
        if date < cutoff {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn log_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let date = name.strip_prefix(LOG_PREFIX)?.strip_suffix(LOG_SUFFIX)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

// ─── Redacting writer wrapping tracing-appender's RollingFileAppender ───

/// Thread-safe redacting log writer backed by [`tracing_appender::rolling::daily`].
///
/// The inner [`RollingFileAppender`] handles date-based file rotation. This
/// wrapper intercepts every write to apply the PRD-05 `redact()` pipeline and
/// maintains an in-memory ring buffer of the last `RECENT_LOG_LINE_CAPACITY`
/// lines for crash dumps.
#[derive(Clone, Debug)]
pub struct SharedRedactingWriter {
    appender: Arc<Mutex<RollingFileAppender>>,
    recent_lines: Arc<Mutex<VecDeque<String>>>,
}

impl SharedRedactingWriter {
    #[must_use]
    pub fn new(appender: RollingFileAppender) -> Self {
        Self {
            appender: Arc::new(Mutex::new(appender)),
            recent_lines: Arc::new(Mutex::new(VecDeque::with_capacity(
                RECENT_LOG_LINE_CAPACITY,
            ))),
        }
    }

    #[must_use]
    pub fn recent_lines(&self) -> Vec<String> {
        self.recent_lines
            .lock()
            .expect("log writer mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    fn write_redacted(&self, buf: &[u8]) -> io::Result<usize> {
        let input = String::from_utf8_lossy(buf);
        let redacted = redact(&input);

        // Track recent lines for crash dump
        {
            let mut lines = self
                .recent_lines
                .lock()
                .expect("recent lines mutex poisoned");
            for line in redacted.lines() {
                if lines.len() == RECENT_LOG_LINE_CAPACITY {
                    lines.pop_front();
                }
                lines.push_back(line.to_owned());
            }
        }

        // Delegate to tracing-appender for rotation + disk write
        let mut appender = self.appender.lock().expect("appender mutex poisoned");
        appender.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    #[cfg(test)]
    pub fn make_test_writer(&self) -> SharedRedactingWriteGuard {
        SharedRedactingWriteGuard {
            parent: self.clone(),
            buf: Vec::new(),
        }
    }
}

/// [`MakeWriter`] implementation for the tracing subscriber. Each event gets
/// a fresh guard that accumulates bytes, applies redaction on flush, then
/// writes to the underlying [`RollingFileAppender`].
impl<'a> MakeWriter<'a> for SharedRedactingWriter {
    type Writer = SharedRedactingWriteGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedRedactingWriteGuard {
            parent: self.clone(),
            buf: Vec::new(),
        }
    }
}

pub struct SharedRedactingWriteGuard {
    parent: SharedRedactingWriter,
    buf: Vec<u8>,
}

impl Write for SharedRedactingWriteGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.parent.write_redacted(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

impl Drop for SharedRedactingWriteGuard {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

// ─── Redaction ───────────────────────────────────────────────────────

#[must_use]
pub fn redact(input: &str) -> String {
    let api_keys = Regex::new(
        r#"(?i)(api_key|api-key|apikey|secret|token)[=:] *['\"]?[A-Za-z0-9_-]{20,}['\"]?"#,
    )
    .expect("valid regex");
    let bearer = Regex::new(r"(?i)bearer +[A-Za-z0-9._~+/=-]+").expect("valid regex");
    let email =
        Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+[.][A-Za-z]{2,}").expect("valid regex");
    let redacted = api_keys.replace_all(input, "[REDACTED_API_KEY]");
    let redacted = bearer.replace_all(&redacted, "Bearer [REDACTED]");
    let redacted = email.replace_all(&redacted, "[REDACTED_EMAIL]");
    redact_long_file_content(&redacted)
}

fn redact_long_file_content(input: &str) -> String {
    for marker in [
        "file_content=",
        "file_content: ",
        "file content=",
        "file content: ",
    ] {
        if let Some(start) = input.find(marker) {
            let value_start = start + marker.len();
            if input[value_start..].chars().count() > 200 {
                let mut output = input[..start].to_owned();
                output.push_str("file_content=[REDACTED_SNIPPET]");
                return output;
            }
        }
    }
    input.to_owned()
}

#[allow(dead_code)]
fn _rust_log_present() -> bool {
    env::var_os("RUST_LOG").is_some()
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::{fs, sync::Mutex};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn redacts_sensitive_fixtures() {
        let input = "api_key=test_key_abcdefghijklmnopqrstuvwx bearer abc.def.ghi user@example.com file_content: \
            Lorem ipsum dolor sit amet, consectetur adipiscing elit. Vestibulum vulputate justo sed tortor aliquam, \
            at egestas massa accumsan. Integer luctus, nisi sit amet mattis imperdiet, tortor justo ultricies sem, \
            vitae blandit ante neque sed augue. Donec nec.";

        let output = redact(input);

        assert!(!output.contains("test_key_abcdefghijklmnopqrstuvwx"));
        assert!(!output.contains("abc.def.ghi"));
        assert!(!output.contains("user@example.com"));
        assert!(output.contains("[REDACTED_API_KEY]"));
        assert!(output.contains("Bearer [REDACTED]"));
        assert!(output.contains("[REDACTED_EMAIL]"));
        assert!(output.contains("[REDACTED_SNIPPET]"));
    }

    proptest! {
        #[test]
        fn api_key_shaped_substrings_do_not_survive(secret in "[A-Za-z0-9_-]{20,80}") {
            let input = format!("api_key={secret}");
            let output = redact(&input);
            prop_assert!(!output.contains(&secret));
        }
    }

    #[test]
    fn writes_daily_log_filename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let appender = tracing_appender::rolling::daily(temp.path(), LOG_PREFIX);
        let writer = SharedRedactingWriter::new(appender);

        let mut guard = writer.make_test_writer();
        guard.write_all(b"hello").expect("write");
        drop(guard);

        // tracing-appender produces filenames like `openspace-2025-01-31` (no
        // `.log` suffix).  Find the single file in the directory and read it.
        let entries: Vec<_> = fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(LOG_PREFIX)
            })
            .collect();
        assert_eq!(entries.len(), 1);
        let content = fs::read_to_string(entries[0].path()).expect("read");
        assert_eq!(content, "hello");
    }

    #[test]
    fn writer_remembers_recent_lines() {
        let temp = tempfile::tempdir().expect("tempdir");
        let appender = tracing_appender::rolling::daily(temp.path(), LOG_PREFIX);
        let writer = SharedRedactingWriter::new(appender);

        for i in 0..250 {
            let mut guard = writer.make_test_writer();
            write!(guard, "line {i}").expect("write");
            drop(guard);
        }

        let lines = writer.recent_lines();
        assert_eq!(lines.len(), RECENT_LOG_LINE_CAPACITY);
        assert!(!lines.contains(&"line 0".to_string()));
        assert!(lines.contains(&"line 249".to_string()));
    }

    #[test]
    fn flame_layer_disabled_when_env_unset() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        env::remove_var(FLAME_ENV);
        let temp = tempfile::tempdir().expect("tempdir");

        let (layer, guard) = flame_layer(temp.path()).expect("flame layer");

        assert!(layer.is_none());
        assert!(guard.is_none());
        assert!(!temp.path().join(FLAME_FILENAME).exists());
    }

    #[test]
    fn flame_layer_enabled_by_env_writes_folded_file() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        env::set_var(FLAME_ENV, "1");
        let temp = tempfile::tempdir().expect("tempdir");

        let (layer, guard) = flame_layer(temp.path()).expect("flame layer");

        assert!(layer.is_some());
        assert!(guard.is_some());
        assert!(temp.path().join(FLAME_FILENAME).exists());
        env::remove_var(FLAME_ENV);
    }

    #[test]
    fn flame_layer_ignores_non_one_env_values() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        env::set_var(FLAME_ENV, "true");
        let temp = tempfile::tempdir().expect("tempdir");

        let (layer, guard) = flame_layer(temp.path()).expect("flame layer");

        assert!(layer.is_none());
        assert!(guard.is_none());
        assert!(!temp.path().join(FLAME_FILENAME).exists());
        env::remove_var(FLAME_ENV);
    }

    #[test]
    fn retention_prunes_logs_older_than_fourteen_days() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = temp.path().join("openspace-2025-01-01.log");
        let kept = temp.path().join("openspace-2025-01-20.log");
        fs::write(&old, "old").expect("old");
        fs::write(&kept, "kept").expect("kept");

        let now = DateTime::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2025, 1, 31)
                .expect("valid date")
                .and_hms_opt(0, 0, 0)
                .expect("valid time"),
            Utc,
        );
        prune_old_logs(temp.path(), now).expect("prune");

        assert!(!old.exists());
        assert!(kept.exists());
    }
}
