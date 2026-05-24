use std::{
    backtrace::Backtrace,
    fs,
    io::{self, Write},
    panic::{self, PanicHookInfo},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::Utc;

use crate::file_logging::{redact, SharedRedactingDailyWriter};

const CRASH_PREFIX: &str = "crash-";
const CRASH_SUFFIX: &str = ".log";

#[derive(Clone, Debug)]
pub struct CrashDumpWriter {
    log_dir: PathBuf,
    recent_logs: SharedRedactingDailyWriter,
}

impl CrashDumpWriter {
    #[must_use]
    pub fn new(data_dir: impl AsRef<Path>, recent_logs: SharedRedactingDailyWriter) -> Self {
        Self {
            log_dir: data_dir.as_ref().join("logs"),
            recent_logs,
        }
    }

    pub fn install(self) {
        let previous_hook = panic::take_hook();
        let previous_hook = Arc::new(Mutex::new(Some(previous_hook)));
        let hook_previous = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            let _ = self.write(info);
            if let Some(previous) = hook_previous
                .lock()
                .expect("panic hook mutex poisoned")
                .as_ref()
            {
                previous(info);
            }
        }));
    }

    pub fn write(&self, info: &PanicHookInfo<'_>) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.log_dir)?;
        let path = unique_crash_path(&self.log_dir);
        let mut file = fs::File::create(&path)?;
        file.write_all(redact(&self.dump(info)).as_bytes())?;
        file.flush()?;
        Ok(path)
    }

    fn dump(&self, info: &PanicHookInfo<'_>) -> String {
        let mut output = String::new();
        output.push_str("panic message:\n");
        output.push_str(&panic_message(info));
        output.push_str("\n\nstack backtrace:\n");
        output.push_str(&format!("{:?}", Backtrace::force_capture()));
        output.push_str("\n\nlast 200 in-memory log lines:\n");
        for line in self.recent_logs.recent_lines() {
            output.push_str(&line);
            if !line.ends_with('\n') {
                output.push('\n');
            }
        }
        output
    }
}

fn unique_crash_path(log_dir: &Path) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    log_dir.join(format!("{CRASH_PREFIX}{timestamp}{CRASH_SUFFIX}"))
}

fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    };

    match info.location() {
        Some(location) => format!(
            "{message}\nlocation: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_logging::{RedactingDailyWriter, RECENT_LOG_LINE_CAPACITY};
    use std::panic;

    #[test]
    fn child_thread_panic_dump_contains_redacted_sections() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs =
            SharedRedactingDailyWriter::new(RedactingDailyWriter::new(temp.path().join("logs")));
        {
            let mut writer = logs.make_test_writer();
            for index in 0..205 {
                writeln!(writer, "log line {index} token=abcdefghijklmnopqrstuvwxyz").expect("log");
            }
        }

        CrashDumpWriter::new(temp.path(), logs).install();
        let _ = std::thread::spawn(|| panic!("boom user@example.com")).join();

        let dumps: Vec<_> = fs::read_dir(temp.path().join("logs"))
            .expect("logs")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("crash-"))
            })
            .collect();
        assert_eq!(dumps.len(), 1);
        let content = fs::read_to_string(&dumps[0]).expect("dump");
        assert!(content.contains("panic message:"));
        assert!(content.contains("boom [REDACTED_EMAIL]"));
        assert!(content.contains("stack backtrace:"));
        assert!(content.contains("last 200 in-memory log lines:"));
        assert!(content.contains("[REDACTED_API_KEY]"));
        assert!(!content.contains("log line 0"));
        assert!(!content.contains("user@example.com"));
        assert!(!content.contains("abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(RECENT_LOG_LINE_CAPACITY, 200);
    }
}
