//! Theme token data containers.
//!
//! These are pure data shapes — no behaviour, no IO. Each type carries
//! the field set the downstream consumer needs to render its surface:
//!
//! - [`Appearance`]       — coarse light/dark/high-contrast tag, used
//!   by the home shell to pick the right base palette.
//! - [`ThemeId`]           — stable slug (`"default-dark"`,
//!   `"high-contrast"`). String newtype, not a [`uuid::Uuid`]: themes
//!   are identified by what they *are*, not by per-install identity,
//!   so the id survives reinstalls and config syncs.
//! - [`UiTokens`]          — chrome colours the home shell paints
//!   (background, foreground, accent, surface, border).
//! - [`SyntaxTokens`]      — the small but realistic colour set the
//!   editor mode's syntax highlighter consumes.
//! - [`TerminalPalette`]   — the standard 16-colour ANSI grid the
//!   terminal mode renders against.
//!
//! Field sets are intentionally narrow: enough to drive a real Iced
//! theme later, but not so wide that adding a token in PRD-04 means
//! revisiting every consumer. Every type is `#[non_exhaustive]` so
//! growing the set stays a non-breaking change for downstream callers.

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::theme::color::Color;

/// Coarse appearance tag for a theme.
///
/// The home shell reads this to pick a base scrollbar style, focus
/// ring, and other chrome details that depend on the overall light /
/// dark stance rather than on individual token values. Themes that
/// straddle the line declare the closest match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    /// Light backgrounds, dark foregrounds.
    Light,
    /// Dark backgrounds, light foregrounds.
    Dark,
    /// Maximum-contrast variant for accessibility — distinct from a
    /// bare `Dark` so the shell can apply extra outline / focus-ring
    /// rules without sniffing colour distances at runtime.
    HighContrast,
}

assert_impl_all!(Appearance: Send, Sync);

/// Stable theme identifier — a kebab-case slug like `"default-dark"`
/// or `"high-contrast"`.
///
/// Newtype over [`String`] (not [`uuid::Uuid`]) because themes are a
/// curated, named set: `"default-dark"` should mean the same theme
/// across machines, and a uuid would force every install to mint its
/// own. The slug is what users reference in config files.
///
/// `#[serde(transparent)]` keeps the wire form a bare string so
/// theme files read naturally:
///
/// ```toml
/// id = "default-dark"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThemeId(String);

impl ThemeId {
    /// Wrap an existing slug. The Domain layer does *not* validate
    /// the slug shape — that is the loader's job (PRD-04). Keeping
    /// the constructor permissive means tests, snapshots, and
    /// in-memory fixtures stay terse.
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

impl std::fmt::Display for ThemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

assert_impl_all!(ThemeId: Send, Sync);

/// UI chrome colours.
///
/// The five-token surface is the smallest set that lets the home shell
/// paint a real (non-placeholder) Iced theme: a base canvas, the text
/// it carries, an accent for selection / focus, an elevated surface
/// for panels and popovers, and a border colour for separators.
///
/// More tokens (hover, pressed, disabled, …) arrive in PRD-04 once the
/// shell actually has the surfaces that need them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UiTokens {
    /// Window canvas behind every other surface.
    pub background: Color,
    /// Default text colour rendered on `background`.
    pub foreground: Color,
    /// Selection, focus ring, and primary action highlight.
    pub accent: Color,
    /// Elevated surface — panels, popovers, the command palette.
    pub surface: Color,
    /// Hairline borders and separators.
    pub border: Color,
}

assert_impl_all!(UiTokens: Send, Sync);

/// Syntax-highlight token colours.
///
/// The set is deliberately small and realistic: enough categories to
/// produce a recognisable highlight for a typical Rust / Markdown
/// buffer without committing to a Tree-sitter capture vocabulary
/// before the editor mode lands. PRD-04 will expand this once the
/// highlighter chooses its capture set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SyntaxTokens {
    /// Reserved keywords (`fn`, `let`, `match`, `if`, …).
    pub keyword: Color,
    /// String and char literals.
    pub string: Color,
    /// Line and block comments. Doc-comments share this colour at
    /// this stage; PRD-04 may split them.
    pub comment: Color,
    /// Numeric literals.
    pub number: Color,
    /// Function names at definition and call sites.
    pub function: Color,
    /// Type names — structs, enums, traits, type aliases.
    pub r#type: Color,
    /// Local and parameter identifiers.
    pub variable: Color,
    /// Operators and punctuation that the highlighter chooses to
    /// colour distinctly from surrounding code.
    pub operator: Color,
}

assert_impl_all!(SyntaxTokens: Send, Sync);

/// 16-colour ANSI palette for the terminal mode.
///
/// Field names mirror the conventional ANSI 0..=15 grid — eight
/// "normal" colours followed by their bright variants. The terminal
/// renderer indexes into this palette by ANSI colour number; the
/// `#[non_exhaustive]` rule still applies because PRD-04 may grow the
/// type with a 256-colour or true-colour fallback strategy and we want
/// that to stay non-breaking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TerminalPalette {
    /// ANSI 0 — black.
    pub black: Color,
    /// ANSI 1 — red.
    pub red: Color,
    /// ANSI 2 — green.
    pub green: Color,
    /// ANSI 3 — yellow.
    pub yellow: Color,
    /// ANSI 4 — blue.
    pub blue: Color,
    /// ANSI 5 — magenta.
    pub magenta: Color,
    /// ANSI 6 — cyan.
    pub cyan: Color,
    /// ANSI 7 — white.
    pub white: Color,
    /// ANSI 8 — bright black (rendered grey by most terminals).
    pub bright_black: Color,
    /// ANSI 9 — bright red.
    pub bright_red: Color,
    /// ANSI 10 — bright green.
    pub bright_green: Color,
    /// ANSI 11 — bright yellow.
    pub bright_yellow: Color,
    /// ANSI 12 — bright blue.
    pub bright_blue: Color,
    /// ANSI 13 — bright magenta.
    pub bright_magenta: Color,
    /// ANSI 14 — bright cyan.
    pub bright_cyan: Color,
    /// ANSI 15 — bright white.
    pub bright_white: Color,
}

