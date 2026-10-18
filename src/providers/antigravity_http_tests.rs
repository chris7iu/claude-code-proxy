//! Synthetic loopback HTTP tests. No real OAuth, Google, CLI import, or inference.
use super::antigravity::{
    AntigravityProvider,
    auth::{Auth, Credential, Store},
    config::Config,
    replay,
};
use crate::{
    config::AliasProvider,
    monitor::{MonitorHandle, RequestStatus},
    provider::Provider,
    registry::Registry,
    server::{AppFeatures, app_with_features},
};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct Server {
    pub(super) url: url::Url,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub(super) async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, task }
    }
}

type Captured = Arc<Mutex<Vec<(http::HeaderMap, Value)>>>;
struct Backend {
    captured: Captured,
    replies: Vec<(u16, String)>,
}
async fn backend(State(state): State<Arc<Backend>>, request: Request) -> Response {
    let headers = request.headers().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    let index = {
        let mut captured = state.captured.lock().unwrap();
        let index = captured.len();
        captured.push((headers, value));
        index
    };
    let (status, body) = state
        .replies
        .get(index)
        .unwrap_or_else(|| state.replies.last().unwrap());
    Response::builder()
        .status(*status)
        .header("content-type", "text/event-stream")
        .header("retry-after", "7")
        .body(Body::from(body.clone()))
        .unwrap()
}
async fn native(replies: Vec<(u16, String)>) -> (Server, Captured) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(Backend {
        captured: captured.clone(),
        replies,
    });
    let server = Server::start(
        Router::new()
            .route("/v1internal:streamGenerateContent", post(backend))
            .with_state(state),
    )
    .await;
    (server, captured)
}
fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn successful(parts: Value) -> String {
    frame(
        json!({"response":{"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":1,"totalTokenCount":13}}}),
    )
}
async fn provider(directory: &Path, url: &url::Url) -> AntigravityProvider {
    let store = Store::new(directory.join("antigravity/auth.json"));
    let lock = store.lock(Duration::from_secs(1)).await.unwrap();
    store
        .save(
            &lock,
            &Credential {
                profile_id: "profile".into(),
                project_id: Some("project".into()),
                access_token: "proxy-owned-access".into(),
                refresh_token: Some("proxy-owned-refresh".into()),
                expires_at: super::antigravity::auth::now() + 3600,
            },
        )
        .unwrap();
    drop(lock);
    let config = Config {
        base_url: url.clone(),
        client_id: Some("synthetic-client".into()),
        ..Config::default()
    };
    AntigravityProvider::with_auth(Auth::new(config, store).unwrap())
}
async fn proxy(provider: AntigravityProvider, monitor: Option<MonitorHandle>) -> Server {
    let registry = Registry::from_providers(
        AliasProvider::Codex,
        [Arc::new(provider) as Arc<dyn Provider>],
    );
    Server::start(app_with_features(
        Arc::new(registry),
        monitor,
        AppFeatures {
            responses_api: true,
            ..AppFeatures::default()
        },
    ))
    .await
}
fn request(stream: bool) -> Value {
    json!({"model":"antigravity/gemini-4-argon-low","max_tokens":64,"stream":stream,"messages":[{"role":"user","content":"synthetic prompt"}],"thinking":{"type":"adaptive","display":"omitted"},"output_config":{"effort":"high"}})
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn antigravity_http2_refused_stream_is_not_automatically_retried() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut preface = [0_u8; 24];
        socket.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        socket
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut attempts = 0;
        loop {
            let mut header = [0_u8; 9];
            if !matches!(
                tokio::time::timeout(Duration::from_millis(500), socket.read_exact(&mut header))
                    .await,
                Ok(Ok(_))
            ) {
                break;
            }
            let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
            assert!(length <= 65536);
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).await.unwrap();
            if header[3] == 4 && header[4] & 1 == 0 {
                socket
                    .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                    .await
                    .unwrap();
            }
            if header[3] == 1 {
                attempts += 1;
                let reset = [
                    0, 0, 4, 3, 0, header[5], header[6], header[7], header[8], 0, 0, 0, 7,
                ];
                socket.write_all(&reset).await.unwrap();
            }
        }
        attempts
    });
    let client = Config::default()
        .client_builder()
        .no_proxy()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    assert!(
        client
            .post(format!("http://{address}/v1internal:streamGenerateContent"))
            .json(&json!({"synthetic":"generation"}))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        upstream.await.unwrap(),
        1,
        "even protocol nacks must not silently buy a second native attempt"
    );
}

