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

pub mod error;
pub mod id;
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
