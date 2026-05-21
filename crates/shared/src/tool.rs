//! Tool trait surface.
//!
//! Tools are the things the agent loop dispatches when the AI emits
//! a `Part::ToolCall`. Every tool implements [`Tool`], a small trait
//! that exposes:
//!
//! - a stable [`ToolId`] the AI sees in its function-calling schema;
//! - a [`ToolSchema`] describing the input arguments;
//! - a [`SafetyClass`] the trust dial consults;
//! - a `sandbox_check` hook the runtime calls *before* execution to
//!   pre-flight the request against the active
//!   [`crate::sandbox::SandboxPolicy`];
//! - an async [`Tool::execute`] that does the work and returns a
//!   [`ToolOutcome`] carrying a summary, a JSON payload, and a list
//!   of [`crate::effect::Effect`]s for the home shell to apply.
//!
//! The surface is intentionally narrow. Anything tool-specific — a
//! filesystem handle, a shell session, an HTTP client — is built up
//! inside the implementation; the trait contract only carries the
//! cross-cutting plumbing every tool needs (cancellation, approval,
//! sandbox, workspace identity).
//!
//! # Why these particular pieces
//!
//! - [`ToolContext`] wraps the per-call plumbing. Holding it in a
//!   struct (rather than threading four arguments through
//!   `execute`) keeps the signature stable as new plumbing lands —
//!   a future telemetry handle or progress sink slots into the
//!   context, not into every tool's signature.
//! - [`CancelToken`] wraps `Arc<AtomicBool>` instead of pulling in
//!   `tokio::CancellationToken`. The Domain layer must stay
//!   runtime-agnostic; the agent loop converts between the two at
//!   the seam.
//! - [`ApprovalChannel`] is a `dyn`-compatible async trait. The
//!   shared crate cannot know how the user is asked (an Iced modal,
//!   a CLI prompt, a scripted approval in tests), so the
//!   implementation lives in feature crates while the trait surface
//!   stays here so every tool sees the same shape.
//! - [`SafetyClass`] is the canonical home for the safety dial. The
//!   slice-3 `sandbox` module re-exports it for backward
//!   compatibility within the crate; new code should import it from
//!   `tool`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

use crate::effect::Effect;
use crate::sandbox::{ApprovalReason, SandboxDecision, SandboxPolicy};
use crate::workspace::WorkspaceRef;

// ─────────────────────────────────────────────────────────────────────
// SafetyClass — canonical definition. The `sandbox` module re-exports
// this type for backward compatibility with slice-3 callers.
// ─────────────────────────────────────────────────────────────────────

/// Coarse safety category for a tool invocation.
///
/// The trust dial in [`crate::sandbox::TrustMode`] consults this
/// class to decide whether to ask for confirmation before firing
/// the call. Tools should pick the most restrictive class that
/// honestly describes the operation: a "read a file" tool is
/// [`SafetyClass::Safe`]; an "apply edits" tool is
/// [`SafetyClass::RequiresApproval`]; a "shell exec" tool is
/// [`SafetyClass::Destructive`].
///
/// This is the canonical home for the type. The `sandbox` module
/// re-exports it for backward compatibility with the slice-3 stub
/// (`pub use crate::tool::SafetyClass;`); new code imports directly
/// from `tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SafetyClass {
    /// Read-only or otherwise side-effect-free operation.
    Safe,
    /// Side-effecting but reversible (a file write, a network GET).
    RequiresApproval,
    /// Irreversible or high-blast-radius (`rm -rf`, schema migration).
    Destructive,
}