#[tokio::test]
async fn antigravity_serving_capability_uses_its_own_auth_and_configuration() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"unused"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let response = client()
        .get(
            proxy
                .url
                .join("/v1/models?include_capabilities=true")
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let value: Value = response.json().await.unwrap();
    let descriptor = &value["capabilities"]["providers"]["antigravity"];
    assert_eq!(descriptor["enabled"], true);
    assert_eq!(descriptor["status"]["auth"], "ready");
    assert_eq!(descriptor["status"]["project"], "configured");
    assert_eq!(descriptor["status"]["configuration_ready"], true);
    assert_eq!(descriptor["status"]["live_verified"], false);
    assert!(captured.lock().unwrap().is_empty());
    assert!(!descriptor.to_string().contains("proxy-owned"));
}

#[tokio::test]
async fn antigravity_opaque_native_signature_corruption_is_upstream_not_local_authentication() {
    use base64::Engine;
    let binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "gemini-4-argon-low".into(),
    };
    let native_contents = json!([{"role":"model","parts":[{"text":"same projection","thoughtSignature":"original-native-signature"}]}]);
    let mut blocks = replay::encode(&native_contents, &binding, "17", "full", &json!([])).unwrap();
    let original = replay::decode(&blocks, &binding).unwrap();
    let mut mutated = original.clone();
    mutated["contents"][0]["parts"][0]["thoughtSignature"] = json!("corrupt-native-signature");
    blocks[0]["signature"] = json!(format!(
        "{}{}",
        replay::PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&mutated).unwrap())
    ));
    let decoded = replay::decode(&blocks, &binding).unwrap();
    assert_eq!(decoded["projection_digest"], original["projection_digest"]);
    let (upstream, captured) = native(vec![(
        400,
        "upstream signature validation rejected corrupt-native-signature".into(),
    )])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let mut body = request(true);
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"user","content":"next"}),
    ]);
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error = response.text().await.unwrap();
    assert!(!error.contains("corrupt-native-signature"));
    assert!(!error.contains("tool_use"));
    assert!(!error.contains("message_stop"));
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "opaque corruption requires exactly one native validation attempt, never repair/retry"
    );
    assert_eq!(
        captured[0].1["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        "corrupt-native-signature"
    );
    let diagnostic = format!("{:?}", monitor.snapshot());
    assert!(!diagnostic.contains("corrupt-native-signature"));
}

#[tokio::test]
async fn antigravity_http_preserves_preheader_errors_without_retry_or_secret_echo() {
    for status in [401, 403, 429, 503] {
        let (upstream, captured) = native(vec![(
            status,
            "private OAuth token and native signature".into(),
        )])
        .await;
        let directory = tempfile::tempdir().unwrap();
        let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
        let response = client()
            .post(proxy.url.join("/v1/messages").unwrap())
            .header("authorization", "Bearer frontend-secret")
            .header("anthropic-beta", "unrelated-beta")
            .json(&request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["retry-after"], "7");
        assert!(!response.text().await.unwrap().contains("private OAuth"));
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0["authorization"], "Bearer proxy-owned-access");
        assert!(!captured[0].0.contains_key("anthropic-beta"));
        let identity = captured[0].0["user-agent"].to_str().unwrap();
        assert!(identity.starts_with("claude-code-proxy/"));
        assert!(identity.contains("Antigravity-compatible"));
    }
}

#[tokio::test]
async fn antigravity_http_replay_restores_native_calls_after_fresh_provider() {
    let original = json!([{"thought":true,"text":"hidden native thought","thoughtSignature":"raw-thought"},
        {"functionCall":{"name":"Read","args":{"limit":1}},"thoughtSignature":"raw-call"}]);
    let (upstream, captured) = native(vec![
        (200, successful(original.clone())),
        (
            200,
            successful(json!([{"text":"done","thoughtSignature":"raw-final"}])),
        ),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let first = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let mut body = request(false);
    body["tools"] = json!([{"name":"Read","input_schema":{"type":"object","properties":{"limit":{"type":"integer","exclusiveMinimum":0}},"required":["limit"],"additionalProperties":false}}]);
    let response = client()
        .post(first.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["content"][0]["thinking"], "");
    let local = message["content"][1]["id"].as_str().unwrap();
    drop(first);
    let second = proxy(provider(directory.path(), &upstream.url).await, None).await;
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":message["content"]}),
        json!({"role":"system","content":"ordered system after resume"}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":local,"content":"{\"untouched\":true}"},{"type":"text","text":"continue"}]})
    ]);
    let response = client()
        .post(second.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    {
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let wire = &captured[1].1["request"];
        assert_eq!(
            wire["contents"][1],
            json!({"role":"model","parts":original})
        );
        assert_eq!(
            wire["contents"][2],
            json!({"role":"model","parts":[{"functionResponse":{"name":"Read","response":{"output":"{\"untouched\":true}"}}}]})
        );
        assert_eq!(
            wire["contents"][3],
            json!({"role":"user","parts":[{"text":"continue"}]})
        );
        assert_eq!(wire["sessionId"], captured[0].1["request"]["sessionId"]);
        assert_eq!(
            wire["systemInstruction"]["parts"],
            json!([{"text":"ordered system after resume"}])
        );
    }
    body["model"] = json!("antigravity/gemini-4-argon-high");
    let response = client()
        .post(second.url.join("/v1/messages").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        captured.lock().unwrap().len(),
        2,
        "model switch must not purchase inference"
    );
}

