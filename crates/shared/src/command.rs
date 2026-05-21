//! Command palette types and registry.
//!
//! Commands are the named, addressable actions the user can invoke
//! through the command palette, a keybinding, or a future scripting
//! surface. Every command is a pure data record ([`Command`])
//! pointing at a runtime [`CommandHandler`] that knows how to
//! execute it. The pair lives in the [`CommandRegistry`] — a deep
//! module that owns registration, lookup, and scope-aware filtering.
//!
//! # Public surface at a glance
//!
//! - [`CommandId`] — string newtype identifier. Stable across
//!   installs so persisted keybinding profiles survive reinstalls.
//!   Same wire form (`#[serde(transparent)]`) as the slice-4 shim
//!   it replaces, so existing serialised state keeps loading.
//! - [`CommandCategory`] — coarse bucket the palette uses for
//!   grouping (`File`, `Edit`, `View`, `Terminal`, `Chat`,
//!   `Workspace`, `Help`, `Custom(String)`).
//! - [`CommandScope`] — five-flag record describing where the
//!   command is available (`welcome`, `home`, `terminal_focused`,
//!   `editor_focused`, `chat_focused`).
//! - [`CommandScopeFilter`] — same five-flag shape, used by the
//!   palette to filter the registry against the user's current
//!   focus state. Matches a [`CommandScope`] if **any** of the
//!   filter's true fields overlaps **any** of the scope's true
//!   fields.
//! - [`Command`] — pure data record bundling the above.
//! - [`CommandError`] — failure surface for handler dispatch
//!   (`NotFound`, `OutOfScope`, `Internal`).
//! - [`CommandContext`] — per-call plumbing handed to a handler
//!   (`workspace` + `cancel_token`). Mirrors [`crate::tool::ToolContext`]
//!   in spirit but trimmed — handlers do not gate on a sandbox in
//!   this slice.
//! - [`CommandHandler`] — `dyn`-compatible async trait every
//!   handler implements. Returns a `Vec<Effect>` for the home-shell
//!   dispatcher to apply, the same effect surface tools emit.
//! - [`CommandRegistry`] — deep module that owns registration,
//!   lookup, scope-filter listing, keybinding-driven lookup, and
//!   unregistration.
//!
//! # Why the registry holds both data and handler
//!
//! The palette renders pure data ([`Command`] alone), but invocation
//! needs the runtime handler. Storing them as a pair behind a single
//! [`CommandId`] keeps the registry as the single source of truth —
//! the palette cannot drift from the dispatcher because both read
//! from the same map. `register` overwrites a prior entry under the
//! same id, which is the contract the test suite below pins.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use thiserror::Error;

use crate::effect::Effect;
use crate::keybinding::KeyBinding;
use crate::tool::CancelToken;
use crate::workspace::WorkspaceRef;

// ─────────────────────────────────────────────────────────────────────
// CommandId — same wire form as the slice-4 shim it replaces. Adding
// new methods here is non-breaking; the underlying string newtype is
// what persistence depends on, and that shape is unchanged.
// ─────────────────────────────────────────────────────────────────────

/// Stable identifier for a command in the command palette.
///
/// String newtype rather than a uuid because commands are a curated,
/// named set: `editor.save`, `palette.toggle`, `chat.new` mean the
/// same thing across installs. The slug is what configs and key
/// bindings reference, so persisted state survives reinstalls.
///
/// `#[serde(transparent)]` keeps the wire form a bare string so
/// keybinding profiles serialise as plain `id -> [shortcut]` maps —
/// the same shape the slice-4 shim used.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    /// Wrap a slug. No validation in this slice; a future PRD can
    /// tighten the grammar (e.g. require dot-separated namespacing)
    /// without breaking the wire form.
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }

    /// Borrow the underlying slug.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

