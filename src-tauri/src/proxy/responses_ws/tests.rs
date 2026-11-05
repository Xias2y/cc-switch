use super::*;
use crate::{
    database::Database,
    live::engine::DeviceStore,
    mode::state::{self, Mode},
    proxy::{server::ProxyServer, types::ProxyConfig},
};
use axum::{extract::State, routing::get, Router};
use std::sync::Arc;
use tokio::sync::mpsc;

fn provider(id: &str, base: &str) -> Provider {
    Provider::with_id(
        id.into(),
        id.into(),
        json!({
            "base_url": base, "auth": {"OPENAI_API_KEY": format!("key-{id}")}
        }),
        None,
    )
}

fn official() -> Provider {
    let mut provider = Provider::with_id(
        crate::database::CODEX_OFFICIAL_PROVIDER_ID.into(),
        "Official".into(),
        json!({"auth": {}, "config": ""}),
        None,
    );
    provider.category = Some("official".into());
    provider
}

#[test]
fn upstream_credentials_are_isolated_and_handshake_is_regenerated() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer official-token"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("official-account"),
    );
    headers.insert(
        "x-codex-turn-state",
        HeaderValue::from_static("official-state"),
    );
    headers.insert("x-api-key", HeaderValue::from_static("unrelated-secret"));
    headers.insert("sec-websocket-key", HeaderValue::from_static("old-key"));
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    headers.insert("version", HeaderValue::from_static("0.161.0"));
    headers.insert("session_id", HeaderValue::from_static("routing-session"));
    headers.insert(
        "sec-websocket-extensions",
        HeaderValue::from_static("permessage-deflate"),
    );
    headers.insert(
        "connection",
        HeaderValue::from_static("Upgrade, x-local-only"),
    );
    headers.insert("x-local-only", HeaderValue::from_static("not-forwarded"));
    let relay = upstream_headers(
        &headers,
        &provider("krill", "https://example.com/v1"),
        false,
        "new-key",
    )
    .unwrap();
    assert_eq!(relay["authorization"], "Bearer key-krill");
    assert_eq!(relay["openai-beta"], "responses_websockets=2026-02-06");
    let native = upstream_headers(
        &headers,
        &provider("krill", "https://api.kbsfdxc.net"),
        false,
        "native-key",
    )
    .unwrap();
    assert_eq!(native["originator"], "codex_cli_rs");
    assert_eq!(native["version"], "0.161.0");
    assert_eq!(native["session_id"], "routing-session");
    assert_eq!(native["authorization"], "Bearer key-krill");
    assert!(!native.contains_key("chatgpt-account-id"));
    assert_eq!(relay["sec-websocket-key"], "new-key");
    for name in [
        "chatgpt-account-id",
        "x-codex-turn-state",
        "x-api-key",
        "x-local-only",
        "sec-websocket-extensions",
    ] {
        assert!(!relay.contains_key(name), "{name}");
    }
    let first_party = upstream_headers(&headers, &official(), true, "another-key").unwrap();
    assert_eq!(first_party["authorization"], "Bearer official-token");
    assert_eq!(first_party["chatgpt-account-id"], "official-account");
    assert_eq!(first_party["x-codex-turn-state"], "official-state");
}

#[test]
fn fallback_is_once_per_session_and_account_and_expires() {
    let state = ResponsesWsState::default();
    let mut first = HeaderMap::new();
    first.insert("session-id", HeaderValue::from_static("conversation"));
    first.insert(
        "authorization",
        HeaderValue::from_static("Bearer account-1"),
    );
    let mut second = first.clone();
    second.insert(
        "authorization",
        HeaderValue::from_static("Bearer account-2"),
    );
    let key = session_key(&first).unwrap();
    assert_ne!(Some(key.clone()), session_key(&second));
    state.mark_http(Some(&key));
    assert!(!state.take_http(session_key(&second).as_deref()));
    assert!(state.take_http(Some(&key)));
    assert!(!state.take_http(Some(&key)));
    state
        .http_sessions
        .lock()
        .unwrap()
        .insert(key.clone(), Instant::now() - FALLBACK_TTL);
    assert!(!state.take_http(Some(&key)));
}