assert_impl_all!(SafetyClass: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolId — newtype around the stable string identifier the AI sees in
// its function-calling schema.
// ─────────────────────────────────────────────────────────────────────

/// Stable identifier for a tool, e.g. `"shell"`, `"read_file"`,
/// `"apply_edits"`.
///
/// This is the string the AI provider sees in the tool-calling
/// schema and references in `Part::ToolCall::name`. The newtype
/// stops a `ToolId` from being mistaken for an arbitrary `String`
/// (or another newtype) at type-check time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    /// Wrap a string. No validation — tool authors are responsible
    /// for picking a stable, lowercase identifier; the registry in
    /// PRD-08 owns deduplication checks.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

assert_impl_all!(ToolId: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolSchema — name + description + JSON input schema. The AI provider
// renders this into its function-calling surface verbatim.
// ─────────────────────────────────────────────────────────────────────

/// Metadata the agent loop hands to the AI provider so the model
/// knows the tool exists and what arguments it accepts.
///
/// `json_schema` is the input-arguments schema (a JSON Schema
/// object). The provider adapter is responsible for wrapping it in
/// whatever envelope its API requires — the Domain layer just owns
/// the canonical shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolSchema {
    /// Human-readable name shown in the AI provider's tool picker.
    /// Often matches [`ToolId`] but is allowed to differ when the
    /// stable id (`apply_edits`) and the display name (`Apply
    /// edits`) target different audiences.
    pub name: String,
    /// One-paragraph description rendered to the model. Should
    /// describe *when* to use the tool, not *how* to call it; the
    /// schema does the latter.
    pub description: String,
    /// JSON Schema object describing the input arguments. The
    /// provider adapter passes this through to the AI verbatim.
    pub json_schema: serde_json::Value,
}

impl ToolSchema {
    /// Construct a schema from its three fields. Pairs with
    /// `#[non_exhaustive]` so adding a new field later (e.g.
    /// `output_schema`) is a non-breaking change for downstream
    /// construction sites.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        json_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            json_schema,
        }
    }
}

assert_impl_all!(ToolSchema: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolArgs — newtype around the JSON value the AI sent. Carries an
// `as_object` accessor for the common case and a `Display` impl for
// debug logging.
// ─────────────────────────────────────────────────────────────────────

/// Arguments the AI emitted for a tool call.
///
/// Wraps a [`serde_json::Value`] rather than a typed shape because
/// each tool defines its own schema and parses the value into a
/// concrete struct on the way in. The wrapper exists so the trait
/// signature does not commit to `serde_json::Value` directly —
/// future PRDs can add a typed-args layer without churning the
/// `Tool::execute` signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolArgs(serde_json::Value);

impl ToolArgs {
    /// Wrap a JSON value as tool arguments.
    #[must_use]
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Borrow the underlying JSON value.
    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// Borrow the arguments as a JSON object, when they are one.
    /// Convenience for the common case — most tools take a struct
    /// of named arguments.
    #[must_use]
    pub fn as_object(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.0.as_object()
    }

    /// Consume the wrapper and return the underlying value.
    #[must_use]
    pub fn into_inner(self) -> serde_json::Value {
        self.0
    }
}

impl std::fmt::Display for ToolArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Compact rendering keeps debug logs readable; pretty
        // printing is opt-in via the underlying value.
        write!(f, "{}", self.0)
    }
}