assert_impl_all!(CommandId: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandCategory — bucket the palette uses for grouping. The closed
// list covers the surfaces in the product today; `Custom(String)` is
// the escape hatch for plugins and feature crates that introduce new
// areas without forcing a Domain change.
// ─────────────────────────────────────────────────────────────────────

/// Coarse grouping bucket for the command palette.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    /// File operations (open, save, close).
    File,
    /// Editing operations (find, replace, format).
    Edit,
    /// View operations (toggle sidebar, zoom).
    View,
    /// Terminal operations.
    Terminal,
    /// Chat-surface operations.
    Chat,
    /// Workspace-level operations.
    Workspace,
    /// Help, about, documentation.
    Help,
    /// Plugin- or feature-defined category. The string is the
    /// category's display label and is shown verbatim.
    Custom(String),
}

assert_impl_all!(CommandCategory: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandScope — five bool fields. Stored as a struct (rather than a
// real bitflag) for the same reason as `Modifiers` in the keybinding
// module: keeps the serde shape readable in TOML/JSON snapshots and
// avoids a dependency on a `bitflags` crate.
// ─────────────────────────────────────────────────────────────────────

/// Where a [`Command`] is available.
///
/// Each flag is independent. A command scoped to `home: true,
/// editor_focused: true` is offered both in the home shell at large
/// and specifically when the editor pane has focus; the registry's
/// scope filter consults each flag in isolation when matching.
///
/// The named constructors ([`CommandScope::everywhere`],
/// [`CommandScope::home_only`], etc.) cover the most common shapes;
/// callers that need a specific combination construct the struct
/// literal directly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandScope {
    /// Available on the welcome screen (no workspace open).
    pub welcome: bool,
    /// Available in the home shell at large (any focus state).
    pub home: bool,
    /// Available specifically when the terminal pane has focus.
    pub terminal_focused: bool,
    /// Available specifically when the editor pane has focus.
    pub editor_focused: bool,
    /// Available specifically when the chat surface has focus.
    pub chat_focused: bool,
}

impl CommandScope {
    /// All flags cleared. Equivalent to [`CommandScope::default`] —
    /// a command with this scope is effectively orphaned (no
    /// surface offers it) and exists for tests that pin the
    /// "filter never matches" branch.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            welcome: false,
            home: false,
            terminal_focused: false,
            editor_focused: false,
            chat_focused: false,
        }
    }

    /// All flags set. The command is offered everywhere.
    #[must_use]
    pub const fn everywhere() -> Self {
        Self {
            welcome: true,
            home: true,
            terminal_focused: true,
            editor_focused: true,
            chat_focused: true,
        }
    }

    /// Only the welcome surface. Typical for `workspace.open`,
    /// `workspace.create_new`, recents-list actions.
    #[must_use]
    pub const fn welcome_only() -> Self {
        Self {
            welcome: true,
            ..Self::empty()
        }
    }

    /// Only the home shell. Typical for the bulk of editor and
    /// chat commands that should not show on the welcome screen.
    #[must_use]
    pub const fn home_only() -> Self {
        Self {
            home: true,
            ..Self::empty()
        }
    }
}

assert_impl_all!(CommandScope: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandScopeFilter — same shape as CommandScope but semantically
// "what's the user's current focus state". A filter matches a scope
// when *any* shared flag is true on both sides. Empty filters match
// nothing — the caller must declare where they are.
// ─────────────────────────────────────────────────────────────────────

/// Focus-state filter the palette passes to the registry.
///
/// Built from "where the user is right now" — set `chat_focused`
/// when the chat surface has focus, `home` when nothing more
/// specific applies, and so on. A filter matches a [`CommandScope`]
/// when **any** of the filter's true fields overlaps **any** of the
/// scope's true fields.
///
/// An empty filter (every flag `false`) matches nothing. That is
/// deliberate — it forces callers to be explicit about "where am I
/// asking from"; the registry will not silently surface every
/// command on a misconfigured filter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandScopeFilter {
    /// Welcome screen is active.
    pub welcome: bool,
    /// Home shell is active (any focus).
    pub home: bool,
    /// Terminal pane has focus.
    pub terminal_focused: bool,
    /// Editor pane has focus.
    pub editor_focused: bool,
    /// Chat surface has focus.
    pub chat_focused: bool,
}

