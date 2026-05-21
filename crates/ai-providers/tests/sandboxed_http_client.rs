//! Integration tests for the sandbox-aware HTTP client.
//!
//! Three behaviour contracts pinned here:
//!
//! 1. Sandbox gate matrix — `Allow` flows, `Block` short-circuits,
//!    `RequireApproval` consults the channel, allowlist overrides
//!    `Block`.
//! 2. Approval flow — approved decisions resume the request, denied
//!    decisions surface as `SandboxDenied`, never-answered prompts
//!    surface as `ApprovalTimeout`.
//! 3. Cancellation — dropping the response stream closes the
//!    underlying TCP connection within one second.
//!
//! The matrix tests use `wiremock` for a real HTTP server bound to
//! `127.0.0.1`. The cancellation test uses a hand-rolled
//! `tokio::net::TcpListener` so we can observe the server-side EOF
//! directly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openspace_ai_providers::test_support::{AlwaysApprove, AlwaysDeny, NeverAnswer};
use openspace_ai_providers::{HttpClientError, SandboxedHttpClient};
use openspace_shared::sandbox::{
    AccessLevel, NetworkPattern, PermissionPolicy, SandboxPolicy, TrustMode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

/// Build a policy whose network gate is exactly `level`, with no
/// allowlist entries. Other operations stay open so the network
/// field is what these tests exercise.
fn policy_with_network(level: AccessLevel) -> SandboxPolicy {
    let permissions = PermissionPolicy::new(
        AccessLevel::Allow,
        AccessLevel::Allow,
        AccessLevel::Allow,
        level,
    );
    SandboxPolicy::new(TrustMode::Fast, permissions, Vec::new(), Vec::new())
}

/// Same, but with an allowlist that should short-circuit to Allow
/// for `127.0.0.1` on any port — the address `wiremock` binds to.
fn policy_with_localhost_allowlist(level: AccessLevel) -> SandboxPolicy {
    let permissions = PermissionPolicy::new(
        AccessLevel::Allow,
        AccessLevel::Allow,
        AccessLevel::Allow,
        level,
    );
    let allow = NetworkPattern::parse("127.0.0.1:*").expect("pattern parses");
    SandboxPolicy::new(TrustMode::Fast, permissions, vec![allow], Vec::new())
}

// ─────────────────────────────────────────────────────────────────────
// Gate matrix
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn allow_lets_request_through() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .mount(&server)
        .await;

    let client = SandboxedHttpClient::new(
        policy_with_network(AccessLevel::Allow),
        Arc::new(AlwaysApprove),
    )
    .expect("client builds");

    let resp = client
        .get(&format!("{}/ping", server.uri()))
        .await
        .expect("GET succeeds under Allow");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
}

#[tokio::test]
async fn block_short_circuits_before_dispatch() {
    // No mock mounted — if the gate leaked through, wiremock would
    // 404, surfacing as `Status` rather than `SandboxBlocked`.
    let server = MockServer::start().await;

    let client = SandboxedHttpClient::new(
        policy_with_network(AccessLevel::Block),
        Arc::new(AlwaysApprove),
    )
    .expect("client builds");

    let err = client
        .get(&format!("{}/anything", server.uri()))
        .await
        .expect_err("Block should refuse");
    assert!(
        matches!(err, HttpClientError::SandboxBlocked(_)),
        "got {err:?}",
    );
}

#[tokio::test]
async fn allowlist_overrides_block() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = SandboxedHttpClient::new(
        policy_with_localhost_allowlist(AccessLevel::Block),
        Arc::new(AlwaysApprove),
    )
    .expect("client builds");

    let resp = client
        .get(&format!("{}/ok", server.uri()))
        .await
        .expect("allowlist should override Block");
    assert_eq!(resp.status(), 200);
}

// ─────────────────────────────────────────────────────────────────────
// Approval flow
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn require_approval_resumes_on_approval() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/approve-me"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = SandboxedHttpClient::new(
        policy_with_network(AccessLevel::RequireApproval),
        Arc::new(AlwaysApprove),
    )
    .expect("client builds");

    let resp = client
        .get(&format!("{}/approve-me", server.uri()))
        .await
        .expect("approved request resumes");
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn require_approval_aborts_on_denial() {
    let server = MockServer::start().await;

    let client = SandboxedHttpClient::new(
        policy_with_network(AccessLevel::RequireApproval),
        Arc::new(AlwaysDeny),
    )
    .expect("client builds");

    let err = client
        .get(&format!("{}/whatever", server.uri()))
        .await
        .expect_err("denial should refuse");
    assert!(
        matches!(err, HttpClientError::SandboxDenied(_)),
        "got {err:?}",
    );
}

#[tokio::test]
async fn require_approval_times_out_when_no_answer() {
    let server = MockServer::start().await;

    let client = SandboxedHttpClient::new(
        policy_with_network(AccessLevel::RequireApproval),
        Arc::new(NeverAnswer),
    )
    .expect("client builds")
    .with_approval_timeout(Duration::from_millis(150));

    let err = client
        .get(&format!("{}/whatever", server.uri()))
        .await
        .expect_err("never-answered prompt times out");
    assert!(
        matches!(err, HttpClientError::ApprovalTimeout(_)),
        "got {err:?}",
    );
}

// ─────────────────────────────────────────────────────────────────────
// Cancellation
// ─────────────────────────────────────────────────────────────────────

/// Hand-rolled HTTP/1 server that accepts a single connection,
/// drains the request, then writes a chunked response. After the
/// initial chunk it watches for a client-side close and signals the
/// observed time on a oneshot channel. The test then asserts the
/// observed time lands within one second of the drop.
async fn drip_server() -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<Instant>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");

        // Drain the request so the kernel buffers do not stall the
        // response. We only need to consume up to the
        // end-of-headers marker (\r\n\r\n).
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

        // Send headers + first chunk. Chunked encoding lets us hold
        // the body open without committing to a Content-Length.
        let head = b"HTTP/1.1 200 OK\r\n\
                     Content-Type: text/plain\r\n\
                     Transfer-Encoding: chunked\r\n\r\n\
                     5\r\nhello\r\n";
        if sock.write_all(head).await.is_err() {
            let _ = tx.send(Instant::now());
            return;
        }

        // Watch for the client to drop. We split the socket so the
        // read half can detect EOF concurrently with the write half
        // probing for broken-pipe — without splitting the same
        // mutable borrow would block one path against the other.
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
async fn dropping_response_closes_connection_within_one_second() {
    let (addr, observed) = drip_server().await;

    let client = SandboxedHttpClient::new(
        policy_with_localhost_allowlist(AccessLevel::Allow),
        Arc::new(AlwaysApprove),
    )
    .expect("client builds");

    let url = format!("http://{addr}/drip");
    let stream = client
        .post_streaming(&url, &serde_json::json!({"hello": "world"}))
        .await
        .expect("stream opens");

    // Read one chunk so the connection is live, then drop.
    use futures::StreamExt;
    let mut stream = Box::pin(stream);
    let _first = stream.next().await;

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
