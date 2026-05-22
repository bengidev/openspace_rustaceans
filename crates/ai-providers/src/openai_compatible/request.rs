//! Conversation → wire-format request mapping for the
//! openai-compatible adapter.
//!
//! Pulled out of [`super::provider`] so the structural mapping rules
//! live next to the wire types they translate into. The text-only
//! slice carried this logic inline; the extended slice (#49) needs
//! room for tool calls, vision parts, and a capability-mismatch
//! guard, and shoehorning all of it into the streaming pump file
//! made the chat-stream surface harder to read than it had to be.
//!
//! # Public-to-the-module surface
//!
//! - [`build_messages`] — projects every [`Turn`] in a
//!   [`Conversation`] onto the wire `messages` array. Emits a leading
//!   `system` message when the conversation carries a system prompt.
//! - [`check_capabilities`] — guards the call against a
//!   [`Capabilities`] view (typically resolved from the per-model
//!   catalogue row). Surfaces [`AiError::Unsupported`] when the
//!   conversation needs a flag the model does not advertise.
//! - [`build_url`] — composes the `/chat/completions` URL from the
//!   configured base URL.
//! - [`build_request`] — assembles the [`reqwest::Request`] with the
//!   `Authorization`, `Content-Type`, `Accept`, and any configured
//!   extra headers.
//! - [`models_url`] — composes the `/models` discovery URL.
//! - [`build_models_request`] — assembles the GET request the
//!   `list_models` path issues.
//!
//! # Mapping rules — Conversation → wire messages
//!
//! - `Role::System` → `"system"`. Carried verbatim as the leading
//!   message when [`Conversation::system_prompt`] is set; inline
//!   `Role::System` turns also surface here in turn order.
//! - `Role::User` → `"user"`. Text parts become string content;
//!   image parts become `image_url` content blocks. A turn that
//!   mixes text and image parts surfaces as a structured
//!   [`MessageContent::Parts`] body.
//! - `Role::Assistant` → `"assistant"`. Text parts become string
//!   content; [`Part::ToolCall`] parts become `tool_calls` entries
//!   on the message envelope. An assistant turn that contains only
//!   tool calls (no prose) emits a `null` content body — the wire
//!   format permits that shape.
//! - `Role::Tool` → `"tool"`. The first
//!   [`Part::ToolResult`] in the turn supplies `tool_call_id` and
//!   the response body; additional tool results in the same turn
//!   become subsequent `tool` messages so the wire array stays
//!   structurally honest. Text parts in a tool turn (rare but
//!   legal) flow through as text content on the same message.
//!
//! `serde_json::to_string` is used to serialise tool-call arguments
//! because the upstream wire format treats `arguments` as an
//! escaped JSON string, not a structured value. The Domain side
//! holds the structured representation; the conversion is one-way.
//!
//! [`Conversation`]: openspace_shared::ai::domain::Conversation
//! [`Conversation::system_prompt`]: openspace_shared::ai::domain::Conversation::system_prompt
//! [`Capabilities`]: openspace_shared::ai::domain::Capabilities
//! [`Turn`]: openspace_shared::ai::domain::Turn
//! [`Part::ToolCall`]: openspace_shared::ai::domain::Part::ToolCall
//! [`Part::ToolResult`]: openspace_shared::ai::domain::Part::ToolResult

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use openspace_shared::ai::domain::{Capabilities, Conversation, ImagePayload, Part, Role};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;

use crate::client::SandboxedHttpClient;

use super::wire::{
    ChatMessage, ContentPart, ImageUrlBlock, MessageContent, ToolCallFunction, ToolCallRequest,
};

/// Compose the chat-completions URL from the configured base URL.
///
/// Trailing slashes on the base URL are tolerated so callers can
/// pass either `https://example/v1` or `https://example/v1/`.
pub(super) fn build_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{trimmed}/chat/completions")
}

/// Compose the `/models` discovery URL from the configured base URL.
pub(super) fn models_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{trimmed}/models")
}

