//! Application settings — the typed shape of `settings.toml`.
//!
//! [`Settings`] is the on-disk record `SettingsStore` (PRD-03 Slice 3)
//! reads, mutates, and writes back. This module owns the *shape* only;
//! filesystem IO, hot-reload, and atomic-write semantics live downstream
//! in the persistence crate.
//!
//! # Public surface
//!
//! - [`Settings`] — the top-level record. Seven fields, every one
//!   documented, derives `Debug + Clone + PartialEq + Default + Serialize
//!   + Deserialize`.
//! - [`ThemeMode`] — coarse light/dark/auto follow-system tag. Independent
//!   of [`crate::theme::Appearance`] because a theme declares its own
//!   appearance, while `ThemeMode` declares *which* of the user's two
//!   selected themes is active right now.
//! - [`FontSettings`] — UI/editor/terminal font family and size.
//!
//! # Privacy guard (US#7)
//!
//! `Settings` deliberately carries **no** API keys, tokens, or other
//! secret material. Credentials live in the OS keychain via
//! [`crate::ai::secret::SecretStore`] and are referenced from settings
//! (when they have to be referenced at all) by a
//! [`crate::ai::secret::SecretRef`] lookup handle, never by value. The
//! unit test `settings_carry_no_secret_field_names` enforces this at
//! the field-name level so a future field addition cannot silently
//! regress the invariant.
//!
//! # Wire stability
//!
//! Both JSON and TOML round-trips are pinned by tests. The TOML form is
//! authoritative — that is what users edit on disk — and the JSON form
//! is exercised in parallel because every other Domain type in this
//! crate locks its serde shape through JSON. The wire field names match
//! the Rust field names (snake_case) so users edit the file in the
//! shape they read in source.

use std::sync::{mpsc, Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;

use crate::keybinding::KeybindingProfileId;
use crate::theme::ThemeId;

// ─────────────────────────────────────────────────────────────────────
// ThemeMode — coarse selection between the user's two theme slots.
// Distinct from `theme::Appearance`: appearance describes a theme's
// inherent light/dark stance, ThemeMode describes which of the user's
// two configured themes the shell is currently using.
// ─────────────────────────────────────────────────────────────────────

/// Selection between the user's two configured theme slots.
///
/// The shell ships two theme picks per user — a light slot and a dark
/// slot, each pointing at a [`ThemeId`]. `ThemeMode` decides which of
/// those slots is active:
///
/// - [`ThemeMode::Light`] always uses `theme_id_light`.
/// - [`ThemeMode::Dark`] always uses `theme_id_dark`.
/// - [`ThemeMode::System`] follows the OS appearance setting, falling
///   through to the matching slot whenever the OS reports a change.
///
/// Distinct from [`crate::theme::Appearance`]: appearance is a property
/// of a *theme* (a high-contrast theme is `Appearance::HighContrast`
/// regardless of how the user picked it). `ThemeMode` is a property of
/// the *user's choice* between their two slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    /// Always use `theme_id_light`.
    Light,
    /// Always use `theme_id_dark`.
    Dark,
    /// Follow the OS appearance setting. Default — desktops without a
    /// system appearance report `light`, so users who never touch this
    /// setting see a sensible default without surprise dark-mode flips.
    #[default]
    System,
}

assert_impl_all!(ThemeMode: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// FontSettings — three independent font slots so the editor and the
// terminal can carry monospace stacks while the chrome stays on a
// proportional UI font. Sizes are f32 points; the renderer is the
// right place to clamp to a sensible range.
// ─────────────────────────────────────────────────────────────────────

/// Font family + size for each rendering surface.
///
/// Three independent slots so the chrome can carry a proportional UI
/// stack while the editor and terminal use monospace stacks. Each
/// `family` is a CSS-style fallback list (`"JetBrains Mono, Menlo,
/// monospace"`); the renderer parses and resolves it. Sizes are
/// device-independent points (`f32`).
///
/// `#[non_exhaustive]` so a later field (line height, ligature toggle,
/// per-platform overrides) can land without a breaking change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FontSettings {
    /// Font family stack for chrome and palette text.
    pub ui_family: String,
    /// UI font size in points.
    pub ui_size: f32,
    /// Font family stack for the editor mode buffer.
    pub editor_family: String,
    /// Editor font size in points.
    pub editor_size: f32,
    /// Font family stack for the terminal mode buffer.
    pub terminal_family: String,
    /// Terminal font size in points.
    pub terminal_size: f32,
}

