//! Shared library for OpenSpace.
//!
//! Houses the Domain types and trait surfaces every feature crate
//! depends on — IDs, workspace identity, error scaffolding. Pure data
//! and traits only: no `iced`, no `tokio`, no IO. Implementations live
//! in feature crates or dedicated infrastructure crates.
//!
//! Additions are deliberate. The bar is "two or more feature crates
//! consume it, and it cannot live in a downstream crate without
//! cyclic dependencies." Speculative types invite drift.
//!
//! # Module layout
//!
//! The full PRD-01 module surface is declared up front so later
//! slices land as additive changes inside an already-stable path:
//!
//! - [`id`]         — domain id newtypes.
//! - [`workspace`]  — workspace identity (`Workspace`, `WorkspaceRef`).
//! - [`ai`]         — AI provider Domain, trait surface, `AiError`.
//! - [`agent`]      — agent loop bookkeeping types.
//! - [`tool`]       — tool trait surface and supporting types.
//! - [`effect`]     — exhaustive list of side-effects tools and commands can request.
//! - [`sandbox`]    — sandbox policy and decision types.
//! - [`command`]    — command-palette types and handler trait.
//! - [`keybinding`] — key-binding parser and profile types.
//! - [`persistence`] — repository / store trait surface and
//!   `PersistenceError`.
//! - [`settings`]   — typed shape of `settings.toml`.
//! - [`theme`]      — theme tokens (UI, syntax, terminal palette).
//!
//! Modules implemented in this slice: `id`, `workspace`. The rest are
//! intentionally empty and arrive in later issues on the PRD-01
//! epic (#2).

pub mod agent;
pub mod ai;
pub mod command;
pub mod effect;
pub mod id;
pub mod keybinding;
pub mod persistence;
pub mod sandbox;
pub mod settings;
pub mod theme;
pub mod tool;
pub mod workspace;

/// Test fixtures shared across the workspace — `ScriptedProvider`,
/// `MockTool`, `TempWorkspace`, etc. Currently empty; later PRD-01
/// slices populate it. Gated behind `test-support` so release builds
/// never carry the fixtures.
#[cfg(feature = "test-support")]
pub mod test_support {}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {}
}
