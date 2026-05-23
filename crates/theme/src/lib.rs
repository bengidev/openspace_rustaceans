//! Theme domain model, TOML parser, validation, bundled themes.

pub mod iced_bridge;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{mpsc as std_mpsc, Arc, Mutex as StdMutex, RwLock},
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use static_assertions::assert_impl_all;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::warn;

pub const REQUIRED_UI_TOKENS: &[&str] =
    &["background", "foreground", "accent", "surface", "border"];
pub const REQUIRED_SYNTAX_TOKENS: &[&str] = &[
    "keyword", "string", "comment", "number", "function", "type", "variable", "operator",
];
pub const REQUIRED_TERMINAL_TOKENS: &[&str] = &[
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
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThemeId(String);
impl ThemeId {
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ThemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    Light,
    Dark,
    HighContrast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}
impl Color {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xff }
    }
}
impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#{:02X}{:02X}{:02X}{:02X}",
            self.r, self.g, self.b, self.a
        )
    }
}
impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = <std::borrow::Cow<'_, str>>::deserialize(deserializer)?;
        parse_color(&raw).map_err(de::Error::custom)
    }
}
fn parse_color(input: &str) -> Result<Color, String> {
    let t = input.trim();
    if let Some(hex) = t.strip_prefix('#') {
        return parse_hex(hex, input);
    };
    if let Some(inner) = t.strip_prefix("rgba(").and_then(|v| v.strip_suffix(')')) {
        return parse_rgba(inner, input);
    };
    if let Some(inner) = t.strip_prefix("rgb(").and_then(|v| v.strip_suffix(')')) {
        return parse_rgb(inner, input);
    };
    Err(format!("invalid color {input:?}: expected `#RRGGBB`, `#RRGGBBAA`, `rgb(r, g, b)`, or `rgba(r, g, b, a)`"))
}
fn parse_hex(hex: &str, original: &str) -> Result<Color, String> {
    let byte = |s: &str, c: &str| {
        u8::from_str_radix(s, 16)
            .map_err(|_| format!("invalid color {original:?}: {c} channel {s:?} is not a hex byte"))
    };
    match hex.len() {
        6 => Ok(Color::new(
            byte(&hex[0..2], "red")?,
            byte(&hex[2..4], "green")?,
            byte(&hex[4..6], "blue")?,
            0xff,
        )),
        8 => Ok(Color::new(
            byte(&hex[0..2], "red")?,
            byte(&hex[2..4], "green")?,
            byte(&hex[4..6], "blue")?,
            byte(&hex[6..8], "alpha")?,
        )),
        _ => Err(format!(
            "invalid color {original:?}: hex form must be `#RRGGBB` or `#RRGGBBAA`"
        )),
    }
}
fn parse_rgb(inner: &str, original: &str) -> Result<Color, String> {
    let p: Vec<_> = inner.split(',').map(str::trim).collect();
    if p.len() != 3 {
        return Err(format!(
            "invalid color {original:?}: `rgb(...)` requires exactly three channels"
        ));
    }
    Ok(Color::new(
        parse_u8(p[0], "red", original)?,
        parse_u8(p[1], "green", original)?,
        parse_u8(p[2], "blue", original)?,
        0xff,
    ))
}
fn parse_rgba(inner: &str, original: &str) -> Result<Color, String> {
    let p: Vec<_> = inner.split(',').map(str::trim).collect();
    if p.len() != 4 {
        return Err(format!(
            "invalid color {original:?}: `rgba(...)` requires exactly four channels"
        ));
    }
    Ok(Color::new(
        parse_u8(p[0], "red", original)?,
        parse_u8(p[1], "green", original)?,
        parse_u8(p[2], "blue", original)?,
        parse_alpha(p[3], original)?,
    ))
}
fn parse_u8(s: &str, c: &str, o: &str) -> Result<u8, String> {
    s.parse::<u8>()
        .map_err(|_| format!("invalid color {o:?}: {c} channel {s:?} must be 0..=255"))
}
fn parse_alpha(s: &str, o: &str) -> Result<u8, String> {
    if s.contains('.') {
        let v = s.parse::<f32>().map_err(|_| {
            format!("invalid color {o:?}: alpha channel {s:?} must be 0.0..=1.0 or 0..=255")
        })?;
        if !(0.0..=1.0).contains(&v) {
            return Err(format!(
                "invalid color {o:?}: alpha channel {s:?} must be 0.0..=1.0"
            ));
        }
        Ok((v * 255.0).round() as u8)
    } else {
        parse_u8(s, "alpha", o)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTokens {
    pub background: Color,
    pub foreground: Color,
    pub accent: Color,
    pub surface: Color,
    pub border: Color,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxTokens {
    pub keyword: Color,
    pub string: Color,
    pub comment: Color,
    pub number: Color,
    pub function: Color,
    pub r#type: Color,
    pub variable: Color,
    pub operator: Color,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPalette {
    pub black: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub magenta: Color,
    pub cyan: Color,
    pub white: Color,
    pub bright_black: Color,
    pub bright_red: Color,
    pub bright_green: Color,
    pub bright_yellow: Color,
    pub bright_blue: Color,
    pub bright_magenta: Color,
    pub bright_cyan: Color,
    pub bright_white: Color,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeMetadata {
    pub display_name: String,
    pub author: Option<String>,
    pub description: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    pub id: ThemeId,
    pub metadata: ThemeMetadata,
    pub appearance: Appearance,
    pub ui: UiTokens,
    pub syntax: SyntaxTokens,
    pub terminal: TerminalPalette,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ThemeError {
    #[error("invalid TOML: {0}")]
    InvalidToml(String),
    #[error("missing required token `{path}`")]
    MissingToken { path: String },
    #[error("invalid colour at `{path}`: {message}")]
    InvalidColor { path: String, message: String },
    #[error("invalid field `{path}`: {message}")]
    InvalidField { path: String, message: String },
}

pub struct ThemeFile;
impl ThemeFile {
    pub fn parse(input: &str) -> Result<Theme, ThemeError> {
        parse_theme(input)
    }
}

fn parse_theme(input: &str) -> Result<Theme, ThemeError> {
    let value: toml::Value =
        toml::from_str(input).map_err(|e| ThemeError::InvalidToml(e.to_string()))?;
    for (section, tokens) in [
        ("ui", REQUIRED_UI_TOKENS),
        ("syntax", REQUIRED_SYNTAX_TOKENS),
        ("terminal", REQUIRED_TERMINAL_TOKENS),
    ] {
        let table = value.get(section).and_then(toml::Value::as_table);
        for token in tokens {
            if table.and_then(|t| t.get(*token)).is_none() {
                return Err(ThemeError::MissingToken {
                    path: format!("{section}.{token}"),
                });
            }
        }
    }
    match value.clone().try_into::<Theme>() {
        Ok(t) => Ok(t),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("invalid color") || msg.contains("invalid colour") {
                Err(ThemeError::InvalidColor {
                    path: infer_color_path(&value).unwrap_or_else(|| "<unknown>".to_string()),
                    message: msg,
                })
            } else {
                Err(ThemeError::InvalidField {
                    path: "<unknown>".to_string(),
                    message: msg,
                })
            }
        }
    }
}
fn infer_color_path(value: &toml::Value) -> Option<String> {
    for (section, tokens) in [
        ("ui", REQUIRED_UI_TOKENS),
        ("syntax", REQUIRED_SYNTAX_TOKENS),
        ("terminal", REQUIRED_TERMINAL_TOKENS),
    ] {
        let table = value.get(section)?.as_table()?;
        for token in tokens {
            if let Some(v) = table.get(*token).and_then(toml::Value::as_str) {
                if parse_color(v).is_err() {
                    return Some(format!("{section}.{token}"));
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledTheme {
    pub id: &'static str,
    pub source: &'static str,
}
pub const BUNDLED_THEMES: &[BundledTheme] = &[
    BundledTheme {
        id: "default-light",
        source: include_str!("themes/default-light.toml"),
    },
    BundledTheme {
        id: "default-dark",
        source: include_str!("themes/default-dark.toml"),
    },
    BundledTheme {
        id: "solarized-light",
        source: include_str!("themes/solarized-light.toml"),
    },
    BundledTheme {
        id: "solarized-dark",
        source: include_str!("themes/solarized-dark.toml"),
    },
    BundledTheme {
        id: "gruvbox",
        source: include_str!("themes/gruvbox.toml"),
    },
    BundledTheme {
        id: "nord",
        source: include_str!("themes/nord.toml"),
    },
];
pub fn bundled_themes() -> impl Iterator<Item = Result<Theme, ThemeError>> {
    BUNDLED_THEMES.iter().map(|b| ThemeFile::parse(b.source))
}
pub fn bundled_theme(id: &str) -> Option<Result<Theme, ThemeError>> {
    BUNDLED_THEMES
        .iter()
        .find(|b| b.id == id)
        .map(|b| ThemeFile::parse(b.source))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeChange {
    Created(ThemeId),
    Reloaded(ThemeId),
    Deleted(ThemeId),
    ActiveThemeChanged(Option<ThemeId>),
}

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Debug)]
struct WatcherHandle {
    watcher: Option<RecommendedWatcher>,
    driver: Option<std::thread::JoinHandle<()>>,
}
impl Drop for WatcherHandle {
    fn drop(&mut self) {
        drop(self.watcher.take());
        if let Some(driver) = self.driver.take() {
            let _ = driver.join();
        }
    }
}

#[derive(Debug)]
pub struct ThemeStore {
    custom: Arc<RwLock<BTreeMap<String, Theme>>>,
    diagnostics: Arc<RwLock<Vec<ThemeDiagnostic>>>,
    active_theme: Arc<RwLock<Option<ThemeId>>>,
    tx: broadcast::Sender<ThemeChange>,
    watcher: StdMutex<Option<WatcherHandle>>,
}
impl ThemeStore {
    #[must_use]
    pub fn bundled() -> Self {
        Self::new(None, false)
    }
    #[must_use]
    pub fn with_custom_dir(custom_dir: impl Into<PathBuf>) -> Self {
        Self::new(Some(custom_dir.into()), true)
    }
    #[must_use]
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self::with_custom_dir(data_dir.into().join("themes"))
    }
    fn new(custom_dir: Option<PathBuf>, watch: bool) -> Self {
        let (tx, _) = broadcast::channel(32);
        let (custom, diagnostics) = load_custom(custom_dir.as_deref());
        let custom = Arc::new(RwLock::new(custom));
        let diagnostics = Arc::new(RwLock::new(diagnostics));
        let active_theme = Arc::new(RwLock::new(None));
        let watcher = if watch {
            custom_dir.as_ref().and_then(|dir| {
                spawn_theme_watcher(
                    dir.clone(),
                    Arc::clone(&custom),
                    Arc::clone(&diagnostics),
                    Arc::clone(&active_theme),
                    tx.clone(),
                    DEFAULT_DEBOUNCE,
                )
                .map_err(|err| {
                    warn!(error = %err, path = %dir.display(), "theme watcher failed to start")
                })
                .ok()
            })
        } else {
            None
        };
        Self {
            custom,
            diagnostics,
            active_theme,
            tx,
            watcher: StdMutex::new(watcher),
        }
    }
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ThemeChange> {
        self.tx.subscribe()
    }
    pub fn set_active_theme(&self, id: Option<ThemeId>) {
        *self
            .active_theme
            .write()
            .expect("ThemeStore active_theme RwLock poisoned") = id.clone();
        let _ = self.tx.send(ThemeChange::ActiveThemeChanged(id));
    }
    #[must_use]
    pub fn active_theme(&self) -> Option<ThemeId> {
        self.active_theme
            .read()
            .expect("ThemeStore active_theme RwLock poisoned")
            .clone()
    }
    #[must_use]
    pub fn diagnostics(&self) -> Vec<ThemeDiagnostic> {
        self.diagnostics
            .read()
            .expect("ThemeStore diagnostics RwLock poisoned")
            .clone()
    }
    #[must_use]
    pub fn list(&self) -> Vec<Theme> {
        let mut themes = self.bundled_map();
        themes.extend(
            self.custom
                .read()
                .expect("ThemeStore custom RwLock poisoned")
                .clone(),
        );
        themes.into_values().collect()
    }
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Theme> {
        self.custom
            .read()
            .expect("ThemeStore custom RwLock poisoned")
            .get(id)
            .cloned()
            .or_else(|| bundled_theme(id).and_then(Result::ok))
    }
    fn bundled_map(&self) -> BTreeMap<String, Theme> {
        bundled_themes()
            .filter_map(Result::ok)
            .map(|theme| (theme.id.as_str().to_string(), theme))
            .collect()
    }
}
impl Drop for ThemeStore {
    fn drop(&mut self) {
        drop(
            self.watcher
                .lock()
                .expect("ThemeStore watcher Mutex poisoned")
                .take(),
        );
    }
}

fn load_custom(dir: Option<&Path>) -> (BTreeMap<String, Theme>, Vec<ThemeDiagnostic>) {
    let Some(dir) = dir else {
        return (BTreeMap::new(), Vec::new());
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return (BTreeMap::new(), Vec::new());
    };
    let mut themes = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_toml(&path) {
            continue;
        }
        match fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|src| ThemeFile::parse(&src).map_err(|e| e.to_string()))
        {
            Ok(theme) => {
                themes.insert(theme.id.as_str().to_string(), theme);
            }
            Err(message) => diagnostics.push(ThemeDiagnostic { path, message }),
        }
    }
    (themes, diagnostics)
}

fn is_toml(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("toml")
}

fn event_matches_theme_file(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) && event.paths.iter().any(|path| is_toml(path))
}

fn spawn_theme_watcher(
    dir: PathBuf,
    custom: Arc<RwLock<BTreeMap<String, Theme>>>,
    diagnostics: Arc<RwLock<Vec<ThemeDiagnostic>>>,
    active_theme: Arc<RwLock<Option<ThemeId>>>,
    tx: broadcast::Sender<ThemeChange>,
    debounce: Duration,
) -> notify::Result<WatcherHandle> {
    fs::create_dir_all(&dir)?;
    let (event_tx, event_rx) = std_mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = event_tx.send(res);
        },
        notify::Config::default(),
    )?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    let driver = std::thread::spawn(move || {
        while let Ok(res) = event_rx.recv() {
            match res {
                Ok(event) if event_matches_theme_file(&event) => {
                    let deadline = Instant::now() + debounce;
                    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                        match event_rx.recv_timeout(remaining) {
                            Ok(Ok(next)) if event_matches_theme_file(&next) => continue,
                            Ok(Ok(_)) => continue,
                            Ok(Err(err)) => warn!(error = %err, "theme watcher event error"),
                            Err(std_mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    reload_themes(&dir, &custom, &diagnostics, &active_theme, &tx);
                }
                Ok(_) => {}
                Err(err) => warn!(error = %err, "theme watcher event error"),
            }
        }
    });
    Ok(WatcherHandle {
        watcher: Some(watcher),
        driver: Some(driver),
    })
}

fn reload_themes(
    dir: &Path,
    custom: &Arc<RwLock<BTreeMap<String, Theme>>>,
    diagnostics: &Arc<RwLock<Vec<ThemeDiagnostic>>>,
    active_theme: &Arc<RwLock<Option<ThemeId>>>,
    tx: &broadcast::Sender<ThemeChange>,
) {
    let old = custom
        .read()
        .expect("ThemeStore custom RwLock poisoned")
        .clone();
    let (new, new_diagnostics) = load_custom(Some(dir));
    *custom.write().expect("ThemeStore custom RwLock poisoned") = new.clone();
    *diagnostics
        .write()
        .expect("ThemeStore diagnostics RwLock poisoned") = new_diagnostics;

    let old_ids = old.keys().cloned().collect::<BTreeSet<_>>();
    let new_ids = new.keys().cloned().collect::<BTreeSet<_>>();
    for id in new_ids.difference(&old_ids) {
        let _ = tx.send(ThemeChange::Created(ThemeId::new(id.clone())));
    }
    for id in old_ids.intersection(&new_ids) {
        if old.get(id) != new.get(id) {
            let theme_id = ThemeId::new(id.clone());
            let _ = tx.send(ThemeChange::Reloaded(theme_id.clone()));
            if active_theme
                .read()
                .expect("ThemeStore active_theme RwLock poisoned")
                .as_ref()
                == Some(&theme_id)
            {
                let _ = tx.send(ThemeChange::ActiveThemeChanged(Some(theme_id)));
            }
        }
    }
    for id in old_ids.difference(&new_ids) {
        let theme_id = ThemeId::new(id.clone());
        let _ = tx.send(ThemeChange::Deleted(theme_id.clone()));
        if active_theme
            .read()
            .expect("ThemeStore active_theme RwLock poisoned")
            .as_ref()
            == Some(&theme_id)
        {
            *active_theme
                .write()
                .expect("ThemeStore active_theme RwLock poisoned") = None;
            let _ = tx.send(ThemeChange::ActiveThemeChanged(None));
        }
    }
}

assert_impl_all!(Theme: Send, Sync);
assert_impl_all!(ThemeError: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;
    const VALID: &str = include_str!("themes/default-dark.toml");
    #[test]
    fn parses_valid_theme() {
        let t = ThemeFile::parse(VALID).unwrap();
        assert_eq!(t.id.as_str(), "default-dark");
    }
    #[test]
    fn bundled_themes_parse() {
        let parsed: Vec<_> = bundled_themes().collect();
        assert_eq!(parsed.len(), 6);
        for theme in parsed {
            theme.unwrap();
        }
    }
    #[test]
    fn missing_required_token_is_typed() {
        let src = VALID
            .lines()
            .filter(|line| *line != "background = \"#1E1E2E\"")
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            ThemeFile::parse(&src),
            Err(ThemeError::MissingToken {
                path: "ui.background".to_string()
            })
        );
    }
    #[test]
    fn invalid_colour_is_typed() {
        let src = VALID.replace("background = \"#1E1E2E\"", "background = \"wat\"");
        match ThemeFile::parse(&src) {
            Err(ThemeError::InvalidColor { path, .. }) => assert_eq!(path, "ui.background"),
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn colour_round_trips() {
        let color: Color = toml::from_str("c = \"rgba(1, 2, 3, 0.5)\"")
            .map(|v: toml::Value| v["c"].clone().try_into().unwrap())
            .unwrap();
        assert_eq!(color.to_string(), "#01020380");
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            color: Color,
        }
        let encoded = toml::to_string(&Wrapper { color }).unwrap();
        let decoded: Wrapper = toml::from_str(&encoded).unwrap();
        assert_eq!(color, decoded.color);
    }
    #[test]
    fn theme_store_loads_custom_theme() {
        let data_dir = temp_data_dir("load");
        let themes_dir = data_dir.join("themes");
        fs::create_dir_all(&themes_dir).unwrap();
        let custom = VALID.replace("default-dark", "custom-dark").replace(
            "display_name = \"Default Dark\"",
            "display_name = \"Custom Dark\"",
        );
        fs::write(themes_dir.join("custom-dark.toml"), custom).unwrap();
        fs::write(themes_dir.join("broken.toml"), "not = ").unwrap();

        let store = ThemeStore::with_data_dir(&data_dir);
        let theme = store.get("custom-dark").unwrap();

        assert_eq!(theme.id.as_str(), "custom-dark");
        assert_eq!(theme.metadata.display_name, "Custom Dark");
        assert!(store
            .list()
            .iter()
            .any(|theme| theme.id.as_str() == "custom-dark"));
        assert_eq!(store.diagnostics().len(), 1);
        assert!(store.get("broken").is_none());
    }
    #[test]
    fn theme_store_custom_theme_overrides_bundled_duplicate_id() {
        let dir = temp_theme_dir("override");
        let custom = VALID.replace(
            "display_name = \"Default Dark\"",
            "display_name = \"Custom Override\"",
        );
        fs::write(dir.join("default-dark.toml"), custom).unwrap();

        let store = ThemeStore::with_custom_dir(&dir);
        let theme = store.get("default-dark").unwrap();
        let listed = store
            .list()
            .into_iter()
            .filter(|theme| theme.id.as_str() == "default-dark")
            .collect::<Vec<_>>();

        assert_eq!(theme.metadata.display_name, "Custom Override");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].metadata.display_name, "Custom Override");
    }
    #[tokio::test]
    async fn live_reload_emits_reload_within_500ms() {
        let dir = temp_theme_dir("live-reload");
        let custom = VALID.replace("default-dark", "custom-live").replace(
            "display_name = \"Default Dark\"",
            "display_name = \"Custom Live\"",
        );
        let path = dir.join("custom-live.toml");
        fs::write(&path, custom).unwrap();
        let store = ThemeStore::with_custom_dir(&dir);
        let mut rx = store.subscribe();
        store.set_active_theme(Some(ThemeId::new("custom-live")));
        drain_events(&mut rx).await;

        let edited = VALID.replace("default-dark", "custom-live").replace(
            "display_name = \"Default Dark\"",
            "display_name = \"Custom Live Edited\"",
        );
        fs::write(&path, edited).unwrap();

        let mut saw_reload = false;
        let mut saw_active = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && !(saw_reload && saw_active) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ThemeChange::Reloaded(id))) if id.as_str() == "custom-live" => {
                    saw_reload = true;
                }
                Ok(Ok(ThemeChange::ActiveThemeChanged(Some(id))))
                    if id.as_str() == "custom-live" =>
                {
                    saw_active = true;
                }
                Ok(Ok(_)) => {}
                other => panic!("unexpected reload wait result: {other:?}"),
            }
        }

        assert!(saw_reload);
        assert!(saw_active);
        assert_eq!(
            store.get("custom-live").unwrap().metadata.display_name,
            "Custom Live Edited"
        );
    }
    async fn drain_events(rx: &mut broadcast::Receiver<ThemeChange>) {
        while tokio::time::timeout(Duration::from_millis(10), rx.recv())
            .await
            .is_ok()
        {}
    }
    fn temp_theme_dir(name: &str) -> PathBuf {
        let dir = temp_data_dir(name).join("themes");
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn temp_data_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("openspace-theme-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
