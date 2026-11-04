//! Native Codex Responses WebSockets for the shared official/Krill ingress.
//!
//! Like Superba's ws_catalog, select the upstream from response.create, keep it
//! pinned for the socket, and reconnect before changing providers. HTTP bridges
//! remain on the existing handlers; an upgraded socket cannot itself return 426.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        FromRequestParts,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_tungstenite::{
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
        Message as UpMessage,
    },
    WebSocketStream,
};

use super::{
    body_filter::filter_private_params_with_whitelist,
    forwarder::{
        should_strip_codex_client_fingerprint_header, validate_codex_official_request,
        ActiveConnectionGuard,
    },
    handler_context::RequestContext,
    providers::{
        self,
        codex_compaction::{self, NativeCompactionState},
        responses_late_arguments::Repair,
        AuthStrategy, CodexAdapter, ProviderAdapter,
    },
    response_processor::{log_usage_internal, sse_data_starts_output, usage_logging_enabled},
    server::ProxyState,
    usage::parser::TokenUsage,
    ProxyError,
};
use crate::{app_config::AppType, provider::Provider};

const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const FALLBACK_TTL: Duration = Duration::from_secs(60);
const MAX_FALLBACKS: usize = 1024;

pub(super) struct ResponsesWsState {
    http_sessions: Mutex<HashMap<String, Instant>>,
    shutdown: watch::Sender<u64>,
}

impl Default for ResponsesWsState {
    fn default() -> Self {
        Self {
            http_sessions: Mutex::new(HashMap::new()),
            shutdown: watch::channel(0).0,
        }
    }
}

impl ResponsesWsState {
    pub(super) fn shutdown(&self) {
        self.shutdown
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn mark_http(&self, key: Option<&str>) {
        if let Some(key) = key {
            let mut sessions = self.http_sessions.lock().expect("WS fallback lock");
            sessions.retain(|_, at| at.elapsed() < FALLBACK_TTL);
            if sessions.len() < MAX_FALLBACKS {
                sessions.insert(key.to_owned(), Instant::now());
            }
        }
    }

    fn take_http(&self, key: Option<&str>) -> bool {
        key.and_then(|key| {
            self.http_sessions
                .lock()
                .expect("WS fallback lock")
                .remove(key)
        })
        .is_some_and(|at| at.elapsed() < FALLBACK_TTL)
    }
}

// Include the calling account in the key, but retain neither tokens nor raw ids.
fn session_key(headers: &HeaderMap) -> Option<String> {
    let session = [
        "session_id",
        "x-session-id",
        "session-id",
        "thread-id",
        "x-client-request-id",
    ]
    .iter()
    .find_map(|name| headers.get(*name))?;
    let mut hash = Sha256::new();
    hash.update(session.as_bytes());
    hash.update([0]);
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        hash.update(auth.as_bytes());
    }
    Some(format!("{:x}", hash.finalize()))
}

fn http_required() -> Response {
    (
        StatusCode::UPGRADE_REQUIRED,
        Json(json!({
            "error": {
                "type": "invalid_request_error",
                "code": "websocket_not_supported",
                "message": "This route requires HTTP Responses; reconnect using HTTP"
            }
        })),
    )
        .into_response()
}

pub(super) async fn upgrade(state: ProxyState, request: axum::extract::Request) -> Response {
    // Keep the existing HTTP retry/circuit-breaker path when routing requires
    // failover. Aggregation resolves an explicitly selected provider instead.
    match state.db.get_proxy_config_for_app("codex").await {
        Ok(config)
            if config.auto_failover_enabled
                && !crate::mode::stack::stack_mode_now(&AppType::Codex) =>
        {
            return http_required();
        }
        Ok(_) => {}
        Err(error) => return ProxyError::DatabaseError(error.to_string()).into_response(),
    }
    let key = session_key(request.headers());
    if state.responses_ws.take_http(key.as_deref()) {
        return http_required();
    }
    // Preserve the old fallback when no Codex provider has been configured.
    match crate::mode::current::provider_in_use(&state.db, &AppType::Codex) {
        Ok(Some(_)) => {}
        Ok(None) => return http_required(),
        Err(error) => return ProxyError::DatabaseError(error.to_string()).into_response(),
    }
    let (mut parts, _) = request.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(error) => return error.into_response(),
    };
    let shutdown = state.responses_ws.shutdown.subscribe();
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| session(state, parts.headers, parts.uri, key, shutdown, socket))
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