/// Build the array of `messages` from a [`Conversation`].
///
/// See the module-level docs for the per-role mapping table. Rejects
/// shapes the wire format cannot represent (e.g. an `Assistant` turn
/// whose tool-call arguments cannot be re-serialised as JSON) with
/// [`AiError::InvalidRequest`].
pub(super) fn build_messages(conv: &Conversation) -> Result<Vec<ChatMessage>, AiError> {
    // Capacity hint: one message per turn plus one for the optional
    // system prompt. Tool turns may emit several messages, so the
    // capacity is a lower bound rather than an exact figure.
    let mut messages = Vec::with_capacity(conv.turns.len() + 1);
    if let Some(system) = conv.system_prompt.as_deref() {
        messages.push(ChatMessage {
            role: "system",
            content: Some(MessageContent::Text(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for turn in &conv.turns {
        match turn.role {
            Role::System => messages.push(system_message(&turn.parts)?),
            Role::User => messages.push(user_message(&turn.parts)?),
            Role::Assistant => messages.push(assistant_message(&turn.parts)?),
            Role::Tool => extend_with_tool_messages(&mut messages, &turn.parts)?,
        }
    }
    Ok(messages)
}

/// Compare the conversation against the resolved [`Capabilities`].
///
/// Returns [`AiError::Unsupported`] when the conversation needs a
/// flag the model does not advertise. The mapping today:
///
/// - [`Part::Image`] in any turn → requires `capabilities.vision`.
/// - [`Part::ToolCall`] or [`Part::ToolResult`] in any turn → requires
///   `capabilities.tool_calling`.
///
/// Streaming is always required by this adapter (the request body
/// pins `stream: true`); a model whose catalogue row reports
/// `streaming: false` rejects the call here too so the upstream
/// does not silently down-grade to a non-streaming reply.
pub(super) fn check_capabilities(
    conv: &Conversation,
    capabilities: Capabilities,
    model_label: &str,
) -> Result<(), AiError> {
    let needs_vision = conv
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .any(|p| matches!(p, Part::Image { .. }));
    if needs_vision && !capabilities.vision {
        return Err(AiError::Unsupported(format!(
            "image input on a non-vision model ({model_label})"
        )));
    }

    let needs_tools = conv
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .any(|p| matches!(p, Part::ToolCall { .. } | Part::ToolResult { .. }));
    if needs_tools && !capabilities.tool_calling {
        return Err(AiError::Unsupported(format!(
            "tool calling on a tool-less model ({model_label})"
        )));
    }

    if !capabilities.streaming {
        return Err(AiError::Unsupported(format!(
            "streaming requested on a non-streaming model ({model_label})"
        )));
    }

    Ok(())
}

fn system_message(parts: &[Part]) -> Result<ChatMessage, AiError> {
    let text = collect_plain_text(parts, "system")?;
    Ok(ChatMessage {
        role: "system",
        content: Some(MessageContent::Text(text)),
        tool_calls: None,
        tool_call_id: None,
    })
}

fn user_message(parts: &[Part]) -> Result<ChatMessage, AiError> {
    // User turns combine text and image parts. The fast path keeps
    // the text-only shape (single `String`) so the wire output stays
    // byte-identical to the earlier slice for plain prose.
    let mut content_parts: Vec<ContentPart> = Vec::with_capacity(parts.len());
    let mut all_text = true;
    for part in parts {
        match part {
            Part::Text(text) => {
                content_parts.push(ContentPart::Text { text: text.clone() });
            }
            Part::Image { mime, data } => {
                all_text = false;
                content_parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrlBlock {
                        url: image_to_url(mime, data),
                    },
                });
            }
            other => {
                return Err(AiError::InvalidRequest(format!(
                    "user turn cannot carry {} parts",
                    part_kind(other)
                )));
            }
        }
    }
    let content = if all_text {
        Some(MessageContent::Text(merge_text_parts(&content_parts)))
    } else {
        Some(MessageContent::Parts(content_parts))
    };
    Ok(ChatMessage {
        role: "user",
        content,
        tool_calls: None,
        tool_call_id: None,
    })
}

fn assistant_message(parts: &[Part]) -> Result<ChatMessage, AiError> {
    // Assistant turns interleave text and tool_calls. Text parts
    // collapse onto the message's `content` body; tool-call parts
    // accumulate into the `tool_calls` array. A turn that emits
    // only tool calls and no prose carries `null` content per the
    // upstream spec.
    let mut text_pieces: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => text_pieces.push(text.clone()),
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                let serialised = serde_json::to_string(arguments).map_err(|e| {
                    AiError::InvalidRequest(format!(
                        "tool-call arguments are not serialisable as JSON: {e}"
                    ))
                })?;
                tool_calls.push(ToolCallRequest {
                    id: id.clone(),
                    call_type: "function",
                    function: ToolCallFunction {
                        name: name.clone(),
                        arguments: serialised,
                    },
                });
            }
            other => {
                return Err(AiError::InvalidRequest(format!(
                    "assistant turn cannot carry {} parts",
                    part_kind(other)
                )));
            }
        }
    }
    let content = if text_pieces.is_empty() {
        None
    } else {
        Some(MessageContent::Text(text_pieces.join("")))
    };
    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    Ok(ChatMessage {
        role: "assistant",
        content,
        tool_calls,
        tool_call_id: None,
    })
}

