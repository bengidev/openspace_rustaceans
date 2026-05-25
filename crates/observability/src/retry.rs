//! Tool retry classification primitives.
//!
//! Maps [`openspace_shared::tool::ToolError`] kinds into retry-facing
//! categories so the agent loop can apply a consistent backoff-and-surface
//! strategy without coupling to every tool's error shape.

use openspace_shared::tool::{ToolError, ToolErrorKind};
use static_assertions::assert_impl_all;

/// Retry-facing classification for a tool failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFailureKind {
    /// Operation may succeed after waiting.
    Transient,
    /// Operation cannot succeed without a different request or implementation.
    Permanent,
    /// Operation needs different permissions or user approval.
    PermissionDenied,
    /// Operation was cancelled by the caller.
    Cancelled,
}

assert_impl_all!(ToolFailureKind: Send, Sync);

/// Public classifier hook used by the runtime to map tool errors into
/// retry policy inputs.
pub trait ToolErrorClassifier: Send + Sync {
    /// Classify a tool error for retry handling.
    fn classify(&self, error: &ToolError) -> ToolFailureKind;
}

assert_impl_all!(dyn ToolErrorClassifier: Send, Sync);

/// Default classifier for the shared [`ToolErrorKind`] vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefaultToolErrorClassifier;

impl ToolErrorClassifier for DefaultToolErrorClassifier {
    fn classify(&self, error: &ToolError) -> ToolFailureKind {
        match error.kind {
            ToolErrorKind::Cancelled => ToolFailureKind::Cancelled,
            ToolErrorKind::SandboxBlocked | ToolErrorKind::PermissionDenied => {
                ToolFailureKind::PermissionDenied
            }
            ToolErrorKind::Io => ToolFailureKind::Transient,
            ToolErrorKind::InvalidArgs | ToolErrorKind::Internal => ToolFailureKind::Permanent,
            // `ToolErrorKind` is `#[non_exhaustive]` — unknown future
            // variants are treated as permanent failures so the agent
            // loop surfaces them rather than retrying blindly.
            _ => ToolFailureKind::Permanent,
        }
    }
}

/// Outcome of applying [`RetryPolicy`] to a classified failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retry after this delay.
    Retry { backoff: std::time::Duration },
    /// Surface the failure without another attempt.
    Surface,
    /// Propagate cancellation without user-facing error escalation.
    PropagateCancellation,
}

assert_impl_all!(RetryDecision: Send, Sync);

/// Retry policy for tool execution failures.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Maximum transient attempts, including the initial attempt.
    pub max_transient_attempts: u32,
    /// Exponential backoff schedule used before transient retries.
    pub transient_backoff: Vec<std::time::Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_transient_attempts: 3,
            transient_backoff: vec![
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(2),
            ],
        }
    }
}

impl RetryPolicy {
    /// Decide whether to retry after `attempts_so_far` completed attempts.
    #[must_use]
    pub fn decide(&self, kind: ToolFailureKind, attempts_so_far: u32) -> RetryDecision {
        match kind {
            ToolFailureKind::Transient if attempts_so_far < self.max_transient_attempts => {
                let idx = attempts_so_far.saturating_sub(1) as usize;
                let backoff =
                    self.transient_backoff.get(idx).copied().unwrap_or_else(|| {
                        self.transient_backoff.last().copied().unwrap_or_default()
                    });
                RetryDecision::Retry { backoff }
            }
            ToolFailureKind::Cancelled => RetryDecision::PropagateCancellation,
            ToolFailureKind::Transient
            | ToolFailureKind::Permanent
            | ToolFailureKind::PermissionDenied => RetryDecision::Surface,
        }
    }

    /// Classify an error with `classifier`, then apply this policy.
    #[must_use]
    pub fn decide_for_error(
        &self,
        classifier: &dyn ToolErrorClassifier,
        error: &ToolError,
        attempts_so_far: u32,
    ) -> RetryDecision {
        self.decide(classifier.classify(error), attempts_so_far)
    }
}

assert_impl_all!(RetryPolicy: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retry_policy_uses_three_step_exponential_backoff() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_transient_attempts, 3);
        assert_eq!(
            policy.transient_backoff,
            vec![
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(2),
            ]
        );
    }

    #[test]
    fn retry_classifier_maps_tool_error_kinds() {
        let classifier = DefaultToolErrorClassifier;
        let cases = [
            (ToolErrorKind::Io, ToolFailureKind::Transient),
            (ToolErrorKind::InvalidArgs, ToolFailureKind::Permanent),
            (ToolErrorKind::Internal, ToolFailureKind::Permanent),
            (
                ToolErrorKind::SandboxBlocked,
                ToolFailureKind::PermissionDenied,
            ),
            (
                ToolErrorKind::PermissionDenied,
                ToolFailureKind::PermissionDenied,
            ),
            (ToolErrorKind::Cancelled, ToolFailureKind::Cancelled),
        ];

        for (kind, expected) in cases {
            let error = ToolError::new(kind, "boom");
            assert_eq!(classifier.classify(&error), expected);
        }
    }

    #[test]
    fn retry_policy_matrix_covers_count_backoff_and_decision() {
        let policy = RetryPolicy::default();
        let cases = [
            (
                ToolFailureKind::Transient,
                1,
                RetryDecision::Retry {
                    backoff: std::time::Duration::from_millis(500),
                },
            ),
            (
                ToolFailureKind::Transient,
                2,
                RetryDecision::Retry {
                    backoff: std::time::Duration::from_secs(1),
                },
            ),
            (ToolFailureKind::Transient, 3, RetryDecision::Surface),
            (ToolFailureKind::Permanent, 1, RetryDecision::Surface),
            (ToolFailureKind::PermissionDenied, 1, RetryDecision::Surface),
            (
                ToolFailureKind::Cancelled,
                1,
                RetryDecision::PropagateCancellation,
            ),
        ];

        for (kind, attempts, expected) in cases {
            assert_eq!(policy.decide(kind, attempts), expected);
        }
    }

    #[test]
    fn decide_for_error_classifies_then_applies_policy() {
        let policy = RetryPolicy::default();
        let classifier = DefaultToolErrorClassifier;
        let err = ToolError::new(ToolErrorKind::Io, "temporary failure");

        assert_eq!(
            policy.decide_for_error(&classifier, &err, 2),
            RetryDecision::Retry {
                backoff: std::time::Duration::from_secs(1),
            }
        );
    }
}