async fn reply_error(socket: &mut WebSocket, status: u16, code: &str, message: &str) -> bool {
    socket
        .send(Message::Text(
            json!({
                "type": "error",
                "status": status,
                "error": {"type": "invalid_request_error", "code": code, "message": message}
            })
            .to_string(),
        ))
        .await
        .is_ok()
}

struct Turn {
    ctx: RequestContext,
    frame: Value,
    prewarm: bool,
    compaction: bool,
    official: bool,
}

async fn select_turn(
    state: &ProxyState,
    headers: &HeaderMap,
    mut frame: Value,
) -> Result<Turn, ProxyError> {
    if frame["type"].as_str() != Some("response.create")
        || frame["model"].as_str().is_none_or(str::is_empty)
    {
        return Err(ProxyError::InvalidRequest(
            "response.create must include a model".into(),
        ));
    }
    let stack = match super::handlers::resolve_stack_target(state, &AppType::Codex, &mut frame) {
        Ok(stack) => stack,
        Err(response) => {
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .map_err(|_| ProxyError::Internal("Cannot read model selection error".into()))?;
            return Err(ProxyError::UpstreamError {
                status,
                body: Some(String::from_utf8_lossy(&body).into_owned()),
            });
        }
    };
    let mut ctx = RequestContext::new(
        state,
        &frame,
        headers,
        AppType::Codex,
        "CodexWS",
        "codex",
        stack,
    )
    .await?;
    let official = providers::is_codex_official_provider(&ctx.provider);
    if !ctx.is_stack {
        frame = super::model_mapper::apply_model_mapping(frame, &ctx.provider).0;
    }
    let prewarm = frame["generate"].as_bool() == Some(false);
    if let Some(overrides) = ctx
        .provider
        .meta
        .as_ref()
        .and_then(|m| m.local_proxy_request_overrides.as_ref())
    {
        super::forwarder::apply_local_proxy_body_overrides(&mut frame, overrides);
        frame = filter_private_params_with_whitelist(frame, &[]);
        // Routing and quota admission were based on these fields before overrides.
        if frame["type"] != "response.create"
            || (frame["generate"].as_bool() == Some(false)) != prewarm
        {
            return Err(ProxyError::InvalidRequest(
                "WS overrides cannot change the operation or prewarm mode".into(),
            ));
        }
    }
    super::codex_native::validate(&ctx.provider, headers, &frame)?;
    let compaction = codex_compaction::is_compaction_request(&frame)
        && !official
        && !super::codex_native::native_compaction(
            &ctx.provider,
            frame["model"].as_str().unwrap_or_default(),
        );
    if official {
        validate_codex_official_request(headers, &ctx.provider, state.app_handle.as_ref()).await?;
        if !prewarm {
            let token = bearer(headers).ok_or_else(|| {
                ProxyError::AuthError("Official route requires a bearer token".into())
            })?;
            super::codex_quota_protection::admit(token, headers).await?;
        }
        codex_compaction::scrub_ccswitch_state_for_official(&mut frame);
    } else {
        super::codex_native::prepare(&ctx.provider, &mut frame);
    }
    frame = super::thinking_rectifier::normalize_thinking_type(frame);
    frame = filter_private_params_with_whitelist(frame, &[]);
    if let Some(object) = frame.as_object_mut() {
        object.remove("stream"); // response.create is already a streaming WS operation.
    }
    ctx.outbound_model = frame["model"].as_str().map(str::to_owned);
    Ok(Turn {
        ctx,
        frame,
        prewarm,
        compaction,
        official,
    })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then_some(token.trim())
}

fn needs_http(provider: &Provider) -> bool {
    providers::should_convert_codex_responses_to_chat(provider, "/responses")
        || providers::should_convert_codex_responses_to_anthropic(provider, "/responses")
        || provider.is_github_copilot()
        || provider.is_xai_oauth()
        || provider.is_codex_oauth() && !providers::is_codex_official_provider(provider)
        || providers::provider_needs_responses_namespace_flatten(provider)
}

