//! Agent loop state and budget types.
//!
//! The *agent loop* is the runtime that drives a chat: it dispatches
//! tool calls, feeds results back to the model, and stops when the
//! provider says it is done, the user cancels, or a budget runs out.
//! That runtime lives in a later PRD; this slice owns only the data
//! shapes the loop and any UI representation of it share.
//!
//! # Why these shapes belong in the Domain layer
//!
//! Two consumers need to agree on the loop's snapshot: the runtime
//! that produces it and the UI that renders it. Putting the shapes
//! here means neither side reaches for the other's types — the UI
//! never imports the runtime crate, the runtime never imports the
//! UI crate, and persistence (PRD-03) reads the same wire format
//! both already use.
//!
//! # Public surface at a glance
//!
//! - [`BudgetDimension`] — the four axes a budget can constrain
//!   (`Turns`, `Tokens`, `ToolCalls`, `WallClock`).
//! - [`AgentLoopBudget`] — per-axis caps. Every field is
//!   [`Option`]-wrapped so a budget can constrain only some axes;
//!   [`AgentLoopBudget::is_exhausted`] checks the axes in a
//!   deterministic order (`Turns`, `Tokens`, `ToolCalls`,
//!   `WallClock`) so the surface that reports "why did the loop
//!   stop" is reproducible.
//! - [`LoopStatus`] — coarse runtime state (`Idle`, `Running`,
//!   `AwaitingApproval`, `Cancelling`, `Completed(...)`).
//! - [`CompletionReason`] — terminal reason carried by
//!   [`LoopStatus::Completed`].
//! - [`AccumulatedUsage`] — running totals (token usage, turn
//!   count, tool-call count, timestamps) plus a small helper API
//!   for the runtime to update them.
//! - [`InjectedInstruction`] — user-injected mid-loop instruction
//!   (e.g. "slow down", "rephrase the last answer"). Scoped to a
//!   single turn or until the loop completes.
//! - [`InjectionScope`] — the lifetime knob on
//!   [`InjectedInstruction`].
//! - [`AgentLoopState`] — the snapshot the UI renders and
//!   persistence stores.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::ai::domain::{Role, Usage};
use crate::id::ChatId;
use crate::sandbox::ApprovalReason;

// ─────────────────────────────────────────────────────────────────────
// BudgetDimension — names the axis a budget can constrain. The order
// here matches the order `AgentLoopBudget::is_exhausted` walks; keep
// them in sync if either ever changes.
// ─────────────────────────────────────────────────────────────────────

/// Axis a budget can constrain.
///
/// Returned from [`AgentLoopBudget::is_exhausted`] to identify which
/// axis tripped first. Carried inside
/// [`CompletionReason::BudgetExhausted`] so the UI can render
/// "stopped after N turns" or "stopped after T seconds" without
/// re-deriving the cause.
///
/// The variant order matches the order `is_exhausted` checks them in;
/// downstream code is free to enumerate them in any order it likes,
/// but the reporting contract pins this sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    /// `max_turns` cap — number of `(user, assistant)` round trips.
    Turns,
    /// `max_total_tokens` cap — sum of input, output, and cached
    /// tokens recorded in [`AccumulatedUsage::usage`].
    Tokens,
    /// `max_tool_calls` cap — number of dispatched tool invocations.
    ToolCalls,
    /// `max_wall_clock` cap — elapsed real time since
    /// [`AccumulatedUsage::started_at`].
    WallClock,
}