#[test]
fn upstream_urls_match_native_and_full_endpoint_configuration() {
    let uri = "/v1/responses?client=codex".parse().unwrap();
    assert_eq!(
        upstream_url(&provider("a", "https://example.com/v1"), &uri).unwrap(),
        "https://example.com/v1/responses?client=codex"
    );
    assert_eq!(
        upstream_url(&provider("a", "https://example.com/prefix/responses"), &uri).unwrap(),
        "https://example.com/prefix/responses?client=codex"
    );
    assert_eq!(
        upstream_url(&official(), &"/responses".parse().unwrap()).unwrap(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
}

struct RestoreState {
    path: std::path::PathBuf,
    before: Option<Vec<u8>>,
}
impl RestoreState {
    fn capture() -> Self {
        assert!(
            std::env::var_os("CC_SWITCH_TEST_HOME").is_some(),
            "Network tests require an isolated test home"
        );
        let path = DeviceStore::for_device().state_path();
        Self {
            before: std::fs::read(&path).ok(),
            path,
        }
    }
}
impl Drop for RestoreState {
    fn drop(&mut self) {
        if let Some(before) = &self.before {
            std::fs::write(&self.path, before).unwrap();
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

type Observed = (HeaderMap, Value);
struct Mock {
    base: String,
    received: mpsc::UnboundedReceiver<Observed>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock(reject_ws: bool) -> Mock {
    mock_options(reject_ws, false).await
}

async fn mock_options(reject_ws: bool, reject_zstd: bool) -> Mock {
    let (tx, received) = mpsc::unbounded_channel::<Observed>();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let native_blob = format!("gAAAA-native-{}", uuid::Uuid::new_v4());
    let http_blob = native_blob.clone();
    let app = Router::new().route("/v1/responses", get(
        move |State(tx): State<mpsc::UnboundedSender<Observed>>, headers: HeaderMap, ws: WebSocketUpgrade| async move {
            if reject_ws { return StatusCode::UPGRADE_REQUIRED.into_response(); }
            ws.on_upgrade(move |mut socket| async move {
                let mut round = 0;
                let mut previous = None::<String>;
                while let Some(Ok(Message::Text(text))) = socket.recv().await {
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    tx.send((headers.clone(), frame.clone())).unwrap();
                    if let Some(id) = frame["previous_response_id"].as_str() {
                        assert_eq!(previous.as_deref(), Some(id), "Continuation must remain on the same upstream socket");
                    }
                    round += 1;
                    let id = format!("resp_mock_{round}");
                    previous = Some(id.clone());
                    let tool = frame["tool_choice"]["name"].as_str() == Some("echo");
                    let item = if codex_compaction::is_compaction_request(&frame) {
                        json!({"type":"compaction","id":"cmp_native","encrypted_content":native_blob})
                    } else if tool {
                        json!({"type":"function_call","id":"fc_mock","call_id":"call_mock","name":"echo","arguments":"{\"value\":7}"})
                    } else {
                        json!({"type":"message","id":"msg_mock","role":"assistant","content":[{"type":"output_text","text":"Remember the code ORCHID-742 and keep the pending task."}]})
                    };
                    let mut events = vec![json!({"type":"response.created","response":{"id":id}})];
                    if tool {
                        events.push(json!({"type":"response.function_call_arguments.done","item_id":"fc_mock","arguments":""}));
                        let mut empty = item.clone(); empty["arguments"] = json!("");
                        events.push(json!({"type":"response.output_item.done","output_index":0,"item":empty}));
                        events.push(json!({"type":"response.function_call_arguments.delta","item_id":"fc_mock","delta":"{\"value\":7}"}));
                    } else {
                        events.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
                    }
                    events.push(json!({"type":"response.completed","response":{
                        "id":id,"status":"completed","model":frame["model"],"output":[item],
                        "usage":{"input_tokens":11,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}
                    }}));
                    for event in events {
                        if socket.send(Message::Text(event.to_string())).await.is_err() { return; }
                    }
                }
            }).into_response()
        }
    ).post(move |State(tx): State<mpsc::UnboundedSender<Observed>>, headers: HeaderMap, bytes: bytes::Bytes| {
        let http_blob = http_blob.clone();
        async move {
            let compressed = headers.get("content-encoding").is_some_and(|v| v == "zstd");
            let bytes = if compressed { zstd::decode_all(bytes.as_ref()).unwrap() } else { bytes.to_vec() };
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            tx.send((headers, body.clone())).unwrap();
            if compressed && reject_zstd { return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(); }
            let output = if codex_compaction::is_compaction_request(&body) {
                json!([{"type":"compaction","id":"cmp_native","encrypted_content":http_blob}])
            } else { json!([]) };
            Json(json!({"id":"resp_http","status":"completed","model":body["model"],"output":output})).into_response()
        }
    })).with_state(tx);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock {
        base,
        received,
        task,
    }
}

async fn proxy(base: &str) -> (ProxyServer, String, Arc<Database>, RestoreState) {
    let restore = RestoreState::capture();
    let db = Arc::new(Database::memory().unwrap());
    for id in ["route", "krill", "other"] {
        db.save_provider("codex", &provider(id, base)).unwrap();
    }
    db.set_current_provider("codex", "route").unwrap();
    state::update(&DeviceStore::for_device(), |live| {
        let app = live.apps.entry("codex".into()).or_default();
        app.mode = Some(Mode::Proxy);
        app.proxy_route = Some("route".into());
        app.stack.enabled = true;
        app.stack.members = vec!["route".into(), "krill".into(), "other".into()];
        app.stack.keys.insert("krill".into(), "krill".into());
        app.stack.keys.insert("other".into(), "other".into());
    })
    .unwrap();
    let proxy = ProxyServer::new(
        ProxyConfig {
            listen_port: 0,
            ..ProxyConfig::default()
        },
        db.clone(),
        None,
    );
    let info = proxy.start().await.unwrap();
    (
        proxy,
        format!("http://127.0.0.1:{}/v1/responses", info.port),
        db,
        restore,
    )
}

async fn client(url: &str, session: &str) -> WebSocketStream<reqwest::Upgraded> {
    let key = generate_key();
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", key)
        .header("session-id", session)
        .header("authorization", "Bearer official-client")
        .header("chatgpt-account-id", "private-account")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    WebSocketStream::from_raw_socket(response.upgrade().await.unwrap(), Role::Client, None).await
}

async fn run_turn(socket: &mut WebSocketStream<reqwest::Upgraded>, frame: Value) -> Vec<Value> {
    socket
        .send(UpMessage::Text(frame.to_string()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            if let Some(Ok(UpMessage::Text(text))) = socket.next().await {
                let event: Value = serde_json::from_str(&text).unwrap();
                let terminal = terminal_status(&event).is_some();
                events.push(event);
                if terminal {
                    return events;
                }
            } else {
                panic!("Response socket closed before a terminal event");
            }
        }
    })
    .await
    .expect("WS response timeout")
}

#[tokio::test]
#[serial_test::serial]
async fn aggregation_keeps_two_round_context_and_isolates_credentials() {
    let mut mock = mock(false).await;
    let (proxy, url, _db, _restore) = proxy(&mock.base).await;
    let mut socket = client(&url, "two-rounds").await;
    let first = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","input":[{"role":"user","content":"Remember ORCHID-742"}],"_private":"remove"})).await;
    let id = first.last().unwrap()["response"]["id"].as_str().unwrap();
    let _ = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","previous_response_id":id,"input":[{"role":"user","content":"What is the code?"}]})).await;
    let (headers, frame) = mock.received.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer key-krill");
    assert!(!headers.contains_key("chatgpt-account-id"));
    assert_eq!(frame["model"], "model-one");
    assert!(frame.get("_private").is_none());
    let (_, second) = mock.received.recv().await.unwrap();
    assert_eq!(second["previous_response_id"], id);
    assert_eq!(second["input"].as_array().unwrap().len(), 1);
    assert_eq!(proxy.get_status().await.failover_count, 0);
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn compaction_and_tool_arguments_use_existing_http_compatibility_logic() {
    let mut mock = mock(false).await;
    let (proxy, url, _db, _restore) = proxy(&mock.base).await;
    let mut socket = client(&url, "compaction").await;
    let result = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","input":[{"role":"user","content":"Remember ORCHID-742"},{"type":"compaction_trigger"}]})).await;
    let output = result.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    let summaries: Vec<_> = output
        .iter()
        .filter(|item| item["type"] == "compaction")
        .collect();
    assert_eq!(summaries.len(), 1);
    let summary = summaries[0].clone();
    assert!(codex_compaction::decode_compaction_summary(
        summary["encrypted_content"].as_str().unwrap()
    )
    .unwrap()
    .contains("ORCHID-742"));
    let (_, request) = mock.received.recv().await.unwrap();
    assert!(!codex_compaction::is_compaction_request(&request));
    let _ = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","input":[summary,{"role":"user","content":"Continue the task"}]})).await;
    let (_, request) = mock.received.recv().await.unwrap();
    assert_eq!(request["input"][0]["type"], "message");
    assert!(request.to_string().contains("ORCHID-742"));
    let events = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","input":[],"tools":[{"type":"function","name":"echo","parameters":{"type":"object","properties":{"value":{"type":"integer"}}}}],"tool_choice":{"type":"function","name":"echo"}})).await;
    let call = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"
        })
        .unwrap();
    assert_eq!(call["item"]["arguments"], "{\"value\":7}");
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn provider_switch_closes_before_leaking_the_new_frame() {
    let mut mock = mock(false).await;
    let (proxy, url, _db, _restore) = proxy(&mock.base).await;
    let mut socket = client(&url, "switch").await;
    run_turn(
        &mut socket,
        json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}),
    )
    .await;
    mock.received.recv().await.unwrap();
    socket
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-other/model-two","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    let close = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(close, UpMessage::Close(Some(frame)) if u16::from(frame.code) == 1012));
    assert!(mock.received.try_recv().is_err());
    let mut socket = client(&url, "switch").await;
    run_turn(
        &mut socket,
        json!({"type":"response.create","model":"ccs-other/model-two","input":[]}),
    )
    .await;
    let (headers, frame) = mock.received.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer key-other");
    assert_eq!(frame["model"], "model-two");
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn unsupported_upstream_falls_back_once_and_http_remains_available() {
    let mock = mock(true).await;
    let (proxy, url, _db, _restore) = proxy(&mock.base).await;
    let mut socket = client(&url, "http-fallback").await;
    socket
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(message, UpMessage::Close(Some(frame)) if u16::from(frame.code) == 1012));
    let response = reqwest::Client::new()
        .get(&url)
        .header("session-id", "http-fallback")
        .header("authorization", "Bearer official-client")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let response = reqwest::Client::new()
        .post(&url)
        .json(&json!({"model":"ccs-krill/model-one","input":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    proxy.stop().await.unwrap();
}

#[path = "live_tests.rs"]
mod live_tests;

fn enable_native(db: &Database, id: &str) {
    let mut provider = db.get_provider_by_id(id, "codex").unwrap().unwrap();
    provider.meta = Some(crate::provider::ProviderMeta {
        codex_native_compaction: Some(true),
        codex_request_zstd: Some(true),
        ..Default::default()
    });
    db.save_provider("codex", &provider).unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn native_ws_keeps_trigger_ciphertext_and_allows_cross_provider_history() {
    let mut mock = mock(false).await;
    let (proxy, url, db, _restore) = proxy(&mock.base).await;
    enable_native(&db, "krill");
    let mut socket = client(&url, "native-compaction").await;
    let output = run_turn(&mut socket, json!({"type":"response.create","model":"ccs-krill/model-one","input":[{"type":"compaction_trigger"}],"tools":[],"tool_choice":"auto"})).await;
    let item = output.last().unwrap()["response"]["output"][0].clone();
    assert_eq!(item["type"], "compaction");
    assert!(item["encrypted_content"]
        .as_str()
        .unwrap()
        .starts_with("gAAAA-native-"));
    let (_, sent) = mock.received.recv().await.unwrap();
    assert_eq!(sent["input"][0]["type"], "compaction_trigger");
    assert_eq!(sent["tool_choice"], "auto");
    socket.close(None).await.unwrap();
    let mut fresh = client(&url, "native-resume").await;
    run_turn(&mut fresh, json!({"type":"response.create","model":"ccs-krill/model-one","input":[item.clone(),{"role":"user","content":"continue"}]})).await;
    let (_, sent) = mock.received.recv().await.unwrap();
    assert_eq!(sent["input"][0], item);
    fresh
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-other/model-two","input":[item.clone()]})
                .to_string(),
        ))
        .await
        .unwrap();
    let moved = fresh.next().await.unwrap().unwrap();
    assert!(matches!(moved, UpMessage::Close(Some(frame)) if u16::from(frame.code) == 1012));
    assert!(mock.received.try_recv().is_err());
    let mut target = client(&url, "native-switch").await;
    let events = run_turn(
        &mut target,
        json!({"type":"response.create","model":"ccs-other/model-two","input":[item.clone()]}),
    )
    .await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    let (headers, sent) = mock.received.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer key-other");
    assert_eq!(sent["input"][0], item);
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn native_http_preserves_compaction_and_zstd_and_records_ownership() {
    let mut mock = mock(false).await;
    let (proxy, url, db, _restore) = proxy(&mock.base).await;
    enable_native(&db, "krill");
    let response = reqwest::Client::new().post(&url).json(&json!({"model":"ccs-krill/model-one","input":[{"role":"user","content":"pad ".repeat(800)},{"type":"compaction_trigger"}]})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    let item = body["output"][0].clone();
    assert_eq!(item["type"], "compaction");
    assert!(item["encrypted_content"]
        .as_str()
        .unwrap()
        .starts_with("gAAAA-native-"));
    let (headers, sent) = mock.received.recv().await.unwrap();
    assert_eq!(headers["content-encoding"], "zstd");
    assert!(codex_compaction::is_compaction_request(&sent));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&json!({"model":"ccs-other/model-two","input":[item.clone()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (headers, sent) = mock.received.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer key-other");
    assert_eq!(sent["input"][0], item);
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn zstd_refusal_retries_plain_once_then_remembers_the_route() {
    let mut mock = mock_options(false, true).await;
    let (proxy, url, db, _restore) = proxy(&mock.base).await;
    enable_native(&db, "krill");
    for _ in 0..2 {
        let response = reqwest::Client::new().post(&url).json(&json!({"model":"ccs-krill/model-one","input":[{"role":"user","content":"pad ".repeat(800)}]})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _: Value = response.json().await.unwrap();
    }
    let (first, _) = mock.received.recv().await.unwrap();
    let (retry, _) = mock.received.recv().await.unwrap();
    let (next, _) = mock.received.recv().await.unwrap();
    assert_eq!(first["content-encoding"], "zstd");
    assert!(!retry.contains_key("content-encoding"));
    assert!(!next.contains_key("content-encoding"));
    assert!(mock.received.try_recv().is_err());
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn interleaved_streams_keep_summary_bridge_above_eight_pending_responses() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let router = Router::new().route("/v1/responses", get(|ws: WebSocketUpgrade| async move {
        ws.on_upgrade(|mut socket| async move {
            let mut frames = Vec::new();
            for _ in 0..9 {
                let Some(Ok(Message::Text(text))) = socket.recv().await else { return; };
                frames.push(serde_json::from_str::<Value>(&text).unwrap());
            }
            for frame in frames.into_iter().rev() {
                let stream_id = frame["stream_id"].clone();
                let item = json!({"type":"message","id":"msg_lane","role":"assistant","content":[{"type":"output_text","text":"Lane-specific summary."}]});
                socket.send(Message::Text(json!({"type":"response.output_item.done","stream_id":stream_id,"output_index":0,"item":item}).to_string())).await.unwrap();
                socket.send(Message::Text(json!({"type":"response.completed","stream_id":stream_id,"response":{"id":"resp_lane","status":"completed","model":"model-one","output":[item]}}).to_string())).await.unwrap();
            }
            let _ = socket.recv().await;
        })
    }));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "interleaved").await;
    for lane in 0..9 {
        let input = if lane == 0 {
            json!([{"type":"compaction_trigger"}])
        } else {
            json!([])
        };
        socket.send(UpMessage::Text(json!({"type":"response.create","model":"ccs-krill/model-one","stream_id":lane,"input":input}).to_string())).await.unwrap();
    }
    let mut completed = std::collections::HashMap::new();
    while completed.len() < 9 {
        let event = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let UpMessage::Text(text) = event {
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["type"] == "response.completed" {
                completed.insert(
                    event["stream_id"].as_u64().unwrap(),
                    if event["response"]["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["type"] == "compaction")
                    {
                        json!("compaction")
                    } else {
                        event["response"]["output"][0]["type"].clone()
                    },
                );
            }
        }
    }
    assert_eq!(completed[&0], "compaction");
    for lane in 1..9 {
        assert_eq!(completed[&lane], "message");
    }
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn transport_handshake_retry_never_replays_a_generation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut received) = mpsc::unbounded_channel::<Value>();
    let task = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 8192];
        assert!(first.read(&mut bytes).await.unwrap() > 0);
        first.shutdown().await.unwrap();
        drop(first);
        let router = Router::new().route("/v1/responses", get(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(Message::Text(text))) = socket.recv().await {
                    tx.send(serde_json::from_str(&text).unwrap()).unwrap();
                    socket.send(Message::Text(json!({"type":"response.completed","response":{"id":"resp_retry","status":"completed","model":"model-one","output":[]}}).to_string())).await.unwrap();
                }
            })
        }));
        axum::serve(listener, router).await.unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "handshake-retry").await;
    run_turn(
        &mut socket,
        json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}),
    )
    .await;
    assert_eq!(received.recv().await.unwrap()["model"], "model-one");
    assert!(received.try_recv().is_err());
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn queued_handshake_can_finish_after_thirty_seconds_without_retry() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut requests) = mpsc::unbounded_channel();
    let router = Router::new().route(
        "/v1/responses",
        get(move |ws: WebSocketUpgrade| {
            tx.send(()).unwrap();
            async move {
                tokio::time::sleep(Duration::from_secs(35)).await;
                ws.on_upgrade(move |mut socket| async move {
                    if let Some(Ok(Message::Text(_))) = socket.recv().await {
                        socket.send(Message::Text(json!({"type":"response.completed","response":{"id":"resp_queued","status":"completed","model":"model-one","output":[]}}).to_string())).await.unwrap();
                    }
                })
            }
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "queued-handshake").await;
    socket
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(45), socket.next())
        .await
        .expect("A queued handshake must not be retried at thirty seconds")
        .unwrap()
        .unwrap();
    let UpMessage::Text(event) = event else {
        panic!("Expected a completed response after the queued handshake");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&event).unwrap()["type"],
        "response.completed"
    );
    requests.recv().await.unwrap();
    assert!(requests.try_recv().is_err());
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn stopping_proxy_cancels_a_pending_upstream_handshake() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut requests) = mpsc::unbounded_channel();
    let router = Router::new().route(
        "/v1/responses",
        get(move || {
            tx.send(()).unwrap();
            std::future::pending::<Response>()
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "cancel-queued-handshake").await;
    socket
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    proxy.stop().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(
        matches!(closed, Some(Ok(UpMessage::Close(Some(frame)))) if u16::from(frame.code) == 1001)
    );
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn route_request_counters_count_turns_skip_prewarm_and_release_active_guard() {
    let _mock = mock(false).await;
    let (proxy, url, _db, _restore) = proxy(&_mock.base).await;
    let mut socket = client(&url, "route-counters").await;
    run_turn(
        &mut socket,
        json!({"type":"response.create","model":"model-one","generate":false,"input":[]}),
    )
    .await;
    assert_eq!(proxy.get_status().await.total_requests, 0);
    for _ in 0..2 {
        run_turn(
            &mut socket,
            json!({"type":"response.create","model":"model-one","input":[]}),
        )
        .await;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = proxy.get_status().await;
            if status.success_requests == 2 && status.active_connections == 0 {
                assert_eq!(status.total_requests, 2);
                assert_eq!(status.success_rate, 100.0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    proxy.stop().await.unwrap();
}

#[path = "regression_tests.rs"]
mod regression_tests;