assert_impl_all!(TerminalPalette: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    /// `Appearance` serialises as the kebab-case slug, not as a Rust
    /// variant name — locks the wire form so theme files written
    /// today keep loading after later refactors.
    #[test]
    fn appearance_serialises_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Appearance::Light).expect("serialize"),
            "\"light\""
        );
        assert_eq!(
            serde_json::to_string(&Appearance::Dark).expect("serialize"),
            "\"dark\""
        );
        assert_eq!(
            serde_json::to_string(&Appearance::HighContrast).expect("serialize"),
            "\"high-contrast\""
        );
    }

    #[test]
    fn appearance_round_trips_every_variant() {
        for original in [
            Appearance::Light,
            Appearance::Dark,
            Appearance::HighContrast,
        ] {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: Appearance = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded);
        }
    }

    /// `ThemeId` is `#[serde(transparent)]` so the wire form is the
    /// bare slug, not `{"0": "..."}`. Lock that explicitly.
    #[test]
    fn theme_id_wire_form_is_a_bare_string() {
        let id = ThemeId::new("default-dark");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"default-dark\"");
    }

    #[test]
    fn theme_id_round_trip() {
        let id = ThemeId::new("high-contrast");
        let json = serde_json::to_string(&id).expect("serialize");
        let decoded: ThemeId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, decoded);
        assert_eq!(decoded.as_str(), "high-contrast");
    }

    fn sample_ui_tokens() -> UiTokens {
        UiTokens {
            background: Color::rgb(0x1E, 0x1E, 0x2E),
            foreground: Color::rgb(0xCD, 0xD6, 0xF4),
            accent: Color::rgb(0x89, 0xB4, 0xFA),
            surface: Color::rgb(0x31, 0x32, 0x44),
            border: Color::new(0x45, 0x47, 0x5A, 0xCC),
        }
    }

    fn sample_syntax_tokens() -> SyntaxTokens {
        SyntaxTokens {
            keyword: Color::rgb(0xCB, 0xA6, 0xF7),
            string: Color::rgb(0xA6, 0xE3, 0xA1),
            comment: Color::rgb(0x6C, 0x70, 0x86),
            number: Color::rgb(0xFA, 0xB3, 0x87),
            function: Color::rgb(0x89, 0xB4, 0xFA),
            r#type: Color::rgb(0xF9, 0xE2, 0xAF),
            variable: Color::rgb(0xCD, 0xD6, 0xF4),
            operator: Color::rgb(0x94, 0xE2, 0xD5),
        }
    }

    fn sample_terminal_palette() -> TerminalPalette {
        let c = Color::rgb;
        TerminalPalette {
            black: c(0x00, 0x00, 0x00),
            red: c(0xCC, 0x00, 0x00),
            green: c(0x4E, 0x9A, 0x06),
            yellow: c(0xC4, 0xA0, 0x00),
            blue: c(0x34, 0x65, 0xA4),
            magenta: c(0x75, 0x50, 0x7B),
            cyan: c(0x06, 0x98, 0x9A),
            white: c(0xD3, 0xD7, 0xCF),
            bright_black: c(0x55, 0x57, 0x53),
            bright_red: c(0xEF, 0x29, 0x29),
            bright_green: c(0x8A, 0xE2, 0x34),
            bright_yellow: c(0xFC, 0xE9, 0x4F),
            bright_blue: c(0x72, 0x9F, 0xCF),
            bright_magenta: c(0xAD, 0x7F, 0xA8),
            bright_cyan: c(0x34, 0xE2, 0xE2),
            bright_white: c(0xEE, 0xEE, 0xEC),
        }
    }

    #[test]
    fn ui_tokens_round_trip() {
        let original = sample_ui_tokens();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: UiTokens = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn syntax_tokens_round_trip() {
        let original = sample_syntax_tokens();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: SyntaxTokens = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `r#type` is the raw identifier in Rust source but should
    /// serialise as the bare key `"type"` so theme files stay readable.
    /// Lock that — silent rename to `r#type` would force theme authors
    /// to escape the keyword.
    #[test]
    fn syntax_tokens_emit_type_key_unprefixed() {
        let tokens = sample_syntax_tokens();
        let value: serde_json::Value = serde_json::to_value(&tokens).expect("serialize");
        let object = value.as_object().expect("object");
        assert!(
            object.contains_key("type"),
            "expected `type` key, got: {object:?}"
        );
        assert!(!object.contains_key("r#type"));
    }

    #[test]
    fn terminal_palette_round_trip() {
        let original = sample_terminal_palette();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: TerminalPalette = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// All 16 ANSI slots must be present on the wire — locks the field
    /// set so a casual rename does not orphan a colour slot in stored
    /// theme files.
    #[test]
    fn terminal_palette_wire_has_all_sixteen_slots() {
        let palette = sample_terminal_palette();
        let value: serde_json::Value = serde_json::to_value(&palette).expect("serialize");
        let object = value.as_object().expect("object");
        for key in [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "white",
            "bright_black",
            "bright_red",
            "bright_green",
            "bright_yellow",
            "bright_blue",
            "bright_magenta",
            "bright_cyan",
            "bright_white",
        ] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(object.len(), 16);
    }
}