assert_impl_all!(BudgetDimension: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AgentLoopBudget — per-axis caps. Every field is Option<T> so a
// budget can constrain only some axes. `is_exhausted` walks the four
// dimensions in the canonical order.
// ─────────────────────────────────────────────────────────────────────

/// Caps the agent loop enforces against a running
/// [`AccumulatedUsage`].
///
/// Every field is [`Option`]-wrapped — a `None` field means "this
/// axis is unconstrained". A budget with every field set to `None`
/// is a valid no-op budget; the runtime treats it as "run until the
/// provider stops or the user cancels".
///
/// Marked `#[non_exhaustive]` so adding a new axis (e.g. cost in
/// dollars, request count) in a later PRD stays non-breaking for
/// downstream construction sites — callers go through
/// [`AgentLoopBudget::new`] or [`AgentLoopBudget::unconstrained`]
/// rather than struct literals.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentLoopBudget {
    /// Hard cap on the number of `(user, assistant)` round trips.
    pub max_turns: Option<u32>,
    /// Hard cap on the sum of input, output, and cached tokens
    /// recorded in [`AccumulatedUsage::usage`].
    pub max_total_tokens: Option<u32>,
    /// Hard cap on elapsed wall-clock time since
    /// [`AccumulatedUsage::started_at`]. Compared against
    /// [`AccumulatedUsage::elapsed`].
    pub max_wall_clock: Option<Duration>,
    /// Hard cap on the number of dispatched tool invocations.
    pub max_tool_calls: Option<u32>,
}

impl AgentLoopBudget {
    /// Build a budget from explicit per-axis caps. Pairs with
    /// `#[non_exhaustive]` — new axes that land later stay
    /// non-breaking for callers that go through this constructor.
    #[must_use]
    pub fn new(
        max_turns: Option<u32>,
        max_total_tokens: Option<u32>,
        max_wall_clock: Option<Duration>,
        max_tool_calls: Option<u32>,
    ) -> Self {
        Self {
            max_turns,
            max_total_tokens,
            max_wall_clock,
            max_tool_calls,
        }
    }

    /// A budget with every axis unconstrained. Equivalent to
    /// [`AgentLoopBudget::default`] — kept as a named constructor
    /// because the intent at a call site reads as "no cap" rather
    /// than "default".
    #[must_use]
    pub fn unconstrained() -> Self {
        Self::default()
    }

    /// First exhausted axis, if any.
    ///
    /// Returns `Some(dim)` for the first axis whose cap is set and
    /// already reached or exceeded; returns `None` when every set
    /// axis is still under its cap (or no axes are set).
    ///
    /// Order of checks is locked: `Turns`, `Tokens`, `ToolCalls`,
    /// `WallClock`. The reporting surface depends on this order so
    /// "why did the loop stop" stays reproducible across runs.
    #[must_use]
    pub fn is_exhausted(&self, usage: &AccumulatedUsage) -> Option<BudgetDimension> {
        if let Some(cap) = self.max_turns {
            if usage.turn_count >= cap {
                return Some(BudgetDimension::Turns);
            }
        }
        if let Some(cap) = self.max_total_tokens {
            if usage.usage.total_tokens() >= cap {
                return Some(BudgetDimension::Tokens);
            }
        }
        if let Some(cap) = self.max_tool_calls {
            if usage.tool_call_count >= cap {
                return Some(BudgetDimension::ToolCalls);
            }
        }
        if let Some(cap) = self.max_wall_clock {
            if usage.elapsed() >= cap {
                return Some(BudgetDimension::WallClock);
            }
        }
        None
    }
}

assert_impl_all!(AgentLoopBudget: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CompletionReason — terminal reason carried by `LoopStatus::Completed`.
// ─────────────────────────────────────────────────────────────────────

/// Why the agent loop reached a terminal state.
///
/// The runtime emits exactly one of these per loop. `BudgetExhausted`
/// carries the [`BudgetDimension`] that tripped first;
/// [`AgentLoopBudget::is_exhausted`] guarantees that field is
/// well-defined.
///
/// `Error` carries the diagnostic verbatim so the UI can render the
/// failure without threading a parallel error channel. The string is
/// already user-facing — adapters that want to attribute the failure
/// to a category can wrap it in their own error type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompletionReason {
    /// Provider yielded `StreamEvent::Done` and no further tool
    /// calls were pending.
    ProviderDone,
    /// User cancelled the loop (clicked stop, switched away, …).
    UserCancelled,
    /// One of the budget axes tripped. The dimension identifies
    /// which.
    BudgetExhausted(BudgetDimension),
    /// Loop aborted on an error. The string is the rendered cause.
    Error(String),
}