/// Tool turns expand into one wire-format `tool` message per
/// [`Part::ToolResult`]. Text parts that share the turn fold onto
/// the first `tool` message's content body; if no tool result is
/// present the turn is rejected as malformed.
fn extend_with_tool_messages(
    messages: &mut Vec<ChatMessage>,
    parts: &[Part],
) -> Result<(), AiError> {
    let mut tool_results: Vec<&Part> = Vec::new();
    let mut leading_text: Vec<String> = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => leading_text.push(text.clone()),
            Part::ToolResult { .. } => tool_results.push(part),
            other => {
                return Err(AiError::InvalidRequest(format!(
                    "tool turn cannot carry {} parts",
                    part_kind(other)
                )));
            }
        }
    }
    if tool_results.is_empty() {
        return Err(AiError::InvalidRequest(
            "tool turn has no Part::ToolResult — at least one is required".to_string(),
        ));
    }
    for (idx, part) in tool_results.into_iter().enumerate() {
        let Part::ToolResult {
            call_id,
            content,
            is_error: _,
        } = part
        else {
            unreachable!("filtered above")
        };
        // The wire format wants a string body. Render structured
        // payloads via `serde_json::to_string` so a `{ ok: true }`
        // result rides through verbatim. The first tool message
        // also picks up any leading text the turn carried.
        let body = match content {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).map_err(|e| {
                AiError::InvalidRequest(format!(
                    "tool-result content is not serialisable as JSON: {e}"
                ))
            })?,
        };
        let body = if idx == 0 && !leading_text.is_empty() {
            format!("{}{body}", leading_text.join(""))
        } else {
            body
        };
        messages.push(ChatMessage {
            role: "tool",
            content: Some(MessageContent::Text(body)),
            tool_calls: None,
            tool_call_id: Some(call_id.clone()),
        });
    }
    Ok(())
}

fn collect_plain_text(parts: &[Part], role_label: &str) -> Result<String, AiError> {
    let mut buf = String::new();
    for part in parts {
        match part {
            Part::Text(text) => buf.push_str(text),
            other => {
                return Err(AiError::InvalidRequest(format!(
                    "{role_label} turn cannot carry {} parts",
                    part_kind(other)
                )));
            }
        }
    }
    Ok(buf)
}

fn merge_text_parts(parts: &[ContentPart]) -> String {
    let mut buf = String::new();
    for part in parts {
        if let ContentPart::Text { text } = part {
            buf.push_str(text);
        }
    }
    buf
}

fn image_to_url(mime: &str, data: &ImagePayload) -> String {
    match data {
        ImagePayload::Url(url) => url.clone(),
        ImagePayload::Bytes(bytes) => {
            let encoded = BASE64_STANDARD.encode(bytes);
            format!("data:{mime};base64,{encoded}")
        }
    }
}

fn part_kind(part: &Part) -> &'static str {
    match part {
        Part::Text(_) => "text",
        Part::ToolCall { .. } => "tool-call",
        Part::ToolResult { .. } => "tool-result",
        Part::Image { .. } => "image",
        // `Part` is `#[non_exhaustive]`; surface unknown variants
        // with a stable label so a future addition does not silently
        // produce an empty diagnostic.
        _ => "unknown",
    }
}

/// Resolve the API key, build the headers, and assemble the
/// [`reqwest::Request`] for `POST /chat/completions`.
pub(super) fn build_request<B: Serialize>(
    http: &SandboxedHttpClient,
    url: &str,
    api_key_ref: &SecretRef,
    secret_store: &dyn SecretStore,
    extra_headers: &[(String, String)],
    body: &B,
) -> Result<reqwest::Request, AiError> {
    let api_key = resolve_api_key(api_key_ref, secret_store)?;
    let headers = build_headers(&api_key, extra_headers, /* streaming = */ true)?;

    http.client()
        .post(url)
        .headers(headers)
        .json(body)
        .build()
        .map_err(|e| AiError::Other(format!("failed to build request: {e}")))
}

/// Resolve the API key, build the headers, and assemble the GET
/// request for `/models`.
///
/// The discovery endpoint does not stream so the `Accept` header is
/// `application/json` rather than `text/event-stream`.
pub(super) fn build_models_request(
    http: &SandboxedHttpClient,
    url: &str,
    api_key_ref: &SecretRef,
    secret_store: &dyn SecretStore,
    extra_headers: &[(String, String)],
) -> Result<reqwest::Request, AiError> {
    let api_key = resolve_api_key(api_key_ref, secret_store)?;
    let headers = build_headers(&api_key, extra_headers, /* streaming = */ false)?;

    http.client()
        .get(url)
        .headers(headers)
        .build()
        .map_err(|e| AiError::Other(format!("failed to build request: {e}")))
}

fn resolve_api_key(
    api_key_ref: &SecretRef,
    secret_store: &dyn SecretStore,
) -> Result<String, AiError> {
    secret_store
        .get(api_key_ref.as_str())
        .map_err(|e| AiError::Auth(format!("secret store error: {e}")))?
        .ok_or_else(|| {
            AiError::Auth(format!(
                "no API key configured for handle {}",
                api_key_ref.as_str()
            ))
        })
}

fn build_headers(
    api_key: &str,
    extra_headers: &[(String, String)],
    streaming: bool,
) -> Result<HeaderMap, AiError> {
    let mut headers = HeaderMap::new();
    let mut auth_value = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
        AiError::Auth("API key contains characters invalid in a header".to_string())
    })?;
    auth_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_value);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        reqwest::header::ACCEPT,
        if streaming {
            HeaderValue::from_static("text/event-stream")
        } else {
            HeaderValue::from_static("application/json")
        },
    );
    for (name, value) in extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AiError::InvalidRequest(format!("invalid extra header name: {name}")))?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            AiError::InvalidRequest(format!("invalid extra header value for {name}"))
        })?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}
