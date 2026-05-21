//! Integration tests for [`openspace_ai_providers::OpenAiCompatibleProvider`].
//!
//! Pins the eight wire-level scenarios the issue's acceptance
//! criteria call out — text completion, streaming text, malformed
//! SSE, 400 / 401 / 429 (with and without `Retry-After`) / 500 —
//! plus the cancellation contract Slice 3 introduced. Every test
//! pins to `wiremock` (no live network) and to a hand-rolled
//! `TcpListener` for the cancellation case so the connection close
//! can be observed server-side.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use openspace_ai_providers::test_support::AlwaysApprove;
use openspace_ai_providers::{InMemorySecretStore, OpenAiCompatibleProvider, SandboxedHttpClient};
use openspace_shared::ai::domain::{Conversation, GenerationParams, ModelRef, Part, Role, Turn};
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
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─────────────────────────────────────────────────────────────────────
// Test scaffolding — policy, client, provider builder, conversation.
// ─────────────────────────────────────────────────────────────────────

const TEST_API_KEY: &str = "test-api-key";
const TEST_PROVIDER_ID: &str = "openai-compat-under-test";
const TEST_MODEL_ID: &str = "stub-model";

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

fn provider_for(server: &MockServer) -> OpenAiCompatibleProvider {
    let secret_store: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let api_key_ref = SecretRef::provider_api_key(TEST_PROVIDER_ID);
    secret_store
        .set(api_key_ref.as_str(), TEST_API_KEY)
        .expect("seeded API key");

    let http = Arc::new(
        SandboxedHttpClient::new(permissive_policy(), Arc::new(AlwaysApprove))
            .expect("client builds"),
    );

    OpenAiCompatibleProvider::new(
        TEST_PROVIDER_ID,
        "OpenAI-compatible (under test)",
        format!("{}/v1", server.uri()),
        api_key_ref,
        secret_store,
        http,
    )
}

fn one_user_turn(text: &str) -> Conversation {
    Conversation::new(
        ChatId::new_v4(),
        None,
        vec![Turn::new(
            TurnId::new_v4(),
            Role::User,
            vec![Part::Text(text.to_string())],
            chrono::Utc::now(),
            None,
        )],
        None,
    )
}

fn model_ref() -> ModelRef {
    ModelRef::new(TEST_PROVIDER_ID, TEST_MODEL_ID)
}

fn sse_chunk(content: &str) -> String {
    let json = serde_json::json!({
        "choices": [ { "delta": { "content": content } } ]
    });
    format!("data: {}\n\n", json)
}

fn sse_usage_chunk(prompt: u32, completion: u32, cached: u32) -> String {
    let json = serde_json::json!({
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "prompt_tokens_details": { "cached_tokens": cached }
        }
    });
    format!("data: {}\n\n", json)
}

fn sse_done() -> &'static str {
    "data: [DONE]\n\n"
}

async fn collect_events(
    mut stream: futures::stream::BoxStream<'_, Result<StreamEvent, AiError>>,
) -> (Vec<StreamEvent>, Option<AiError>) {
    let mut events = Vec::new();
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ev) => events.push(ev),
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    (events, error)
}

// ─────────────────────────────────────────────────────────────────────
// Streaming success scenarios
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn streaming_text_emits_deltas_then_done() {
    let server = MockServer::start().await;

    let body = format!(
        "{}{}{}{}{}",
        sse_chunk("Hello"),
        sse_chunk(", "),
        sse_chunk("world!"),
        sse_usage_chunk(7, 3, 0),
        sse_done(),
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", &*format!("Bearer {TEST_API_KEY}")))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(
        one_user_turn("hi"),
        model_ref(),
        GenerationParams::default(),
    );
    let (events, error) = collect_events(stream).await;

    assert!(error.is_none(), "stream errored: {error:?}");
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts.concat(), "Hello, world!");

    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::UsageReport(u) if u.input_tokens == 7 && u.output_tokens == 3)),
        "expected UsageReport with 7/3, got {events:?}",
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::Done)),
        "stream did not terminate with Done: {events:?}",
    );
}

#[tokio::test]
async fn text_completion_via_single_chunk_assembles_correctly() {
    // Same wire format, delivered as one big chunk — pins that the
    // SSE parser handles back-to-back events without a flush in
    // between.
    let server = MockServer::start().await;
    let body = format!(
        "{}{}{}",
        sse_chunk("ok"),
        sse_usage_chunk(1, 1, 0),
        sse_done()
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(
        one_user_turn("hello"),
        model_ref(),
        GenerationParams::default(),
    );
    let (events, error) = collect_events(stream).await;

    assert!(error.is_none(), "{error:?}");
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "ok"));
    assert!(matches!(&events[1], StreamEvent::UsageReport(_)));
    assert!(matches!(&events[2], StreamEvent::Done));
}

// ─────────────────────────────────────────────────────────────────────
// Malformed SSE — protocol noise must not panic and must not break
// the JSON decoding of the surrounding events.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn malformed_sse_lines_are_tolerated() {
    let server = MockServer::start().await;

    // Inject blank lines, comment lines, a partial frame split on
    // an awkward boundary, and an unrecognised `event:` framing
    // line. The provider must still surface "Hello world" + Done.
    let body = format!(
        ": keepalive\n\n\n: another comment\nevent: ignored\n{}\n: mid-comment\n{}{}{}",
        sse_chunk("Hello "),
        sse_chunk("world"),
        sse_usage_chunk(2, 2, 0),
        sse_done(),
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(
        one_user_turn("hi"),
        model_ref(),
        GenerationParams::default(),
    );
    let (events, error) = collect_events(stream).await;

    assert!(error.is_none(), "{error:?}");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    assert!(matches!(events.last(), Some(StreamEvent::Done)));
}