assert_impl_all!(ToolArgs: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CancelToken — Arc<AtomicBool> wrapper. Domain-side cancellation
// handle that does not pull `tokio` into the shared crate.
// ─────────────────────────────────────────────────────────────────────

/// Cancellation handle threaded through every tool call.
///
/// Wraps an `Arc<AtomicBool>` so the agent loop can flip the flag
/// from outside while a tool is mid-execution; the tool checks
/// [`CancelToken::cancelled`] at sensible yield points and bails
/// out via [`ToolErrorKind::Cancelled`] when set.
///
/// The shape is deliberately runtime-agnostic — no
/// `tokio::CancellationToken`, no `futures::future::AbortHandle`. A
/// later seam in the agent loop wires one of those to this token's
/// `cancel`/`cancelled` API; tools never see the runtime-specific
/// type.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Create a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Snapshot whether cancellation has been requested.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Flip the token to the cancelled state. Idempotent — calling
    /// this on an already-cancelled token is a no-op.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

assert_impl_all!(CancelToken: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ApprovalChannel — async trait the runtime uses to ask the user. The
// shared crate cannot know how the prompt is rendered (Iced modal,
// CLI prompt, scripted decision in tests), so implementations live in
// feature crates.
// ─────────────────────────────────────────────────────────────────────

/// Decision the user (or a scripted stand-in) returned for an
/// [`ApprovalChannel::request`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ApprovalDecision {
    /// User confirmed the action — proceed.
    Approved,
    /// User rejected the action — bail with
    /// [`ToolErrorKind::SandboxBlocked`].
    Denied,
    /// No response arrived inside the channel's timeout — the
    /// runtime treats this as an implicit deny but keeps the
    /// distinction so telemetry can tell "user said no" from
    /// "prompt was never answered".
    Timeout,
}

assert_impl_all!(ApprovalDecision: Send, Sync);

/// Surface the runtime calls to ask the user about an approval
/// request.
///
/// `dyn`-compatible: implementations land in feature crates (`iced`
/// modal in the home shell, scripted decisions in tests). The
/// trait is `Send + Sync` so a single implementation can be
/// shared across tasks behind `Arc<dyn ApprovalChannel>`.
#[async_trait]
pub trait ApprovalChannel: Send + Sync {
    /// Render the prompt and return the user's decision.
    ///
    /// The reason carries the rendered copy that is shown next to
    /// the confirm/deny buttons; a single rendering layer covers
    /// both sandbox-driven and tool-driven prompts.
    async fn request(&self, reason: ApprovalReason) -> ApprovalDecision;
}

assert_impl_all!(dyn ApprovalChannel: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolContext — per-call plumbing handed to `Tool::execute`. Holding
// it in a struct keeps the signature stable as new plumbing lands.
// ─────────────────────────────────────────────────────────────────────

/// Per-call plumbing the agent loop hands to a tool at execute time.
///
/// Carries the workspace identity, the active sandbox policy, the
/// cancellation handle, and an approval channel. Tools borrow the
/// fields they need; nothing forces a tool to consume every piece.
///
/// Marked `#[non_exhaustive]` so adding a new field (a telemetry
/// handle, a progress sink) stays non-breaking — callers go through
/// [`ToolContext::new`] rather than struct literals.
#[derive(Clone)]
#[non_exhaustive]
pub struct ToolContext {
    /// Workspace handle the tool is operating against. Lightweight
    /// reference — tools fetch the full record only when they need
    /// the on-disk path.
    pub workspace: WorkspaceRef,
    /// Active sandbox policy. Tools call `sandbox.check_*` to gate
    /// their own operations; `sandbox_check` on the trait is the
    /// pre-flight, while in-flight checks happen here.
    pub sandbox: SandboxPolicy,
    /// Cancellation handle. Polled by the tool at yield points to
    /// honour user-driven cancellation.
    pub cancel_token: CancelToken,
    /// Approval channel. Tools call `request_approval.request(...)`
    /// when they need user confirmation outside the static
    /// sandbox check.
    pub request_approval: Arc<dyn ApprovalChannel + Send + Sync>,
}

impl ToolContext {
    /// Construct a context from its four pieces. Pairs with
    /// `#[non_exhaustive]`.
    #[must_use]
    pub fn new(
        workspace: WorkspaceRef,
        sandbox: SandboxPolicy,
        cancel_token: CancelToken,
        request_approval: Arc<dyn ApprovalChannel + Send + Sync>,
    ) -> Self {
        Self {
            workspace,
            sandbox,
            cancel_token,
            request_approval,
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Skip `request_approval` — `dyn ApprovalChannel` does not
        // implement `Debug` and pulling that requirement into the
        // trait would force every implementation to derive it.
        f.debug_struct("ToolContext")
            .field("workspace", &self.workspace)
            .field("sandbox", &self.sandbox)
            .field("cancel_token", &self.cancel_token)
            .finish_non_exhaustive()
    }
}

assert_impl_all!(ToolContext: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolOutcome — what `Tool::execute` returns on success. Three pieces:
// human summary, structured payload, list of effects for the home
// shell.
// ─────────────────────────────────────────────────────────────────────

/// Successful tool result.
///
/// `summary` is the short line shown in the chat transcript and the
/// tool-result rendering. `payload` is the structured value the AI
/// receives back as the tool's `Part::ToolResult`. `effects` is the
/// list of side-effects the home-shell dispatcher applies once the
/// result is delivered.
///
/// Marked `#[non_exhaustive]` so a future PRD (e.g. structured
/// timing data) can add a field without breaking construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolOutcome {
    /// Single-line human summary.
    pub summary: String,
    /// Structured payload returned to the AI.
    pub payload: serde_json::Value,
    /// Side-effects the dispatcher applies. May be empty.
    pub effects: Vec<Effect>,
}

impl ToolOutcome {
    /// Construct an outcome from its three fields.
    #[must_use]
    pub fn new(
        summary: impl Into<String>,
        payload: serde_json::Value,
        effects: Vec<Effect>,
    ) -> Self {
        Self {
            summary: summary.into(),
            payload,
            effects,
        }
    }
}

assert_impl_all!(ToolOutcome: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ToolError / ToolErrorKind — failure surface. `thiserror`-derived
// enum so callers can pattern-match on the kind to drive UI without
// re-parsing a string.
// ─────────────────────────────────────────────────────────────────────

/// Coarse failure category for [`ToolError`].
///
/// Pattern-match on the kind to drive UI rendering and retry logic.
/// Marked `#[non_exhaustive]` so future PRDs can add categories
/// (rate limiting, structured timeouts, …) without breaking
/// matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolErrorKind {
    /// Arguments did not validate against the tool's schema, or
    /// failed a runtime invariant (negative limit, empty path, …).
    InvalidArgs,
    /// The active sandbox policy refused the call. The error
    /// message carries the rendered reason from
    /// [`SandboxDecision::Block`].
    SandboxBlocked,
    /// The cancellation token was tripped during execution.
    Cancelled,
    /// An IO operation underneath the tool failed (file not found,
    /// permission denied, broken pipe, …).
    Io,
    /// A bug in the tool itself — invariant violated, unexpected
    /// state. Distinct from `Io` so the UI can surface "this is our
    /// fault" copy instead of the OS error.
    Internal,
}

assert_impl_all!(ToolErrorKind: Send, Sync);

/// Failure raised from [`Tool::execute`] or [`Tool::sandbox_check`]
/// on the error path.
///
/// The kind drives UI rendering; the message carries the diagnostic
/// detail. `Serialize + Deserialize` so a recorded run survives a
/// trip through telemetry or a snapshot test.
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[error("{kind:?}: {message}")]
pub struct ToolError {
    /// Coarse category — what *kind* of failure this is.
    pub kind: ToolErrorKind,
    /// Human-readable diagnostic.
    pub message: String,
}

impl ToolError {
    /// Construct a tool error from a kind and message.
    #[must_use]
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Convenience for the common "arguments did not validate" path.
    #[must_use]
    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::InvalidArgs, message)
    }

    /// Convenience for translating a [`SandboxDecision::Block`]
    /// into the matching error.
    #[must_use]
    pub fn sandbox_blocked(reason: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::SandboxBlocked, reason)
    }

    /// Convenience for the cancellation path.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(ToolErrorKind::Cancelled, "tool execution cancelled")
    }
}