impl CommandScopeFilter {
    /// Every flag cleared. Will not match any non-empty scope.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            welcome: false,
            home: false,
            terminal_focused: false,
            editor_focused: false,
            chat_focused: false,
        }
    }

    /// Every flag set. Matches every non-empty scope.
    #[must_use]
    pub const fn everywhere() -> Self {
        Self {
            welcome: true,
            home: true,
            terminal_focused: true,
            editor_focused: true,
            chat_focused: true,
        }
    }

    /// Match a scope: returns `true` when at least one flag is
    /// `true` on both `self` and `scope`.
    #[must_use]
    pub fn matches(self, scope: &CommandScope) -> bool {
        (self.welcome && scope.welcome)
            || (self.home && scope.home)
            || (self.terminal_focused && scope.terminal_focused)
            || (self.editor_focused && scope.editor_focused)
            || (self.chat_focused && scope.chat_focused)
    }
}

assert_impl_all!(CommandScopeFilter: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Command — pure data the palette renders. Marked `#[non_exhaustive]`
// so adding a future field (e.g. `icon`, `accelerator_hint`) stays
// non-breaking for downstream construction sites.
// ─────────────────────────────────────────────────────────────────────

/// Pure-data record describing a command.
///
/// The palette renders these directly — `label` is the row text,
/// `category` drives grouping, `default_keybinding` is rendered as
/// the trailing accelerator hint, `description` is the optional
/// long-form explanation a power user can expand.
///
/// `default_keybinding` is `Option<KeyBinding>` because not every
/// command has a default shortcut — some are only reachable through
/// the palette. The user's profile (in
/// [`crate::keybinding::KeybindingProfile`]) can additionally bind
/// the command to other shortcuts; the registry's keybinding lookup
/// only consults `default_keybinding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Command {
    /// Stable identifier.
    pub id: CommandId,
    /// Display label rendered in the palette row.
    pub label: String,
    /// Coarse grouping bucket.
    pub category: CommandCategory,
    /// Default keybinding rendered as the row accelerator hint, if
    /// any. `None` for palette-only commands.
    pub default_keybinding: Option<KeyBinding>,
    /// Where the command is available.
    pub scope: CommandScope,
    /// Optional long-form description. `None` when the label is
    /// self-explanatory.
    pub description: Option<String>,
}

impl Command {
    /// Bundle the six fields. Pairs with `#[non_exhaustive]` —
    /// callers go through this constructor instead of struct
    /// literals so adding a new field later stays non-breaking.
    #[must_use]
    pub fn new(
        id: CommandId,
        label: impl Into<String>,
        category: CommandCategory,
        default_keybinding: Option<KeyBinding>,
        scope: CommandScope,
        description: Option<String>,
    ) -> Self {
        Self {
            id,
            label: label.into(),
            category,
            default_keybinding,
            scope,
            description,
        }
    }
}

assert_impl_all!(Command: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandError — failure surface returned by handler dispatch and the
// registry's lookup helpers. `thiserror`-derived for the standard
// `Display` / `Error` plumbing.
// ─────────────────────────────────────────────────────────────────────

/// Failure raised by [`CommandHandler::execute`] or by the registry
/// when a lookup cannot satisfy the request.
///
/// `NotFound` and `OutOfScope` are caller-side errors: the palette
/// asked for something that does not exist or is not currently
/// available. `Internal` wraps a handler-side bug or unexpected
/// state and carries the diagnostic verbatim.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandError {
    /// No command registered under this id.
    #[error("command not found: {0}")]
    NotFound(CommandId),
    /// The command exists but the active scope filter does not
    /// admit it.
    #[error("command out of scope: {0}")]
    OutOfScope(CommandId),
    /// Handler-side failure. The string carries the diagnostic.
    #[error("internal command error: {0}")]
    Internal(String),
}

