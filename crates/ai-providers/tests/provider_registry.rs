//! Integration tests for [`openspace_ai_providers::ProviderRegistry`].
//!
//! Pins the acceptance criteria from issue #52:
//!
//! 1. Two providers — one `OpenAiCompatibleProvider`, one
//!    `AnthropicProvider` — register cleanly under distinct ids.
//! 2. [`ProviderRegistry::lookup`] resolves each id and the returned
//!    handles drive `chat_stream` end-to-end against `wiremock`,
//!    yielding the expected event sequences for each wire format.
//! 3. [`ProviderRegistry::list`] surfaces both entries with
//!    capability snapshots derived from the adapters themselves.
//! 4. [`ProviderRegistry::remove`] drops an entry; subsequent
//!    [`ProviderRegistry::lookup`] returns `None`.
//! 5. Re-registering a duplicate id is rejected with
//!    [`AiError::InvalidRequest`].
//! 6. [`ProviderRegistry::notify_change`] is callable and stays a
//!    no-op for now (the contract — not the behaviour — is what
//!    matters this slice).
//!
//! All HTTP traffic is fixture-bound through `wiremock`. No live
//! network calls.

use std::sync::Arc;

use futures::StreamExt;
use openspace_ai_providers::test_support::AlwaysApprove;
use openspace_ai_providers::{
    InMemorySecretStore, ProviderConfig, ProviderId, ProviderRegistry, SandboxedHttpClient,
};
use openspace_shared::ai::domain::{Conversation, GenerationParams, ModelRef, Part, Role, Turn};
use openspace_shared::ai::error::AiError;
use openspace_shared::ai::provider::StreamEvent;
use openspace_shared::ai::secret::{SecretRef, SecretStore};
use openspace_shared::id::{ChatId, TurnId};
use openspace_shared::sandbox::{
    AccessLevel, NetworkPattern, PermissionPolicy, SandboxPolicy, TrustMode,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─────────────────────────────────────────────────────────────────────
// Constants + helpers
// ─────────────────────────────────────────────────────────────────────

const OPENAI_COMPAT_ID: &str = "openai-compat-under-test";
const ANTHROPIC_ID: &str = "anthropic";
const TEST_API_KEY: &str = "test-api-key";

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

/// Build the secret store + sandbox client + registry triple every
/// test starts from. The store is seeded with the same API key for
/// both adapters; the registry is empty.
fn fresh_registry() -> (Arc<dyn SecretStore>, ProviderRegistry) {
    let secret_store: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let openai_ref = SecretRef::provider_api_key(OPENAI_COMPAT_ID);
    let anthropic_ref = SecretRef::provider_api_key(ANTHROPIC_ID);
    secret_store
        .set(openai_ref.as_str(), TEST_API_KEY)
        .expect("seed openai key");
    secret_store
        .set(anthropic_ref.as_str(), TEST_API_KEY)
        .expect("seed anthropic key");

    let http = Arc::new(
        SandboxedHttpClient::new(permissive_policy(), Arc::new(AlwaysApprove))
            .expect("client builds"),
    );

    let registry = ProviderRegistry::new(Arc::clone(&secret_store), http);
    (secret_store, registry)
}

fn openai_config(base_url: String) -> ProviderConfig {
    ProviderConfig::OpenAiCompatible {
        id: ProviderId::from_static(OPENAI_COMPAT_ID),
        display_name: "OpenAI-compatible (under test)".to_string(),
        base_url,
        api_key_ref: SecretRef::provider_api_key(OPENAI_COMPAT_ID),
        extra_headers: Vec::new(),
        request_id_header: None,
        model_filter: None,
        aggregator_preset: None,
    }
}

fn anthropic_config(base_url: String) -> ProviderConfig {
    ProviderConfig::Anthropic {
        id: ProviderId::from_static(ANTHROPIC_ID),
        display_name: "Alternative chat wire format".to_string(),
        base_url,
        api_key_ref: SecretRef::provider_api_key(ANTHROPIC_ID),
        version: None,
    }
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

fn sse_chunk(content: &str) -> String {
    let json = serde_json::json!({
        "choices": [ { "delta": { "content": content } } ]
    });
    format!("data: {}\n\n", json)
}

fn sse_done() -> &'static str {
    "data: [DONE]\n\n"
}

/// Mount a streaming `/v1/chat/completions` mock that returns three
/// text deltas plus the terminal `[DONE]`.
async fn mount_openai_compat_stream(server: &MockServer) {
    let body = format!(
        "{}{}{}{}",
        sse_chunk("Hello"),
        sse_chunk(", "),
        sse_chunk("world!"),
        sse_done(),
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

/// Mount an alternative-wire-format `/v1/messages` mock that returns
/// a JSON-NL message_start → text delta → message_stop sequence.
async fn mount_anthropic_stream(server: &MockServer) {
    let lines = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi from alt"}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body = lines.join("\n") + "\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}
// ─────────────────────────────────────────────────────────────────────
// Acceptance: registry holds two providers and routes chat_stream
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_routes_chat_stream_to_openai_compat_provider() {
    let openai_server = MockServer::start().await;
    mount_openai_compat_stream(&openai_server).await;

    let (_secrets, mut registry) = fresh_registry();
    let handle = registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("openai-compat registers");
    assert_eq!(handle.id.as_str(), OPENAI_COMPAT_ID);

    let provider = registry
        .lookup(&ProviderId::from_static(OPENAI_COMPAT_ID))
        .expect("provider resolved");

    let mut stream = provider.chat_stream(
        one_user_turn("hi"),
        ModelRef::new(OPENAI_COMPAT_ID, "stub-model"),
        GenerationParams::default(),
    );

    let mut texts = Vec::new();
    let mut saw_done = false;
    let mut errored: Option<AiError> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::TextDelta(t)) => texts.push(t),
            Ok(StreamEvent::Done) => saw_done = true,
            Ok(_) => {}
            Err(e) => {
                errored = Some(e);
                break;
            }
        }
    }
    assert!(errored.is_none(), "stream errored: {errored:?}");
    assert!(saw_done, "stream did not terminate with Done");
    assert_eq!(texts.concat(), "Hello, world!");
}

