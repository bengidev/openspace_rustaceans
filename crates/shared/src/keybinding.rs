//! Key binding types — [`KeyBinding`], [`Modifiers`], [`Key`], and
//! [`KeybindingProfile`].
//!
//! The command palette and any feature module that wants to expose a
//! shortcut declares its bindings as values of these types. Domain
//! layer rules apply: pure data, no UI dependency, no IO. Iced or any
//! other event source converts its native key events into a
//! [`KeyBinding`] at the boundary, then dispatch is a plain map lookup.
//!
//! # Public surface
//!
//! - [`Modifiers`] — bitflags-style record over `ctrl`, `shift`, `alt`,
//!   and `cmd`. Constructed via the parser in production; the
//!   [`Modifiers::CTRL`], [`Modifiers::SHIFT`], [`Modifiers::ALT`], and
//!   [`Modifiers::CMD`] consts plus [`BitOr`](std::ops::BitOr) keep
//!   tests readable (`Modifiers::CTRL | Modifiers::SHIFT`).
//! - [`Key`] — the closed set of keys a binding can target: literal
//!   characters via [`Key::Char`], the function row `F1..F24`, and a
//!   handful of named keys (`Escape`, `Tab`, `Enter`, `Backspace`,
//!   `Delete`, `Home`, `End`, `PageUp`, `PageDown`, `ArrowUp`,
//!   `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Space`).
//! - [`KeyBinding`] — `{ modifiers, key }`. Round-trips through its
//!   canonical string form via [`KeyBinding::parse`] and the
//!   [`fmt::Display`] impl. The serde shape is the same string so
//!   persisted profiles read like plain config: `"ctrl+shift+p"`.
//! - [`KeybindingProfileId`] — stable slug newtype that identifies a
//!   profile (`"default"`, `"vim"`, …). Lives in the Domain layer so
//!   downstream settings types can reference profiles by a typed
//!   handle instead of a free-form string.
//! - [`KeybindingProfile`] — named bag of bindings, keyed by
//!   [`CommandId`]. Each command may carry multiple bindings (a
//!   primary plus alternates).
//! - [`KeyBindingParseError`] — [`thiserror`] enum surfaced by the
//!   parser. The variants pin the contract listed below.
//!
//! # Parser contract (locked)
//!
//! 1. Tokens are `+`-separated. Whitespace around tokens is ignored.
//! 2. Parsing is case-insensitive: `Ctrl+Shift+P`, `ctrl+shift+p`,
//!    and `CTRL+SHIFT+P` are equivalent.
//! 3. Modifier aliases: `cmd`, `meta`, and `super` all map to the
//!    `cmd` flag. `option` maps to `alt`. `control` is accepted as a
//!    long-form alias for `ctrl`.
//! 4. Exactly one non-modifier token is allowed and it must come last.
//! 5. Single-char tokens are normalised to lowercase before becoming
//!    [`Key::Char`].
//! 6. Function keys parse from `f1` through `f24`.
//! 7. Named keys parse case-insensitively against the variant name
//!    (`escape`, `tab`, `arrowleft`, …). The snake_case spelling
//!    (`arrow_left`) is also accepted as an alias so configuration
//!    files can stay readable.
//! 8. Empty input, leading `+`, and trailing `+` all error.
//!
//! # Display
//!
//! [`fmt::Display`] always emits the canonical lowercase form with
//! modifiers in a fixed order — `ctrl+shift+alt+cmd+<key>` — so
//! `parse(display(kb)) == kb` for any binding the parser produced.
//! Direct struct construction with a non-lowercase
//! [`Key::Char`] is technically possible but breaks round-trip; tests
//! and config files should always go through the parser.

use std::collections::HashMap;
use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use static_assertions::assert_impl_all;
use thiserror::Error;

use crate::command::CommandId;

// ─────────────────────────────────────────────────────────────────────
// Modifiers — bitflags-style record over the four supported modifier
// keys. Stored as four bools rather than a real bitflag type to keep
// the serde shape readable in TOML/JSON snapshots and to avoid pulling
// in a `bitflags` crate dependency just for four flags.
// ─────────────────────────────────────────────────────────────────────