assert_impl_all!(ToolError: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Tool — the trait every concrete tool implements. `dyn`-compatible
// so the registry can hold `Arc<dyn Tool>` and dispatch by id.
// ─────────────────────────────────────────────────────────────────────

/// The boundary every concrete tool crosses.
///
/// Implementations are expected to be cheap to clone via `Arc`; the
/// registry holds shared handles and dispatches concurrent calls
/// off the same instance. State that varies per call lives in the
/// arguments, not in the tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable identifier the AI references in `Part::ToolCall::name`.
    fn id(&self) -> ToolId;

    /// Schema rendered into the AI provider's function-calling
    /// surface. The agent loop calls this once at registration
    /// time; the result is cached.
    fn schema(&self) -> ToolSchema;

    /// Coarse safety class. Read by [`crate::sandbox::TrustMode`]
    /// to decide whether an approval prompt is required.
    fn safety_class(&self) -> SafetyClass;

    /// Pre-flight the call against the active sandbox policy.
    ///
    /// Returns the decision the runtime acts on:
    /// [`SandboxDecision::Allow`] fires `execute`,
    /// [`SandboxDecision::RequireApproval`] surfaces an approval
    /// card first, [`SandboxDecision::Block`] short-circuits to a
    /// [`ToolErrorKind::SandboxBlocked`] tool result. Tools that
    /// have no static check beyond what the policy already
    /// enforces should return `SandboxDecision::Allow` and rely
    /// on in-flight `sandbox.check_*` calls during execute.
    fn sandbox_check(&self, args: &ToolArgs, sandbox: &SandboxPolicy) -> SandboxDecision;

    /// Run the tool. The runtime guarantees `sandbox_check` was
    /// called first and either returned `Allow` or transitioned
    /// through an approved prompt.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] when arguments do not validate, the
    /// sandbox refuses an in-flight check, the cancellation token
    /// trips, or the underlying operation fails.
    async fn execute(&self, args: ToolArgs, ctx: &ToolContext) -> Result<ToolOutcome, ToolError>;
}

