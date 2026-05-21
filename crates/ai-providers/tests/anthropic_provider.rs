//! Integration tests for the alternative wire-format provider.
//!
//! Behaviour pinned here:
//!
//! 1. Outgoing request shape — `system` lifts out of `messages`,
//!    `messages` carries no `Role::System` entry.
//! 2. Streaming text — JSON-NL events parse into the documented
//!    `StreamEvent` sequence terminating in `Done`.
//! 3. Error mapping matrix — 401 → `Auth`, 429 (with `Retry-After`)
//!    → `RateLimited { retry_after: Some(_) }`, 500 →
//!    `ProviderError`, malformed JSON-NL → `ProviderError`.
//! 4. Cancellation — dropping the stream future terminates the
//!    underlying connection within one second.
//!
//! All HTTP traffic is fixture-bound; no live network calls.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use openspace_ai_providers::test_support::AlwaysApprove;
use openspace_ai_providers::{
    AnthropicProvider, InMemorySecretStore, SandboxedHttpClient, ToolCallAccumulator,
};
use openspace_shared::ai::domain::{
    Conversation, GenerationParams, ImagePayload, ModelRef, Part, Role, Turn,
};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::{AiProvider, StreamEvent};
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use openspace_shared::id::{ChatId, TurnId};
use openspace_shared::sandbox::{
    AccessLevel, NetworkPattern, PermissionPolicy, SandboxPolicy, TrustMode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WiremockRequest, Respond, ResponseTemplate};

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn permissive_policy() -> SandboxPolicy {
    let permissions = PermissionPolicy::new(
        AccessLevel::Allow,
        AccessLevel::Allow,
        AccessLevel::Allow,
        AccessLevel::Allow,
    );
    let allow = NetworkPattern::parse("127.0.0.1:*").expect("pattern parses");
    SandboxPolicy::new(TrustMode::Fast, permissions, vec![allow], Vec::new())
}

fn provider_at(base_url: &str) -> AnthropicProvider {
    let secret_store: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let api_key_ref = SecretRef::provider_api_key("anthropic");
    secret_store
        .set(api_key_ref.as_str(), "test-api-key")
        .expect("seed credential");

    let http = SandboxedHttpClient::new(permissive_policy(), Arc::new(AlwaysApprove))
        .expect("client builds");

    AnthropicProvider::new(base_url.to_string(), api_key_ref, http, secret_store)
}

fn conv_with_system(system: &str, user: &str) -> Conversation {
    Conversation::new(
        ChatId::new_v4(),
        None,
        vec![Turn::new(
            TurnId::new_v4(),
            Role::User,
            vec![Part::Text(user.to_string())],
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
            None,
        )],
        Some(system.to_string()),
    )
}

async fn collect_events(
    provider: &AnthropicProvider,
    conv: Conversation,
) -> Vec<Result<StreamEvent, AiError>> {
    let mut stream = provider.chat_stream(
        conv,
        ModelRef::new("anthropic", "model-x"),
        GenerationParams::default(),
    );
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item);
    }
    out
}

/// Wiremock responder that captures the outgoing request body into a
/// shared slot so a structural assertion can run on it after the
/// call.
struct CapturingResponder {
    captured: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    template: ResponseTemplate,
}

impl Respond for CapturingResponder {
    fn respond(&self, req: &WiremockRequest) -> ResponseTemplate {
        *self.captured.lock().expect("capture lock") = Some(req.body.clone());
        self.template.clone()
    }
}

fn jsonnl_response(lines: &[&str]) -> ResponseTemplate {
    let body = lines.join("\n") + "\n";
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(body)
}