/// Set of modifier keys held alongside a [`Key`].
///
/// All four modifiers are independent bool fields. Construct via the
/// parser in production code, or via the public consts plus
/// [`BitOr`](std::ops::BitOr) for ergonomic test fixtures:
///
/// ```
/// use openspace_shared::keybinding::Modifiers;
/// let m = Modifiers::CTRL | Modifiers::SHIFT;
/// assert!(m.ctrl && m.shift && !m.alt && !m.cmd);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers {
    /// Control key (Ctrl on every platform).
    pub ctrl: bool,
    /// Shift key.
    pub shift: bool,
    /// Alt key. The `option` alias parses into this same flag.
    pub alt: bool,
    /// Command / meta / super. macOS Command, Windows / Linux Super.
    pub cmd: bool,
}

impl Modifiers {
    /// All flags cleared. Equivalent to [`Modifiers::default`].
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        cmd: false,
    };

    /// Only the ctrl flag set. Combine with `|` for compound modifiers.
    pub const CTRL: Self = Self {
        ctrl: true,
        shift: false,
        alt: false,
        cmd: false,
    };

    /// Only the shift flag set.
    pub const SHIFT: Self = Self {
        ctrl: false,
        shift: true,
        alt: false,
        cmd: false,
    };

    /// Only the alt flag set.
    pub const ALT: Self = Self {
        ctrl: false,
        shift: false,
        alt: true,
        cmd: false,
    };

    /// Only the cmd flag set.
    pub const CMD: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        cmd: true,
    };

    /// `true` when no modifier is set. Cheap to call; pairs nicely
    /// with the [`KeyBinding`] [`fmt::Display`] impl, which omits the
    /// leading `+` separator when the modifier set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !(self.ctrl || self.shift || self.alt || self.cmd)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            ctrl: self.ctrl || rhs.ctrl,
            shift: self.shift || rhs.shift,
            alt: self.alt || rhs.alt,
            cmd: self.cmd || rhs.cmd,
        }
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

assert_impl_all!(Modifiers: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Key — closed set of key targets. Char is the catch-all for printable
// characters; the function row and named keys cover the rest of the
// surface a command palette plausibly binds against.
// ─────────────────────────────────────────────────────────────────────

/// Key portion of a [`KeyBinding`].
///
/// [`Key::Char`] holds a single printable character, normalised to
/// lowercase by the parser so round-tripping through
/// [`fmt::Display`] is deterministic. Constructing
/// `Key::Char('A')` directly is not forbidden, but [`fmt::Display`]
/// will emit `a` and the round-trip will observe `Key::Char('a')` —
/// always go through [`KeyBinding::parse`] when reading user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    /// Single printable character (letter, digit, punctuation).
    Char(char),
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Delete (forward delete).
    Delete,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Up arrow.
    ArrowUp,
    /// Down arrow.
    ArrowDown,
    /// Left arrow.
    ArrowLeft,
    /// Right arrow.
    ArrowRight,
    /// Space bar.
    Space,
    /// Function key F1.
    F1,
    /// Function key F2.
    F2,
    /// Function key F3.
    F3,
    /// Function key F4.
    F4,
    /// Function key F5.
    F5,
    /// Function key F6.
    F6,
    /// Function key F7.
    F7,
    /// Function key F8.
    F8,
    /// Function key F9.
    F9,
    /// Function key F10.
    F10,
    /// Function key F11.
    F11,
    /// Function key F12.
    F12,
    /// Function key F13.
    F13,
    /// Function key F14.
    F14,
    /// Function key F15.
    F15,
    /// Function key F16.
    F16,
    /// Function key F17.
    F17,
    /// Function key F18.
    F18,
    /// Function key F19.
    F19,
    /// Function key F20.
    F20,
    /// Function key F21.
    F21,
    /// Function key F22.
    F22,
    /// Function key F23.
    F23,
    /// Function key F24.
    F24,
}