fn upstream_headers(
    source: &HeaderMap,
    provider: &Provider,
    official: bool,
    key: &str,
) -> Result<HeaderMap, ProxyError> {
    let mut headers = source.clone();
    let connection_tokens: Vec<String> = source
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for name in connection_tokens {
        headers.remove(name);
    }
    for name in [
        "host",
        "content-length",
        "content-type",
        "content-encoding",
        "accept-encoding",
        "connection",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "sec-websocket-key",
        "sec-websocket-accept",
        "sec-websocket-extensions",
        "sec-websocket-protocol",
        "x-api-key",
        "x-goog-api-key",
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        headers.remove(name);
    }
    if !official {
        headers.remove(header::AUTHORIZATION);
        let names: Vec<_> = headers
            .keys()
            .filter(|name| {
                (name.as_str() != "openai-beta"
                    && !super::codex_native::protocol_header(provider, name.as_str())
                    && should_strip_codex_client_fingerprint_header(
                        name.as_str(),
                        "/responses",
                        false,
                        false,
                        true,
                        false,
                    ))
                    || name.as_str() == "x-codex-turn-state"
            })
            .cloned()
            .collect();
        for name in names {
            headers.remove(name);
        }
        let adapter = CodexAdapter::new();
        let auth = adapter
            .extract_auth(provider)
            .ok_or_else(|| ProxyError::AuthError("Responses provider has no API key".into()))?;
        if auth.strategy != AuthStrategy::Bearer {
            return Err(ProxyError::ConfigError(
                "This authentication strategy requires HTTP".into(),
            ));
        }
        for (name, value) in adapter.get_auth_headers(&auth)? {
            headers.insert(name, value);
        }
    }
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    if let Some(ua) = provider
        .meta
        .as_ref()
        .and_then(|m| m.custom_user_agent_header().ok().flatten())
    {
        headers.insert(header::USER_AGENT, ua);
    }
    super::forwarder::apply_local_proxy_header_overrides(
        &mut headers,
        provider
            .meta
            .as_ref()
            .and_then(|m| m.local_proxy_request_overrides.as_ref()),
        false,
    );
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_str(key)
            .map_err(|_| ProxyError::Internal("Cannot create WebSocket key".into()))?,
    );
    Ok(headers)
}

fn upstream_url(provider: &Provider, uri: &Uri) -> Result<String, ProxyError> {
    let adapter = CodexAdapter::new();
    let base = adapter.extract_base_url(provider)?;
    let full = provider.meta.as_ref().and_then(|meta| meta.is_full_url) == Some(true)
        || url::Url::parse(&base)
            .is_ok_and(|url| url.path().trim_end_matches('/').ends_with("/responses"));
    let value = if full {
        base
    } else {
        adapter.build_url(&base, "/responses")
    };
    let mut url = url::Url::parse(&value)
        .map_err(|_| ProxyError::ConfigError("Invalid Responses upstream URL".into()))?;
    if let Some(query) = uri.query() {
        let combined = match url.query() {
            Some(existing) => format!("{existing}&{query}"),
            None => query.to_owned(),
        };
        url.set_query(Some(&combined));
    }
    Ok(url.into())
}

enum Connected {
    Open(Box<WebSocketStream<reqwest::Upgraded>>),
    Http,
}

async fn connect(
    state: &ProxyState,
    headers: &HeaderMap,
    uri: &Uri,
    turn: &Turn,
) -> Result<Connected, ProxyError> {
    if needs_http(&turn.ctx.provider) {
        return Ok(Connected::Http);
    }
    let url = upstream_url(&turn.ctx.provider, uri)?;
    let status = state.status.read().await;
    super::upstream_guard::reject_self_forwarding(&url, &status.address, status.port).await?;
    drop(status);
    let key = generate_key();
    let headers = upstream_headers(headers, &turn.ctx.provider, turn.official, &key)?;
    let request = super::http_client::websocket_for_url(&url)
        .map_err(ProxyError::ConfigError)?
        .get(&url)
        // Relays may queue after TCP/TLS connects. Keep the existing client's
        // 600-second request bound rather than imposing a 30-second deadline.
        .headers(headers.clone());
    // No response.create has been sent: a transport-only handshake retry is safe.
    let response = match request
        .try_clone()
        .expect("buffered handshake")
        .send()
        .await
    {
        Ok(response) => response,
        Err(first) => {
            log::warn!(
                "[CodexWS] retrying handshake before any payload: {}",
                first.without_url()
            );
            match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    log::warn!(
                        "[CodexWS] handshake failed after retry; using HTTP (connect={}, timeout={}): {}",
                        error.is_connect(),
                        error.is_timeout(),
                        error.without_url()
                    );
                    return Ok(Connected::Http);
                }
            }
        }
    };
    if matches!(
        response.status().as_u16(),
        404 | 405 | 426 | 501 | 502 | 503 | 504
    ) {
        return Ok(Connected::Http);
    }
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(ProxyError::UpstreamError {
            status: response.status().as_u16(),
            body: Some(
                "Responses WebSocket handshake was rejected by the selected upstream".into(),
            ),
        });
    }
    if response
        .headers()
        .get("sec-websocket-accept")
        .and_then(|v| v.to_str().ok())
        != Some(derive_accept_key(key.as_bytes()).as_str())
        || response.headers().contains_key("sec-websocket-extensions")
    {
        return Err(ProxyError::ForwardFailed(
            "Invalid upstream WebSocket upgrade".into(),
        ));
    }
    if turn.official {
        if let Some(token) = bearer(&headers) {
            super::codex_quota_protection::observe_response(token, &headers, response.headers());
        }
    }
    let upgraded = response
        .upgrade()
        .await
        .map_err(|_| ProxyError::ForwardFailed("Upstream upgrade failed".into()))?;
    let config = WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE),
        max_frame_size: Some(MAX_MESSAGE),
        ..WebSocketConfig::default()
    };
    log::info!(
        "[CodexWS] connected to websocket: provider={}, model={}",
        turn.ctx.provider.id,
        turn.ctx.outbound_model.as_deref().unwrap_or("unknown")
    );
    Ok(Connected::Open(Box::new(
        WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await,
    )))
}

