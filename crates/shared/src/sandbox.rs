//! Sandbox policy and decision types.
//!
//! The sandbox is the security backbone every tool call runs through:
//! it decides whether a filesystem read, a write, a terminal command,
//! or an outbound network connection is allowed, requires user
//! approval, or is blocked outright. The shape lives here in the
//! Domain layer so tools can be authored without depending on a UI
//! runtime, and so the same policy round-trips cleanly through
//! persistence (PRD-03).
//!
//! # Public surface
//!
//! - [`TrustMode`] — three coarse modes the user picks from
//!   (`Guarded`, `Fast`, `Yolo`). Each materialises a concrete
//!   [`PermissionPolicy`] via [`PermissionPolicy::from_trust_mode`].
//! - [`PermissionPolicy`] — per-operation [`AccessLevel`] for `read`,
//!   `write`, `terminal`, `network`. This is the field the runtime
//!   consults for the gate decision.
//! - [`NetworkPattern`] — host/port glob (`*`, `*.example.com`,
//!   `localhost:*`, `api.example.com:443`). Naive string matching is
//!   sufficient for this slice; production-grade matching (CIDR, IPv6)
//!   can layer on later behind the same `matches` API.
//! - [`EnvAction`] — per-environment-variable allow / block / prompt
//!   directive used when materialising a child process environment.
//! - [`SandboxPolicy`] — the entry point. Holds the trust mode, the
//!   permission policy, the network allowlist, and the env actions,
//!   and exposes `check_*` methods that produce a [`SandboxDecision`].
//! - [`SandboxDecision`] — `Allow`, `RequireApproval(ApprovalReason)`,
//!   or `Block(reason)`. The `Block` reason is human-readable so the
//!   UI and the AI tool-result can both surface the same string.
//!
//! # Behaviour contract
//!
//! 1. `TrustMode::Guarded` defaults to `RequireApproval` for write,
//!    terminal, and network; `Allow` for read.
//! 2. `TrustMode::Fast` allows read and write, requires approval for
//!    terminal and network. (Workspace-scoped path checks land in a
//!    later slice — see the TODO on [`SandboxPolicy::check_write`].)
//! 3. `TrustMode::Yolo` allows everything that is not explicitly
//!    blocked. There is no internal blocklist.
//! 4. The runtime always reads from `policy.permissions`. Trust mode
//!    only seeds the policy via [`PermissionPolicy::from_trust_mode`];
//!    callers are free to override any operation independently.
//! 5. Decisions are deterministic given the same `(policy, operation)`
//!    pair — no hidden time-, environment-, or randomness-driven
//!    branches. The matrix test in this module pins that behaviour.

use std::path::Path;

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

// ─────────────────────────────────────────────────────────────────────
// SafetyClass — re-exported from the canonical home in `tool`. Slice 6
// (#32) relocated the type to `crate::tool` and turned this slot into
// a re-export so existing call sites that import from `sandbox` keep
// working without churn. New code is expected to import directly
// from `crate::tool`.
// ─────────────────────────────────────────────────────────────────────

pub use crate::tool::SafetyClass;

// ─────────────────────────────────────────────────────────────────────
// ApprovalMode / ExecutionStyle — small enums consumed by TrustMode
// helpers. They describe *how* the runtime treats a tool call rather
// than *whether* it is allowed.
// ─────────────────────────────────────────────────────────────────────

/// How aggressively the runtime should ask the user before firing a
/// tool call. Returned by [`TrustMode::approval_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalMode {
    /// Never prompt — fire the call straight through.
    Never,
    /// Prompt only when the call is classified as
    /// [`SafetyClass::Destructive`].
    OnDestructive,
    /// Always prompt — every call gates on user confirmation.
    Always,
}

assert_impl_all!(ApprovalMode: Send, Sync);

/// How a sequence of tool calls is sequenced relative to user input.
/// Returned by [`TrustMode::execution_style`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStyle {
    /// Each call yields control to the user before the next one.
    Interactive,
    /// Calls run back-to-back without a hand-back to the user
    /// in between.
    Batched,
}