assert_impl_all!(CommandError: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandContext — per-call plumbing handed to handlers. Mirrors the
// `ToolContext` shape but trimmed: command handlers do not gate on a
// sandbox in this slice. Adding fields later stays non-breaking via
// `#[non_exhaustive]`.
// ─────────────────────────────────────────────────────────────────────

/// Per-call plumbing passed to a [`CommandHandler::execute`].
///
/// Carries the workspace handle and a cancellation token. A future
/// PRD can attach additional fields (a telemetry sink, a structured
/// progress channel, …) without churning every handler signature.
#[derive(Clone)]
#[non_exhaustive]
pub struct CommandContext {
    /// Workspace the command is operating against.
    pub workspace: WorkspaceRef,
    /// Cancellation handle. Handlers poll
    /// [`CancelToken::cancelled`] at sensible yield points.
    pub cancel_token: CancelToken,
}

impl CommandContext {
    /// Bundle the two fields. Pairs with `#[non_exhaustive]`.
    #[must_use]
    pub fn new(workspace: WorkspaceRef, cancel_token: CancelToken) -> Self {
        Self {
            workspace,
            cancel_token,
        }
    }
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandContext")
            .field("workspace", &self.workspace)
            .field("cancel_token", &self.cancel_token)
            .finish_non_exhaustive()
    }
}

assert_impl_all!(CommandContext: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandHandler — async, dyn-compatible. Returns a `Vec<Effect>`
// (the same effect surface tools emit) so the home-shell dispatcher
// has a single replay path for both command- and tool-driven
// outcomes.
// ─────────────────────────────────────────────────────────────────────

/// Runtime handler attached to a [`Command`].
///
/// The registry holds the handler behind an
/// `Arc<dyn CommandHandler + Send + Sync>` so the palette and the
/// keybinding dispatcher can fetch a clone without mutating the
/// registry. Implementations are expected to be cheap to clone via
/// `Arc`.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Stable identifier matching [`Command::id`] of the registered
    /// command. The registry already keys on the command's id, but
    /// returning it from the handler lets callers double-check
    /// before dispatch (e.g. when a single handler covers several
    /// related ids).
    fn id(&self) -> CommandId;

    /// Execute the command.
    ///
    /// Returns the side-effects the dispatcher should apply once
    /// the handler is done. May be empty when the command's whole
    /// payload is some externally observable state change (e.g. a
    /// `view.toggle_sidebar` flips a boolean elsewhere).
    ///
    /// # Errors
    ///
    /// Returns a [`CommandError::Internal`] for handler-side
    /// failures. `NotFound` and `OutOfScope` are produced by the
    /// registry, not by handlers.
    async fn execute(&self, ctx: &CommandContext) -> Result<Vec<Effect>, CommandError>;
}