impl Key {
    /// Canonical lowercase token for [`fmt::Display`]. Function keys
    /// render as `f1`..`f24`; named keys render as the lowercased
    /// variant name without underscores (`arrowleft`); chars render
    /// as the bare character.
    fn canonical_token(self) -> CanonicalToken {
        match self {
            Key::Char(c) => CanonicalToken::Char(c),
            Key::Escape => CanonicalToken::Static("escape"),
            Key::Tab => CanonicalToken::Static("tab"),
            Key::Enter => CanonicalToken::Static("enter"),
            Key::Backspace => CanonicalToken::Static("backspace"),
            Key::Delete => CanonicalToken::Static("delete"),
            Key::Home => CanonicalToken::Static("home"),
            Key::End => CanonicalToken::Static("end"),
            Key::PageUp => CanonicalToken::Static("pageup"),
            Key::PageDown => CanonicalToken::Static("pagedown"),
            Key::ArrowUp => CanonicalToken::Static("arrowup"),
            Key::ArrowDown => CanonicalToken::Static("arrowdown"),
            Key::ArrowLeft => CanonicalToken::Static("arrowleft"),
            Key::ArrowRight => CanonicalToken::Static("arrowright"),
            Key::Space => CanonicalToken::Static("space"),
            Key::F1 => CanonicalToken::Static("f1"),
            Key::F2 => CanonicalToken::Static("f2"),
            Key::F3 => CanonicalToken::Static("f3"),
            Key::F4 => CanonicalToken::Static("f4"),
            Key::F5 => CanonicalToken::Static("f5"),
            Key::F6 => CanonicalToken::Static("f6"),
            Key::F7 => CanonicalToken::Static("f7"),
            Key::F8 => CanonicalToken::Static("f8"),
            Key::F9 => CanonicalToken::Static("f9"),
            Key::F10 => CanonicalToken::Static("f10"),
            Key::F11 => CanonicalToken::Static("f11"),
            Key::F12 => CanonicalToken::Static("f12"),
            Key::F13 => CanonicalToken::Static("f13"),
            Key::F14 => CanonicalToken::Static("f14"),
            Key::F15 => CanonicalToken::Static("f15"),
            Key::F16 => CanonicalToken::Static("f16"),
            Key::F17 => CanonicalToken::Static("f17"),
            Key::F18 => CanonicalToken::Static("f18"),
            Key::F19 => CanonicalToken::Static("f19"),
            Key::F20 => CanonicalToken::Static("f20"),
            Key::F21 => CanonicalToken::Static("f21"),
            Key::F22 => CanonicalToken::Static("f22"),
            Key::F23 => CanonicalToken::Static("f23"),
            Key::F24 => CanonicalToken::Static("f24"),
        }
    }
}

/// Internal helper so [`Key::canonical_token`] can return either a
/// borrowed static string or an owned char without allocating in the
/// common (named-key / function-key) case.
enum CanonicalToken {
    Static(&'static str),
    Char(char),
}

impl fmt::Display for CanonicalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(s) => f.write_str(s),
            Self::Char(c) => write!(f, "{c}"),
        }
    }
}

assert_impl_all!(Key: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// KeyBindingParseError — structured failures for the parser. Variants
// align cell-by-cell with the parser contract above so error messages
// reference exactly which rule the input violated.
// ─────────────────────────────────────────────────────────────────────

/// Reasons [`KeyBinding::parse`] can fail. Each variant carries the
/// offending fragment of the source string so the loader can render
/// useful diagnostics without threading the original input separately.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyBindingParseError {
    /// Input was empty or contained only whitespace.
    #[error("empty key binding")]
    Empty,

    /// Input ended in a separator (`ctrl+`) or began with one (`+a`),
    /// or contained a `++` run that produced an empty token.
    #[error("malformed key binding {input:?}: empty token")]
    EmptyToken {
        /// The original input, verbatim.
        input: String,
    },

    /// A token in the modifier position was not a recognised modifier.
    /// The token is given verbatim (with its original casing).
    #[error("unknown modifier {token:?} in {input:?}")]
    UnknownModifier {
        /// Original input.
        input: String,
        /// The offending modifier token.
        token: String,
    },

    /// The same modifier flag was set twice (e.g. `ctrl+ctrl+a`).
    #[error("duplicate modifier {modifier:?} in {input:?}")]
    DuplicateModifier {
        /// Original input.
        input: String,
        /// The repeated modifier name (canonical form, lowercase).
        modifier: String,
    },

    /// The trailing token was not a valid key. Either it was an
    /// unknown name, or it was multi-character but not a function /
    /// named key.
    #[error("unknown key {token:?} in {input:?}")]
    UnknownKey {
        /// Original input.
        input: String,
        /// The offending key token.
        token: String,
    },

    /// The trailing token was a modifier name (`ctrl`, `shift`, …).
    /// Bindings must end with a real key.
    #[error("missing key in {input:?}")]
    MissingKey {
        /// Original input.
        input: String,
    },
}

