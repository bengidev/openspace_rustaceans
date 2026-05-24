//! Terminal mode crate.
//!
//! Hosts the interactive shell surface plus the AI sidekick wiring.
//! Implementation lives in later slices; this skeleton exists so the
//! workspace builds and tests run.

/// Default terminal text font.
pub const DEFAULT_FONT: iced::Font = iced::Font::with_name("JetBrains Mono");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_uses_bundled_monospace_family() {
        assert_eq!(DEFAULT_FONT, iced::Font::with_name("JetBrains Mono"));
    }
}