assert_impl_all!(dyn CommandHandler: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// CommandRegistry — the deep module. Internal representation is a
// flat HashMap; `lookup_by_keybinding` iterates linearly. Indexed
// lookup can land later if it ever becomes hot — the registry size
// is small (low hundreds) and palette opens are not on a frame
// budget.
// ─────────────────────────────────────────────────────────────────────

struct Entry {
    command: Command,
    handler: Arc<dyn CommandHandler + Send + Sync>,
}

/// Registry of [`Command`] + [`CommandHandler`] pairs.
///
/// One source of truth for both palette rendering and dispatch.
/// `register` is idempotent on `id`: calling it twice with the same
/// [`CommandId`] overwrites the previous entry (command + handler).
/// The test suite locks this contract so a feature crate that
/// re-registers a command on hot reload does not have to first
/// `unregister`.
pub struct CommandRegistry {
    entries: HashMap<CommandId, Entry>,
}

impl CommandRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Insert a `(command, handler)` pair under `command.id`.
    ///
    /// Overwrites any previous entry with the same id — both the
    /// command record and the handler are replaced. The previous
    /// handler's `Arc` is dropped at the call site, which is the
    /// right thing for hot-reload paths that swap in a new
    /// implementation.
    pub fn register(&mut self, command: Command, handler: Arc<dyn CommandHandler + Send + Sync>) {
        let id = command.id.clone();
        self.entries.insert(id, Entry { command, handler });
    }

    /// Borrow the [`Command`] registered under `id`, if any.
    #[must_use]
    pub fn get(&self, id: &CommandId) -> Option<&Command> {
        self.entries.get(id).map(|e| &e.command)
    }

    /// Fetch a clone of the [`CommandHandler`] registered under
    /// `id`, if any. Returned as an `Arc` so the dispatcher can
    /// hand it off across tasks without touching the registry.
    #[must_use]
    pub fn handler(&self, id: &CommandId) -> Option<Arc<dyn CommandHandler + Send + Sync>> {
        self.entries.get(id).map(|e| Arc::clone(&e.handler))
    }

    /// List every registered command. Order is unspecified — the
    /// palette is responsible for whatever sort it renders.
    #[must_use]
    pub fn list(&self) -> Vec<&Command> {
        self.entries.values().map(|e| &e.command).collect()
    }

    /// List the commands whose [`CommandScope`] intersects the
    /// filter.
    ///
    /// Matching uses [`CommandScopeFilter::matches`] — any shared
    /// flag is enough. Order is unspecified.
    #[must_use]
    pub fn list_for_scope(&self, scope_filter: CommandScopeFilter) -> Vec<&Command> {
        self.entries
            .values()
            .map(|e| &e.command)
            .filter(|cmd| scope_filter.matches(&cmd.scope))
            .collect()
    }

    /// Find a command by its default keybinding, gated by the
    /// scope filter.
    ///
    /// Returns `Some(cmd)` only when both conditions hold: the
    /// `default_keybinding` equals `kb`, **and** the scope filter
    /// admits the command's scope. Returns `None` when no command
    /// matches or when more than one would (the first match wins;
    /// duplicate default keybindings are a configuration bug the
    /// future profile-validation slice will catch).
    #[must_use]
    pub fn lookup_by_keybinding(
        &self,
        kb: &KeyBinding,
        scope_filter: CommandScopeFilter,
    ) -> Option<&Command> {
        self.entries.values().map(|e| &e.command).find(|cmd| {
            cmd.default_keybinding.as_ref() == Some(kb) && scope_filter.matches(&cmd.scope)
        })
    }

    /// Drop the entry under `id`. Returns `true` when an entry was
    /// removed, `false` when no such id was registered.
    pub fn unregister(&mut self, id: &CommandId) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Number of registered commands. Convenience for tests and
    /// debug surfaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Skip the handler `Arc`s — `dyn CommandHandler` does not
        // implement `Debug` and pulling that in would force every
        // implementation to derive it.
        f.debug_struct("CommandRegistry")
            .field("len", &self.entries.len())
            .field("ids", &self.entries.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

assert_impl_all!(CommandRegistry: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::WorkspaceId;
    use crate::keybinding::{Key, KeyBinding, Modifiers};

    // ── Fixtures ──────────────────────────────────────────────────

    /// Tiny `CommandHandler` returning a fixed effect list. Captures
    /// the id passed to `new` so tests can prove which handler ran.
    struct StubHandler {
        id: CommandId,
        effects: Vec<Effect>,
    }

    impl StubHandler {
        fn new(id: CommandId, effects: Vec<Effect>) -> Self {
            Self { id, effects }
        }
    }

    #[async_trait]
    impl CommandHandler for StubHandler {
        fn id(&self) -> CommandId {
            self.id.clone()
        }

        async fn execute(&self, _ctx: &CommandContext) -> Result<Vec<Effect>, CommandError> {
            Ok(self.effects.clone())
        }
    }

    fn save_command() -> Command {
        Command::new(
            CommandId::new("editor.save"),
            "Save",
            CommandCategory::File,
            Some(KeyBinding::new(Modifiers::CTRL, Key::Char('s'))),
            CommandScope {
                home: true,
                editor_focused: true,
                ..CommandScope::empty()
            },
            Some("Persist the active buffer to disk.".to_string()),
        )
    }

    fn palette_command() -> Command {
        Command::new(
            CommandId::new("palette.toggle"),
            "Toggle Command Palette",
            CommandCategory::View,
            Some(KeyBinding::new(
                Modifiers::CTRL | Modifiers::SHIFT,
                Key::Char('p'),
            )),
            CommandScope::everywhere(),
            None,
        )
    }

    fn welcome_command() -> Command {
        Command::new(
            CommandId::new("workspace.open"),
            "Open Workspace",
            CommandCategory::Workspace,
            None,
            CommandScope::welcome_only(),
            None,
        )
    }

    fn sample_context() -> CommandContext {
        CommandContext::new(
            WorkspaceRef::new(WorkspaceId::new_v4(), "Example".to_string()),
            CancelToken::new(),
        )
    }

    // ── CommandId / Command serde ─────────────────────────────────

    /// `CommandId` keeps the slice-4 wire form: a bare quoted string.
    #[test]
    fn command_id_wire_form_is_a_bare_string() {
        let id = CommandId::new("editor.save");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"editor.save\"");
        let decoded: CommandId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, id);
    }

    /// Round-trips a populated `Command` covering the
    /// `default_keybinding`, `description`, and a non-trivial
    /// scope. Headline acceptance criterion for issue #34.
    #[test]
    fn command_round_trips_through_json() {
        let original = save_command();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `CommandCategory::Custom(...)` survives a JSON hop with the
    /// inner string preserved.
    #[test]
    fn command_category_custom_round_trips() {
        let original = CommandCategory::Custom("Plugin: Format Toolkit".to_string());
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: CommandCategory = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    // ── CommandScope / CommandScopeFilter ─────────────────────────

    /// `everywhere` matches every non-empty scope; `empty` matches
    /// none.
    #[test]
    fn scope_filter_extremes_match_as_expected() {
        let scope = CommandScope::home_only();
        assert!(CommandScopeFilter::everywhere().matches(&scope));
        assert!(!CommandScopeFilter::empty().matches(&scope));
        // Empty scope: never matches even under `everywhere`.
        assert!(!CommandScopeFilter::everywhere().matches(&CommandScope::empty()));
    }

    /// Filter matches when at least one shared flag is true on
    /// both sides; no false-positive on disjoint flags.
    #[test]
    fn scope_filter_matches_on_intersection_only() {
        let editor_scope = CommandScope {
            home: true,
            editor_focused: true,
            ..CommandScope::empty()
        };
        let editor_focus_filter = CommandScopeFilter {
            editor_focused: true,
            ..CommandScopeFilter::empty()
        };
        let chat_focus_filter = CommandScopeFilter {
            chat_focused: true,
            ..CommandScopeFilter::empty()
        };
        assert!(editor_focus_filter.matches(&editor_scope));
        assert!(!chat_focus_filter.matches(&editor_scope));
    }

    // ── Registry: register, get, list ─────────────────────────────

    /// `register` + `get` round trip: the same command record
    /// comes back out under the same id.
    #[test]
    fn register_then_get_returns_the_command() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        let handler: Arc<dyn CommandHandler + Send + Sync> =
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new()));
        registry.register(cmd.clone(), handler);

        let fetched = registry.get(&cmd.id).expect("registered");
        assert_eq!(fetched, &cmd);
    }

    /// `handler` returns a handle that resolves to the registered
    /// implementation — the stub's `execute` returns its captured
    /// effects.
    #[test]
    fn handler_returns_the_registered_implementation() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        let effects = vec![Effect::EmitNotification {
            level: crate::effect::NotificationLevel::Info,
            message: "saved".to_string(),
        }];
        let handler: Arc<dyn CommandHandler + Send + Sync> =
            Arc::new(StubHandler::new(cmd.id.clone(), effects.clone()));
        registry.register(cmd.clone(), handler);

        let fetched = registry.handler(&cmd.id).expect("handler");
        assert_eq!(fetched.id(), cmd.id);
        let result =
            futures::executor::block_on(fetched.execute(&sample_context())).expect("execute");
        assert_eq!(result, effects);
    }

    /// `list` returns every registered command (any order).
    #[test]
    fn list_returns_every_registered_command() {
        let mut registry = CommandRegistry::new();
        let a = save_command();
        let b = palette_command();
        registry.register(
            a.clone(),
            Arc::new(StubHandler::new(a.id.clone(), Vec::new())),
        );
        registry.register(
            b.clone(),
            Arc::new(StubHandler::new(b.id.clone(), Vec::new())),
        );
        let listed: Vec<&CommandId> = registry.list().iter().map(|c| &c.id).collect();
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&&a.id));
        assert!(listed.contains(&&b.id));
    }

    // ── Registry: scope-filter listing ────────────────────────────

    /// `list_for_scope` includes commands whose scope intersects
    /// the filter and excludes those that do not.
    #[test]
    fn list_for_scope_filters_on_intersection() {
        let mut registry = CommandRegistry::new();
        let editor_cmd = save_command();
        let welcome_cmd = welcome_command();
        registry.register(
            editor_cmd.clone(),
            Arc::new(StubHandler::new(editor_cmd.id.clone(), Vec::new())),
        );
        registry.register(
            welcome_cmd.clone(),
            Arc::new(StubHandler::new(welcome_cmd.id.clone(), Vec::new())),
        );

        // User in the editor: editor command shows; welcome
        // command does not.
        let editor_filter = CommandScopeFilter {
            home: true,
            editor_focused: true,
            ..CommandScopeFilter::empty()
        };
        let listed: Vec<&CommandId> = registry
            .list_for_scope(editor_filter)
            .iter()
            .map(|c| &c.id)
            .collect();
        assert_eq!(listed, vec![&editor_cmd.id]);

        // User on welcome: welcome command shows; editor does not.
        let welcome_filter = CommandScopeFilter {
            welcome: true,
            ..CommandScopeFilter::empty()
        };
        let listed: Vec<&CommandId> = registry
            .list_for_scope(welcome_filter)
            .iter()
            .map(|c| &c.id)
            .collect();
        assert_eq!(listed, vec![&welcome_cmd.id]);
    }

    // ── Registry: keybinding lookup ───────────────────────────────

    /// Positive path: the binding matches the command's
    /// `default_keybinding`, and the scope filter admits the
    /// command.
    #[test]
    fn lookup_by_keybinding_returns_match_when_scope_admits() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        registry.register(
            cmd.clone(),
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new())),
        );

        let kb = KeyBinding::new(Modifiers::CTRL, Key::Char('s'));
        let filter = CommandScopeFilter {
            editor_focused: true,
            ..CommandScopeFilter::empty()
        };
        let found = registry.lookup_by_keybinding(&kb, filter).expect("match");
        assert_eq!(found.id, cmd.id);
    }

    /// Negative path: same binding, but the scope filter does not
    /// admit the command — no match.
    #[test]
    fn lookup_by_keybinding_returns_none_when_scope_excludes() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        registry.register(
            cmd.clone(),
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new())),
        );

        let kb = KeyBinding::new(Modifiers::CTRL, Key::Char('s'));
        // Filter only on welcome — the editor.save command is not
        // welcome-scoped.
        let filter = CommandScopeFilter {
            welcome: true,
            ..CommandScopeFilter::empty()
        };
        assert!(registry.lookup_by_keybinding(&kb, filter).is_none());
    }

    /// Negative path: scope admits the command but the binding
    /// does not match.
    #[test]
    fn lookup_by_keybinding_returns_none_when_binding_differs() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        registry.register(
            cmd.clone(),
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new())),
        );

        let unrelated = KeyBinding::new(Modifiers::CTRL, Key::Char('q'));
        let filter = CommandScopeFilter::everywhere();
        assert!(registry.lookup_by_keybinding(&unrelated, filter).is_none());
    }

    /// A command with no `default_keybinding` is never returned by
    /// `lookup_by_keybinding`, even when the scope admits it.
    #[test]
    fn lookup_by_keybinding_skips_commands_without_default_binding() {
        let mut registry = CommandRegistry::new();
        let cmd = welcome_command(); // default_keybinding: None
        registry.register(
            cmd.clone(),
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new())),
        );
        let kb = KeyBinding::new(Modifiers::CTRL, Key::Char('s'));
        assert!(registry
            .lookup_by_keybinding(&kb, CommandScopeFilter::everywhere())
            .is_none());
    }

    // ── Registry: overwrite-on-reregister ────────────────────────

    /// Re-registering the same id replaces both the command record
    /// and the handler.
    #[test]
    fn re_register_overwrites_command_and_handler() {
        let mut registry = CommandRegistry::new();
        let id = CommandId::new("editor.save");
        let original = Command::new(
            id.clone(),
            "Save",
            CommandCategory::File,
            None,
            CommandScope::home_only(),
            None,
        );
        registry.register(original, Arc::new(StubHandler::new(id.clone(), Vec::new())));
        assert_eq!(registry.len(), 1);

        // Replacement: different label, different effect set.
        let replacement_effects = vec![Effect::EmitNotification {
            level: crate::effect::NotificationLevel::Warning,
            message: "saved (v2)".to_string(),
        }];
        let replacement = Command::new(
            id.clone(),
            "Save (v2)",
            CommandCategory::File,
            None,
            CommandScope::home_only(),
            None,
        );
        registry.register(
            replacement,
            Arc::new(StubHandler::new(id.clone(), replacement_effects.clone())),
        );

        // Still one entry — the id collided.
        assert_eq!(registry.len(), 1);
        // Command record reflects the replacement.
        let fetched = registry.get(&id).expect("present");
        assert_eq!(fetched.label, "Save (v2)");
        // Handler reflects the replacement.
        let handler = registry.handler(&id).expect("handler");
        let result =
            futures::executor::block_on(handler.execute(&sample_context())).expect("execute");
        assert_eq!(result, replacement_effects);
    }

    // ── Registry: unregister ─────────────────────────────────────

    /// `unregister` returns `true` for a registered id and clears
    /// the entry; `false` for an unknown id.
    #[test]
    fn unregister_clears_and_reports() {
        let mut registry = CommandRegistry::new();
        let cmd = save_command();
        registry.register(
            cmd.clone(),
            Arc::new(StubHandler::new(cmd.id.clone(), Vec::new())),
        );
        assert!(registry.unregister(&cmd.id));
        assert!(registry.get(&cmd.id).is_none());
        assert!(registry.is_empty());

        // Idempotent on the second call: now returns `false`.
        assert!(!registry.unregister(&cmd.id));
        // Unknown id: also `false`.
        assert!(!registry.unregister(&CommandId::new("never.registered")));
    }

    // ── CommandError ─────────────────────────────────────────────

    /// `Display` glues the variant's category prefix to the id /
    /// message. Locked so log readers and error UIs do not silently
    /// drift.
    #[test]
    fn command_error_display_carries_id_and_message() {
        let id = CommandId::new("editor.save");
        assert_eq!(
            CommandError::NotFound(id.clone()).to_string(),
            "command not found: editor.save"
        );
        assert_eq!(
            CommandError::OutOfScope(id).to_string(),
            "command out of scope: editor.save"
        );
        assert_eq!(
            CommandError::Internal("disk full".to_string()).to_string(),
            "internal command error: disk full"
        );
    }

    /// `CommandError` is `#[non_exhaustive]`. Lock the wildcard-arm
    /// contract.
    #[test]
    fn command_error_match_uses_wildcard_arm() {
        let cases = [
            CommandError::NotFound(CommandId::new("a")),
            CommandError::OutOfScope(CommandId::new("b")),
            CommandError::Internal("x".to_string()),
        ];
        for e in cases {
            #[allow(unreachable_patterns)]
            let label = match e {
                CommandError::NotFound(_) => "not_found",
                CommandError::OutOfScope(_) => "out_of_scope",
                CommandError::Internal(_) => "internal",
                _ => "unknown",
            };
            assert!(matches!(label, "not_found" | "out_of_scope" | "internal"));
        }
    }
}
