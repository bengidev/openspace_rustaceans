//! Theme tokens — `Theme`, `ThemeId`, `Appearance`, `UiTokens`,
//! `SyntaxTokens`, `TerminalPalette`, `Color`.
//!
//! This module owns the *shape* of a theme; loading themes from disk
//! lives in PRD-04. The Domain layer carries:
//!
//! - [`color::Color`] — 8-bit RGBA value with a TOML/JSON-friendly
//!   parser. Custom parser, no `palette` / `csscolorparser` dependency.

pub mod color;

pub use color::Color;