// ─────────────────────────────────────────────────────────────────────
// 1. Outgoing-request structural assertion
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn outgoing_request_lifts_system_outside_messages() {
    let server = MockServer::start().await;
    let captured: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(None));

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-api-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(CapturingResponder {
            captured: captured.clone(),
            template: jsonnl_response(&[r#"{"type":"message_stop"}"#]),
        })
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let conv = conv_with_system("be brief", "hi there");
    let events = collect_events(&provider, conv).await;
    assert!(matches!(events.last(), Some(Ok(StreamEvent::Done))));

    let body = captured.lock().unwrap().clone().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");

    // Headline structural invariant: `system` lifted, `messages`
    // contains no system entry.
    assert_eq!(parsed["system"], serde_json::json!("be brief"));
    let messages = parsed["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    for msg in messages {
        assert_ne!(msg["role"], "system", "no system role inside messages");
    }
    assert_eq!(parsed["stream"], serde_json::json!(true));
    assert_eq!(parsed["model"], serde_json::json!("model-x"));
}

// ─────────────────────────────────────────────────────────────────────
// 2. Streaming text — JSON-NL → StreamEvent sequence
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn streaming_text_emits_text_deltas_and_terminal_done() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(jsonnl_response(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":", world"}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ]))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "hi")).await;

    let oks: Vec<&StreamEvent> = events
        .iter()
        .map(|e| e.as_ref().expect("no errors expected"))
        .collect();
    assert!(matches!(oks[0], StreamEvent::UsageReport(_)));
    assert!(matches!(oks[1], StreamEvent::TextDelta(t) if t == "Hello"));
    assert!(matches!(oks[2], StreamEvent::TextDelta(t) if t == ", world"));
    assert!(matches!(oks[3], StreamEvent::UsageReport(_)));
    assert!(matches!(oks.last(), Some(StreamEvent::Done)));
}

// ─────────────────────────────────────────────────────────────────────
// 3. Error-mapping matrix
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_401_maps_to_authentication_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid x-api-key"))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "hi")).await;
    let err = events
        .into_iter()
        .next()
        .expect("at least one event")
        .expect_err("err");
    assert!(matches!(err, AiError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn http_429_with_retry_after_maps_to_rate_limited_with_duration() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "12")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "hi")).await;
    let err = events
        .into_iter()
        .next()
        .expect("at least one event")
        .expect_err("err");
    match err {
        AiError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(12)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn http_500_maps_to_provider_error_with_attribution() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broken"))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "hi")).await;
    let err = events
        .into_iter()
        .next()
        .expect("at least one event")
        .expect_err("err");
    match err {
        AiError::ProviderError {
            provider_id,
            message,
        } => {
            assert_eq!(provider_id, "anthropic");
            assert!(message.contains("500"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_nl_surfaces_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(jsonnl_response(&[
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{not valid json"#,
        ]))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "hi")).await;
    // First event delivers the legitimate text delta, second surfaces
    // the parse error and ends the stream.
    assert!(matches!(
        events[0].as_ref().expect("delta"),
        StreamEvent::TextDelta(t) if t == "ok",
    ));
    let err = events
        .into_iter()
        .nth(1)
        .expect("second event")
        .expect_err("err");
    match err {
        AiError::ProviderError {
            provider_id,
            message,
        } => {
            assert_eq!(provider_id, "anthropic");
            assert!(message.contains("malformed JSON-NL event"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// 4. Cancellation — dropping the stream closes the connection
// ─────────────────────────────────────────────────────────────────────

/// Hand-rolled HTTP/1 server that holds a streaming response open
/// after the first chunk and reports the time it observes a
/// client-side close. Mirrors the cancellation harness that pins the
/// sandbox HTTP client's drop semantics.
async fn drip_server() -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<Instant>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");

        // Drain request headers — we only need the body delimiter.
        let mut buf = [0u8; 1024];
        loop {
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        // Send headers + first JSON-NL line as a chunked body.
        let head = b"HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Transfer-Encoding: chunked\r\n\r\n";
        if sock.write_all(head).await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }
        let line = br#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}
"#;
        let chunk_header = format!("{:x}\r\n", line.len());
        if sock.write_all(chunk_header.as_bytes()).await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }
        if sock.write_all(line).await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }
        if sock.write_all(b"\r\n").await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }

        // Probe for the client-side EOF concurrently with a
        // periodic keepalive write — same pattern the sandbox HTTP
        // client cancellation test uses.
        let (mut reader, mut writer) = sock.split();
        let mut probe = [0u8; 1];
        loop {
            tokio::select! {
                res = writer.write_all(b":\r\n") => {
                    if res.is_err() { break; }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                res = reader.read(&mut probe) => {
                    if matches!(res, Ok(0) | Err(_)) { break; }
                }
            }
        }
        let _ = tx.send(Instant::now());
    });

    (addr, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_stream_closes_connection_within_one_second() {
    let (addr, observed) = drip_server().await;
    let base = format!("http://{addr}");
    let provider = provider_at(&base);

    let mut stream = provider.chat_stream(
        conv_with_system("", "hi"),
        ModelRef::new("anthropic", "claude-3-5-sonnet-latest"),
        GenerationParams::default(),
    );

    // Read one event so the connection is live, then drop.
    let _first = stream.next().await.expect("first event");
    let dropped_at = Instant::now();
    drop(stream);

    let observed_at = tokio::time::timeout(Duration::from_secs(5), observed)
        .await
        .expect("server saw close inside test ceiling")
        .expect("server task did not panic");
    let elapsed = observed_at.saturating_duration_since(dropped_at);
    assert!(
        elapsed < Duration::from_secs(1),
        "server saw close after {elapsed:?}, expected < 1s",
    );
}

// ─────────────────────────────────────────────────────────────────────
// 5. Tool-call streaming — single-turn
// ─────────────────────────────────────────────────────────────────────

/// Streamed tool-call assembly. The wire format announces a tool call
/// via `content_block_start` (carrying `id` and `name`), then streams
/// the argument JSON in `input_json_delta` fragments. The adapter
/// surfaces each piece as `StreamEvent::ToolCallDelta` and the consumer
/// feeds the deltas through [`ToolCallAccumulator`] to recover the full
/// argument object.
#[tokio::test]
async fn streaming_single_tool_call_assembles_through_accumulator() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(jsonnl_response(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"search"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ]))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let mut events = Vec::new();
    let mut stream = provider.chat_stream(
        conv_with_system("", "look it up"),
        ModelRef::new("anthropic", "claude-3-5-sonnet-latest"),
        GenerationParams::default(),
    );
    while let Some(item) = stream.next().await {
        events.push(item.expect("no errors expected"));
    }

    // First event is usage; the next three are tool-call deltas.
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0].name.as_deref(), Some("search"));
    assert_eq!(deltas[0].arguments_delta, "");
    assert!(deltas[1].name.is_none());
    assert!(deltas[2].name.is_none());

    // Reassemble the call by feeding the deltas through the
    // accumulator and asserting the structural payload.
    let mut acc = ToolCallAccumulator::new();
    let id = deltas[0].call_id.clone();
    let name = deltas[0].name.clone().expect("first delta carries name");
    // Synthesise the full assembly target the accumulator expects:
    // `{"id":"…","name":"…","arguments":<deltas joined>}`.
    let joined: String = deltas.iter().map(|d| d.arguments_delta.clone()).collect();
    let payload = format!(r#"{{"id":"{id}","name":"{name}","arguments":{joined}}}"#);
    acc.append(&payload);
    let call = acc.take_finished().expect("accumulator parses payload");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "search");
    assert_eq!(call.arguments["query"], "rust");

    // Stream ends with Done.
    assert!(matches!(events.last(), Some(StreamEvent::Done)));
}

// ─────────────────────────────────────────────────────────────────────
// 6. Tool-call streaming — multi-turn (back-to-back tool calls)
// ─────────────────────────────────────────────────────────────────────

/// Two tool calls in the same stream. The adapter must thread distinct
/// `call_id`s through the deltas (via the index map maintained across
/// `content_block_start` / `content_block_stop` boundaries) so the
/// consumer cannot conflate the two argument objects.
#[tokio::test]
async fn streaming_multi_turn_tool_calls_route_to_distinct_ids() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(jsonnl_response(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"first"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"k\":1}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"b","name":"second"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"k\":2}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ]))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect_events(&provider, conv_with_system("", "two calls")).await;
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::ToolCallDelta(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 4);
    assert_eq!(deltas[0].call_id, "a");
    assert_eq!(deltas[0].name.as_deref(), Some("first"));
    assert_eq!(deltas[1].call_id, "a");
    assert_eq!(deltas[1].arguments_delta, r#"{"k":1}"#);
    assert_eq!(deltas[2].call_id, "b");
    assert_eq!(deltas[2].name.as_deref(), Some("second"));
    assert_eq!(deltas[3].call_id, "b");
    assert_eq!(deltas[3].arguments_delta, r#"{"k":2}"#);
}

// ─────────────────────────────────────────────────────────────────────
// 7. Image input lifts into the wire-format `image` content block
// ─────────────────────────────────────────────────────────────────────

/// User turn with an inline image part. The outgoing request body must
/// carry an `image` block whose `source.type == "base64"` and whose
/// `data` is the standard base64 encoding of the inline payload.
#[tokio::test]
async fn outgoing_request_carries_image_input_as_base64_block() {
    let server = MockServer::start().await;
    let captured: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(CapturingResponder {
            captured: captured.clone(),
            template: jsonnl_response(&[r#"{"type":"message_stop"}"#]),
        })
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let conv = Conversation::new(
        ChatId::new_v4(),
        None,
        vec![Turn::new(
            TurnId::new_v4(),
            Role::User,
            vec![
                Part::Text("describe this".into()),
                Part::Image {
                    mime: "image/png".into(),
                    data: ImagePayload::Bytes(vec![0x89, 0x50, 0x4E, 0x47]),
                },
            ],
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
            None,
        )],
        None,
    );
    // Use a multimodal model id directly — `collect_events`'s default
    // `model-x` falls through to the conservative-fallback path and
    // would short-circuit the request with `AiError::Unsupported`
    // before any HTTP traffic, leaving `captured` empty.
    let mut stream = provider.chat_stream(
        conv,
        ModelRef::new("anthropic", "claude-3-5-sonnet-latest"),
        GenerationParams::default(),
    );
    while let Some(item) = stream.next().await {
        item.expect("no errors expected");
    }

    let body = captured.lock().unwrap().clone().expect("body captured");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    let content = &parsed["messages"][0]["content"];
    let blocks = content.as_array().expect("content array");
    assert_eq!(blocks.len(), 2, "text + image blocks");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    // Standard base64 of [0x89, 0x50, 0x4E, 0x47] is "iVBORw==".
    assert_eq!(blocks[1]["source"]["data"], "iVBORw==");
}

// ─────────────────────────────────────────────────────────────────────
// 8. list_models — capability table
// ─────────────────────────────────────────────────────────────────────

/// `list_models` returns the static catalogue with documented
/// capability flags. The acceptance criteria call for capability
/// reporting per model — this test pins the multimodal vs legacy
/// split so a future catalogue tweak does not silently regress.
#[tokio::test]
async fn list_models_reports_capability_table() {
    let server = MockServer::start().await;
    let provider = provider_at(&server.uri());
    let models = provider.list_models().await.expect("list_models");
    assert!(!models.is_empty());

    // Multimodal flagship row carries vision + tool calling + streaming.
    let sonnet = models
        .iter()
        .find(|m| m.model_ref.model_id == "claude-3-5-sonnet-latest")
        .expect("multimodal row present");
    assert!(sonnet.capabilities.vision);
    assert!(sonnet.capabilities.tool_calling);
    assert!(sonnet.capabilities.streaming);

    // Legacy text-only row carries streaming only.
    let legacy = models
        .iter()
        .find(|m| m.model_ref.model_id == "claude-2.1")
        .expect("legacy row present");
    assert!(!legacy.capabilities.vision);
    assert!(!legacy.capabilities.tool_calling);
    assert!(legacy.capabilities.streaming);

    // Every row carries the provider id and a non-empty display name.
    for info in &models {
        assert_eq!(info.model_ref.provider_id, "anthropic");
        assert!(!info.display_name.is_empty());
    }
}

// ─────────────────────────────────────────────────────────────────────
// 9. Capability-mismatch surfaces AiError::Unsupported
// ─────────────────────────────────────────────────────────────────────

/// Image part on a legacy text-only model rejects with
/// `AiError::Unsupported(_)` before any HTTP traffic is initiated.
/// The wiremock server has no expectations registered — if the guard
/// failed, the request would 404 or hang and the assertion would not
/// match `Unsupported`.
#[tokio::test]
async fn image_part_on_text_only_model_surfaces_unsupported() {
    let server = MockServer::start().await;
    let provider = provider_at(&server.uri());
    let conv = Conversation::new(
        ChatId::new_v4(),
        None,
        vec![Turn::new(
            TurnId::new_v4(),
            Role::User,
            vec![Part::Image {
                mime: "image/png".into(),
                data: ImagePayload::Url("https://example.invalid/x.png".into()),
            }],
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
            None,
        )],
        None,
    );
    let mut stream = provider.chat_stream(
        conv,
        ModelRef::new("anthropic", "claude-2.1"),
        GenerationParams::default(),
    );
    let first = stream.next().await.expect("at least one event");
    match first {
        Err(AiError::Unsupported(reason)) => {
            assert!(
                reason.contains("image input on a non-vision model"),
                "got: {reason}"
            );
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

/// Tool-call part on a tool-less model rejects with the matching
/// reason string.
#[tokio::test]
async fn tool_call_on_tool_less_model_surfaces_unsupported() {
    let server = MockServer::start().await;
    let provider = provider_at(&server.uri());
    let conv = Conversation::new(
        ChatId::new_v4(),
        None,
        vec![Turn::new(
            TurnId::new_v4(),
            Role::Assistant,
            vec![Part::ToolCall {
                id: "c1".into(),
                name: "lookup".into(),
                arguments: serde_json::json!({}),
            }],
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).expect("ts"),
            None,
        )],
        None,
    );
    let mut stream = provider.chat_stream(
        conv,
        ModelRef::new("anthropic", "claude-2.1"),
        GenerationParams::default(),
    );
    let first = stream.next().await.expect("at least one event");
    match first {
        Err(AiError::Unsupported(reason)) => {
            assert!(
                reason.contains("tool calling on a tool-less model"),
                "got: {reason}"
            );
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}
