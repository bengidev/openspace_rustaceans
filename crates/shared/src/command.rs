//! Command palette types — `Command`, `CommandId`, `CommandCategory`,
//! `CommandScope`, `CommandHandler` trait.
//!
//! Slice 4 lands a minimal [`CommandId`] shim so the [`keybinding`]
//! module (issue #30) can declare a `KeybindingProfile` whose binding
//! map keys on a stable command identifier without waiting for the
//! full command surface (slice 8). The shim is intentionally a string
//! newtype: it carries the same wire form a richer `CommandId` would
//! adopt later, so persisted profiles round-trip cleanly across the
//! upgrade.
//!
//! [`keybinding`]: crate::keybinding

// TODO(slice-8): replace this shim with the full command module
// (Command, CommandCategory, CommandScope, CommandHandler trait). The
// replacement keeps `CommandId` as the same string-newtype shape so
// downstream serialised state — including `KeybindingProfile` blobs —
// stays compatible.

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

/// Stable identifier for a command in the command palette.
///
/// String newtype rather than a uuid because commands are a curated,
/// named set: `editor.save`, `palette.toggle`, `chat.new` mean the
/// same thing across installs. The slug is what configs and key
/// bindings reference, so persisted state survives reinstalls.
///
/// `#[serde(transparent)]` keeps the wire form a bare string so
/// keybinding profiles serialise as plain `id -> [shortcut]` maps.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    /// Wrap a string slug. No validation in this slice; slice 8 will
    /// tighten the contract once the full command surface lands and
    /// the canonical slug grammar is locked.
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

assert_impl_all!(CommandId: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtype_round_trips_through_json_as_a_bare_string() {
        let id = CommandId::new("editor.save");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"editor.save\"");
        let decoded: CommandId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, id);
    }

    #[test]
    fn as_str_borrows_the_underlying_slug() {
        let id = CommandId::new("palette.toggle");
        assert_eq!(id.as_str(), "palette.toggle");
    }
}