// `dyn Tool: Send + Sync` so the registry can store the trait object
// behind an `Arc`. If a future change to the trait inadvertently
// breaks `dyn`-compatibility, this assertion fails to compile and
// tells us at the trait-surface level rather than at the call site.
assert_impl_all!(dyn Tool: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// MockTool — `test-support`-gated trivial implementation. Proves the
// trait shape compiles end-to-end and gives downstream crates a
// drop-in fixture for compile-time wiring tests. The full builder
// lands in PRD-16; this is the smoke-test impl.
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "test-support")]
mod mock {
    use super::{
        SafetyClass, SandboxDecision, SandboxPolicy, Tool, ToolArgs, ToolContext, ToolError,
        ToolId, ToolOutcome, ToolSchema,
    };
    use async_trait::async_trait;
    use static_assertions::assert_impl_all;

    /// Trivial [`Tool`] implementation used by downstream crates to
    /// verify their wiring compiles against the real trait. Returns
    /// a fixed [`ToolOutcome`] regardless of input.
    ///
    /// Gated behind the `test-support` feature so release builds
    /// never carry the fixture. The full builder (configurable
    /// outcomes, recording, scripted failures) lands in PRD-16;
    /// this is the smoke-test impl.
    #[derive(Debug, Clone)]
    pub struct MockTool {
        id: ToolId,
        outcome: ToolOutcome,
    }

    impl MockTool {
        /// Construct a mock with a fixed id and a fixed outcome
        /// returned from every `execute` call.
        #[must_use]
        pub fn new(id: ToolId, outcome: ToolOutcome) -> Self {
            Self { id, outcome }
        }
    }

    #[async_trait]
    impl Tool for MockTool {
        fn id(&self) -> ToolId {
            self.id.clone()
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                self.id.as_str(),
                "Mock tool used by test fixtures.",
                serde_json::json!({ "type": "object", "properties": {} }),
            )
        }

        fn safety_class(&self) -> SafetyClass {
            SafetyClass::Safe
        }

        fn sandbox_check(&self, _args: &ToolArgs, _sandbox: &SandboxPolicy) -> SandboxDecision {
            SandboxDecision::Allow
        }

        async fn execute(
            &self,
            _args: ToolArgs,
            _ctx: &ToolContext,
        ) -> Result<ToolOutcome, ToolError> {
            Ok(self.outcome.clone())
        }
    }

    assert_impl_all!(MockTool: Send, Sync);
}

