//! Test fixtures shared by the sandbox HTTP client tests and by
//! downstream provider integration tests.
//!
//! The pieces here are intentionally minimal:
//!
//! - [`AlwaysApprove`] / [`AlwaysDeny`] / [`NeverAnswer`] —
//!   `ApprovalChannel` stand-ins covering the three branches of the
//!   sandbox gate.
//! - [`ScriptedChannel`] — channel whose answer is pre-set
//!   per-call via a `Vec<ApprovalDecision>`. Useful when a single
//!   test exercises a sequence of approvals.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openspace_shared::sandbox::ApprovalReason;
use openspace_shared::tool::{ApprovalChannel, ApprovalDecision};

/// Approves every prompt. Equivalent to "user clicked Yes".
pub struct AlwaysApprove;

#[async_trait]
impl ApprovalChannel for AlwaysApprove {
    async fn request(&self, _reason: ApprovalReason) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}

/// Denies every prompt. Equivalent to "user clicked No".
pub struct AlwaysDeny;

#[async_trait]
impl ApprovalChannel for AlwaysDeny {
    async fn request(&self, _reason: ApprovalReason) -> ApprovalDecision {
        ApprovalDecision::Denied
    }
}

/// Never returns. Used to exercise the approval-timeout path in
/// the sandbox HTTP client.
pub struct NeverAnswer;

#[async_trait]
impl ApprovalChannel for NeverAnswer {
    async fn request(&self, _reason: ApprovalReason) -> ApprovalDecision {
        std::future::pending().await
    }
}

/// Approval channel whose answers are scripted in advance.
///
/// Pop semantics: the first request consumes index 0, the second
/// consumes index 1, etc. Once the script is exhausted the channel
/// falls back to [`ApprovalDecision::Timeout`] so a buggy test
/// surfaces a deterministic failure rather than hanging.
pub struct ScriptedChannel {
    script: Mutex<Vec<ApprovalDecision>>,
}

impl ScriptedChannel {
    /// Wrap a sequence of decisions into a shareable channel.
    #[must_use]
    pub fn new(decisions: Vec<ApprovalDecision>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(decisions),
        })
    }
}

#[async_trait]
impl ApprovalChannel for ScriptedChannel {
    async fn request(&self, _reason: ApprovalReason) -> ApprovalDecision {
        let mut guard = self.script.lock().expect("scripted channel poisoned");
        if guard.is_empty() {
            ApprovalDecision::Timeout
        } else {
            guard.remove(0)
        }
    }
}