impl Default for FontSettings {
    /// Defaults match the documented `settings.toml` empty-file
    /// behaviour: a system UI font for chrome, a monospace stack for
    /// the editor and terminal, all at conventional default sizes.
    /// Concrete family names stay generic (`"sans-serif"`,
    /// `"monospace"`) so the platform-default font picker resolves to
    /// each OS's conventional choice without baking a brand into the
    /// schema.
    fn default() -> Self {
        Self {
            ui_family: "sans-serif".to_string(),
            ui_size: 14.0,
            editor_family: "monospace".to_string(),
            editor_size: 13.0,
            terminal_family: "monospace".to_string(),
            terminal_size: 13.0,
        }
    }
}

assert_impl_all!(FontSettings: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Settings — the seven-field record locked by PRD-03. Field types
// reach for the typed handles already in this crate (ThemeId,
// KeybindingProfileId, ProviderId) instead of bare strings so the
// settings layer composes with the rest of the Domain vocabulary
// without an extra translation step.
// ─────────────────────────────────────────────────────────────────────

/// Application-wide settings — the typed shape of `settings.toml`.
///
/// Seven fields, locked by PRD-03:
///
/// 1. [`theme_mode`](Self::theme_mode) — light / dark / follow-system.
/// 2. [`theme_id_light`](Self::theme_id_light) — theme used when
///    `theme_mode` resolves to light.
/// 3. [`theme_id_dark`](Self::theme_id_dark) — theme used when
///    `theme_mode` resolves to dark.
/// 4. [`default_provider`](Self::default_provider) — provider id new
///    chats / tool calls dispatch against by default.
/// 5. [`font_settings`](Self::font_settings) — UI / editor / terminal
///    font slots.
/// 6. [`keybinding_profile`](Self::keybinding_profile) — active
///    [`crate::keybinding::KeybindingProfile`] id.
/// 7. [`update_check_enabled`](Self::update_check_enabled) — opt-in
///    flag for the update checker.
///
/// # Privacy
///
/// `Settings` carries **no** API keys, tokens, or secret material —
/// US#7 of PRD-03 is non-negotiable. Credentials live in the OS
/// keychain ([`crate::ai::secret::SecretStore`]); when settings need
/// to refer to a credential, they do so by
/// [`crate::ai::secret::SecretRef`] lookup handle, never by value. The
/// `settings_carry_no_secret_field_names` test guards the field set
/// against accidental regression.
///
/// # Defaults
///
/// [`Settings::default`] returns the shape an empty `settings.toml`
/// would imply: dark and light slots both pointing at the canonical
/// `default-dark` / `default-light` slugs, the `default` keybinding
/// profile, the `local` provider id, default fonts, and the update
/// check off. Loaders should treat the default as the substrate and
/// merge user overrides on top.
///
/// `#[non_exhaustive]` so later PRDs can grow the record (telemetry
/// opt-in, accessibility flags, language code, …) without breaking
/// downstream construction sites.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Settings {
    /// Coarse light / dark / follow-system selector. See
    /// [`ThemeMode`] for the semantics.
    pub theme_mode: ThemeMode,

    /// Theme used when [`theme_mode`](Self::theme_mode) resolves to
    /// light.
    pub theme_id_light: ThemeId,

    /// Theme used when [`theme_mode`](Self::theme_mode) resolves to
    /// dark.
    pub theme_id_dark: ThemeId,

    /// Provider that the agent loop dispatches to when no per-session
    /// override is set.
    ///
    /// Stored as a `String` because the canonical
    /// `ProviderId` newtype lives in the downstream
    /// `openspace-ai-providers` crate; importing it here would invert
    /// the dependency direction (Domain → Infrastructure). The
    /// settings layer treats this field as the *handle* the registry
    /// will resolve at boot. Loaders are expected to wrap it in
    /// `ProviderId::from(...)` at the boundary.
    pub default_provider: String,

    /// UI / editor / terminal font slots. See [`FontSettings`].
    pub font_settings: FontSettings,

    /// Active keybinding profile. Typed as
    /// [`KeybindingProfileId`] so loaders index into the profile
    /// registry through the same handle that `KeybindingProfile::id`
    /// carries.
    pub keybinding_profile: KeybindingProfileId,

    /// Whether the app contacts the update endpoint at startup.
    /// Off by default — opt-in network access is the privacy posture
    /// PRD-03 documents.
    pub update_check_enabled: bool,
}