#[tokio::test]
async fn registry_routes_chat_stream_to_anthropic_provider() {
    let anth_server = MockServer::start().await;
    mount_anthropic_stream(&anth_server).await;

    let (_secrets, mut registry) = fresh_registry();
    registry
        .register(anthropic_config(anth_server.uri()))
        .expect("anthropic registers");

    let provider = registry
        .lookup(&ProviderId::from_static(ANTHROPIC_ID))
        .expect("provider resolved");

    let mut stream = provider.chat_stream(
        one_user_turn("hello"),
        ModelRef::new(ANTHROPIC_ID, "claude-stub"),
        GenerationParams::default(),
    );

    let mut texts = Vec::new();
    let mut saw_done = false;
    let mut errored: Option<AiError> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::TextDelta(t)) => texts.push(t),
            Ok(StreamEvent::Done) => saw_done = true,
            Ok(_) => {}
            Err(e) => {
                errored = Some(e);
                break;
            }
        }
    }
    assert!(errored.is_none(), "stream errored: {errored:?}");
    assert!(
        saw_done,
        "alternative-wire stream did not terminate with Done"
    );
    assert_eq!(texts.concat(), "hi from alt");
}

#[tokio::test]
async fn registry_drives_both_providers_end_to_end() {
    // Same orchestration acceptance criteria #52 spells out: one
    // registry, two providers, both reachable end-to-end against
    // their own wiremock fixtures.
    let openai_server = MockServer::start().await;
    let anth_server = MockServer::start().await;
    mount_openai_compat_stream(&openai_server).await;
    mount_anthropic_stream(&anth_server).await;

    let (_secrets, mut registry) = fresh_registry();
    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("openai-compat registers");
    registry
        .register(anthropic_config(anth_server.uri()))
        .expect("anthropic registers");

    let listing = registry.list();
    assert_eq!(listing.len(), 2, "both entries present: {listing:?}");
    let ids: Vec<&str> = listing.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&OPENAI_COMPAT_ID));
    assert!(ids.contains(&ANTHROPIC_ID));

    // Drive each provider once.
    let openai = registry
        .lookup(&ProviderId::from_static(OPENAI_COMPAT_ID))
        .expect("openai-compat resolves");
    let anthropic = registry
        .lookup(&ProviderId::from_static(ANTHROPIC_ID))
        .expect("anthropic resolves");

    let mut s1 = openai.chat_stream(
        one_user_turn("ping"),
        ModelRef::new(OPENAI_COMPAT_ID, "stub-model"),
        GenerationParams::default(),
    );
    let mut s2 = anthropic.chat_stream(
        one_user_turn("ping"),
        ModelRef::new(ANTHROPIC_ID, "claude-stub"),
        GenerationParams::default(),
    );

    let mut openai_text = String::new();
    while let Some(Ok(ev)) = s1.next().await {
        if let StreamEvent::TextDelta(t) = ev {
            openai_text.push_str(&t);
        }
    }
    let mut anth_text = String::new();
    while let Some(Ok(ev)) = s2.next().await {
        if let StreamEvent::TextDelta(t) = ev {
            anth_text.push_str(&t);
        }
    }
    assert_eq!(openai_text, "Hello, world!");
    assert_eq!(anth_text, "hi from alt");
}
// ─────────────────────────────────────────────────────────────────────
// Acceptance: list / remove / duplicate / notify_change
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_handles_sorted_by_id() {
    let openai_server = MockServer::start().await;
    let anth_server = MockServer::start().await;

    let (_secrets, mut registry) = fresh_registry();
    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("openai registers");
    registry
        .register(anthropic_config(anth_server.uri()))
        .expect("anthropic registers");

    let listing = registry.list();
    assert_eq!(listing.len(), 2);

    // BTreeMap iteration is sorted by id; "anthropic" precedes
    // "openai-compat-under-test" lexicographically.
    assert_eq!(listing[0].id.as_str(), ANTHROPIC_ID);
    assert_eq!(listing[1].id.as_str(), OPENAI_COMPAT_ID);

    // Capability snapshots come from the adapters themselves.
    assert!(listing[0].capabilities.streaming);
    assert!(listing[1].capabilities.streaming);
}