assert_impl_all!(CompletionReason: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// LoopStatus — coarse runtime state.
// ─────────────────────────────────────────────────────────────────────

/// Coarse state the agent loop is in.
///
/// Five variants cover the lifecycle: the loop sits `Idle` until the
/// user submits a turn, transitions to `Running` while the provider
/// streams, parks in `AwaitingApproval` when a tool call needs the
/// user's confirmation, drops into `Cancelling` while the runtime
/// unwinds an in-flight call, and lands in
/// [`LoopStatus::Completed`] once the run terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoopStatus {
    /// No active loop.
    Idle,
    /// Loop is processing turns / streaming from the provider.
    Running,
    /// Loop is parked waiting for a user approval response. The
    /// pending [`ApprovalReason`] lives on
    /// [`AgentLoopState::pending_approval`] so the UI can render
    /// the prompt without re-deriving it from the status alone.
    AwaitingApproval,
    /// Loop is unwinding an in-flight call after a cancel signal.
    Cancelling,
    /// Loop reached a terminal state. The reason carries the cause.
    Completed(CompletionReason),
}

assert_impl_all!(LoopStatus: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AccumulatedUsage — running totals plus a tiny helper API the
// runtime calls to keep them current. `Usage::total_tokens` lives in
// the AI domain module; the helper here just forwards into it.
// ─────────────────────────────────────────────────────────────────────

/// Running totals the loop maintains across turns.
///
/// `usage` accumulates the [`Usage`] reports the provider streams.
/// `turn_count` and `tool_call_count` are bumped explicitly by the
/// runtime — they are *not* derived from `usage` because a single
/// provider report can cover multiple turns and the runtime would
/// otherwise have to thread a parallel counter.
///
/// `started_at` is set once when the loop kicks off; `last_event_at`
/// is bumped on every helper call so [`AccumulatedUsage::elapsed`]
/// reports a meaningful "loop has been running for X" duration even
/// when no token traffic has flowed for a while.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccumulatedUsage {
    /// Token-accounting record summed across the loop so far.
    pub usage: Usage,
    /// Number of turns completed in this loop.
    pub turn_count: u32,
    /// Number of tool calls dispatched in this loop.
    pub tool_call_count: u32,
    /// Wall-clock when the loop started.
    pub started_at: DateTime<Utc>,
    /// Wall-clock of the most recent helper update.
    pub last_event_at: DateTime<Utc>,
}

impl AccumulatedUsage {
    /// Build a fresh totals record stamped with `now`.
    ///
    /// Both `started_at` and `last_event_at` start at the same
    /// instant; subsequent helper calls bump `last_event_at` while
    /// `started_at` stays pinned.
    #[must_use]
    pub fn started_at(now: DateTime<Utc>) -> Self {
        Self {
            usage: Usage::default(),
            turn_count: 0,
            tool_call_count: 0,
            started_at: now,
            last_event_at: now,
        }
    }

    /// Sum a [`Usage`] delta into the running total and bump
    /// `last_event_at` to `now`.
    pub fn add_usage(&mut self, delta: Usage, now: DateTime<Utc>) {
        // `Usage`'s fields are u32; saturating_add keeps a
        // pathologically long-running loop from wrapping silently
        // into 0. The numbers we cap at are far above any realistic
        // run, so saturation is a safer floor than wrap.
        self.usage = Usage::new(
            self.usage.input_tokens.saturating_add(delta.input_tokens),
            self.usage.output_tokens.saturating_add(delta.output_tokens),
            self.usage
                .cached_input_tokens
                .saturating_add(delta.cached_input_tokens),
        );
        self.last_event_at = now;
    }

    /// Increment `turn_count` and bump `last_event_at` to `now`.
    pub fn record_turn(&mut self, now: DateTime<Utc>) {
        self.turn_count = self.turn_count.saturating_add(1);
        self.last_event_at = now;
    }