// ─────────────────────────────────────────────────────────────────────
// Error mapping matrix
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_400_maps_to_invalid_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"temperature out of range","type":"invalid_request_error"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(one_user_turn("x"), model_ref(), GenerationParams::default());
    let (events, error) = collect_events(stream).await;

    assert!(
        events.is_empty(),
        "no events expected on hard error: {events:?}"
    );
    let err = error.expect("400 surfaced as Err");
    assert!(
        matches!(err, AiError::InvalidRequest(ref m) if m.contains("temperature out of range")),
        "got {err:?}",
    );
}

#[tokio::test]
async fn http_401_maps_to_authentication_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"message":"invalid api key","type":"authentication_error"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(one_user_turn("x"), model_ref(), GenerationParams::default());
    let (_events, error) = collect_events(stream).await;

    let err = error.expect("401 surfaced as Err");
    assert!(matches!(err, AiError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn http_429_with_retry_after_surfaces_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_string(""),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(one_user_turn("x"), model_ref(), GenerationParams::default());
    let (_events, error) = collect_events(stream).await;

    let err = error.expect("429 surfaced as Err");
    assert!(
        matches!(
            err,
            AiError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(30)
        ),
        "got {err:?}",
    );
}

#[tokio::test]
async fn http_429_without_retry_after_carries_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string(""))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(one_user_turn("x"), model_ref(), GenerationParams::default());
    let (_events, error) = collect_events(stream).await;

    let err = error.expect("429 surfaced as Err");
    assert!(
        matches!(err, AiError::RateLimited { retry_after: None }),
        "got {err:?}",
    );
}

#[tokio::test]
async fn http_500_maps_to_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream blew up"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.chat_stream(one_user_turn("x"), model_ref(), GenerationParams::default());
    let (_events, error) = collect_events(stream).await;

    let err = error.expect("500 surfaced as Err");
    // The Domain `AiError` collapses 5xx into `Network`; the issue
    // calls for `ServerError(u16)` but the Domain crate already
    // pinned the surface used by the agent loop, and `Network`
    // carries the status code in the message. Pin both here so a
    // future refactor that splits them stays a one-liner.
    assert!(
        matches!(err, AiError::Network(ref m) if m.contains("500")),
        "got {err:?}",
    );
}

// ─────────────────────────────────────────────────────────────────────
// Cancellation — dropping the stream surfaces as the connection
// closing within one second on the server side. Reuses the same
// drip-server pattern Slice 3 introduced.
// ─────────────────────────────────────────────────────────────────────

async fn drip_sse_server() -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<Instant>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");

        // Drain the request headers.
        let mut buf = [0u8; 4096];
        loop {
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        // Reply with chunked SSE that emits one event then idles.
        let head = b"HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream\r\n\
                     Transfer-Encoding: chunked\r\n\r\n";
        if sock.write_all(head).await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }
        // One real SSE event followed by a heartbeat the parser
        // ignores. Encoded as a single chunked frame to keep the
        // server-side state simple.
        let first_event = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let frame = format!("{:x}\r\n", first_event.len());
        let _ = sock.write_all(frame.as_bytes()).await;
        let _ = sock.write_all(first_event).await;
        let _ = sock.write_all(b"\r\n").await;

        // Watch for the client to drop. Reuse the split-socket
        // pattern from the sandbox HTTP client tests.
        let (mut reader, mut writer) = sock.split();
        let mut probe = [0u8; 1];
        loop {
            tokio::select! {
                res = writer.write_all(b"3\r\n: \n\r\n") => {
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
async fn dropping_chat_stream_closes_connection_within_one_second() {
    let (addr, observed) = drip_sse_server().await;

    let secret_store: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let api_key_ref = SecretRef::provider_api_key(TEST_PROVIDER_ID);
    secret_store
        .set(api_key_ref.as_str(), TEST_API_KEY)
        .expect("seed");
    let http = Arc::new(
        SandboxedHttpClient::new(permissive_policy(), Arc::new(AlwaysApprove))
            .expect("client builds"),
    );
    let provider = OpenAiCompatibleProvider::new(
        TEST_PROVIDER_ID,
        "OpenAI-compatible (drip)",
        format!("http://{addr}/v1"),
        api_key_ref,
        secret_store,
        http,
    );

    let mut stream = provider.chat_stream(
        one_user_turn("hi"),
        model_ref(),
        GenerationParams::default(),
    );

    // Pull the first event so the connection is live, then drop.
    let first = stream.next().await.expect("first event");
    assert!(
        matches!(first, Ok(StreamEvent::TextDelta(_))),
        "got {first:?}"
    );

    let dropped_at = Instant::now();
    drop(stream);

    let observed_at = tokio::time::timeout(Duration::from_secs(5), observed)
        .await
        .expect("server observed close inside the test ceiling")
        .expect("server task did not panic");

    let elapsed = observed_at.saturating_duration_since(dropped_at);
    assert!(
        elapsed < Duration::from_secs(1),
        "server saw close after {elapsed:?}, expected < 1s",
    );
}

// ─────────────────────────────────────────────────────────────────────
// Provider configuration round-trip — pins that the optional
// builder methods actually persist their arguments. Cheap insurance
// against a future refactor silently dropping a field.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn provider_builder_persists_optional_configuration() {
    let server = MockServer::start().await;
    let provider = provider_for(&server)
        .with_request_id_header("x-request-id")
        .with_model_filter("free-")
        .with_extra_header("x-vendor-org", "acme");

    assert_eq!(provider.request_id_header(), Some("x-request-id"));
    assert_eq!(provider.model_filter(), Some("free-"));
    assert_eq!(provider.id(), TEST_PROVIDER_ID);
}