assert_impl_all!(KeyBindingParseError: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// KeyBinding — the headline type. Custom Serialize / Deserialize so
// the wire form is the canonical string, not a struct literal. The
// derived in-memory equality and hash come for free from the field
// derives on Modifiers and Key.
// ─────────────────────────────────────────────────────────────────────

/// A single keyboard shortcut: a set of modifier flags plus the key
/// they accompany.
///
/// Round-trips through [`KeyBinding::parse`] and the [`fmt::Display`]
/// impl. The serde wire form is the same canonical string so persisted
/// keybinding profiles read like plain config (`"ctrl+shift+p"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    /// Modifier flags held while the key is pressed.
    pub modifiers: Modifiers,
    /// The non-modifier key that fires the binding.
    pub key: Key,
}

impl KeyBinding {
    /// Construct directly from a modifier set and a key.
    ///
    /// Most callers should use [`KeyBinding::parse`] instead so input
    /// is validated against the canonical grammar. This constructor
    /// exists for tests and code paths that already hold structured
    /// values (e.g. an event-translation layer that has already
    /// matched the platform key code).
    #[must_use]
    pub const fn new(modifiers: Modifiers, key: Key) -> Self {
        Self { modifiers, key }
    }

    /// Parse from the canonical `[modifier+]*key` shape.
    ///
    /// See the module-level "Parser contract" section for the full
    /// grammar. In short: `+`-separated, case-insensitive, modifiers
    /// before the key, exactly one key, `cmd` / `meta` / `super` all
    /// alias to the cmd flag, `option` aliases to alt, `control`
    /// aliases to ctrl.
    ///
    /// # Errors
    ///
    /// Returns a [`KeyBindingParseError`] when the input violates any
    /// rule. The variant names which rule failed.
    pub fn parse(input: &str) -> Result<Self, KeyBindingParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(KeyBindingParseError::Empty);
        }

        // `split` on `+` with no further trim: a leading `+`, trailing
        // `+`, or `++` run produces an empty segment which we surface
        // as `EmptyToken` rather than letting it propagate into the
        // modifier / key parsing branches with a confusing message.
        let segments: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(KeyBindingParseError::EmptyToken {
                input: input.to_string(),
            });
        }

        // SAFETY (logic, not unsafe): segments is non-empty because
        // `trimmed` is non-empty and splitting on `+` always returns
        // at least one element.
        let (key_token, modifier_tokens) = segments
            .split_last()
            .expect("trimmed input is non-empty so split has at least one segment");

        let key_token_lower = key_token.to_ascii_lowercase();
        if matches!(
            key_token_lower.as_str(),
            "ctrl" | "control" | "shift" | "alt" | "option" | "cmd" | "meta" | "super"
        ) {
            return Err(KeyBindingParseError::MissingKey {
                input: input.to_string(),
            });
        }

        let key = parse_key(&key_token_lower).ok_or_else(|| KeyBindingParseError::UnknownKey {
            input: input.to_string(),
            token: (*key_token).to_string(),
        })?;

        let mut modifiers = Modifiers::NONE;
        for token in modifier_tokens {
            let lower = token.to_ascii_lowercase();
            let (flag, canonical) = match lower.as_str() {
                "ctrl" | "control" => (ModifierFlag::Ctrl, "ctrl"),
                "shift" => (ModifierFlag::Shift, "shift"),
                "alt" | "option" => (ModifierFlag::Alt, "alt"),
                "cmd" | "meta" | "super" => (ModifierFlag::Cmd, "cmd"),
                _ => {
                    return Err(KeyBindingParseError::UnknownModifier {
                        input: input.to_string(),
                        token: (*token).to_string(),
                    });
                }
            };
            if flag.is_set(modifiers) {
                return Err(KeyBindingParseError::DuplicateModifier {
                    input: input.to_string(),
                    modifier: canonical.to_string(),
                });
            }
            modifiers = flag.apply(modifiers);
        }

        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for KeyBinding {
    /// Canonical lowercase rendering: `ctrl+shift+alt+cmd+<key>`.
    /// Modifiers are emitted in the fixed order regardless of how they
    /// were originally parsed, so two equivalent inputs always render
    /// identically and the round-trip property holds.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.modifiers.shift {
            f.write_str("shift+")?;
        }
        if self.modifiers.alt {
            f.write_str("alt+")?;
        }
        if self.modifiers.cmd {
            f.write_str("cmd+")?;
        }
        write!(f, "{}", self.key.canonical_token())
    }
}