#[cfg(feature = "test-support")]
pub use mock::MockTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::WorkspaceId;
    use crate::sandbox::TrustMode;
    use serde_json::json;

    fn sample_outcome() -> ToolOutcome {
        ToolOutcome::new(
            "did the thing",
            json!({ "ok": true, "count": 3 }),
            vec![Effect::EmitNotification {
                level: crate::effect::NotificationLevel::Info,
                message: "done".to_string(),
            }],
        )
    }

    /// Round-trips a populated `ToolOutcome`. Acceptance criteria
    /// require this to lock the wire shape so snapshot logging in
    /// later phases survives the JSON hop.
    #[test]
    fn tool_outcome_round_trips_through_json() {
        let original = sample_outcome();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: ToolOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `ToolArgs` uses `#[serde(transparent)]` so the wire form is
    /// the bare value. Lock that — it is what every tool's input
    /// path depends on.
    #[test]
    fn tool_args_wire_form_is_the_bare_value() {
        let args = ToolArgs::new(json!({ "path": "x.rs" }));
        let json_str = serde_json::to_string(&args).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("parse");
        assert_eq!(parsed, json!({ "path": "x.rs" }));
    }

    /// `ToolId` uses `#[serde(transparent)]` so the wire form is a
    /// bare string. Locked here so a Rust rename refactor cannot
    /// silently switch to a wrapper object.
    #[test]
    fn tool_id_wire_form_is_a_bare_string() {
        let id = ToolId::new("apply_edits");
        let json_str = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json_str, "\"apply_edits\"");
    }

    /// `ToolError`'s `Display` glues kind and message together. Lock
    /// the format so log readers and error UIs do not silently drift.
    #[test]
    fn tool_error_display_contains_kind_and_message() {
        let err = ToolError::invalid_args("missing path");
        let rendered = err.to_string();
        assert!(rendered.contains("InvalidArgs"));
        assert!(rendered.contains("missing path"));
    }

    /// `CancelToken` flips once and stays flipped. The agent loop
    /// relies on this monotonicity — there is no "uncancel" path.
    #[test]
    fn cancel_token_flips_monotonically() {
        let token = CancelToken::new();
        assert!(!token.cancelled());
        token.cancel();
        assert!(token.cancelled());
        token.cancel(); // idempotent
        assert!(token.cancelled());
    }

    /// Cloning a `CancelToken` shares the flag — a clone observes
    /// cancellation requested on the original. This is the contract
    /// the agent loop relies on when handing a token to a spawned
    /// task.
    #[test]
    fn cancel_token_clone_shares_state() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.cancelled());
        a.cancel();
        assert!(b.cancelled());
    }

    /// Trivial [`ApprovalChannel`] used to prove the trait is
    /// `dyn`-compatible and that an `Arc<dyn ApprovalChannel + Send
    /// + Sync>` builds.
    struct AlwaysApprove;

    #[async_trait]
    impl ApprovalChannel for AlwaysApprove {
        async fn request(&self, _reason: ApprovalReason) -> ApprovalDecision {
            ApprovalDecision::Approved
        }
    }

    #[test]
    fn approval_channel_is_dyn_compatible() {
        let channel: Arc<dyn ApprovalChannel + Send + Sync> = Arc::new(AlwaysApprove);
        let decision =
            futures::executor::block_on(channel.request(ApprovalReason::new("just checking")));
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    /// Tiny `Tool` implementation that proves the trait surface
    /// compiles end-to-end without `test-support`. The full
    /// `MockTool` lives behind the feature gate; this in-test
    /// double is enough to exercise the trait at unit-test time.
    struct SmokeTool;

    #[async_trait]
    impl Tool for SmokeTool {
        fn id(&self) -> ToolId {
            ToolId::new("smoke")
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "smoke",
                "Smoke-test tool",
                json!({ "type": "object", "properties": {} }),
            )
        }

        fn safety_class(&self) -> SafetyClass {
            SafetyClass::Safe
        }

        fn sandbox_check(&self, _args: &ToolArgs, _sandbox: &SandboxPolicy) -> SandboxDecision {
            SandboxDecision::Allow
        }

        async fn execute(
            &self,
            _args: ToolArgs,
            _ctx: &ToolContext,
        ) -> Result<ToolOutcome, ToolError> {
            Ok(sample_outcome())
        }
    }

    fn sample_context() -> ToolContext {
        ToolContext::new(
            WorkspaceRef::new(WorkspaceId::new_v4(), "Example".to_string()),
            SandboxPolicy::with_trust_mode(TrustMode::Guarded),
            CancelToken::new(),
            Arc::new(AlwaysApprove),
        )
    }

    /// Exercises the `Tool` trait through `dyn` dispatch — proves
    /// `dyn Tool: Send + Sync` is buildable and that
    /// `execute` resolves through the trait object.
    #[test]
    fn tool_trait_is_dyn_compatible_and_executes() {
        let tool: Arc<dyn Tool + Send + Sync> = Arc::new(SmokeTool);
        let ctx = sample_context();
        let outcome = futures::executor::block_on(tool.execute(ToolArgs::new(json!({})), &ctx))
            .expect("smoke execute");
        assert_eq!(outcome, sample_outcome());
        assert_eq!(tool.id(), ToolId::new("smoke"));
        assert_eq!(tool.safety_class(), SafetyClass::Safe);
    }

    /// `ToolErrorKind` is `#[non_exhaustive]`. Lock the wildcard-arm
    /// contract the same way the AI-stream tests do.
    #[test]
    fn tool_error_kind_match_uses_wildcard_arm() {
        let kinds = [
            ToolErrorKind::InvalidArgs,
            ToolErrorKind::SandboxBlocked,
            ToolErrorKind::Cancelled,
            ToolErrorKind::Io,
            ToolErrorKind::Internal,
        ];
        for k in kinds {
            #[allow(unreachable_patterns)]
            let label = match k {
                ToolErrorKind::InvalidArgs => "args",
                ToolErrorKind::SandboxBlocked => "sandbox",
                ToolErrorKind::Cancelled => "cancel",
                ToolErrorKind::Io => "io",
                ToolErrorKind::Internal => "internal",
                _ => "unknown",
            };
            assert!(matches!(
                label,
                "args" | "sandbox" | "cancel" | "io" | "internal"
            ));
        }
    }
}
