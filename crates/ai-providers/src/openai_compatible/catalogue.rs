//! Model catalogue and capability detection for the openai-compatible
//! adapter.
//!
//! The adapter learns about models from one of three sources, in
//! priority order:
//!
//! 1. **Dynamic** — the `/models` response carries
//!    `supported_parameters` (or an equivalent capability hint)
//!    advertising the per-model feature set. The adapter parses the
//!    hint into [`Capabilities`] verbatim.
//! 2. **Heuristic** — the model id matches a well-known family in
//!    [`heuristic_capabilities`]. The catalogue ships a small static
//!    table covering the chat-class families documented at the time
//!    of writing.
//! 3. **Fallback** — neither source produced a hint. The adapter
//!    falls back to a conservative default of
//!    `Capabilities { vision: false, tool_calling: true,
//!    streaming: true }` for chat-class models, matching the issue's
//!    acceptance criteria.
//!
//! The dynamic path also recognises a small alias set on the
//! capability tokens that surface in the wild — `tools` /
//! `function_calling` → tool calling; `vision` / `image_url` /
//! `images` / `multimodal` → vision; `stream` / `streaming` →
//! streaming. Unknown tokens are ignored so a future capability
//! flag never crashes the parser.
//!
//! Naming policy: nothing in this file names a vendor or hosted
//! service. The heuristic table keys off neutral substrings the
//! upstream chat-class families happen to share (`"gpt"`, `"o-"`,
//! `"glm"`, …) so the table reads like a feature gate rather than a
//! product catalogue. New rows go through the same review as any
//! commit — see `AGENTS.md` for the rule.

use openspace_shared::ai::domain::{Capabilities, ModelInfo, ModelRef};

use super::wire::ModelEntry;

/// Default context window when the upstream omits the field.
///
/// Most modern chat-class models ship with at least 8k context; the
/// catalogue picks a conservative 4k so a model picker rendering an
/// unknown model never advertises a window the upstream cannot
/// honour. Operators can replace the value via a future
/// `with_context_window_default` knob if it ever becomes a
/// constraint.
const DEFAULT_CONTEXT_WINDOW: u32 = 4_096;

/// Conservative fallback advertised when neither the dynamic hint
/// nor the heuristic table produced a verdict.
///
/// Tool calling is the only feature this fallback turns on: the
/// upstream wire format requires `/v1/chat/completions` to honour
/// `tool_calls` for any chat-class model, and the issue's acceptance
/// criteria explicitly call for `tool_calling: true` here. Vision
/// stays off because a model that does not advertise it cannot accept
/// `image_url` content blocks; streaming stays on because the
/// adapter only speaks the streaming variant.
pub(super) fn fallback_capabilities() -> Capabilities {
    Capabilities::new(false, true, true)
}

/// Detect [`Capabilities`] from a `/models` row.
///
/// Priority follows the module-level docs: dynamic first, then
/// heuristic, then fallback.
pub(super) fn detect_capabilities(entry: &ModelEntry) -> Capabilities {
    if let Some(tokens) = entry.supported_parameters.as_deref() {
        return capabilities_from_tokens(tokens);
    }
    if let Some(caps) = heuristic_capabilities(&entry.id) {
        return caps;
    }
    fallback_capabilities()
}

/// Resolve a [`Capabilities`] view for a bare model id.
///
/// Pre-flight gate in `chat_stream` calls into this without an
/// upstream round trip — heuristic table first, fallback if the id
/// is not recognised. The dynamic `/models`-driven path stays
/// available through [`detect_capabilities`] for callers that have
/// already consumed a row.
pub(super) fn capabilities_for(model_id: &str) -> Capabilities {
    heuristic_capabilities(model_id).unwrap_or_else(fallback_capabilities)
}

/// Translate a `supported_parameters` token list into a
/// [`Capabilities`] view.
///
/// Tokens are matched case-insensitively. Any unrecognised token is
/// silently ignored — a future capability flag never breaks parsing.
fn capabilities_from_tokens(tokens: &[String]) -> Capabilities {
    let mut vision = false;
    let mut tool_calling = false;
    let mut streaming = false;
    for raw in tokens {
        let token = raw.trim().to_ascii_lowercase();
        match token.as_str() {
            "tools" | "tool_calling" | "function_calling" | "functions" => {
                tool_calling = true;
            }
            "vision" | "image_url" | "image" | "images" | "multimodal" => {
                vision = true;
            }
            "stream" | "streaming" => {
                streaming = true;
            }
            _ => {}
        }
    }
    Capabilities::new(vision, tool_calling, streaming)
}

