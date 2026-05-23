//! Theme domain model, TOML parser, validation, bundled themes.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use static_assertions::assert_impl_all;
use thiserror::Error;

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
}