    /// Increment `tool_call_count` and bump `last_event_at` to `now`.
    pub fn record_tool_call(&mut self, now: DateTime<Utc>) {
        self.tool_call_count = self.tool_call_count.saturating_add(1);
        self.last_event_at = now;
    }

    /// Wall-clock between [`AccumulatedUsage::started_at`] and
    /// [`AccumulatedUsage::last_event_at`].
    ///
    /// Returns [`Duration::ZERO`] when `last_event_at` is before
    /// `started_at`, which can only happen if a clock skew or a
    /// manual override moved the timestamps backwards. Treating
    /// that as zero keeps the budget check well-defined without
    /// surfacing a separate error path.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        let delta = self.last_event_at - self.started_at;
        delta.to_std().unwrap_or(Duration::ZERO)
    }
}

assert_impl_all!(AccumulatedUsage: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Usage::total_tokens lives in the AI domain module — but the budget
// check needs it. Adding a tiny extension trait here keeps the helper
// next to its consumer without churning the AI module.
// ─────────────────────────────────────────────────────────────────────

trait UsageTotalTokensExt {
    fn total_tokens(&self) -> u32;
}

impl UsageTotalTokensExt for Usage {
    fn total_tokens(&self) -> u32 {
        // Saturating arithmetic for the same reason as in
        // `add_usage`: the cap on long-running loops is set far
        // below `u32::MAX`, but a wrapping sum could silently
        // mis-report exhaustion as headroom.
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_input_tokens)
    }
}

// ─────────────────────────────────────────────────────────────────────
// InjectionScope + InjectedInstruction — user-injected mid-loop
// instructions.
// ─────────────────────────────────────────────────────────────────────

/// Lifetime of an [`InjectedInstruction`].
///
/// `SingleTurn` instructions are consumed by the next turn and
/// dropped afterwards; `UntilLoopComplete` ones stay attached for
/// the rest of the run. The runtime is responsible for the actual
/// pruning — this enum just describes intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InjectionScope {
    /// Apply once, then drop.
    SingleTurn,
    /// Apply on every remaining turn until the loop completes.
    UntilLoopComplete,
}

assert_impl_all!(InjectionScope: Send, Sync);

/// Mid-loop instruction the user (or the runtime itself) inserts
/// outside the normal turn flow.
///
/// Examples: "slow down", "answer in bullet points", "rephrase the
/// last answer". The runtime renders the instruction into the next
/// provider request as a synthetic message of `role`; the `id` is
/// the correlation handle the UI uses to remove the instruction
/// when the user retracts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InjectedInstruction {
    /// Stable identity for the instruction inside the active loop.
    /// Stays a `String` rather than a UUID because the runtime
    /// composes it from a counter or a content hash; the Domain
    /// layer does not commit to one strategy.
    pub id: String,
    /// Role the runtime should attach to the synthetic message.
    /// Most injections are [`Role::User`] (the user is steering),
    /// but `System` covers framing nudges and `Assistant` covers
    /// "act as if you said this" rewrites.
    pub role: Role,
    /// Instruction body. Rendered verbatim into the synthetic
    /// message.
    pub content: String,
    /// Lifetime of the injection.
    pub scope: InjectionScope,
}

impl InjectedInstruction {
    /// Bundle the four fields. Pairs with `#[non_exhaustive]`.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        role: Role,
        content: impl Into<String>,
        scope: InjectionScope,
    ) -> Self {
        Self {
            id: id.into(),
            role,
            content: content.into(),
            scope,
        }
    }
}