struct Pending {
    turn: Turn,
    first_token_ms: Option<u64>,
    repair: Option<Repair>,
    compaction: Option<NativeCompactionState>,
    connection_guard: Option<ActiveConnectionGuard>,
}

fn parse_sse_events(blocks: Vec<bytes::Bytes>) -> Vec<Value> {
    blocks
        .into_iter()
        .filter_map(|block| {
            let data = std::str::from_utf8(&block)
                .ok()?
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&data).ok()
        })
        .collect()
}

impl Pending {
    async fn new(turn: Turn, state: &ProxyState) -> Self {
        // Match the HTTP path: aggregation aliases do not affect route counters,
        // and a WS prewarm is not a productive request.
        let connection_guard = if !turn.ctx.is_stack && !turn.prewarm {
            let guard = ActiveConnectionGuard::acquire(state.status.clone()).await;
            let mut status = state.status.write().await;
            status.total_requests = status.total_requests.saturating_add(1);
            status.last_request_at = Some(chrono::Utc::now().to_rfc3339());
            Some(guard)
        } else {
            None
        };
        Self {
            first_token_ms: None,
            repair: (!turn.official).then(Repair::default),
            compaction: turn.compaction.then(NativeCompactionState::default),
            connection_guard,
            turn,
        }
    }

    fn events(&mut self, event: Value) -> Vec<Value> {
        let stream_id = event.get("stream_id").cloned();
        let data = event.to_string();
        let kind = event["type"].as_str();
        if self.first_token_ms.is_none() && sse_data_starts_output(kind, &data) {
            self.first_token_ms = Some(self.turn.ctx.start_time.elapsed().as_millis() as u64);
        }
        let events = match &mut self.repair {
            Some(repair) => parse_sse_events(repair.push(&format!("data: {data}"))),
            None => vec![event],
        };
        let mut events = match &mut self.compaction {
            Some(compaction) => events
                .into_iter()
                .flat_map(|event| {
                    parse_sse_events(compaction.handle_block(&format!("data: {event}")))
                })
                .collect(),
            None => events,
        };
        if let Some(stream_id) = stream_id {
            for event in &mut events {
                event["stream_id"] = stream_id.clone();
            }
        }
        events
    }