assert_impl_all!(ExecutionStyle: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// TrustMode — the user-facing knob. The three modes are deliberately
// coarse; finer-grained tuning happens via PermissionPolicy.
// ─────────────────────────────────────────────────────────────────────

/// Coarse trust dial the user picks from in the workspace settings.
///
/// `Guarded` is the safe default: the assistant can read freely but
/// every side-effecting call gates on approval. `Fast` relaxes
/// reads/writes for the hot loop while keeping the dangerous edges
/// (terminal, network) gated. `Yolo` waives every gate — by design,
/// per the locked Q20 PATH 2 decision, there is no internal
/// blocklist that overrides `Yolo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustMode {
    /// Approval required for every side-effecting call.
    Guarded,
    /// Reads and writes flow without approval; terminal and network
    /// still gate.
    Fast,
    /// Everything is allowed. No internal blocklist.
    Yolo,
}

impl TrustMode {
    /// Map a `(TrustMode, SafetyClass)` pair to the approval policy
    /// the runtime should apply.
    ///
    /// `Yolo` collapses to [`ApprovalMode::Never`] regardless of the
    /// safety class — that is the contract of the mode. `Guarded`
    /// always asks for destructive and approval-required classes;
    /// `Safe` reads through `Guarded` still flow, falling under
    /// `OnDestructive` so the UI can keep its uniform "ask for the
    /// scary stuff" rendering.
    #[must_use]
    pub fn approval_for(self, safety_class: SafetyClass) -> ApprovalMode {
        match (self, safety_class) {
            (TrustMode::Yolo, _) => ApprovalMode::Never,
            (TrustMode::Guarded, SafetyClass::Safe) => ApprovalMode::OnDestructive,
            (TrustMode::Guarded, _) => ApprovalMode::Always,
            (TrustMode::Fast, SafetyClass::Destructive) => ApprovalMode::Always,
            (TrustMode::Fast, SafetyClass::RequiresApproval) => ApprovalMode::OnDestructive,
            (TrustMode::Fast, SafetyClass::Safe) => ApprovalMode::Never,
        }
    }

    /// Whether tool calls under this mode hand control back to the
    /// user between invocations or batch.
    #[must_use]
    pub fn execution_style(self) -> ExecutionStyle {
        match self {
            TrustMode::Guarded => ExecutionStyle::Interactive,
            TrustMode::Fast | TrustMode::Yolo => ExecutionStyle::Batched,
        }
    }

    /// Always returns `false` per the locked Q20 PATH 2 decision —
    /// the sandbox carries no internal blocklist. The method exists
    /// so consumers can write `if mode.enforces_blocklist() { … }`
    /// today and have the call site survive a future PRD that may
    /// reintroduce one without changing the public surface.
    #[must_use]
    pub fn enforces_blocklist(self) -> bool {
        false
    }
}

assert_impl_all!(TrustMode: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AccessLevel / PermissionPolicy — the per-operation dials the runtime
// actually consults at gate time.
// ─────────────────────────────────────────────────────────────────────

/// Per-operation gate. Read by [`SandboxPolicy::check_read`] and
/// friends to produce a [`SandboxDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessLevel {
    /// Allow without prompting.
    Allow,
    /// Prompt the user; outcome depends on the answer.
    RequireApproval,
    /// Refuse the operation. The runtime surfaces a Block decision
    /// with a human-readable reason.
    Block,
}

assert_impl_all!(AccessLevel: Send, Sync);

/// Composite policy describing the per-operation defaults the
/// sandbox enforces.
///
/// Each field is independent. `from_trust_mode` seeds a policy from a
/// coarse [`TrustMode`]; callers can then override any field to make
/// the policy stricter (`AccessLevel::Block`) or more permissive
/// (`AccessLevel::Allow`) without going through the trust-mode dial.
///
/// Marked `#[non_exhaustive]` so adding a new operation column —
/// say a `process_spawn` field — in a later PRD stays non-breaking
/// for downstream construction sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionPolicy {
    /// Filesystem reads.
    pub read: AccessLevel,
    /// Filesystem writes (create, modify, delete).
    pub write: AccessLevel,
    /// Terminal / shell command execution.
    pub terminal: AccessLevel,
    /// Outbound network access (any host/port the allowlist does
    /// not already cover).
    pub network: AccessLevel,
}