impl Default for Settings {
    /// Defaults match the documented "empty `settings.toml`" shape.
    ///
    /// - `theme_mode = "system"` so users without a preference inherit
    ///   the OS appearance.
    /// - `theme_id_light = "default-light"`,
    ///   `theme_id_dark = "default-dark"` — the canonical slugs PRD-04
    ///   reserves for the bundled themes.
    /// - `default_provider = "local"` — the abstract local-engine
    ///   provider id, matching the secret-namespace examples in
    ///   `crate::ai::secret`.
    /// - `font_settings = FontSettings::default()`.
    /// - `keybinding_profile = "default"` — the canonical bundled
    ///   profile.
    /// - `update_check_enabled = false` — opt-in network access.
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::default(),
            theme_id_light: ThemeId::new("default-light"),
            theme_id_dark: ThemeId::new("default-dark"),
            default_provider: "local".to_string(),
            font_settings: FontSettings::default(),
            keybinding_profile: KeybindingProfileId::new("default"),
            update_check_enabled: false,
        }
    }
}

assert_impl_all!(Settings: Send, Sync);

/// Effective OS chrome appearance used by [`ThemeMode::System`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemAppearance {
    Light,
    Dark,
}

/// Event emitted when active-theme resolution picks a new theme id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveThemeChange {
    pub theme_id: ThemeId,
}

/// Mockable source for OS appearance.
pub trait SystemAppearanceSource: Send + Sync {
    fn current_appearance(&self) -> SystemAppearance;
    fn subscribe(&self) -> mpsc::Receiver<SystemAppearance>;
}

/// Safe fallback source for platforms without OS push notifications.
#[derive(Debug)]
pub struct PollingSystemAppearanceSource {
    appearance: Arc<RwLock<SystemAppearance>>,
    subscribers: Mutex<Vec<mpsc::Sender<SystemAppearance>>>,
}