impl Serialize for KeyBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = <std::borrow::Cow<'_, str>>::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

assert_impl_all!(KeyBinding: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// ModifierFlag — internal helper so the parser loop can name the four
// flags by-value without cloning the boolean fields each time.
// ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ModifierFlag {
    Ctrl,
    Shift,
    Alt,
    Cmd,
}

impl ModifierFlag {
    fn is_set(self, m: Modifiers) -> bool {
        match self {
            Self::Ctrl => m.ctrl,
            Self::Shift => m.shift,
            Self::Alt => m.alt,
            Self::Cmd => m.cmd,
        }
    }

    fn apply(self, mut m: Modifiers) -> Modifiers {
        match self {
            Self::Ctrl => m.ctrl = true,
            Self::Shift => m.shift = true,
            Self::Alt => m.alt = true,
            Self::Cmd => m.cmd = true,
        }
        m
    }
}

// ─────────────────────────────────────────────────────────────────────
// Key parsing — a single function so the contract for "what counts as
// a valid key token" lives in one place. Function keys, named keys,
// and the snake_case alias for arrows / page-keys all funnel through
// here; single chars are the fallback.
// ─────────────────────────────────────────────────────────────────────

fn parse_key(token: &str) -> Option<Key> {
    // Function keys: `f1`..`f24`. We require an `f` prefix followed by
    // a positive integer in `1..=24` to keep the contract tight (no
    // `f0`, no `f25`).
    if let Some(rest) = token.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            return function_key(n);
        }
    }

    // Named keys, including snake_case aliases for the multi-word
    // names so config files can stay readable.
    let key = match token {
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "enter" | "return" => Key::Enter,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" | "pgup" => Key::PageUp,
        "pagedown" | "page_down" | "pgdn" => Key::PageDown,
        "arrowup" | "arrow_up" | "up" => Key::ArrowUp,
        "arrowdown" | "arrow_down" | "down" => Key::ArrowDown,
        "arrowleft" | "arrow_left" | "left" => Key::ArrowLeft,
        "arrowright" | "arrow_right" | "right" => Key::ArrowRight,
        "space" => Key::Space,
        _ => {
            // Fallback: a single character is always allowed and is
            // interpreted as `Key::Char` after lowercasing (which has
            // already happened by the time we get here).
            let mut chars = token.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return Some(Key::Char(first));
        }
    };
    Some(key)
}

fn function_key(n: u8) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        21 => Key::F21,
        22 => Key::F22,
        23 => Key::F23,
        24 => Key::F24,
        _ => return None,
    })
}

// ─────────────────────────────────────────────────────────────────────
// KeybindingProfileId — stable slug newtype identifying a profile.
// Mirrors the convention used by every other handle-shaped type in
// this crate: `#[serde(transparent)]` so the wire form is the bare
// string, no validation in the constructor (loaders are the right
// place for that), `Display` for snapshots and error messages.
// ─────────────────────────────────────────────────────────────────────

/// Stable identifier for a [`KeybindingProfile`] — a kebab-case slug
/// like `"default"` or `"vim"`.
///
/// Newtype over [`String`] (not [`uuid::Uuid`]) because profiles are a
/// curated, named set: `"default"` should mean the same profile across
/// machines, and a uuid would force every install to mint its own. The
/// slug is what users reference in config files.
///
/// `#[serde(transparent)]` keeps the wire form a bare string so
/// settings files read naturally:
///
/// ```toml
/// keybinding_profile = "default"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeybindingProfileId(String);