impl PermissionPolicy {
    /// Materialise the default permission shape implied by a
    /// [`TrustMode`].
    ///
    /// `Guarded` allows reads and gates everything else. `Fast`
    /// allows reads and writes while gating terminal and network.
    /// `Yolo` allows everything. These defaults are exactly what the
    /// behaviour contract in the module-level docs promises; the
    /// matrix test pins that mapping cell-by-cell.
    #[must_use]
    pub fn from_trust_mode(mode: TrustMode) -> Self {
        match mode {
            TrustMode::Guarded => Self {
                read: AccessLevel::Allow,
                write: AccessLevel::RequireApproval,
                terminal: AccessLevel::RequireApproval,
                network: AccessLevel::RequireApproval,
            },
            TrustMode::Fast => Self {
                read: AccessLevel::Allow,
                write: AccessLevel::Allow,
                terminal: AccessLevel::RequireApproval,
                network: AccessLevel::RequireApproval,
            },
            TrustMode::Yolo => Self {
                read: AccessLevel::Allow,
                write: AccessLevel::Allow,
                terminal: AccessLevel::Allow,
                network: AccessLevel::Allow,
            },
        }
    }

    /// Construct a policy from explicit per-operation levels. Pairs
    /// with `#[non_exhaustive]`: callers go through this method
    /// rather than struct literals so adding a new field stays
    /// non-breaking.
    #[must_use]
    pub fn new(
        read: AccessLevel,
        write: AccessLevel,
        terminal: AccessLevel,
        network: AccessLevel,
    ) -> Self {
        Self {
            read,
            write,
            terminal,
            network,
        }
    }
}

assert_impl_all!(PermissionPolicy: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// NetworkPattern — naive host/port glob matcher. The grammar is
// host[:port], where host is `*`, `*.suffix`, or an exact label, and
// port is `*` or a u16. CIDR ranges and IPv6 brackets are explicitly
// out of scope for this slice.
// ─────────────────────────────────────────────────────────────────────

/// Host portion of a [`NetworkPattern`].
///
/// `Any` is the bare `*`; `Suffix("example.com")` corresponds to the
/// pattern `*.example.com` and matches strict subdomains only — the
/// bare `example.com` does *not* match, by design, so allowlists do
/// not silently widen. `Exact` matches a single host string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostPattern {
    /// `*` — any host.
    Any,
    /// `*.suffix` — strict subdomain match against `suffix`.
    Suffix(String),
    /// Exact host string.
    Exact(String),
}

assert_impl_all!(HostPattern: Send, Sync);

/// Port portion of a [`NetworkPattern`]. Either any port or an
/// exact `u16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortPattern {
    /// `*` — any port.
    Any,
    /// Exact port number.
    Exact(u16),
}

assert_impl_all!(PortPattern: Send, Sync);

/// Allowlist entry for outbound network access.
///
/// Built from the `host[:port]` mini-grammar via
/// [`NetworkPattern::parse`]. The matcher is intentionally naive
/// string handling — sufficient to express "any host", "any
/// subdomain of example.com", "localhost on any port", and exact
/// `host:port` pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NetworkPattern {
    /// Host portion.
    pub host: HostPattern,
    /// Port portion. `Any` when the source string omits the port or
    /// uses the literal `*`.
    pub port: PortPattern,
}

/// Parse failure for [`NetworkPattern::parse`]. The inner string
/// describes the offending input verbatim so error messages stay
/// debuggable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPatternParseError {
    raw: String,
    reason: &'static str,
}

impl std::fmt::Display for NetworkPatternParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid network pattern {:?}: {}", self.raw, self.reason)
    }
}

impl std::error::Error for NetworkPatternParseError {}

assert_impl_all!(NetworkPatternParseError: Send, Sync);

impl NetworkPattern {
    /// Construct from explicit host and port shapes.
    #[must_use]
    pub fn new(host: HostPattern, port: PortPattern) -> Self {
        Self { host, port }
    }