impl PollingSystemAppearanceSource {
    #[must_use]
    pub fn new(initial: SystemAppearance) -> Self {
        Self {
            appearance: Arc::new(RwLock::new(initial)),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    pub fn poll_update(&self, appearance: SystemAppearance) {
        let mut current = self
            .appearance
            .write()
            .expect("PollingSystemAppearanceSource appearance RwLock poisoned");
        if *current == appearance {
            return;
        }
        *current = appearance;
        drop(current);
        self.subscribers
            .lock()
            .expect("PollingSystemAppearanceSource subscribers Mutex poisoned")
            .retain(|tx| tx.send(appearance).is_ok());
    }
}

impl SystemAppearanceSource for PollingSystemAppearanceSource {
    fn current_appearance(&self) -> SystemAppearance {
        *self
            .appearance
            .read()
            .expect("PollingSystemAppearanceSource appearance RwLock poisoned")
    }

    fn subscribe(&self) -> mpsc::Receiver<SystemAppearance> {
        let (tx, rx) = mpsc::channel();
        self.subscribers
            .lock()
            .expect("PollingSystemAppearanceSource subscribers Mutex poisoned")
            .push(tx);
        rx
    }
}

#[derive(Debug)]
pub struct ActiveThemeResolver<S> {
    settings: Settings,
    source: S,
    active_theme_id: ThemeId,
}

impl<S: SystemAppearanceSource> ActiveThemeResolver<S> {
    #[must_use]
    pub fn new(settings: Settings, source: S) -> Self {
        let active_theme_id = resolve_theme_id(&settings, source.current_appearance());
        Self {
            settings,
            source,
            active_theme_id,
        }
    }

    #[must_use]
    pub fn active_theme_id(&self) -> &ThemeId {
        &self.active_theme_id
    }

    #[must_use]
    pub fn subscribe_system_appearance(&self) -> mpsc::Receiver<SystemAppearance> {
        self.source.subscribe()
    }

    pub fn apply_system_appearance(
        &mut self,
        appearance: SystemAppearance,
    ) -> Option<ActiveThemeChange> {
        if self.settings.theme_mode != ThemeMode::System {
            return None;
        }
        let next = resolve_theme_id(&self.settings, appearance);
        if next == self.active_theme_id {
            return None;
        }
        self.active_theme_id = next.clone();
        Some(ActiveThemeChange { theme_id: next })
    }
}

#[must_use]
pub fn resolve_theme_id(settings: &Settings, system_appearance: SystemAppearance) -> ThemeId {
    match settings.theme_mode {
        ThemeMode::Light => settings.theme_id_light.clone(),
        ThemeMode::Dark => settings.theme_id_dark.clone(),
        ThemeMode::System => match system_appearance {
            SystemAppearance::Light => settings.theme_id_light.clone(),
            SystemAppearance::Dark => settings.theme_id_dark.clone(),
        },
    }
}

assert_impl_all!(SystemAppearance: Send, Sync);
assert_impl_all!(ActiveThemeChange: Send, Sync);
assert_impl_all!(PollingSystemAppearanceSource: Send, Sync);
assert_impl_all!(ActiveThemeResolver<PollingSystemAppearanceSource>: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MockSystemAppearanceSource {
        appearance: SystemAppearance,
    }

    impl MockSystemAppearanceSource {
        fn new(appearance: SystemAppearance) -> Self {
            Self { appearance }
        }
    }

    impl SystemAppearanceSource for MockSystemAppearanceSource {
        fn current_appearance(&self) -> SystemAppearance {
            self.appearance
        }

        fn subscribe(&self) -> mpsc::Receiver<SystemAppearance> {
            let (_tx, rx) = mpsc::channel();
            rx
        }
    }

    fn theme_settings(mode: ThemeMode) -> Settings {
        Settings {
            theme_mode: mode,
            theme_id_light: ThemeId::new("day"),
            theme_id_dark: ThemeId::new("night"),
            ..Settings::default()
        }
    }

    /// `Settings::default()` returns the documented empty-file shape.
    /// Locks every default slot so a casual change to one of the
    /// constants below shows up as a test diff.
    #[test]
    fn settings_default_matches_documented_empty_file_shape() {
        let s = Settings::default();
        assert_eq!(s.theme_mode, ThemeMode::System);
        assert_eq!(s.theme_id_light.as_str(), "default-light");
        assert_eq!(s.theme_id_dark.as_str(), "default-dark");
        assert_eq!(s.default_provider, "local");
        assert_eq!(s.keybinding_profile.as_str(), "default");
        assert!(!s.update_check_enabled);
        // Font settings ride their own Default impl; assert one
        // representative slot so a stray rename to `family` /
        // `monospace_family` shows up here too.
        assert_eq!(s.font_settings.editor_family, "monospace");
        assert!((s.font_settings.editor_size - 13.0).abs() < f32::EPSILON);
    }

    /// `ThemeMode` serialises as kebab-case strings — locks the wire
    /// form so persisted files stay readable through later refactors.
    #[test]
    fn theme_mode_serialises_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ThemeMode::Light).expect("serialize"),
            "\"light\""
        );
        assert_eq!(
            serde_json::to_string(&ThemeMode::Dark).expect("serialize"),
            "\"dark\""
        );
        assert_eq!(
            serde_json::to_string(&ThemeMode::System).expect("serialize"),
            "\"system\""
        );
    }

    /// `Settings::default()` round-trips through JSON unchanged.
    /// JSON is the secondary wire form — TOML is authoritative — but
    /// every other Domain type in this crate locks its serde shape
    /// through JSON, so we keep the convention here too.
    #[test]
    fn settings_default_round_trips_through_json() {
        let original = Settings::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// `Settings::default()` round-trips through TOML unchanged. This
    /// is the *authoritative* wire form — `settings.toml` is what the
    /// user edits, so any silent change to the wire shape would break
    /// every running install at next launch.
    #[test]
    fn settings_default_round_trips_through_toml() {
        let original = Settings::default();
        let toml_text = toml::to_string(&original).expect("serialize toml");
        let decoded: Settings = toml::from_str(&toml_text).expect("deserialize toml");
        assert_eq!(original, decoded);
    }

    /// Build a fully-populated, *non-default* `Settings` so the
    /// fixture round-trip exercises every field carrying a value
    /// that differs from the default.
    fn sample_non_default_settings() -> Settings {
        Settings {
            theme_mode: ThemeMode::Dark,
            theme_id_light: ThemeId::new("solarized-light"),
            theme_id_dark: ThemeId::new("high-contrast"),
            default_provider: "hosted-router".to_string(),
            font_settings: FontSettings {
                ui_family: "Inter, system-ui, sans-serif".to_string(),
                ui_size: 13.5,
                editor_family: "JetBrains Mono, monospace".to_string(),
                editor_size: 14.0,
                terminal_family: "Fira Code, monospace".to_string(),
                terminal_size: 12.5,
            },
            keybinding_profile: KeybindingProfileId::new("vim"),
            update_check_enabled: true,
        }
    }

    /// A populated, non-default `Settings` survives the TOML round-trip
    /// unchanged. Pairs with the default-shape test above so both ends
    /// of the wire matrix are exercised.
    #[test]
    fn settings_non_default_fixture_round_trips_through_toml() {
        let original = sample_non_default_settings();
        let toml_text = toml::to_string(&original).expect("serialize toml");
        let decoded: Settings = toml::from_str(&toml_text).expect("deserialize toml");
        assert_eq!(original, decoded);
    }

    /// Same fixture, JSON axis. Two encoders, same value, must agree.
    #[test]
    fn settings_non_default_fixture_round_trips_through_json() {
        let original = sample_non_default_settings();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    /// Lock the seven-field surface of `Settings` exactly. The PRD-03
    /// schema is locked at this set; growing it requires a deliberate
    /// PRD update plus a migration story, not a casual `pub` field.
    /// This test catches both accidental additions and accidental
    /// renames in one pass.
    #[test]
    fn settings_wire_field_names_are_stable() {
        let value: serde_json::Value =
            serde_json::to_value(Settings::default()).expect("serialize as value");
        let object = value.as_object().expect("object");
        let expected = [
            "theme_mode",
            "theme_id_light",
            "theme_id_dark",
            "default_provider",
            "font_settings",
            "keybinding_profile",
            "update_check_enabled",
        ];
        for key in expected {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(
            object.len(),
            expected.len(),
            "unexpected field set: {:?}",
            object.keys().collect::<Vec<_>>()
        );
    }

    /// US#7 privacy guard. `Settings` must not declare any field that
    /// names or stores an API key, token, secret, password, or
    /// credential — those live in the keychain
    /// ([`crate::ai::SecretStore`]) and are referenced by
    /// [`crate::ai::SecretRef`] when they have to be referenced at all.
    ///
    /// We enforce the rule on the *serialised* field names rather than
    /// at compile time because a runtime check still fires on additions
    /// added by future PRDs and stays close to the wire shape users
    /// actually see in `settings.toml`.
    #[test]
    fn settings_carry_no_secret_field_names() {
        let value: serde_json::Value =
            serde_json::to_value(Settings::default()).expect("serialize as value");
        let object = value.as_object().expect("object");
        for key in object.keys() {
            let lowered = key.to_ascii_lowercase();
            for forbidden in [
                "api_key",
                "apikey",
                "token",
                "secret",
                "password",
                "credential",
            ] {
                assert!(
                    !lowered.contains(forbidden),
                    "Settings field {key:?} looks like it carries a {forbidden} — \
                     secrets must live in SecretStore, referenced by SecretRef. \
                     See US#7 in PRD-03."
                );
            }
        }
    }

    /// `default_provider` flows through `ProviderId`. Lock the wire
    /// form (bare string) explicitly so the settings layer stays
    /// compatible with the registry vocabulary.
    #[test]
    fn default_provider_serialises_as_bare_string() {
        let value: serde_json::Value =
            serde_json::to_value(Settings::default()).expect("serialize as value");
        let object = value.as_object().expect("object");
        let provider = object
            .get("default_provider")
            .expect("default_provider field present");
        assert_eq!(provider.as_str(), Some("local"));
    }

    /// `keybinding_profile` flows through `KeybindingProfileId`. Same
    /// shape rationale as `default_provider`: bare string in the wire,
    /// typed handle in memory.
    #[test]
    fn keybinding_profile_serialises_as_bare_string() {
        let value: serde_json::Value =
            serde_json::to_value(Settings::default()).expect("serialize as value");
        let object = value.as_object().expect("object");
        let profile = object
            .get("keybinding_profile")
            .expect("keybinding_profile field present");
        assert_eq!(profile.as_str(), Some("default"));
    }

    #[test]
    fn explicit_theme_modes_select_configured_slots() {
        assert_eq!(
            resolve_theme_id(&theme_settings(ThemeMode::Light), SystemAppearance::Dark).as_str(),
            "day"
        );
        assert_eq!(
            resolve_theme_id(&theme_settings(ThemeMode::Dark), SystemAppearance::Light).as_str(),
            "night"
        );
    }

    #[test]
    fn system_mode_reads_initial_system_appearance() {
        let light = ActiveThemeResolver::new(
            theme_settings(ThemeMode::System),
            MockSystemAppearanceSource::new(SystemAppearance::Light),
        );
        assert_eq!(light.active_theme_id().as_str(), "day");

        let dark = ActiveThemeResolver::new(
            theme_settings(ThemeMode::System),
            MockSystemAppearanceSource::new(SystemAppearance::Dark),
        );
        assert_eq!(dark.active_theme_id().as_str(), "night");
    }

    #[test]
    fn system_appearance_changes_emit_active_theme_changes() {
        let mut resolver = ActiveThemeResolver::new(
            theme_settings(ThemeMode::System),
            MockSystemAppearanceSource::new(SystemAppearance::Light),
        );
        let dark = resolver
            .apply_system_appearance(SystemAppearance::Dark)
            .expect("light to dark emits");
        assert_eq!(dark.theme_id.as_str(), "night");
        let light = resolver
            .apply_system_appearance(SystemAppearance::Light)
            .expect("dark to light emits");
        assert_eq!(light.theme_id.as_str(), "day");
    }

    #[test]
    fn explicit_modes_ignore_system_appearance_changes() {
        let mut resolver = ActiveThemeResolver::new(
            theme_settings(ThemeMode::Light),
            MockSystemAppearanceSource::new(SystemAppearance::Light),
        );
        assert!(resolver
            .apply_system_appearance(SystemAppearance::Dark)
            .is_none());
        assert_eq!(resolver.active_theme_id().as_str(), "day");
    }

    #[test]
    fn polling_fallback_notifies_subscribers_safely() {
        let source = PollingSystemAppearanceSource::new(SystemAppearance::Light);
        let rx = source.subscribe();
        source.poll_update(SystemAppearance::Dark);
        assert_eq!(rx.recv().expect("appearance event"), SystemAppearance::Dark);
    }
}