assert_impl_all!(InjectedInstruction: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// AgentLoopState — the snapshot. The UI renders it; persistence
// stores it; the runtime produces it.
// ─────────────────────────────────────────────────────────────────────

/// Snapshot of a single loop's state.
///
/// One per active chat. The runtime publishes a fresh snapshot
/// whenever any field changes; the UI subscribes to the stream and
/// re-renders. Persistence stores the most recent snapshot so a
/// restart can resume a run that was parked in
/// [`LoopStatus::AwaitingApproval`] without losing pending
/// instructions.
///
/// Marked `#[non_exhaustive]` so a later PRD can attach additional
/// fields (a structured cancellation reason, per-tool stats, …)
/// without breaking downstream construction sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentLoopState {
    /// Chat the loop is driving.
    pub chat_id: ChatId,
    /// Coarse runtime state.
    pub status: LoopStatus,
    /// Active budget. May be unconstrained.
    pub budget: AgentLoopBudget,
    /// Running totals.
    pub usage: AccumulatedUsage,
    /// When the loop is parked in
    /// [`LoopStatus::AwaitingApproval`], the reason carried by
    /// the prompt. `None` in every other state.
    pub pending_approval: Option<ApprovalReason>,
    /// User-injected mid-loop instructions still in scope. Pruned
    /// by the runtime as `SingleTurn` ones fire.
    pub injected_instructions: Vec<InjectedInstruction>,
}

impl AgentLoopState {
    /// Bundle a fully populated snapshot. Pairs with
    /// `#[non_exhaustive]`.
    #[must_use]
    pub fn new(
        chat_id: ChatId,
        status: LoopStatus,
        budget: AgentLoopBudget,
        usage: AccumulatedUsage,
        pending_approval: Option<ApprovalReason>,
        injected_instructions: Vec<InjectedInstruction>,
    ) -> Self {
        Self {
            chat_id,
            status,
            budget,
            usage,
            pending_approval,
            injected_instructions,
        }
    }

    /// Convenience constructor for a fresh idle snapshot stamped at
    /// `now`. Used by the runtime when a new chat opens — every
    /// other field falls into the canonical "nothing has happened
    /// yet" shape.
    #[must_use]
    pub fn fresh(chat_id: ChatId, budget: AgentLoopBudget, now: DateTime<Utc>) -> Self {
        Self::new(
            chat_id,
            LoopStatus::Idle,
            budget,
            AccumulatedUsage::started_at(now),
            None,
            Vec::new(),
        )
    }
}