    /// Parse from `host[:port]`.
    ///
    /// Accepted host forms: `*`, `*.suffix`, `exact-host`. Accepted
    /// port forms: `*` or a `u16`. A missing `:port` defaults to
    /// `PortPattern::Any` so `*.example.com` covers every port.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkPatternParseError`] for empty hosts, empty
    /// suffixes (`*.`), or non-numeric ports.
    pub fn parse(pattern: &str) -> Result<Self, NetworkPatternParseError> {
        let (host_str, port_str) = match pattern.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => (pattern, "*"),
        };

        let host = if host_str == "*" {
            HostPattern::Any
        } else if let Some(suffix) = host_str.strip_prefix("*.") {
            if suffix.is_empty() {
                return Err(NetworkPatternParseError {
                    raw: pattern.to_string(),
                    reason: "empty suffix after `*.`",
                });
            }
            HostPattern::Suffix(suffix.to_string())
        } else if host_str.is_empty() {
            return Err(NetworkPatternParseError {
                raw: pattern.to_string(),
                reason: "empty host",
            });
        } else {
            HostPattern::Exact(host_str.to_string())
        };

        let port = if port_str == "*" {
            PortPattern::Any
        } else {
            let parsed = port_str
                .parse::<u16>()
                .map_err(|_| NetworkPatternParseError {
                    raw: pattern.to_string(),
                    reason: "port is not a u16",
                })?;
            PortPattern::Exact(parsed)
        };

        Ok(Self { host, port })
    }

    /// Naive `(host, port)` match. Strict-subdomain semantics for
    /// `Suffix`: `*.example.com` matches `api.example.com` but not
    /// the bare `example.com`.
    #[must_use]
    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host_ok = match &self.host {
            HostPattern::Any => true,
            HostPattern::Exact(h) => h == host,
            HostPattern::Suffix(suffix) => {
                // Strict subdomain: host must end with `.<suffix>`
                // and the dot-prefixed needle must be shorter than
                // the host (so the bare suffix itself fails).
                let needle = format!(".{suffix}");
                host.ends_with(&needle) && host.len() > needle.len()
            }
        };
        let port_ok = match self.port {
            PortPattern::Any => true,
            PortPattern::Exact(p) => p == port,
        };
        host_ok && port_ok
    }
}

assert_impl_all!(NetworkPattern: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// EnvVarName / EnvAction — directives applied when materialising a
// child process environment. The action variants carry the variable
// name so a flat `Vec<EnvAction>` can express "allow PATH, prompt for
// AWS_PROFILE, block ANTHROPIC_API_KEY" without a parallel index.
// ─────────────────────────────────────────────────────────────────────

/// Newtype over the *name* of an environment variable. Wraps a
/// `String` rather than a borrowed slice so the value can travel
/// through serde and across thread boundaries without lifetime gymnastics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvVarName(String);

impl EnvVarName {
    /// Wrap a string. No validation in this slice; environment
    /// variable name rules are platform-specific and we leave that
    /// to the layer that actually spawns processes.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrow the underlying name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

assert_impl_all!(EnvVarName: Send, Sync);

/// Directive describing how a single environment variable should be
/// handled when constructing the child process environment for a
/// terminal or tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvAction {
    /// Pass through to the child process.
    Allow(EnvVarName),
    /// Strip from the child process environment.
    Block(EnvVarName),
    /// Prompt the user before deciding.
    Prompt(EnvVarName),
}

assert_impl_all!(EnvAction: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// SandboxDecision / ApprovalReason — the gate output. RequireApproval
// carries a structured reason so the UI and the AI tool-result render
// the same prompt.
// ─────────────────────────────────────────────────────────────────────

/// Human-readable explanation attached to a `RequireApproval`
/// decision. `summary` is the short line shown next to the prompt
/// button; `detail` carries optional long-form context for the
/// expanded view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApprovalReason {
    /// Single-line summary suitable for a button row.
    pub summary: String,
    /// Optional long-form detail for the expanded approval card.
    pub detail: Option<String>,
}

impl ApprovalReason {
    /// Build a reason with summary only.
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            detail: None,
        }
    }

    /// Build a reason with both summary and detail.
    #[must_use]
    pub fn with_detail(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            detail: Some(detail.into()),
        }
    }
}

assert_impl_all!(ApprovalReason: Send, Sync);