impl KeybindingProfileId {
    /// Wrap an existing slug. The Domain layer does *not* validate
    /// the slug shape — that is the loader's job. Keeping the
    /// constructor permissive means tests, snapshots, and in-memory
    /// fixtures stay terse.
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

impl fmt::Display for KeybindingProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for KeybindingProfileId {
    fn from(slug: &str) -> Self {
        Self(slug.to_string())
    }
}

impl From<String> for KeybindingProfileId {
    fn from(slug: String) -> Self {
        Self(slug)
    }
}

assert_impl_all!(KeybindingProfileId: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// KeybindingProfile — named bag of bindings. Multiple bindings per
// command so a profile can express a primary plus alternates.
// HashMap rules out a Hash derive, but Eq is fine since both Vec and
// HashMap implement structural equality.
// ─────────────────────────────────────────────────────────────────────

/// Named collection of [`KeyBinding`]s grouped by [`CommandId`].
///
/// A profile pairs a stable identifier (`"default"`, `"vim"`, …) with
/// the bindings active under it. Each command may carry multiple
/// bindings — a primary plus one or more alternates — so users can
/// keep familiar muscle memory while migrating between profiles.
///
/// `#[non_exhaustive]` keeps adding fields (a `parent` profile id, a
/// `description`) non-breaking for downstream construction sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeybindingProfile {
    /// Stable profile id. Typed as [`KeybindingProfileId`] so the
    /// settings layer (PRD-03) and any downstream profile registry
    /// reference profiles by a single shared handle instead of a
    /// free-form string.
    pub id: KeybindingProfileId,
    /// Bindings in this profile, keyed by command id.
    pub bindings: HashMap<CommandId, Vec<KeyBinding>>,
}

impl KeybindingProfile {
    /// Construct an empty profile with the given id. Callers populate
    /// [`KeybindingProfile::bindings`] afterwards or feed an existing
    /// map via the struct literal — the field is public.
    ///
    /// Accepts anything that converts into a [`KeybindingProfileId`]
    /// so call sites can pass a `&str` literal, an owned `String`, or
    /// a pre-built id without ceremony.
    #[must_use]
    pub fn new(id: impl Into<KeybindingProfileId>) -> Self {
        Self {
            id: id.into(),
            bindings: HashMap::new(),
        }
    }
}

assert_impl_all!(KeybindingProfile: Send, Sync);

// ─────────────────────────────────────────────────────────────────────
// Tests — anchor the contract from the issue:
//   * sample inputs from acceptance criteria all parse;
//   * Display round-trips;
//   * every error variant fires on the relevant invalid input;
//   * serde wire form is the canonical string (KeyBinding) and a
//     plain map (KeybindingProfile).
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::prelude::*;

    #[test]
    fn parses_ctrl_shift_p() {
        let kb = KeyBinding::parse("ctrl+shift+p").expect("parse");
        assert_eq!(
            kb,
            KeyBinding::new(Modifiers::CTRL | Modifiers::SHIFT, Key::Char('p'))
        );
    }

    #[test]
    fn parses_cmd_w() {
        let kb = KeyBinding::parse("cmd+w").expect("parse");
        assert_eq!(kb, KeyBinding::new(Modifiers::CMD, Key::Char('w')));
    }

    #[test]
    fn meta_and_super_alias_to_cmd() {
        let from_meta = KeyBinding::parse("meta+w").expect("parse meta");
        let from_super = KeyBinding::parse("super+w").expect("parse super");
        let from_cmd = KeyBinding::parse("cmd+w").expect("parse cmd");
        assert_eq!(from_meta, from_cmd);
        assert_eq!(from_super, from_cmd);
    }

    #[test]
    fn option_aliases_to_alt() {
        let from_option = KeyBinding::parse("option+a").expect("parse option");
        let from_alt = KeyBinding::parse("alt+a").expect("parse alt");
        assert_eq!(from_option, from_alt);
    }

    #[test]
    fn control_aliases_to_ctrl() {
        let long = KeyBinding::parse("control+s").expect("parse control");
        let short = KeyBinding::parse("ctrl+s").expect("parse ctrl");
        assert_eq!(long, short);
    }

    #[test]
    fn parses_f12_function_key() {
        let kb = KeyBinding::parse("f12").expect("parse");
        assert_eq!(kb, KeyBinding::new(Modifiers::NONE, Key::F12));
    }

