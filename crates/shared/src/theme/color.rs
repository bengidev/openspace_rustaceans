//! `Color` value type and its TOML/JSON-friendly parser.
//!
//! `Color` is intentionally a plain struct over four `u8` channels —
//! the Domain layer never depends on `palette`, `csscolorparser`, or any
//! other colour crate. UI adapters (PRD-04 onward) convert to whatever
//! the rendering layer needs at the boundary.
//!
//! The serde shape is the contract:
//!
//! - **Deserialiser** accepts four input forms so theme authors can pick
//!   the one that reads best in TOML:
//!     - `#RRGGBB` (alpha defaults to `0xFF`)
//!     - `#RRGGBBAA`
//!     - `rgb(r, g, b)` — decimal `u8` channels
//!     - `rgba(r, g, b, a)` — decimal `u8` for r/g/b; alpha is either a
//!       decimal `u8` (`0..=255`) or a decimal `f32` (`0.0..=1.0`).
//! - **Serialiser** always emits the canonical `#RRGGBBAA` form so a
//!   value written to disk and read back is bit-identical.
//!
//! Invalid input returns a serde error whose message names the offending
//! string so theme authors see a useful diagnostic in the loader (PRD-04).

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use static_assertions::assert_impl_all;

/// 8-bit-per-channel RGBA colour.
///
/// All four channels are public because consumers (Iced theme, syntax
/// highlighter, terminal grid) need direct access to compose with other
/// colour types. The struct is `Copy` so passing it through layout code
/// is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    /// Red channel, `0..=255`.
    pub r: u8,
    /// Green channel, `0..=255`.
    pub g: u8,
    /// Blue channel, `0..=255`.
    pub b: u8,
    /// Alpha channel, `0..=255`. `0xFF` is fully opaque.
    pub a: u8,
}

impl Color {
    /// Construct a colour from explicit RGBA channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Construct an opaque colour (alpha defaults to `0xFF`).
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }
}

impl fmt::Display for Color {
    /// Renders the canonical `#RRGGBBAA` form. Matches the serde wire
    /// shape so log output and persisted bytes line up.
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
        // Reuse `Display` so the canonical form has exactly one
        // implementation; a divergence between `to_string` and the
        // serialised form would be a silent bug.
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Borrow when possible (`&str`), but fall back to `String` so
        // the deserialiser also works against owning formats like JSON
        // values built with `serde_json::to_value`.
        let raw = <std::borrow::Cow<'_, str>>::deserialize(deserializer)?;
        parse_color(&raw).map_err(de::Error::custom)
    }
}

/// Parse a colour from one of the four supported input forms.
///
/// Returned errors always quote the original input so the loader can
/// surface "the value at `theme.ui.background` is invalid: …" without
/// having to thread context through the call.
fn parse_color(input: &str) -> Result<Color, String> {
    let trimmed = input.trim();

    if let Some(hex) = trimmed.strip_prefix('#') {
        return parse_hex(hex, input);
    }
    // Order matters: `rgba(` is a strict prefix of nothing in particular
    // but `rgb(` is a strict prefix of `rgba(` if we stripped naively.
    // Check the longer form first.
    if let Some(inner) = trimmed
        .strip_prefix("rgba(")
        .and_then(|t| t.strip_suffix(')'))
    {
        return parse_rgba(inner, input);
    }
    if let Some(inner) = trimmed
        .strip_prefix("rgb(")
        .and_then(|t| t.strip_suffix(')'))
    {
        return parse_rgb(inner, input);
    }

    Err(format!(
        "invalid color {input:?}: expected `#RRGGBB`, `#RRGGBBAA`, \
         `rgb(r, g, b)`, or `rgba(r, g, b, a)`"
    ))
}

