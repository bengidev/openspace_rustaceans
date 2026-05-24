//! Application startup configuration.

use std::borrow::Cow;

use iced::{font, Font, Settings};

/// Bundled monospace family used by editor and terminal surfaces.
pub const MONOSPACE_FAMILY: &str = "JetBrains Mono";

/// Iced handle for the bundled monospace regular face.
pub const MONOSPACE_FONT: Font = Font::with_name(MONOSPACE_FAMILY);

/// Iced handle for the bundled monospace bold face.
pub const MONOSPACE_BOLD_FONT: Font = Font {
    weight: font::Weight::Bold,
    ..MONOSPACE_FONT
};

/// Iced handle for the bundled monospace italic face.
pub const MONOSPACE_ITALIC_FONT: Font = Font {
    style: font::Style::Italic,
    ..MONOSPACE_FONT
};

/// Iced handle for the bundled monospace bold italic face.
pub const MONOSPACE_BOLD_ITALIC_FONT: Font = Font {
    weight: font::Weight::Bold,
    style: font::Style::Italic,
    ..MONOSPACE_FONT
};

/// Embedded font faces available during startup on every supported platform.
pub const BUNDLED_FONT_BYTES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
];

/// Register bundled fonts while keeping the UI chrome on Iced's system default.
#[must_use]
pub fn iced_startup_settings() -> Settings {
    Settings {
        fonts: BUNDLED_FONT_BYTES
            .iter()
            .map(|bytes| Cow::Borrowed(*bytes))
            .collect(),
        default_font: Font::DEFAULT,
        ..Settings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_are_registered_without_overriding_ui_default() {
        let settings = iced_startup_settings();

        assert_eq!(settings.fonts.len(), 4);
        assert_eq!(settings.default_font, Font::DEFAULT);
        assert_eq!(MONOSPACE_FONT, Font::with_name(MONOSPACE_FAMILY));
    }

    #[test]
    fn editor_and_terminal_can_reference_bundled_monospace_faces() {
        assert_eq!(MONOSPACE_BOLD_FONT.weight, font::Weight::Bold);
        assert_eq!(MONOSPACE_ITALIC_FONT.style, font::Style::Italic);
        assert_eq!(MONOSPACE_BOLD_ITALIC_FONT.weight, font::Weight::Bold);
        assert_eq!(MONOSPACE_BOLD_ITALIC_FONT.style, font::Style::Italic);
    }
}
