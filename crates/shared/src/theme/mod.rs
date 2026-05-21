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
//! - [`Theme`] — the container that bundles a slug, a display name,
//!   an appearance tag, and the three token groups into a single
//!   serde-round-trippable record.

pub mod color;
pub mod tokens;

pub use color::Color;
pub use tokens::{Appearance, SyntaxTokens, TerminalPalette, ThemeId, UiTokens};

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

/// A complete theme: identity, appearance tag, and every token group
/// the modes need to render.
///
/// `Theme` is the unit theme files load to and the unit the home shell
/// hands to mode adapters. The container is intentionally narrow — id,
/// human-friendly name, appearance, and the three token groups —
/// because every consumer reads from one of these three groups, never
/// from a fourth ad-hoc surface.
///
/// `#[non_exhaustive]` so PRD-04 can grow the record (for example with
/// an explicit `version`, a `parent` slug for inheritance, or a
/// `metadata` blob) without a breaking change for downstream callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Theme {
    /// Stable slug. Matches the on-disk filename in PRD-04.
    pub id: ThemeId,
    /// Human-friendly label rendered in the theme picker. Independent
    /// of the slug so users see "Default Dark" while config files
    /// reference `"default-dark"`.
    pub display_name: String,
    /// Coarse light / dark / high-contrast tag. The home shell reads
    /// this to pick base chrome rules.
    pub appearance: Appearance,
    /// UI chrome colours (background, foreground, accent, …).
    pub ui: UiTokens,
    /// Editor syntax-highlight token colours.
    pub syntax: SyntaxTokens,
    /// Terminal mode's ANSI 0..=15 palette.
    pub terminal: TerminalPalette,
}

impl Theme {
    /// Bundle a fully populated theme from its parts.
    ///
    /// Wrapping construction in a method (instead of relying on struct
    /// literals) is what makes `#[non_exhaustive]` meaningful: callers
    /// cannot accidentally depend on the field set, so adding a new
    /// optional field later is non-breaking.
    #[must_use]
    pub fn new(
        id: ThemeId,
        display_name: String,
        appearance: Appearance,
        ui: UiTokens,
        syntax: SyntaxTokens,
        terminal: TerminalPalette,
    ) -> Self {
        Self {
            id,
            display_name,
            appearance,
            ui,
            syntax,
            terminal,
        }
    }
}

assert_impl_all!(Theme: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a fully populated `Theme` so the round-trip assertion
    /// exercises every field of every nested token group at once.
    fn sample_theme() -> Theme {
        Theme::new(
            ThemeId::new("default-dark"),
            "Default Dark".to_string(),
            Appearance::Dark,
            UiTokens {
                background: Color::rgb(0x1E, 0x1E, 0x2E),
                foreground: Color::rgb(0xCD, 0xD6, 0xF4),
                accent: Color::rgb(0x89, 0xB4, 0xFA),
                surface: Color::rgb(0x31, 0x32, 0x44),
                border: Color::new(0x45, 0x47, 0x5A, 0xCC),
            },
            SyntaxTokens {
                keyword: Color::rgb(0xCB, 0xA6, 0xF7),
                string: Color::rgb(0xA6, 0xE3, 0xA1),
                comment: Color::rgb(0x6C, 0x70, 0x86),
                number: Color::rgb(0xFA, 0xB3, 0x87),
                function: Color::rgb(0x89, 0xB4, 0xFA),
                r#type: Color::rgb(0xF9, 0xE2, 0xAF),
                variable: Color::rgb(0xCD, 0xD6, 0xF4),
                operator: Color::rgb(0x94, 0xE2, 0xD5),
            },
            TerminalPalette {
                black: Color::rgb(0x00, 0x00, 0x00),
                red: Color::rgb(0xCC, 0x00, 0x00),
                green: Color::rgb(0x4E, 0x9A, 0x06),
                yellow: Color::rgb(0xC4, 0xA0, 0x00),
                blue: Color::rgb(0x34, 0x65, 0xA4),
                magenta: Color::rgb(0x75, 0x50, 0x7B),
                cyan: Color::rgb(0x06, 0x98, 0x9A),
                white: Color::rgb(0xD3, 0xD7, 0xCF),
                bright_black: Color::rgb(0x55, 0x57, 0x53),
                bright_red: Color::rgb(0xEF, 0x29, 0x29),
                bright_green: Color::rgb(0x8A, 0xE2, 0x34),
                bright_yellow: Color::rgb(0xFC, 0xE9, 0x4F),
                bright_blue: Color::rgb(0x72, 0x9F, 0xCF),
                bright_magenta: Color::rgb(0xAD, 0x7F, 0xA8),
                bright_cyan: Color::rgb(0x34, 0xE2, 0xE2),
                bright_white: Color::rgb(0xEE, 0xEE, 0xEC),
            },
        )
    }

    /// Headline acceptance criterion for issue #28: a fully populated
    /// `Theme` round-trips through serde unchanged.
    #[test]
    fn theme_round_trips_through_json() {
        let original = sample_theme();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Theme = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Locks the wire field names. Persistence (PRD-03 / PRD-04)
    /// depends on the JSON keys staying exactly these names so a Rust
    /// rename refactor cannot silently break stored theme files.
    #[test]
    fn theme_wire_field_names_are_stable() {
        let value: serde_json::Value =
            serde_json::to_value(sample_theme()).expect("serialize as value");
        let object = value.as_object().expect("object");
        for key in [
            "id",
            "display_name",
            "appearance",
            "ui",
            "syntax",
            "terminal",
        ] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(object.len(), 6, "no unexpected fields");
    }
}