fn parse_hex(hex: &str, original: &str) -> Result<Color, String> {
    let byte = |slice: &str, channel: &str| -> Result<u8, String> {
        u8::from_str_radix(slice, 16).map_err(|_| {
            format!(
                "invalid color {original:?}: {channel} channel {slice:?} \
                 is not a hex byte"
            )
        })
    };

    match hex.len() {
        6 => {
            let r = byte(&hex[0..2], "red")?;
            let g = byte(&hex[2..4], "green")?;
            let b = byte(&hex[4..6], "blue")?;
            // `#RRGGBB` defaults alpha to fully opaque so theme authors
            // can drop the channel when they do not need translucency.
            Ok(Color::new(r, g, b, 0xFF))
        }
        8 => {
            let r = byte(&hex[0..2], "red")?;
            let g = byte(&hex[2..4], "green")?;
            let b = byte(&hex[4..6], "blue")?;
            let a = byte(&hex[6..8], "alpha")?;
            Ok(Color::new(r, g, b, a))
        }
        _ => Err(format!(
            "invalid color {original:?}: hex form must be `#RRGGBB` or `#RRGGBBAA`"
        )),
    }
}

fn parse_rgb(inner: &str, original: &str) -> Result<Color, String> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        return Err(format!(
            "invalid color {original:?}: `rgb(...)` requires exactly three channels"
        ));
    }
    let r = parse_u8(parts[0], "red", original)?;
    let g = parse_u8(parts[1], "green", original)?;
    let b = parse_u8(parts[2], "blue", original)?;
    Ok(Color::new(r, g, b, 0xFF))
}

fn parse_rgba(inner: &str, original: &str) -> Result<Color, String> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "invalid color {original:?}: `rgba(...)` requires exactly four channels"
        ));
    }
    let r = parse_u8(parts[0], "red", original)?;
    let g = parse_u8(parts[1], "green", original)?;
    let b = parse_u8(parts[2], "blue", original)?;
    let a = parse_alpha(parts[3], original)?;
    Ok(Color::new(r, g, b, a))
}

fn parse_u8(field: &str, channel: &str, original: &str) -> Result<u8, String> {
    field.parse::<u8>().map_err(|_| {
        format!("invalid color {original:?}: {channel} channel {field:?} must be 0..=255")
    })
}

/// Alpha accepts two encodings inside `rgba(...)`:
///
/// - integer `0..=255` (matches the other channels), or
/// - float `0.0..=1.0` (matches CSS `rgba(...)` convention).
///
/// We disambiguate by whether the slice contains a decimal point. That
/// is a deliberate, narrow rule — `1` is a u8, `1.0` is a float — so
/// that rounding behaviour stays predictable.
fn parse_alpha(field: &str, original: &str) -> Result<u8, String> {
    if field.contains('.') {
        let value: f32 = field
            .parse()
            .map_err(|_| format!("invalid color {original:?}: alpha {field:?} is not a number"))?;
        if value.is_nan() || !(0.0..=1.0).contains(&value) {
            return Err(format!(
                "invalid color {original:?}: float alpha {field:?} must be in 0.0..=1.0"
            ));
        }
        // Round-half-away-from-zero so `0.5 → 128` instead of the
        // banker's-rounding surprise that `as u8` would produce on its
        // own. The cast is safe because we just clamped to `[0, 1]`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok((value * 255.0).round() as u8)
    } else {
        parse_u8(field, "alpha", original)
    }
}

