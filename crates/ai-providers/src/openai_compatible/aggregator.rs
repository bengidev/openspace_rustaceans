//! Preset surface for the hosted multi-model aggregator that
//! exposes the industry-standard `/v1/chat/completions` wire format.
//!
//! Some users reach several model vendors through a single hosted
//! aggregator service. The aggregator speaks the same wire format
//! the [`super::OpenAiCompatibleProvider`] already implements; the
//! only delta is a pinned base URL plus an optional pair of
//! attribution headers the aggregator surfaces on its public
//! dashboards.
//!
//! This module contributes:
//!
//! - [`AGGREGATOR_BASE_URL`] — the documented chat-completions
//!   endpoint, baked in so callers do not retype it. Treated as a
//!   wire-format constant; the brand domain in the URL is
//!   unavoidable infrastructure, but no identifier, doc string, or
//!   error string in this crate names the vendor.
//! - [`HEADER_REFERER`] / [`HEADER_TITLE`] — the canonical header
//!   names the aggregator's documentation specifies. Wire-format
//!   constants per the naming policy in `AGENTS.md`.
//! - [`AttributionMode`] — an explicit enum that forces the caller
//!   to choose between sending the attribution headers and omitting
//!   them. Default app behaviour wires `Off`; surfacing the opt-in
//!   is a job for a later settings slice.
//!
//! The actual constructor lives on
//! [`super::OpenAiCompatibleProvider`] (`aggregator(...)`); see its
//! doc for the full builder contract.

/// Documented chat-completions base URL for the hosted aggregator.
///
/// Stored as a wire-format constant: callers consume it through the
/// preset constructor on [`super::OpenAiCompatibleProvider`] rather
/// than referencing it directly. Kept `pub(crate)` so the
/// integration tests can pin the round-trip behaviour without
/// re-exporting the URL into downstream application code, where it
/// would invite hard-coding.
pub(crate) const AGGREGATOR_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Canonical header name carrying the calling site / app origin
/// when [`AttributionMode::OptIn`] is selected.
///
/// Wire-format constant; kept verbatim per the naming-policy
/// carve-out for protocol identifiers.
pub(crate) const HEADER_REFERER: &str = "HTTP-Referer";

/// Canonical header name carrying the calling app's display title
/// when [`AttributionMode::OptIn`] is selected.
///
/// Wire-format constant; kept verbatim per the naming-policy
/// carve-out for protocol identifiers.
pub(crate) const HEADER_TITLE: &str = "X-Title";

/// Whether the aggregator preset attaches attribution headers to
/// every outbound request.
///
/// The aggregator's public documentation describes the headers as
/// optional; the calling app must opt in deliberately. The default
/// app behaviour therefore wires [`AttributionMode::Off`] and the
/// settings layer (later slice) flips it on with user-supplied
/// values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributionMode {
    /// Omit both attribution headers entirely. Default app
    /// behaviour.
    Off,

    /// Send both attribution headers with the configured values.
    ///
    /// Both fields are required: the aggregator treats the pair as
    /// a single attribution unit, and surfacing only one would be
    /// confusing on the dashboards.
    OptIn {
        /// Value sent as `HEADER_REFERER`. Typically the calling
        /// site's URL or app origin.
        ///
        /// Plain backticks rather than an intra-doc link: the
        /// constant is `pub(crate)` and `cargo doc` would refuse
        /// to resolve a public link to a private item.
        referer: String,

        /// Value sent as `HEADER_TITLE`. Typically the calling
        /// app's display name.
        ///
        /// Plain backticks rather than an intra-doc link, for the
        /// same reason as `referer` above.
        title: String,
    },
}

impl AttributionMode {
    /// Convenience: build an [`AttributionMode::OptIn`] from string
    /// slices without forcing the caller to write `String::from`
    /// twice. Matches the ergonomic shape of the other builders on
    /// [`super::OpenAiCompatibleProvider`].
    #[must_use]
    pub fn opt_in(referer: impl Into<String>, title: impl Into<String>) -> Self {
        Self::OptIn {
            referer: referer.into(),
            title: title.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_constructor_round_trips_values() {
        let mode = AttributionMode::opt_in("https://example.test", "Example App");
        match mode {
            AttributionMode::OptIn { referer, title } => {
                assert_eq!(referer, "https://example.test");
                assert_eq!(title, "Example App");
            }
            AttributionMode::Off => panic!("expected OptIn, got Off"),
        }
    }

    #[test]
    fn off_variant_is_distinct_from_opt_in() {
        assert_ne!(AttributionMode::Off, AttributionMode::opt_in("a", "b"));
    }

    #[test]
    fn header_constants_match_documented_canonical_names() {
        // Pinning the canonical header names here keeps an
        // accidental rename ("HTTP-Referrer", "X-App-Title", ...)
        // from sailing through review unnoticed.
        assert_eq!(HEADER_REFERER, "HTTP-Referer");
        assert_eq!(HEADER_TITLE, "X-Title");
    }
}