    fn finish(self, state: &ProxyState, headers: &HeaderMap, event: Option<&Value>, status: u16) {
        if self.connection_guard.is_some() {
            let state = state.clone();
            let provider = self.turn.ctx.provider.clone();
            tokio::spawn(async move {
                let mut stats = state.status.write().await;
                if status < 400 {
                    stats.success_requests = stats.success_requests.saturating_add(1);
                    stats.last_error = None;
                } else {
                    stats.failed_requests = stats.failed_requests.saturating_add(1);
                    stats.last_error = Some(format!(
                        "Responses WebSocket request ended with status {status}"
                    ));
                }
                if stats.total_requests > 0 {
                    stats.success_rate =
                        stats.success_requests as f32 / stats.total_requests as f32 * 100.0;
                }
                drop(stats);
                if status < 400 {
                    state
                        .current_providers
                        .write()
                        .await
                        .insert("codex".into(), (provider.id, provider.name));
                }
            });
        }
        if status == 200 && !self.turn.prewarm {
            if let Some(response) = event.and_then(|event| event.get("response")) {
                super::codex_native::observe(
                    &self.turn.ctx.provider,
                    headers,
                    &self.turn.frame,
                    response,
                );
            }
        }
        if self.turn.prewarm || !usage_logging_enabled(state) {
            return;
        }
        let state = state.clone();
        let response = event.and_then(|event| event.get("response"));
        let usage = response
            .and_then(TokenUsage::from_codex_response_auto)
            .unwrap_or_default();
        let model = response
            .and_then(|response| response["model"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.turn.ctx.outbound_model.clone().unwrap_or_default());
        tokio::spawn(async move {
            let ctx = self.turn.ctx;
            log_usage_internal(
                &state,
                &ctx.provider.id,
                "codex",
                &model,
                &ctx.request_model,
                ctx.outbound_model.as_deref().unwrap_or(&ctx.request_model),
                usage,
                ctx.start_time.elapsed().as_millis() as u64,
                self.first_token_ms,
                true,
                status,
                Some(ctx.session_id),
            )
            .await;
        });
    }
}

fn terminal_status(event: &Value) -> Option<u16> {
    match event["type"].as_str()? {
        "response.completed" => Some(200),
        "response.incomplete" => Some(206),
        "response.failed" | "error" => Some(
            event["status"]
                .as_u64()
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(502),
        ),
        _ => None,
    }
}

async fn fallback(state: &ProxyState, key: Option<&str>, socket: &mut WebSocket) {
    state.responses_ws.mark_http(key);
    log::info!("[CodexWS] falling back from websocket to existing HTTP Responses handler");
    let _ = socket
        .send(close(1012, "Selected route requires HTTP Responses"))
        .await;
}

async fn session(
    state: ProxyState,
    headers: HeaderMap,
    uri: Uri,
    key: Option<String>,
    mut shutdown: watch::Receiver<u64>,
    mut down: WebSocket,
) {
    let mut upstream: Option<Box<WebSocketStream<reqwest::Upgraded>>> = None;
    let mut pinned_route = None;
    let mut pending = VecDeque::<Pending>::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let _ = down.send(close(1001, "CC Switch routing stopped")).await;
                break;
            }
            message = down.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(Message::Binary(_))) => {
                        let _ = down.send(close(1003, "Responses requires JSON text frames")).await;
                        break;
                    }
                };
                let frame = match serde_json::from_str::<Value>(&text) {
                    Ok(frame) => frame,
                    Err(_) => {
                        if !reply_error(&mut down, 400, "invalid_json", "Expected a JSON response.create frame").await { break; }
                        continue;
                    }
                };
                if frame["type"].as_str() != Some("response.create") {
                    if let Some(up) = &mut upstream {
                        if up.send(UpMessage::Text(text)).await.is_err() { break; }
                    } else if !reply_error(&mut down, 400, "response_create_required", "Send response.create before other events").await { break; }
                    continue;
                }
                if pending.len() >= 8 {
                    if !reply_error(&mut down, 429, "too_many_inflight_responses", "Too many unfinished responses on this connection").await { break; }
                    continue;
                }
                let turn = match select_turn(&state, &headers, frame).await {
                    Ok(turn) => turn,
                    Err(error) => {
                        let response = error.into_response();
                        let status = response.status().as_u16();
                        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap_or_default();
                        let message = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["error"]["message"].as_str().map(str::to_owned)).unwrap_or_else(|| "Cannot select Responses provider".into());
                        let code = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["error"]["code"].as_str().map(str::to_owned)).unwrap_or_else(|| "responses_request_rejected".into());
                        if !reply_error(&mut down, status, &code, &message).await { break; }
                        continue;
                    }
                };
                if turn.ctx.app_config.auto_failover_enabled || needs_http(&turn.ctx.provider) {
                    fallback(&state, key.as_deref(), &mut down).await;
                    break;
                }
                let route = upstream_url(&turn.ctx.provider, &uri).ok().map(|url| super::codex_native::transport_key(&turn.ctx.provider, &url));
                if pinned_route.as_ref().is_some_and(|pinned| Some(pinned) != route.as_ref()) {
                    // Do not send this frame or the old socket's continuation state to another provider.
                    let _ = down.send(close(1012, "Model uses another provider; reconnect")).await;
                    break;
                }
                if upstream.is_none() {
                    let connected = tokio::select! {
                        _ = shutdown.changed() => {
                            let _ = down.send(close(1001, "CC Switch routing stopped")).await;
                            break;
                        }
                        connected = connect(&state, &headers, &uri, &turn) => connected,
                    };
                    match connected {
                        Ok(Connected::Open(up)) => {
                            pinned_route = route;
                            upstream = Some(up);
                        }
                        Ok(Connected::Http) => {
                            fallback(&state, key.as_deref(), &mut down).await;
                            break;
                        }
                        Err(error) => {
                            let status = match error {
                                // This key belongs to the selected third party,
                                // not the official login carried by the client.
                                ProxyError::UpstreamError { status: 401, .. } if !turn.official => 502,
                                ProxyError::UpstreamError { status, .. } => status,
                                ProxyError::AuthError(_) if !turn.official => 502,
                                ProxyError::AuthError(_) => 401,
                                _ => 502,
                            };
                            let _ = reply_error(&mut down, status, "upstream_websocket_error", &error.to_string()).await;
                            let _ = down.send(close(1011, "Upstream WebSocket unavailable")).await;
                            break;
                        }
                    }
                }
                let frame = turn.frame.to_string();
                let request = Pending::new(turn, &state).await;
                if upstream.as_mut().expect("connected upstream").send(UpMessage::Text(frame)).await.is_err() {
                    request.finish(&state, &headers, None, 502);
                    break;
                }
                pending.push_back(request);
            }
            message = async { upstream.as_mut().expect("connected upstream").next().await }, if upstream.is_some() => {
                match message {
                    Some(Ok(UpMessage::Text(text))) => {
                        let mut event = match serde_json::from_str::<Value>(&text) {
                            Ok(event) => event,
                            Err(_) => {
                                if down.send(Message::Text(text)).await.is_err() { break; }
                                continue;
                            }
                        };
                        let status = terminal_status(&event);
                        let pending_index = pending.iter().position(|head| head.turn.frame.get("stream_id") == event.get("stream_id"));
                        if status == Some(401)
                            && pending_index.and_then(|index| pending.get(index)).is_some_and(|head| !head.turn.official)
                        {
                            event["status"] = json!(502);
                            event["upstream_status"] = json!(401);
                        }
                        // Preserve the existing encrypted-context rectifier by returning to
                        // its HTTP handler before any productive output has been exposed.
                        if matches!(status, Some(400..=599)) {
                            if let Some(head) = pending_index.and_then(|index| pending.get(index)) {
                                let error = ProxyError::UpstreamError { status: status.unwrap_or(502), body: Some(event.to_string()) };
                                if head.first_token_ms.is_none()
                                    && super::opaque_state_rectifier::detect_opaque_state_rejection(
                                        &error, &head.turn.ctx.rectifier_config, &head.turn.frame, !head.turn.official
                                    ).is_some_and(|rejection| !(rejection.compaction
                                        && super::codex_native::carries_native_compaction(&head.turn.frame)
                                        && super::codex_native::native_compaction(&head.turn.ctx.provider, head.turn.frame["model"].as_str().unwrap_or_default())))
                                {
                                    fallback(&state, key.as_deref(), &mut down).await;
                                    break;
                                }
                            }
                        }
                        let events = match pending_index.and_then(|index| pending.get_mut(index)) {
                            Some(head) => head.events(event),
                            None => vec![event],
                        };
                        let mut ended = false;
                        for event in &events {
                            if down.send(Message::Text(event.to_string())).await.is_err() { ended = true; break; }
                        }
                        if status.is_some() {
                            if let Some(done) = pending_index.and_then(|index| pending.remove(index)) {
                                let terminal = events.iter().rev().find(|event| terminal_status(event).is_some());
                                let status = terminal.and_then(terminal_status).or(status).unwrap_or(502);
                                done.finish(&state, &headers, terminal, status);
                            }
                        }
                        if ended { break; }
                    }
                    Some(Ok(UpMessage::Ping(_) | UpMessage::Pong(_))) => {}
                    Some(Ok(UpMessage::Close(frame))) => {
                        let _ = down.send(Message::Close(frame.map(|frame| CloseFrame {
                            code: frame.code.into(), reason: frame.reason,
                        }))).await;
                        break;
                    }
                    Some(Ok(UpMessage::Binary(bytes))) => {
                        if down.send(Message::Binary(bytes)).await.is_err() { break; }
                    }
                    Some(Ok(UpMessage::Frame(_))) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
    for unfinished in pending {
        unfinished.finish(&state, &headers, None, 499);
    }
    if let Some(up) = &mut upstream {
        let _ = tokio::time::timeout(Duration::from_secs(1), up.close()).await;
    }
    let _ = down.close().await;
}

#[cfg(test)]
mod tests;