assert_impl_all!(Color: Send, Sync);

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::prelude::*;
    use serde::de::value::{Error as DeError, StrDeserializer};
    use serde::de::IntoDeserializer;

    /// Helper: drive `Deserialize` from a raw string the way a real
    /// loader would. Using the stock `StrDeserializer` means the test
    /// exercises the same code path serde uses against TOML/JSON.
    fn deserialize(input: &str) -> Result<Color, DeError> {
        let de: StrDeserializer<'_, DeError> = input.into_deserializer();
        Color::deserialize(de)
    }

    #[test]
    fn hex_rgb_form_defaults_alpha_to_opaque() {
        let color = deserialize("#1A2B3C").expect("parse");
        assert_eq!(color, Color::new(0x1A, 0x2B, 0x3C, 0xFF));
    }

    #[test]
    fn hex_rgba_form_preserves_alpha() {
        let color = deserialize("#1A2B3C7F").expect("parse");
        assert_eq!(color, Color::new(0x1A, 0x2B, 0x3C, 0x7F));
    }

    #[test]
    fn hex_form_is_case_insensitive() {
        let upper = deserialize("#AABBCC").expect("parse upper");
        let lower = deserialize("#aabbcc").expect("parse lower");
        assert_eq!(upper, lower);
    }

    #[test]
    fn rgb_form_accepts_decimal_channels() {
        let color = deserialize("rgb(10, 20, 30)").expect("parse");
        assert_eq!(color, Color::new(10, 20, 30, 0xFF));
    }

    #[test]
    fn rgba_form_accepts_integer_alpha() {
        let color = deserialize("rgba(10, 20, 30, 200)").expect("parse");
        assert_eq!(color, Color::new(10, 20, 30, 200));
    }

    #[test]
    fn rgba_form_accepts_float_alpha() {
        let color = deserialize("rgba(10, 20, 30, 0.5)").expect("parse");
        assert_eq!(color, Color::new(10, 20, 30, 128));
    }

    #[test]
    fn rgba_float_alpha_clamped_endpoints() {
        let zero = deserialize("rgba(0, 0, 0, 0.0)").expect("parse");
        let one = deserialize("rgba(0, 0, 0, 1.0)").expect("parse");
        assert_eq!(zero.a, 0);
        assert_eq!(one.a, 255);
    }

    #[test]
    fn whitespace_around_input_is_tolerated() {
        let color = deserialize("   #112233   ").expect("parse");
        assert_eq!(color, Color::new(0x11, 0x22, 0x33, 0xFF));
    }

    #[test]
    fn rejects_unknown_format() {
        let err = deserialize("hsl(0, 0%, 0%)").expect_err("must reject");
        let message = err.to_string();
        assert!(
            message.contains("hsl(0, 0%, 0%)"),
            "diagnostic must quote the offending input, got: {message}"
        );
    }

    #[test]
    fn rejects_short_hex() {
        let err = deserialize("#FFF").expect_err("must reject");
        assert!(err.to_string().contains("\"#FFF\""));
    }

    #[test]
    fn rejects_non_hex_characters() {
        let err = deserialize("#ZZZZZZ").expect_err("must reject");
        assert!(err.to_string().contains("\"#ZZZZZZ\""));
    }

    #[test]
    fn rejects_rgb_channel_out_of_range() {
        let err = deserialize("rgb(300, 0, 0)").expect_err("must reject");
        assert!(err.to_string().contains("\"rgb(300, 0, 0)\""));
    }

    #[test]
    fn rejects_rgba_float_alpha_out_of_range() {
        let err = deserialize("rgba(0, 0, 0, 1.5)").expect_err("must reject");
        assert!(err.to_string().contains("\"rgba(0, 0, 0, 1.5)\""));
    }

    #[test]
    fn rejects_rgba_with_wrong_arity() {
        let err = deserialize("rgba(0, 0, 0)").expect_err("must reject");
        assert!(err.to_string().contains("four channels"));
    }

    #[test]
    fn serialize_emits_canonical_uppercase_rgba() {
        let color = Color::new(0x1A, 0x2B, 0x3C, 0x4D);
        let json = serde_json::to_string(&color).expect("serialize");
        assert_eq!(json, "\"#1A2B3C4D\"");
    }

    #[test]
    fn serialize_pads_single_digit_channels() {
        let color = Color::new(0x01, 0x02, 0x03, 0x04);
        assert_eq!(color.to_string(), "#01020304");
    }

    proptest! {
        /// Every `(r, g, b, a)` tuple must round-trip through the
        /// canonical wire form. This is the headline property from the
        /// acceptance criteria for issue #28.
        #[test]
        fn color_round_trips_through_json(
            r in any::<u8>(),
            g in any::<u8>(),
            b in any::<u8>(),
            a in any::<u8>(),
        ) {
            let original = Color::new(r, g, b, a);
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: Color = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(original, decoded);
        }

        /// `rgb(r, g, b)` always parses to opaque, regardless of the
        /// channels. Pairs the hex form's alpha defaulting rule.
        #[test]
        fn rgb_form_is_always_opaque(
            r in any::<u8>(),
            g in any::<u8>(),
            b in any::<u8>(),
        ) {
            let input = format!("rgb({r}, {g}, {b})");
            let parsed = parse_color(&input).expect("parse");
            prop_assert_eq!(parsed, Color::new(r, g, b, 0xFF));
        }
    }
}
