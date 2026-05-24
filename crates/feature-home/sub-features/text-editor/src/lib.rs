//! Editor mode crate.
//!
//! Text/code editor with inline AI assistance. Implementation lives in
//! later slices; this skeleton exists so the workspace builds and tests
//! run.

/// Default editor text font.
pub const DEFAULT_FONT: iced::Font = iced::Font::with_name("JetBrains Mono");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_uses_bundled_monospace_family() {
        assert_eq!(DEFAULT_FONT, iced::Font::with_name("JetBrains Mono"));
    }
}