#[tokio::test]
async fn lookup_returns_none_for_unknown_id() {
    let (_secrets, registry) = fresh_registry();
    assert!(registry
        .lookup(&ProviderId::from_static("not-registered"))
        .is_none());
}

#[tokio::test]
async fn remove_drops_entry_and_subsequent_lookup_misses() {
    let openai_server = MockServer::start().await;
    let (_secrets, mut registry) = fresh_registry();
    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("registers");

    let id = ProviderId::from_static(OPENAI_COMPAT_ID);
    assert!(registry.lookup(&id).is_some(), "registered before remove");
    let removed = registry.remove(&id);
    assert!(removed.is_some(), "remove returns the dropped handle");
    assert!(registry.lookup(&id).is_none(), "lookup misses after remove");
    assert!(registry.list().is_empty(), "list empty after remove");

    // Removing again is a clean miss — no panic, no error variant.
    assert!(registry.remove(&id).is_none());
}

#[tokio::test]
async fn duplicate_registration_is_rejected() {
    let openai_server = MockServer::start().await;
    let (_secrets, mut registry) = fresh_registry();
    let cfg = openai_config(format!("{}/v1", openai_server.uri()));
    registry.register(cfg.clone()).expect("first register");

    let err = registry
        .register(cfg)
        .expect_err("duplicate id must be rejected");
    assert!(
        matches!(err, AiError::InvalidRequest(ref msg) if msg.contains(OPENAI_COMPAT_ID)),
        "got {err:?}",
    );

    // The original registration is still intact.
    assert!(registry
        .lookup(&ProviderId::from_static(OPENAI_COMPAT_ID))
        .is_some());
}

#[tokio::test]
async fn re_register_after_remove_succeeds() {
    let openai_server = MockServer::start().await;
    let (_secrets, mut registry) = fresh_registry();
    let id = ProviderId::from_static(OPENAI_COMPAT_ID);

    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("first register");
    registry.remove(&id).expect("remove succeeds");

    // PRD-03 settings reload pattern: remove-then-register on edit.
    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("re-register succeeds");
    assert!(registry.lookup(&id).is_some());
}

#[tokio::test]
async fn notify_change_is_callable_no_op() {
    // Slice 10 ships the hook as a no-op + tracing::debug! call. The
    // contract is that the future settings layer (PRD-03) can call it
    // without crashing the registry; the reconciliation logic lands
    // there. Pinning the signature here means a future change that
    // alters the contract breaks the test on purpose.
    let (_secrets, mut registry) = fresh_registry();
    let configs: Vec<ProviderConfig> = Vec::new();
    registry.notify_change(&configs);
    assert!(registry.list().is_empty(), "no-op leaves state untouched");

    // Same hook with a non-empty incoming snapshot — still a no-op.
    let openai_server = MockServer::start().await;
    let incoming = vec![openai_config(format!("{}/v1", openai_server.uri()))];
    registry.notify_change(&incoming);
    assert!(
        registry.list().is_empty(),
        "stub does not register entries from notify_change",
    );
}

#[tokio::test]
async fn handle_carries_capabilities_from_adapter() {
    // The acceptance criteria require `list()` to surface a handle
    // with id, display name, and a capability snapshot. The snapshot
    // must come from the live adapter, not the config.
    let openai_server = MockServer::start().await;
    let (_secrets, mut registry) = fresh_registry();
    registry
        .register(openai_config(format!("{}/v1", openai_server.uri())))
        .expect("registers");
    let listing = registry.list();
    let handle = &listing[0];
    assert_eq!(handle.id.as_str(), OPENAI_COMPAT_ID);
    assert_eq!(handle.display_name, "OpenAI-compatible (under test)");
    // OpenAI-compat in the current crate advertises text streaming
    // only; tool-calling / vision flip on once Slice 7 lands.
    assert!(handle.capabilities.streaming);
}
