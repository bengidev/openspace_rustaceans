//! Bridge OpenSpace theme tokens into the UI runtime theme surface.

use crate::{Color, Theme, ThemeChange, ThemeStore};

/// Iced-facing theme for one window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowIcedTheme {
    theme_id: Option<String>,
    iced: iced::Theme,
}

impl WindowIcedTheme {
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            theme_id: Some(theme.id.as_str().to_string()),
            iced: iced_theme_from_theme(theme),
        }
    }

    #[must_use]
    pub fn fallback() -> Self {
        Self {
            theme_id: None,
            iced: iced::Theme::Dark,
        }
    }

    #[must_use]
    pub fn theme_id(&self) -> Option<&str> {
        self.theme_id.as_deref()
    }

    #[must_use]
    pub fn iced(&self) -> &iced::Theme {
        &self.iced
    }

    #[must_use]
    pub fn into_iced(self) -> iced::Theme {
        self.iced
    }
}

/// Mutable per-window cache refreshed during normal update handling.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowThemeState {
    current: WindowIcedTheme,
}

impl WindowThemeState {
    #[must_use]
    pub fn from_store(store: &ThemeStore) -> Self {
        Self {
            current: iced_theme_from_store(store),
        }
    }

    #[must_use]
    pub fn current(&self) -> &WindowIcedTheme {
        &self.current
    }

    pub fn apply_theme_change(&mut self, store: &ThemeStore, change: &ThemeChange) -> bool {
        if matches!(change, ThemeChange::ActiveThemeChanged(_)) {
            let next = iced_theme_from_store(store);
            if next != self.current {
                self.current = next;
                return true;
            }
        }
        false
    }
}

#[must_use]
pub fn iced_theme_from_store(store: &ThemeStore) -> WindowIcedTheme {
    store
        .active_theme()
        .and_then(|id| store.get(id.as_str()))
        .as_ref()
        .map(WindowIcedTheme::from_theme)
        .unwrap_or_else(WindowIcedTheme::fallback)
}

#[must_use]
pub fn iced_theme_from_theme(theme: &Theme) -> iced::Theme {
    iced::Theme::custom(
        theme.metadata.display_name.clone(),
        iced::theme::Palette {
            background: to_iced(theme.ui.background),
            text: to_iced(theme.ui.foreground),
            primary: to_iced(theme.ui.accent),
            success: to_iced(theme.terminal.green),
            warning: to_iced(theme.terminal.yellow),
            danger: to_iced(theme.terminal.red),
        },
    )
}

fn to_iced(color: Color) -> iced::Color {
    iced::Color::from_rgba8(color.r, color.g, color.b, f32::from(color.a) / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bundled_themes, ThemeId};

    #[test]
    fn builds_iced_theme_from_every_bundled_theme() {
        for theme in bundled_themes() {
            let theme = theme.expect("bundled theme parses");
            let iced = iced_theme_from_theme(&theme);
            let palette = iced.palette();
            assert_eq!(palette.background, to_iced(theme.ui.background));
            assert_eq!(palette.text, to_iced(theme.ui.foreground));
            assert_eq!(palette.primary, to_iced(theme.ui.accent));
        }
    }

    #[test]
    fn window_theme_state_updates_on_active_theme_change() {
        let store = ThemeStore::bundled();
        store.set_active_theme(Some(ThemeId::new("default-dark")));
        let mut state = WindowThemeState::from_store(&store);

        store.set_active_theme(Some(ThemeId::new("default-light")));
        let changed = state.apply_theme_change(
            &store,
            &ThemeChange::ActiveThemeChanged(Some(ThemeId::new("default-light"))),
        );

        assert!(changed);
        assert_eq!(state.current().theme_id(), Some("default-light"));
    }
}