    #[test]
    fn parses_ctrl_arrow_left_alias() {
        let snake = KeyBinding::parse("ctrl+arrow_left").expect("parse snake");
        let canonical = KeyBinding::parse("ctrl+arrowleft").expect("parse canonical");
        assert_eq!(snake, canonical);
        assert_eq!(snake, KeyBinding::new(Modifiers::CTRL, Key::ArrowLeft));
    }

    #[test]
    fn parses_escape_no_modifiers() {
        let kb = KeyBinding::parse("escape").expect("parse");
        assert_eq!(kb, KeyBinding::new(Modifiers::NONE, Key::Escape));
    }

    #[test]
    fn parser_is_case_insensitive() {
        let upper = KeyBinding::parse("CTRL+SHIFT+P").expect("upper");
        let mixed = KeyBinding::parse("Ctrl+Shift+P").expect("mixed");
        let lower = KeyBinding::parse("ctrl+shift+p").expect("lower");
        assert_eq!(upper, mixed);
        assert_eq!(mixed, lower);
    }

    #[test]
    fn whitespace_around_tokens_is_ignored() {
        let kb = KeyBinding::parse("  ctrl  +  shift  +  p  ").expect("parse");
        assert_eq!(
            kb,
            KeyBinding::new(Modifiers::CTRL | Modifiers::SHIFT, Key::Char('p'))
        );
    }

    #[test]
    fn rejects_empty_input() {
        let err = KeyBinding::parse("").expect_err("must reject");
        assert!(matches!(err, KeyBindingParseError::Empty));
        let err_ws = KeyBinding::parse("   ").expect_err("must reject ws");
        assert!(matches!(err_ws, KeyBindingParseError::Empty));
    }

    #[test]
    fn rejects_trailing_plus() {
        let err = KeyBinding::parse("ctrl+").expect_err("must reject");
        assert!(matches!(err, KeyBindingParseError::EmptyToken { .. }));
    }

    #[test]
    fn rejects_leading_plus() {
        let err = KeyBinding::parse("+a").expect_err("must reject");
        assert!(matches!(err, KeyBindingParseError::EmptyToken { .. }));
    }