#[tokio::test]
async fn antigravity_late_error_sets_one_failed_monitor_outcome_and_no_success_events() {
    let (upstream,captured)=native(vec![(200,frame(json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"must not publish"}]}}]}})))]).await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&request(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("event: error"));
    for prohibited in [
        "message_stop",
        "tool_use",
        "must not publish",
        "message_start",
    ] {
        assert!(!text.contains(prohibited));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let snapshot = monitor.snapshot();
    assert!(snapshot.active.is_empty());
    assert_eq!(snapshot.recent.len(), 1);
    assert_eq!(snapshot.recent[0].status, RequestStatus::Failed);
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn antigravity_effective_tier_is_observable_and_openai_stays_unsupported() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"ok"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let monitor = MonitorHandle::new(10);
    let proxy = proxy(
        provider(directory.path(), &upstream.url).await,
        Some(monitor.clone()),
    )
    .await;
    let response = client()
        .post(proxy.url.join("/v1/messages").unwrap())
        .json(&request(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-antigravity-reasoning-tier")
            .and_then(|v| v.to_str().ok()),
        Some("low")
    );
    let _ = response.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(monitor.snapshot().recent[0].effort.as_deref(), Some("low"));
    assert_eq!(monitor.snapshot().recent[0].input_tokens, Some(10));
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(3));
    let response=client().post(proxy.url.join("/v1/chat/completions").unwrap()).json(&json!({"model":"antigravity/gemini-4-argon-low","messages":[{"role":"user","content":"x"}]})).send().await.unwrap();
    assert_eq!(response.status(), 501);
    assert_eq!(captured.lock().unwrap().len(), 1);
}

async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        raw.extend_from_slice(&buffer[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&raw[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if raw.len() >= end + 4 + length {
                return;
            }
        }
    }
}

#[tokio::test]
async fn antigravity_real_disconnect_cancels_preheader_nonstream_and_stream_collection() {
    for (headers_sent, streaming) in [(false, false), (false, true), (true, false), (true, true)] {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}", upstream.local_addr().unwrap())).unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let native_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            read_request(&mut socket).await;
            if headers_sent {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            }
            let _ = accepted_tx.send(());
            let mut byte = [0u8; 1];
            let closed = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte)).await;
            let _ = closed_tx.send(matches!(closed, Ok(Ok(0)) | Ok(Err(_))));
        });
        let directory = tempfile::tempdir().unwrap();
        let monitor = MonitorHandle::new(10);
        let proxy = proxy(
            provider(directory.path(), &url).await,
            Some(monitor.clone()),
        )
        .await;
        let mut downstream =
            tokio::net::TcpStream::connect(("127.0.0.1", proxy.url.port().unwrap()))
                .await
                .unwrap();
        let body = request(streaming).to_string();
        downstream.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        accepted_rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(downstream);
        assert!(
            closed_rx.await.unwrap(),
            "upstream task survived disconnect: headers={headers_sent} stream={streaming}"
        );
        native_task.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(monitor.snapshot().active.is_empty());
    }
}

#[tokio::test]
async fn antigravity_count_estimate_is_signature_size_invariant_and_network_free() {
    let (upstream, captured) = native(vec![(200, successful(json!([{"text":"unused"}])))]).await;
    let directory = tempfile::tempdir().unwrap();
    let proxy = proxy(provider(directory.path(), &upstream.url).await, None).await;
    let binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "gemini-4-argon-low".into(),
    };
    let mut estimates = Vec::new();
    for signature in ["x".to_string(), "x".repeat(100000)] {
        let contents =
            json!([{"role":"model","parts":[{"text":"same text","thoughtSignature":signature}]}]);
        let blocks = replay::encode(&contents, &binding, "123", "omitted", &json!([])).unwrap();
        let mut body = request(false);
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":blocks}),
            json!({"role":"user","content":"next"}),
        ]);
        let response = client()
            .post(proxy.url.join("/v1/messages/count_tokens").unwrap())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        estimates.push(response.json::<Value>().await.unwrap()["input_tokens"].clone());
    }
    assert_eq!(estimates[0], estimates[1]);
    assert!(captured.lock().unwrap().is_empty());
}
