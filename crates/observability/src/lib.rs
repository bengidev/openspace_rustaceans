//! Notification spine and in-memory error log.

use std::{
    collections::VecDeque,
    fs, io,
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::file_logging::redact;

pub mod crash_dump;
pub mod file_logging;
pub mod notify;
pub mod tracing_support;

pub const DEFAULT_ERROR_LOG_CAPACITY: usize = 1_000;
const DEFAULT_SUBSCRIBER_CAPACITY: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UiSurface {
    Passive,
    Toast,
    Modal,
    Blocking,
}

impl Severity {
    #[must_use]
    pub const fn ui_surface(self) -> UiSurface {
        match self {
            Self::Low => UiSurface::Passive,
            Self::Medium => UiSurface::Toast,
            Self::High => UiSurface::Modal,
            Self::Critical => UiSurface::Blocking,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub severity: Severity,
    pub source: String,
    pub message: String,
    pub detail: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub surface: UiSurface,
}

impl Notification {
    #[must_use]
    pub fn new(severity: Severity, source: impl Into<String>, message: impl Into<String>) -> Self {
        Self::with_detail(severity, source, message, None::<String>)
    }

    #[must_use]
    pub fn with_detail(
        severity: Severity,
        source: impl Into<String>,
        message: impl Into<String>,
        detail: Option<impl Into<String>>,
    ) -> Self {
        Self {
            severity,
            source: source.into(),
            message: message.into(),
            detail: detail.map(Into::into),
            timestamp: Utc::now(),
            surface: severity.ui_surface(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ErrorLog {
    entries: VecDeque<Notification>,
    capacity: usize,
}

impl Default for ErrorLog {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_ERROR_LOG_CAPACITY)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorLogExportFilter<'a> {
    pub severity: Option<Severity>,
    pub source: Option<&'a str>,
}

impl ErrorLog {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn record(&mut self, event: Notification) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(event);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn entries(&self) -> Vec<Notification> {
        self.entries.iter().cloned().collect()
    }

    #[must_use]
    pub fn filter(&self, severity: Option<Severity>, source: Option<&str>) -> Vec<Notification> {
        self.filtered_entries(ErrorLogExportFilter { severity, source })
            .cloned()
            .collect()
    }

    pub fn export(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.export_filtered(path, ErrorLogExportFilter::default())
    }

    pub fn export_filtered(
        &self,
        path: impl AsRef<Path>,
        filter: ErrorLogExportFilter<'_>,
    ) -> io::Result<()> {
        fs::write(path, self.export_snapshot(filter))
    }

    #[must_use]
    pub fn export_snapshot(&self, filter: ErrorLogExportFilter<'_>) -> String {
        let mut snapshot = String::new();
        for event in self.filtered_entries(filter) {
            snapshot.push_str(&redact(&format!(
                "timestamp={} severity={:?} source={} message={}\n",
                event.timestamp.to_rfc3339(),
                event.severity,
                event.source,
                event.message
            )));
            if let Some(detail) = &event.detail {
                snapshot.push_str(&redact(&format!("detail={}\n", detail)));
            }
        }
        snapshot
    }

    fn filtered_entries<'a>(
        &'a self,
        filter: ErrorLogExportFilter<'a>,
    ) -> impl Iterator<Item = &'a Notification> {
        self.entries
            .iter()
            .filter(move |event| {
                filter
                    .severity
                    .is_none_or(|expected| event.severity == expected)
            })
            .filter(move |event| {
                filter
                    .source
                    .is_none_or(|expected| event.source == expected)
            })
    }
}

#[derive(Clone, Debug)]
pub struct Observability {
    log: Arc<Mutex<ErrorLog>>,
    sender: broadcast::Sender<Notification>,
}

impl Default for Observability {
    fn default() -> Self {
        Self::new(DEFAULT_ERROR_LOG_CAPACITY)
    }
}

impl Observability {
    #[must_use]
    pub fn new(log_capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(DEFAULT_SUBSCRIBER_CAPACITY);
        Self {
            log: Arc::new(Mutex::new(ErrorLog::with_capacity(log_capacity))),
            sender,
        }
    }

    pub fn emit(
        &self,
        severity: Severity,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Notification {
        let event = Notification::new(severity, source, message);
        self.record(event.clone());
        event
    }

    pub fn record(&self, event: Notification) {
        self.log
            .lock()
            .expect("error log mutex poisoned")
            .record(event.clone());
        let _ = self.sender.send(event);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.sender.subscribe()
    }

    #[must_use]
    pub fn log(&self) -> ErrorLog {
        self.log.lock().expect("error log mutex poisoned").clone()
    }
}

pub use tokio::sync::broadcast::error::RecvError as NotificationStreamRecvError;
pub type NotificationStream = tokio::sync::broadcast::Receiver<Notification>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_error_log_capacity_is_1000() {
        assert_eq!(ErrorLog::default().capacity(), DEFAULT_ERROR_LOG_CAPACITY);
    }

    #[test]
    fn ring_buffer_drops_oldest_entry() {
        let mut log = ErrorLog::with_capacity(2);
        log.record(Notification::new(Severity::Low, "one", "oldest"));
        log.record(Notification::new(Severity::Medium, "two", "middle"));
        log.record(Notification::new(Severity::High, "three", "newest"));

        let messages: Vec<_> = log
            .entries()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert_eq!(messages, ["middle", "newest"]);
    }

    #[test]
    fn filters_by_severity_and_source() {
        let mut log = ErrorLog::with_capacity(10);
        log.record(Notification::new(Severity::Low, "ui", "hint"));
        log.record(Notification::new(Severity::High, "network", "down"));
        log.record(Notification::new(Severity::High, "ui", "blocked"));

        assert_eq!(log.filter(Some(Severity::High), None).len(), 2);
        assert_eq!(log.filter(None, Some("ui")).len(), 2);
        assert_eq!(
            log.filter(Some(Severity::High), Some("ui"))[0].message,
            "blocked"
        );
    }

    #[test]
    fn exports_filtered_redacted_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("error-log.txt");
        let mut log = ErrorLog::with_capacity(10);
        log.record(Notification::with_detail(
            Severity::Low,
            "ui",
            "ignored user@example.com",
            Some("ignored"),
        ));
        log.record(Notification::with_detail(
            Severity::High,
            "network",
            "api_key=abcdefghijklmnopqrstuvwxyz",
            Some("Bearer abc.def.ghi email admin@example.com file_content: Lorem ipsum dolor sit amet, consectetur adipiscing elit. Vestibulum vulputate justo sed tortor aliquam, at egestas massa accumsan. Integer luctus, nisi sit amet mattis imperdiet, tortor justo ultricies sem, vitae blandit ante neque sed augue. Donec nec."),
        ));

        log.export_filtered(
            &path,
            ErrorLogExportFilter {
                severity: Some(Severity::High),
                source: Some("network"),
            },
        )
        .expect("export writes snapshot");

        let output = std::fs::read_to_string(path).expect("snapshot readable");
        assert!(output.contains("timestamp="));
        assert!(output.contains("severity=High"));
        assert!(output.contains("source=network"));
        assert!(output.contains("message=[REDACTED_API_KEY]"));
        assert!(output.contains(
            "detail=Bearer [REDACTED] email [REDACTED_EMAIL] file_content=[REDACTED_SNIPPET]"
        ));
        assert!(!output.contains("ui"));
        assert!(!output.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!output.contains("abc.def.ghi"));
        assert!(!output.contains("admin@example.com"));
        assert!(!output.contains("Vestibulum vulputate justo"));
    }

    #[test]
    fn severity_routes_to_expected_surfaces() {
        assert_eq!(Severity::Low.ui_surface(), UiSurface::Passive);
        assert_eq!(Severity::Medium.ui_surface(), UiSurface::Toast);
        assert_eq!(Severity::High.ui_surface(), UiSurface::Modal);
        assert_eq!(Severity::Critical.ui_surface(), UiSurface::Blocking);
    }

    #[tokio::test]
    async fn subscribers_receive_new_notifications() {
        let observability = Observability::default();
        let mut subscriber = observability.subscribe();

        let emitted = observability.emit(Severity::Critical, "runtime", "boom");
        let received = subscriber.recv().await.expect("notification delivered");

        assert_eq!(received, emitted);
    }

    #[test]
    fn emit_records_event_with_surface() {
        let observability = Observability::default();
        let emitted = observability.emit(Severity::Medium, "settings", "saved");

        assert_eq!(emitted.surface, UiSurface::Toast);
        assert_eq!(observability.log().entries(), [emitted]);
    }
}
