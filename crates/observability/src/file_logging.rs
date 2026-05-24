use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use regex::Regex;
use tracing_subscriber::{
    fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter,
};

pub const LOG_RETENTION_DAYS: i64 = 14;
const LOG_PREFIX: &str = "openspace-";
const LOG_SUFFIX: &str = ".log";

#[derive(Debug)]
pub struct FileLoggingGuard {
    _writer: SharedRedactingDailyWriter,
}

pub fn init_file_logging(data_dir: impl AsRef<Path>) -> io::Result<FileLoggingGuard> {
    let log_dir = data_dir.as_ref().join("logs");
    prune_old_logs(&log_dir, Utc::now())?;

    let writer = SharedRedactingDailyWriter::new(RedactingDailyWriter::new(log_dir));
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter()));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(writer.clone()),
        )
        .try_init()
        .map_err(|err| io::Error::new(io::ErrorKind::AlreadyExists, err))?;

    Ok(FileLoggingGuard { _writer: writer })
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

#[derive(Clone, Debug)]
pub struct SharedRedactingDailyWriter(Arc<Mutex<RedactingDailyWriter>>);

impl SharedRedactingDailyWriter {
    #[must_use]
    pub fn new(writer: RedactingDailyWriter) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }
}

impl<'a> MakeWriter<'a> for SharedRedactingDailyWriter {
    type Writer = SharedRedactingDailyWriteGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedRedactingDailyWriteGuard(self.0.clone())
    }
}

pub struct SharedRedactingDailyWriteGuard(Arc<Mutex<RedactingDailyWriter>>);

impl Write for SharedRedactingDailyWriteGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log writer mutex poisoned").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().expect("log writer mutex poisoned").flush()
    }
}

#[derive(Debug)]
pub struct RedactingDailyWriter {
    log_dir: PathBuf,
    today: Option<NaiveDate>,
    file: Option<fs::File>,
    clock: fn() -> NaiveDate,
}

impl RedactingDailyWriter {
    #[must_use]
    pub fn new(log_dir: impl Into<PathBuf>) -> Self {
        Self {
            log_dir: log_dir.into(),
            today: None,
            file: None,
            clock: || Utc::now().date_naive(),
        }
    }

    #[cfg(test)]
    fn set_clock_for_test(&mut self, clock: fn() -> NaiveDate) {
        self.clock = clock;
        self.file = None;
    }

    fn ensure_file(&mut self) -> io::Result<&mut fs::File> {
        let today = (self.clock)();
        if self.today != Some(today) {
            self.file = None;
        }
        if self.file.is_none() {
            fs::create_dir_all(&self.log_dir)?;
            let path = self.log_dir.join(log_filename(today));
            self.file = Some(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            );
        }
        self.today = Some(today);
        Ok(self.file.as_mut().expect("file initialized"))
    }
}

impl Write for RedactingDailyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let input = String::from_utf8_lossy(buf);
        let redacted = redact(&input);
        self.ensure_file()?.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}

#[must_use]
pub fn log_filename(date: NaiveDate) -> String {
    format!("{LOG_PREFIX}{}{LOG_SUFFIX}", date.format("%Y-%m-%d"))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    #[test]
    fn redacts_sensitive_fixtures() {
        let input = "api_key=sk_test_12345678901234567890 bearer abc.def.ghi user@example.com file_content: \
            Lorem ipsum dolor sit amet, consectetur adipiscing elit. Vestibulum vulputate justo sed tortor aliquam, \
            at egestas massa accumsan. Integer luctus, nisi sit amet mattis imperdiet, tortor justo ultricies sem, \
            vitae blandit ante neque sed augue. Donec nec.";

        let output = redact(input);

        assert!(!output.contains("sk_test_12345678901234567890"));
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
        let mut writer = RedactingDailyWriter::new(temp.path());
        writer.set_clock_for_test(|| NaiveDate::from_ymd_opt(2025, 1, 31).expect("valid date"));
        writer.write_all(b"hello").expect("write");
        writer.flush().expect("flush");

        let path = temp.path().join("openspace-2025-01-31.log");
        assert_eq!(fs::read_to_string(path).expect("read"), "hello");
    }

    #[test]
    fn rolls_over_to_new_daily_log_file() {
        fn jan_31() -> NaiveDate {
            NaiveDate::from_ymd_opt(2025, 1, 31).expect("valid date")
        }
        fn feb_01() -> NaiveDate {
            NaiveDate::from_ymd_opt(2025, 2, 1).expect("valid date")
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let mut writer = RedactingDailyWriter::new(temp.path());
        writer.set_clock_for_test(jan_31);
        writer.write_all(b"first").expect("first");
        writer.set_clock_for_test(feb_01);
        writer.write_all(b"second").expect("second");
        writer.flush().expect("flush");

        assert_eq!(
            fs::read_to_string(temp.path().join("openspace-2025-01-31.log")).expect("jan"),
            "first"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("openspace-2025-02-01.log")).expect("feb"),
            "second"
        );
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