/// Outcome of a `SandboxPolicy::check_*` call.
///
/// `Block` carries a single `String` — the human-readable reason —
/// because it surfaces in two places that share the same copy: the
/// approval card the user sees and the tool result the model reads.
/// Splitting it into structured fields would either duplicate the
/// rendering logic or force one consumer to re-stringify the other's
/// shape, so we keep it pre-rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxDecision {
    /// Operation may proceed without prompting.
    Allow,
    /// Operation must surface to the user; the reason carries the
    /// rendered prompt copy.
    RequireApproval(ApprovalReason),
    /// Operation refused. The string is the rendered reason.
    Block(String),
}

impl SandboxDecision {
    /// Convenience for the common "is this just a green light"
    /// branch in tool plumbing.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

assert_impl_all!(SandboxDecision: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// SandboxPolicy — the entry point. Holds the four configuration
// shapes and exposes the four `check_*` methods every tool plumbs
// through.
// ─────────────────────────────────────────────────────────────────────

/// Top-level policy a workspace exposes to the runtime.
///
/// Constructed via [`SandboxPolicy::with_trust_mode`] for the common
/// case (a trust-mode preset with empty allowlists / env actions),
/// or field-by-field via [`SandboxPolicy::new`] for the rare case a
/// caller wants finer control up front.
///
/// Marked `#[non_exhaustive]` for the same reason as
/// [`PermissionPolicy`]: future PRDs can attach additional
/// configuration (a workspace root for path-aware checks, a
/// process-spawn allowlist, …) without breaking downstream
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SandboxPolicy {
    /// Coarse trust dial. Persisted alongside the policy so the UI
    /// can render the user's last selection without inferring it
    /// from the permission shape.
    pub trust_mode: TrustMode,
    /// Per-operation gates the runtime actually consults.
    pub permissions: PermissionPolicy,
    /// Hosts/ports that bypass `permissions.network` and resolve to
    /// `Allow` regardless of trust mode. Empty by default.
    pub network_allowlist: Vec<NetworkPattern>,
    /// Per-variable directives applied when materialising a child
    /// process environment. Empty by default.
    pub env_actions: Vec<EnvAction>,
}

impl SandboxPolicy {
    /// Build a policy from a [`TrustMode`] preset. Empty allowlists,
    /// no env actions. The permission shape is exactly what
    /// [`PermissionPolicy::from_trust_mode`] returns.
    #[must_use]
    pub fn with_trust_mode(trust_mode: TrustMode) -> Self {
        Self {
            trust_mode,
            permissions: PermissionPolicy::from_trust_mode(trust_mode),
            network_allowlist: Vec::new(),
            env_actions: Vec::new(),
        }
    }

    /// Build a policy from explicit field values. Pairs with
    /// `#[non_exhaustive]`.
    #[must_use]
    pub fn new(
        trust_mode: TrustMode,
        permissions: PermissionPolicy,
        network_allowlist: Vec<NetworkPattern>,
        env_actions: Vec<EnvAction>,
    ) -> Self {
        Self {
            trust_mode,
            permissions,
            network_allowlist,
            env_actions,
        }
    }

    /// Gate a filesystem read.
    ///
    /// Today the path is unused — the runtime treats reads
    /// uniformly. The argument is in the signature so the
    /// future workspace-aware slice can plumb path scoping (e.g.
    /// "outside the workspace requires approval") without churning
    /// every call site.
    #[must_use]
    pub fn check_read(&self, _path: &Path) -> SandboxDecision {
        access_decision(self.permissions.read, Operation::Read)
    }

    /// Gate a filesystem write.
    ///
    /// Same path-argument rationale as [`SandboxPolicy::check_read`].
    /// The contract that `Fast` allows writes "inside the workspace"
    /// collapses to "allows writes" in this slice; the path-aware
    /// refinement lands when the workspace root is threaded through
    /// to the policy.
    // TODO(workspace-scope): tighten Fast write to inside-workspace
    // only once the policy carries a workspace root.
    #[must_use]
    pub fn check_write(&self, _path: &Path) -> SandboxDecision {
        access_decision(self.permissions.write, Operation::Write)
    }

    /// Gate a terminal command. The command is unused at this layer;
    /// the runtime can layer command-pattern matching on top of the
    /// base decision in a later slice.
    #[must_use]
    pub fn check_terminal(&self, _command: &str) -> SandboxDecision {
        access_decision(self.permissions.terminal, Operation::Terminal)
    }

    /// Gate an outbound network access. The allowlist short-circuits
    /// to `Allow` when any pattern matches — that is the entire
    /// reason the allowlist exists. Otherwise the decision falls
    /// through to `permissions.network`.
    #[must_use]
    pub fn check_network(&self, host: &str, port: u16) -> SandboxDecision {
        if self
            .network_allowlist
            .iter()
            .any(|pattern| pattern.matches(host, port))
        {
            return SandboxDecision::Allow;
        }
        access_decision(self.permissions.network, Operation::Network)
    }
}

assert_impl_all!(SandboxPolicy: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Internal: shared decision-rendering helper. Centralised so the
// approval-reason and block-reason copy stay identical across the
// four `check_*` entry points and any later additions.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Operation {
    Read,
    Write,
    Terminal,
    Network,
}

impl Operation {
    fn label(self) -> &'static str {
        match self {
            Operation::Read => "filesystem read",
            Operation::Write => "filesystem write",
            Operation::Terminal => "terminal command",
            Operation::Network => "network access",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Write => "write",
            Operation::Terminal => "terminal",
            Operation::Network => "network",
        }
    }
}

fn access_decision(level: AccessLevel, op: Operation) -> SandboxDecision {
    match level {
        AccessLevel::Allow => SandboxDecision::Allow,
        AccessLevel::RequireApproval => {
            SandboxDecision::RequireApproval(ApprovalReason::with_detail(
                format!("approval required for {}", op.short()),
                format!("policy gates {} on user confirmation", op.label()),
            ))
        }
        AccessLevel::Block => {
            SandboxDecision::Block(format!("{} blocked by sandbox policy", op.label()))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests. Organised by surface so a regression points at the right
// concept: trust-mode helpers, permission-policy seeds, the matrix,
// allowlist short-circuit, network parsing/matching, and serde shape.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── TrustMode helpers ─────────────────────────────────────────

    #[test]
    fn trust_mode_enforces_blocklist_is_always_false() {
        // Locked behaviour from issue #29's contract — Q20 PATH 2.
        // If this ever flips to `true` it must come with a PRD
        // change, not an accidental tweak.
        for mode in [TrustMode::Guarded, TrustMode::Fast, TrustMode::Yolo] {
            assert!(!mode.enforces_blocklist(), "{mode:?}");
        }
    }

    #[test]
    fn trust_mode_yolo_collapses_approval_for_every_safety_class() {
        for class in [
            SafetyClass::Safe,
            SafetyClass::RequiresApproval,
            SafetyClass::Destructive,
        ] {
            assert_eq!(
                TrustMode::Yolo.approval_for(class),
                ApprovalMode::Never,
                "Yolo + {class:?}"
            );
        }
    }

    #[test]
    fn trust_mode_guarded_always_asks_for_destructive_and_approval_classes() {
        assert_eq!(
            TrustMode::Guarded.approval_for(SafetyClass::Destructive),
            ApprovalMode::Always,
        );
        assert_eq!(
            TrustMode::Guarded.approval_for(SafetyClass::RequiresApproval),
            ApprovalMode::Always,
        );
        assert_eq!(
            TrustMode::Guarded.approval_for(SafetyClass::Safe),
            ApprovalMode::OnDestructive,
        );
    }

    #[test]
    fn trust_mode_execution_style() {
        assert_eq!(
            TrustMode::Guarded.execution_style(),
            ExecutionStyle::Interactive
        );
        assert_eq!(TrustMode::Fast.execution_style(), ExecutionStyle::Batched);
        assert_eq!(TrustMode::Yolo.execution_style(), ExecutionStyle::Batched);
    }

    // ── 12-cell permission matrix (3 modes × 4 ops) ───────────────

    /// Spelled out cell-by-cell so a failure points at exactly which
    /// `(mode, operation)` pair regressed rather than reporting a
    /// generic loop iteration index.
    #[test]
    fn permission_matrix_guarded_read_allows() {
        assert_eq!(
            policy(TrustMode::Guarded).check_read(&path()),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_guarded_write_requires_approval() {
        assert!(matches!(
            policy(TrustMode::Guarded).check_write(&path()),
            SandboxDecision::RequireApproval(_)
        ));
    }
    #[test]
    fn permission_matrix_guarded_terminal_requires_approval() {
        assert!(matches!(
            policy(TrustMode::Guarded).check_terminal("ls"),
            SandboxDecision::RequireApproval(_)
        ));
    }
    #[test]
    fn permission_matrix_guarded_network_requires_approval() {
        assert!(matches!(
            policy(TrustMode::Guarded).check_network("api.example.com", 443),
            SandboxDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn permission_matrix_fast_read_allows() {
        assert_eq!(
            policy(TrustMode::Fast).check_read(&path()),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_fast_write_allows() {
        assert_eq!(
            policy(TrustMode::Fast).check_write(&path()),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_fast_terminal_requires_approval() {
        assert!(matches!(
            policy(TrustMode::Fast).check_terminal("ls"),
            SandboxDecision::RequireApproval(_)
        ));
    }
    #[test]
    fn permission_matrix_fast_network_requires_approval() {
        assert!(matches!(
            policy(TrustMode::Fast).check_network("api.example.com", 443),
            SandboxDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn permission_matrix_yolo_read_allows() {
        assert_eq!(
            policy(TrustMode::Yolo).check_read(&path()),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_yolo_write_allows() {
        assert_eq!(
            policy(TrustMode::Yolo).check_write(&path()),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_yolo_terminal_allows() {
        assert_eq!(
            policy(TrustMode::Yolo).check_terminal("rm -rf /tmp/x"),
            SandboxDecision::Allow
        );
    }
    #[test]
    fn permission_matrix_yolo_network_allows() {
        assert_eq!(
            policy(TrustMode::Yolo).check_network("api.example.com", 443),
            SandboxDecision::Allow
        );
    }

    fn policy(mode: TrustMode) -> SandboxPolicy {
        SandboxPolicy::with_trust_mode(mode)
    }

    fn path() -> PathBuf {
        PathBuf::from("/workspace/file.txt")
    }

    // ── PermissionPolicy override (both directions) ───────────────

    #[test]
    fn permission_policy_override_makes_policy_more_strict() {
        // Yolo seeds Allow everywhere; tightening write to Block
        // must surface as Block, proving permissions wins over the
        // mode default.
        let mut p = SandboxPolicy::with_trust_mode(TrustMode::Yolo);
        p.permissions.write = AccessLevel::Block;
        match p.check_write(&path()) {
            SandboxDecision::Block(reason) => {
                assert!(reason.contains("filesystem write"), "reason: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn permission_policy_override_makes_policy_more_permissive() {
        // Guarded seeds RequireApproval for terminal; flipping to
        // Allow must surface as Allow.
        let mut p = SandboxPolicy::with_trust_mode(TrustMode::Guarded);
        p.permissions.terminal = AccessLevel::Allow;
        assert_eq!(p.check_terminal("ls"), SandboxDecision::Allow);
    }

    // ── Determinism ────────────────────────────────────────────────

    #[test]
    fn check_calls_are_deterministic_for_the_same_inputs() {
        let p = SandboxPolicy::with_trust_mode(TrustMode::Guarded);
        let a = p.check_network("api.example.com", 443);
        let b = p.check_network("api.example.com", 443);
        assert_eq!(a, b);
    }

    // ── Network allowlist short-circuit ───────────────────────────

    #[test]
    fn network_allowlist_short_circuits_permissions() {
        // Permissions block the world, but the allowlist must still
        // win for matching entries — that's what makes an allowlist
        // useful.
        let p = SandboxPolicy::new(
            TrustMode::Guarded,
            PermissionPolicy::new(
                AccessLevel::Block,
                AccessLevel::Block,
                AccessLevel::Block,
                AccessLevel::Block,
            ),
            vec![NetworkPattern::parse("api.example.com:443").expect("parse")],
            Vec::new(),
        );
        assert_eq!(
            p.check_network("api.example.com", 443),
            SandboxDecision::Allow
        );
        // Non-matching host falls through to the underlying Block.
        assert!(matches!(
            p.check_network("other.example.com", 443),
            SandboxDecision::Block(_)
        ));
    }

    // ── NetworkPattern parse + match matrix ───────────────────────

    #[test]
    fn network_pattern_exact_host_and_port() {
        let p = NetworkPattern::parse("api.example.com:443").expect("parse");
        assert!(p.matches("api.example.com", 443));
        assert!(!p.matches("api.example.com", 80));
        assert!(!p.matches("other.example.com", 443));
    }

    #[test]
    fn network_pattern_wildcard_subdomain_strict() {
        let p = NetworkPattern::parse("*.example.com").expect("parse");
        assert!(p.matches("api.example.com", 443));
        assert!(p.matches("a.b.example.com", 80));
        // Bare suffix is *not* matched — that is the strict-subdomain
        // contract documented on `HostPattern::Suffix`.
        assert!(!p.matches("example.com", 443));
        assert!(!p.matches("notexample.com", 443));
    }

    #[test]
    fn network_pattern_port_wildcard() {
        let p = NetworkPattern::parse("localhost:*").expect("parse");
        assert!(p.matches("localhost", 1));
        assert!(p.matches("localhost", 65535));
        assert!(!p.matches("127.0.0.1", 80));
    }

    #[test]
    fn network_pattern_full_wildcard() {
        let p = NetworkPattern::parse("*").expect("parse");
        assert!(p.matches("anything.example.com", 443));
        assert!(p.matches("localhost", 0));
    }

    #[test]
    fn network_pattern_missing_port_defaults_to_any() {
        // `host` without `:port` matches every port — convenient for
        // allowlist authors who rarely care about the port column.
        let p = NetworkPattern::parse("api.example.com").expect("parse");
        assert!(p.matches("api.example.com", 443));
        assert!(p.matches("api.example.com", 80));
    }

    #[test]
    fn network_pattern_rejects_bad_input() {
        assert!(NetworkPattern::parse(":443").is_err(), "empty host");
        assert!(NetworkPattern::parse("*.").is_err(), "empty suffix");
        assert!(
            NetworkPattern::parse("api.example.com:not-a-port").is_err(),
            "non-numeric port"
        );
    }

    // ── Serde round-trip ───────────────────────────────────────────

    #[test]
    fn fully_populated_sandbox_policy_round_trips_through_json() {
        let original = SandboxPolicy::new(
            TrustMode::Fast,
            PermissionPolicy::new(
                AccessLevel::Allow,
                AccessLevel::RequireApproval,
                AccessLevel::Block,
                AccessLevel::RequireApproval,
            ),
            vec![
                NetworkPattern::parse("*.example.com").expect("parse"),
                NetworkPattern::parse("localhost:*").expect("parse"),
                NetworkPattern::parse("api.internal:8443").expect("parse"),
            ],
            vec![
                EnvAction::Allow(EnvVarName::new("PATH")),
                EnvAction::Block(EnvVarName::new("AWS_SECRET_ACCESS_KEY")),
                EnvAction::Prompt(EnvVarName::new("AWS_PROFILE")),
            ],
        );

        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: SandboxPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn env_var_name_is_transparent_on_the_wire() {
        // `#[serde(transparent)]` lock — stored env actions encode
        // the bare string, not a `{"0": "..."}` wrapper.
        let json = serde_json::to_string(&EnvVarName::new("PATH")).expect("serialize");
        assert_eq!(json, "\"PATH\"");
    }

    // ── Decision constructors ─────────────────────────────────────

    #[test]
    fn approval_reason_constructors_match_field_layout() {
        let bare = ApprovalReason::new("summary");
        assert_eq!(bare.summary, "summary");
        assert!(bare.detail.is_none());

        let full = ApprovalReason::with_detail("summary", "long form");
        assert_eq!(full.summary, "summary");
        assert_eq!(full.detail.as_deref(), Some("long form"));
    }

    #[test]
    fn sandbox_decision_is_allow_only_for_allow() {
        assert!(SandboxDecision::Allow.is_allow());
        assert!(!SandboxDecision::RequireApproval(ApprovalReason::new("x")).is_allow());
        assert!(!SandboxDecision::Block("x".into()).is_allow());
    }
}
