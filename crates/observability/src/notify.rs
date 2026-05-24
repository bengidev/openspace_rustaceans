use crate::{Notification, Observability, Severity};

pub fn emit(
    observability: &Observability,
    severity: Severity,
    source: impl Into<String>,
    message: impl Into<String>,
) -> Notification {
    observability.emit(severity, source, message)
}