assert_impl_all!(AgentLoopState: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000, 0)
            .expect("timestamp is in chrono's representable range")
    }

    fn fixed_later(offset_secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000 + offset_secs, 0)
            .expect("timestamp is in chrono's representable range")
    }

    // ── AccumulatedUsage helpers ──────────────────────────────────

    /// `add_usage` sums the three token axes and bumps
    /// `last_event_at`.
    #[test]
    fn add_usage_sums_and_bumps_last_event() {
        let mut acc = AccumulatedUsage::started_at(fixed_now());
        acc.add_usage(Usage::new(10, 5, 2), fixed_later(3));
        assert_eq!(acc.usage, Usage::new(10, 5, 2));
        assert_eq!(acc.last_event_at, fixed_later(3));
        acc.add_usage(Usage::new(1, 1, 1), fixed_later(7));
        assert_eq!(acc.usage, Usage::new(11, 6, 3));
        assert_eq!(acc.last_event_at, fixed_later(7));
    }

    /// `record_turn` and `record_tool_call` bump their counters
    /// monotonically and update `last_event_at`.
    #[test]
    fn record_helpers_bump_counters_and_event_stamp() {
        let mut acc = AccumulatedUsage::started_at(fixed_now());
        acc.record_turn(fixed_later(1));
        acc.record_turn(fixed_later(2));
        acc.record_tool_call(fixed_later(3));
        assert_eq!(acc.turn_count, 2);
        assert_eq!(acc.tool_call_count, 1);
        assert_eq!(acc.last_event_at, fixed_later(3));
    }

    /// `elapsed` reports the gap between `started_at` and
    /// `last_event_at` and clamps to zero on a backwards skew.
    #[test]
    fn elapsed_reports_gap_and_clamps_negative() {
        let mut acc = AccumulatedUsage::started_at(fixed_now());
        acc.last_event_at = fixed_later(60);
        assert_eq!(acc.elapsed(), Duration::from_secs(60));

        // Backwards skew — `last_event_at` before `started_at`.
        acc.last_event_at = fixed_later(-5);
        assert_eq!(acc.elapsed(), Duration::ZERO);
    }

    // ── AgentLoopBudget::is_exhausted order ───────────────────────

    /// Helper that builds an exhausted-by-`Turns` setup.
    fn turns_exhausted() -> (AgentLoopBudget, AccumulatedUsage) {
        let budget = AgentLoopBudget::new(
            Some(2),                       // turns
            Some(1_000),                   // tokens
            Some(Duration::from_secs(60)), // wall_clock
            Some(5),                       // tool_calls
        );
        let mut usage = AccumulatedUsage::started_at(fixed_now());
        usage.turn_count = 2;
        // Set every other dimension to exhausted as well so the
        // order-of-check guarantee is what's actually being tested.
        usage.usage = Usage::new(1_000, 0, 0);
        usage.tool_call_count = 5;
        usage.last_event_at = fixed_later(60);
        (budget, usage)
    }

    #[test]
    fn is_exhausted_returns_turns_first_when_every_axis_tripped() {
        let (b, u) = turns_exhausted();
        assert_eq!(b.is_exhausted(&u), Some(BudgetDimension::Turns));
    }

    #[test]
    fn is_exhausted_returns_tokens_when_turns_under_cap() {
        let (mut b, mut u) = turns_exhausted();
        u.turn_count = 1; // turns now under cap
        b.max_turns = Some(2);
        assert_eq!(b.is_exhausted(&u), Some(BudgetDimension::Tokens));
    }

    #[test]
    fn is_exhausted_returns_tool_calls_when_turns_and_tokens_under_cap() {
        let (mut b, mut u) = turns_exhausted();
        u.turn_count = 0;
        u.usage = Usage::new(0, 0, 0);
        b.max_turns = Some(2);
        b.max_total_tokens = Some(1_000);
        assert_eq!(b.is_exhausted(&u), Some(BudgetDimension::ToolCalls));
    }

    #[test]
    fn is_exhausted_returns_wall_clock_when_other_axes_under_cap() {
        let (mut b, mut u) = turns_exhausted();
        u.turn_count = 0;
        u.usage = Usage::new(0, 0, 0);
        u.tool_call_count = 0;
        b.max_turns = Some(2);
        b.max_total_tokens = Some(1_000);
        b.max_tool_calls = Some(5);
        assert_eq!(b.is_exhausted(&u), Some(BudgetDimension::WallClock));
    }

    #[test]
    fn is_exhausted_returns_none_when_every_axis_under_cap() {
        let budget = AgentLoopBudget::new(
            Some(10),
            Some(10_000),
            Some(Duration::from_secs(3_600)),
            Some(50),
        );
        let usage = AccumulatedUsage::started_at(fixed_now());
        assert_eq!(budget.is_exhausted(&usage), None);
    }

    #[test]
    fn is_exhausted_skips_unconstrained_axes() {
        // Only `Tokens` constrained; turn_count > 0 should not
        // trigger because `max_turns` is `None`.
        let budget = AgentLoopBudget::new(None, Some(10), None, None);
        let mut usage = AccumulatedUsage::started_at(fixed_now());
        usage.turn_count = 100;
        usage.usage = Usage::new(20, 0, 0);
        assert_eq!(budget.is_exhausted(&usage), Some(BudgetDimension::Tokens));
    }

    #[test]
    fn unconstrained_budget_never_exhausts() {
        let budget = AgentLoopBudget::unconstrained();
        let mut usage = AccumulatedUsage::started_at(fixed_now());
        usage.turn_count = u32::MAX;
        usage.tool_call_count = u32::MAX;
        usage.usage = Usage::new(u32::MAX, u32::MAX, u32::MAX);
        usage.last_event_at = fixed_later(i64::from(i32::MAX));
        assert_eq!(budget.is_exhausted(&usage), None);
    }

    // ── Round-trips ───────────────────────────────────────────────

    /// Headline acceptance criterion: a populated `AgentLoopState`
    /// covering every variant axis (`status`, `pending_approval`,
    /// `injected_instructions`) round-trips through JSON.
    #[test]
    fn agent_loop_state_round_trips_through_json() {
        let mut usage = AccumulatedUsage::started_at(fixed_now());
        usage.add_usage(Usage::new(40, 12, 3), fixed_later(5));
        usage.record_turn(fixed_later(6));
        usage.record_tool_call(fixed_later(7));

        let state = AgentLoopState::new(
            ChatId::new_v4(),
            LoopStatus::AwaitingApproval,
            AgentLoopBudget::new(
                Some(8),
                Some(2_000),
                Some(Duration::from_secs(120)),
                Some(20),
            ),
            usage,
            Some(ApprovalReason::with_detail(
                "tool wants to write outside the workspace",
                "/etc/hosts is outside the workspace root",
            )),
            vec![
                InjectedInstruction::new(
                    "inj-1",
                    Role::User,
                    "answer in bullet points",
                    InjectionScope::SingleTurn,
                ),
                InjectedInstruction::new(
                    "inj-2",
                    Role::System,
                    "be concise",
                    InjectionScope::UntilLoopComplete,
                ),
            ],
        );

        let json = serde_json::to_string(&state).expect("serialize");
        let decoded: AgentLoopState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, decoded);
    }

    /// `LoopStatus` is `#[non_exhaustive]`. Lock the wildcard-arm
    /// contract — same shape as the Effect/Part tests.
    #[test]
    fn loop_status_match_uses_wildcard_arm() {
        let statuses = [
            LoopStatus::Idle,
            LoopStatus::Running,
            LoopStatus::AwaitingApproval,
            LoopStatus::Cancelling,
            LoopStatus::Completed(CompletionReason::ProviderDone),
            LoopStatus::Completed(CompletionReason::UserCancelled),
            LoopStatus::Completed(CompletionReason::BudgetExhausted(BudgetDimension::Turns)),
            LoopStatus::Completed(CompletionReason::Error("boom".to_string())),
        ];
        for s in statuses {
            #[allow(unreachable_patterns)]
            let label = match s {
                LoopStatus::Idle => "idle",
                LoopStatus::Running => "running",
                LoopStatus::AwaitingApproval => "approval",
                LoopStatus::Cancelling => "cancel",
                LoopStatus::Completed(_) => "done",
                _ => "unknown",
            };
            assert!(matches!(
                label,
                "idle" | "running" | "approval" | "cancel" | "done"
            ));
        }
    }

    /// Every `CompletionReason` variant survives a JSON hop.
    /// Dedicated test so a wire-form rename surfaces the failure
    /// next to the variant rather than buried inside the headline
    /// `AgentLoopState` round-trip.
    #[test]
    fn completion_reason_round_trips_every_variant() {
        let cases = [
            CompletionReason::ProviderDone,
            CompletionReason::UserCancelled,
            CompletionReason::BudgetExhausted(BudgetDimension::Turns),
            CompletionReason::BudgetExhausted(BudgetDimension::Tokens),
            CompletionReason::BudgetExhausted(BudgetDimension::ToolCalls),
            CompletionReason::BudgetExhausted(BudgetDimension::WallClock),
            CompletionReason::Error("disk full".to_string()),
        ];
        for original in cases {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: CompletionReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded);
        }
    }

    /// `AgentLoopState::fresh` produces an idle, zero-usage,
    /// no-pending-approval, no-injections snapshot stamped at
    /// `now`.
    #[test]
    fn fresh_state_starts_from_canonical_zero() {
        let chat = ChatId::new_v4();
        let budget = AgentLoopBudget::unconstrained();
        let state = AgentLoopState::fresh(chat, budget.clone(), fixed_now());
        assert_eq!(state.chat_id, chat);
        assert_eq!(state.status, LoopStatus::Idle);
        assert_eq!(state.budget, budget);
        assert_eq!(state.usage.turn_count, 0);
        assert_eq!(state.usage.tool_call_count, 0);
        assert_eq!(state.usage.started_at, fixed_now());
        assert_eq!(state.usage.last_event_at, fixed_now());
        assert!(state.pending_approval.is_none());
        assert!(state.injected_instructions.is_empty());
    }
}