/// Heuristic capability table for well-known chat-class families.
///
/// Returns `None` for ids the table does not recognise; the caller
/// then falls back to [`fallback_capabilities`].
///
/// The table keys off neutral substrings the upstream families
/// happen to share. New rows must:
///
/// - Use a substring stable enough that minor revisions of the same
///   family share it.
/// - Stay tight enough to avoid false matches against unrelated ids
///   (e.g. `"gpt"` matches the well-known chat family without also
///   matching every model whose name contains the three letters
///   incidentally).
fn heuristic_capabilities(id: &str) -> Option<Capabilities> {
    let lower = id.to_ascii_lowercase();
    // Known multimodal chat-class families — vision + tool calling +
    // streaming.
    const MULTIMODAL_NEEDLES: &[&str] = &[
        "gpt-4o",
        "gpt-4-vision",
        "gpt-4-turbo",
        "gpt-4.1",
        "gemini-1.5",
        "gemini-2",
        "qwen-vl",
        "qwen2-vl",
        "qwen2.5-vl",
        "llava",
        "pixtral",
        "molmo",
        "internvl",
    ];
    if MULTIMODAL_NEEDLES
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return Some(Capabilities::new(true, true, true));
    }
    // Text-only chat-class families that still support tool calling.
    const TOOL_TEXT_NEEDLES: &[&str] = &[
        "gpt-4-",
        "gpt-3.5",
        "gpt-3.5-turbo",
        "qwen2",
        "qwen-2",
        "qwen2.5",
        "llama-3",
        "llama3",
        "mixtral",
        "mistral",
        "command-r",
        "deepseek",
        "glm-4",
    ];
    if TOOL_TEXT_NEEDLES
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return Some(Capabilities::new(false, true, true));
    }
    None
}

/// Build a [`ModelInfo`] from a `/models` row.
///
/// Pairs the [`detect_capabilities`] verdict with the row's id, an
/// optional human-friendly display label derived from `owned_by`, and
/// a context-window figure that falls back to
/// [`DEFAULT_CONTEXT_WINDOW`] when the upstream omits it.
pub(super) fn make_model_info(provider_id: &str, entry: &ModelEntry) -> ModelInfo {
    let display_name = match entry.owned_by.as_deref() {
        Some(owner) if !owner.is_empty() => format!("{} ({owner})", entry.id),
        _ => entry.id.clone(),
    };
    let context_window = entry
        .context_window_value()
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let capabilities = detect_capabilities(entry);
    ModelInfo::new(
        ModelRef::new(provider_id, &entry.id),
        display_name,
        context_window,
        capabilities,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(
        id: &str,
        owned_by: Option<&str>,
        context_window: Option<u32>,
        params: Option<Vec<&str>>,
    ) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            owned_by: owned_by.map(str::to_string),
            context_window,
            context_length: None,
            max_context_length: None,
            supported_parameters: params
                .map(|tokens| tokens.into_iter().map(str::to_string).collect()),
        }
    }

    #[test]
    fn dynamic_hint_takes_precedence_over_heuristic() {
        // `gpt-4o` heuristically advertises everything, but the
        // dynamic hint is authoritative — if upstream says no
        // vision, we believe it.
        let entry = entry_with("gpt-4o", None, None, Some(vec!["tools", "streaming"]));
        let caps = detect_capabilities(&entry);
        assert!(!caps.vision, "dynamic hint must beat the heuristic");
        assert!(caps.tool_calling);
        assert!(caps.streaming);
    }

    #[test]
    fn heuristic_recognises_multimodal_family() {
        let caps = heuristic_capabilities("gpt-4o-mini").expect("matches multimodal");
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert!(caps.streaming);
    }

    #[test]
    fn heuristic_recognises_text_only_tool_family() {
        let caps = heuristic_capabilities("gpt-3.5-turbo-0125").expect("matches text-tool family");
        assert!(!caps.vision);
        assert!(caps.tool_calling);
        assert!(caps.streaming);
    }

    #[test]
    fn fallback_kicks_in_for_unknown_ids() {
        let entry = entry_with("brand-new-model-001", None, None, None);
        let caps = detect_capabilities(&entry);
        assert_eq!(caps, fallback_capabilities());
    }

    #[test]
    fn token_aliases_collapse_onto_capability_flags() {
        let entry = entry_with(
            "x",
            None,
            None,
            Some(vec!["FUNCTION_CALLING", "image_url", "stream"]),
        );
        let caps = detect_capabilities(&entry);
        assert!(caps.tool_calling);
        assert!(caps.vision);
        assert!(caps.streaming);
    }

    #[test]
    fn unknown_tokens_are_ignored_without_breaking_parsing() {
        let entry = entry_with("x", None, None, Some(vec!["future_flag", "stream", "noop"]));
        let caps = detect_capabilities(&entry);
        assert!(caps.streaming);
        assert!(!caps.tool_calling);
        assert!(!caps.vision);
    }

    #[test]
    fn make_model_info_decorates_display_name_with_owner() {
        let entry = entry_with("model-x", Some("acme"), Some(32_768), None);
        let info = make_model_info("provider-id", &entry);
        assert_eq!(info.model_ref.provider_id, "provider-id");
        assert_eq!(info.model_ref.model_id, "model-x");
        assert_eq!(info.context_window, 32_768);
        assert!(info.display_name.contains("model-x"));
        assert!(info.display_name.contains("acme"));
    }

    #[test]
    fn make_model_info_uses_default_context_window_when_missing() {
        let entry = entry_with("model-x", None, None, None);
        let info = make_model_info("p", &entry);
        assert_eq!(info.context_window, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn context_window_aliases_collapse_to_one_value() {
        let mut entry = entry_with("m", None, None, None);
        entry.context_length = Some(8_192);
        let info = make_model_info("p", &entry);
        assert_eq!(info.context_window, 8_192);

        let mut entry = entry_with("m", None, None, None);
        entry.max_context_length = Some(16_384);
        let info = make_model_info("p", &entry);
        assert_eq!(info.context_window, 16_384);

        // `context_window` wins over the aliases when populated.
        let mut entry = entry_with("m", None, Some(2_048), None);
        entry.context_length = Some(8_192);
        let info = make_model_info("p", &entry);
        assert_eq!(info.context_window, 2_048);
    }
}
