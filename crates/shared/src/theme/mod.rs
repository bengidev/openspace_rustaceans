//! Theme tokens — `Theme`, `ThemeId`, `Appearance`, `UiTokens`,
//! `SyntaxTokens`, `TerminalPalette`, `Color`.
//!
//! This module owns the *shape* of a theme; loading themes from disk
//! lives in PRD-04. The Domain layer carries:
//!
//! - [`color::Color`] — 8-bit RGBA value with a TOML/JSON-friendly
//!   parser. Custom parser, no `palette` / `csscolorparser` dependency.
//! - [`tokens::Appearance`], [`tokens::ThemeId`], [`tokens::UiTokens`],
//!   [`tokens::SyntaxTokens`], [`tokens::TerminalPalette`] — pure data
//!   shapes consumed by the home shell, the editor highlighter, and
//!   the terminal renderer respectively.

pub mod color;
pub mod tokens;

pub use color::Color;
pub use tokens::{Appearance, SyntaxTokens, TerminalPalette, ThemeId, UiTokens};