    #[test]
    fn rejects_unknown_modifier() {
        let err = KeyBinding::parse("hyper+a").expect_err("must reject");
        match err {
            KeyBindingParseError::UnknownModifier { token, .. } => {
                assert_eq!(token, "hyper");
            }
            other => panic!("expected UnknownModifier, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_key() {
        let err = KeyBinding::parse("ctrl+wibble").expect_err("must reject");
        match err {
            KeyBindingParseError::UnknownKey { token, .. } => {
                assert_eq!(token, "wibble");
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn rejects_two_non_modifier_tokens() {
        // The middle `a` is not a modifier; the parser surfaces this
        // as `UnknownModifier` because the contract requires every
        // token before the last to be a modifier.
        let err = KeyBinding::parse("a+b").expect_err("must reject");
        assert!(matches!(err, KeyBindingParseError::UnknownModifier { .. }));
    }

    #[test]
    fn rejects_duplicate_modifier() {
        let err = KeyBinding::parse("ctrl+ctrl+a").expect_err("must reject");
        match err {
            KeyBindingParseError::DuplicateModifier { modifier, .. } => {
                assert_eq!(modifier, "ctrl");
            }
            other => panic!("expected DuplicateModifier, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_modifier_via_alias() {
        let err = KeyBinding::parse("cmd+meta+a").expect_err("must reject");
        match err {
            KeyBindingParseError::DuplicateModifier { modifier, .. } => {
                assert_eq!(modifier, "cmd");
            }
            other => panic!("expected DuplicateModifier, got {other:?}"),
        }
    }

    #[test]
    fn rejects_lone_modifier_as_missing_key() {
        let err = KeyBinding::parse("ctrl").expect_err("must reject");
        assert!(matches!(err, KeyBindingParseError::MissingKey { .. }));
        let err_alias = KeyBinding::parse("cmd+meta").expect_err("must reject alias");
        assert!(matches!(err_alias, KeyBindingParseError::MissingKey { .. }));
    }

    #[test]
    fn rejects_function_key_out_of_range() {
        assert!(matches!(
            KeyBinding::parse("f0"),
            Err(KeyBindingParseError::UnknownKey { .. })
        ));
        assert!(matches!(
            KeyBinding::parse("f25"),
            Err(KeyBindingParseError::UnknownKey { .. })
        ));
    }

    #[test]
    fn display_emits_canonical_lowercase_form() {
        let kb = KeyBinding::new(
            Modifiers::CTRL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::CMD,
            Key::Char('p'),
        );
        assert_eq!(kb.to_string(), "ctrl+shift+alt+cmd+p");
    }

    #[test]
    fn display_emits_named_keys_without_underscores() {
        let kb = KeyBinding::new(Modifiers::CTRL, Key::ArrowLeft);
        assert_eq!(kb.to_string(), "ctrl+arrowleft");
    }

    #[test]
    fn display_omits_separator_when_no_modifiers() {
        let kb = KeyBinding::new(Modifiers::NONE, Key::F12);
        assert_eq!(kb.to_string(), "f12");
    }

    #[test]
    fn round_trip_holds_for_listed_acceptance_inputs() {
        let inputs = [
            "ctrl+shift+p",
            "cmd+w",
            "f12",
            "ctrl+arrowleft",
            "escape",
            "alt+enter",
            "ctrl+shift+alt+cmd+p",
        ];
        for input in inputs {
            let kb = KeyBinding::parse(input).expect("parse acceptance input");
            let displayed = kb.to_string();
            let again = KeyBinding::parse(&displayed).expect("parse displayed");
            assert_eq!(kb, again, "round-trip failed for {input:?}");
            assert_eq!(displayed, input, "canonical form mismatch for {input:?}");
        }
    }

    #[test]
    fn json_round_trip_uses_canonical_string() {
        let kb = KeyBinding::parse("ctrl+shift+p").expect("parse");
        let json = serde_json::to_string(&kb).expect("serialize");
        assert_eq!(json, "\"ctrl+shift+p\"");
        let decoded: KeyBinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, kb);
    }

    #[test]
    fn keybinding_profile_round_trips_through_json() {
        let mut profile = KeybindingProfile::new("default");
        profile.bindings.insert(
            CommandId::new("editor.save"),
            vec![KeyBinding::parse("ctrl+s").expect("parse")],
        );
        profile.bindings.insert(
            CommandId::new("palette.toggle"),
            vec![
                KeyBinding::parse("ctrl+shift+p").expect("primary"),
                KeyBinding::parse("cmd+shift+p").expect("alternate"),
            ],
        );

        let json = serde_json::to_string(&profile).expect("serialize");
        let decoded: KeybindingProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, profile);
    }

    proptest! {
        /// Any binding the parser produced must round-trip through its
        /// canonical string form. We generate over the closed key set
        /// plus modifier permutations and assert
        /// `parse(display(kb)) == kb`.
        #[test]
        fn round_trip_holds_for_any_parser_produced_binding(
            ctrl in any::<bool>(),
            shift in any::<bool>(),
            alt in any::<bool>(),
            cmd in any::<bool>(),
            key_idx in 0usize..NAMED_KEYS.len() + 36,
        ) {
            let key = if key_idx < NAMED_KEYS.len() {
                NAMED_KEYS[key_idx]
            } else {
                let offset = key_idx - NAMED_KEYS.len();
                if offset < 26 {
                    Key::Char((b'a' + offset as u8) as char)
                } else {
                    Key::Char((b'0' + (offset - 26) as u8) as char)
                }
            };
            let modifiers = Modifiers { ctrl, shift, alt, cmd };
            let kb = KeyBinding::new(modifiers, key);
            let displayed = kb.to_string();
            let parsed = KeyBinding::parse(&displayed).expect("round-trip parse");
            prop_assert_eq!(parsed, kb);
        }
    }

    /// Exhaustive sample of named/function keys for the proptest. The
    /// proptest combines this with all 16 modifier permutations and a
    /// span of `Key::Char` values, so the matrix it covers is wide
    /// enough to catch any divergence between Display and parse.
    const NAMED_KEYS: &[Key] = &[
        Key::Escape,
        Key::Tab,
        Key::Enter,
        Key::Backspace,
        Key::Delete,
        Key::Home,
        Key::End,
        Key::PageUp,
        Key::PageDown,
        Key::ArrowUp,
        Key::ArrowDown,
        Key::ArrowLeft,
        Key::ArrowRight,
        Key::Space,
        Key::F1,
        Key::F12,
        Key::F24,
    ];
}
