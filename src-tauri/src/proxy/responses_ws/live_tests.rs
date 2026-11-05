//! Opt-in live acceptance test. Credentials come from a private, untracked file;
//! no account ids, keys or response bodies are written to the public report.
use super::*;
use std::path::PathBuf;

const LIVE_WAIT: Duration = Duration::from_secs(600);

async fn live_socket(
    url: &str,
    auth: &HeaderMap,
) -> Result<WebSocketStream<reqwest::Upgraded>, String> {
    let mut headers = auth.clone();
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_str(&generate_key()).unwrap(),
    );
    let response = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .timeout(LIVE_WAIT)
        .build()
        .map_err(|_| "client")?
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|error| {
            format!(
                "WebSocket handshake failed (connect={}, timeout={})",
                error.is_connect(),
                error.is_timeout()
            )
        })?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!(
            "WebSocket handshake HTTP {}",
            response.status().as_u16()
        ));
    }
    Ok(WebSocketStream::from_raw_socket(
        response.upgrade().await.map_err(|_| "local upgrade")?,
        Role::Client,
        None,
    )
    .await)
}

#[tokio::test]
#[ignore = "Private fixture required; handshake only, no response.create or model generation"]
async fn live_krill_three_line_handshake_diagnostic() {
    let fixture: Value = serde_json::from_slice(
        &std::fs::read(std::env::var_os("CCS_WS_LIVE_FIXTURE").unwrap()).unwrap(),
    )
    .unwrap();
    crate::proxy::http_client::init(None).unwrap();
    let provider = Provider::with_id(
        "live-krill".into(),
        "Krill".into(),
        fixture["krill_settings"].clone(),
        None,
    );
    let mut source = HeaderMap::new();
    source.insert(
        "user-agent",
        HeaderValue::from_static("codex_cli_rs/0.161.0"),
    );
    source.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    source.insert("version", HeaderValue::from_static("0.161.0"));
    source.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    let mut report = json!({"scope":"Handshake only; no model generation","results":[]});
    for (line, host) in [
        ("cn_fast", "api-slb.kbsfdxc.net"),
        ("overseas", "api.kbsfdxc.net"),
        ("cdn", "api.kbsfdxc.com"),
    ] {
        let url = format!("https://{host}/v1/responses");
        for (policy, client) in [
            (
                "ccs_proxy_policy",
                crate::proxy::http_client::websocket_for_url(&url).unwrap(),
            ),
            (
                "direct",
                reqwest::Client::builder()
                    .http1_only()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(30))
                    .timeout(LIVE_WAIT)
                    .build()
                    .unwrap(),
            ),
        ] {
            let headers = upstream_headers(&source, &provider, false, &generate_key()).unwrap();
            let started = Instant::now();
            let mut result = json!({"line":line,"policy":policy});
            match client.get(&url).headers(headers).send().await {
                Ok(response) => {
                    result["status"] = json!(response.status().as_u16());
                    result["passed"] = json!(response.status() == StatusCode::SWITCHING_PROTOCOLS);
                }
                Err(error) => {
                    result["passed"] = json!(false);
                    result["is_connect"] = json!(error.is_connect());
                    result["is_timeout"] = json!(error.is_timeout());
                }
            }
            result["elapsed_ms"] = json!(started.elapsed().as_millis());
            println!("Handshake diagnostic: {result}");
            report["results"].as_array_mut().unwrap().push(result);
            std::fs::write(
                std::env::var_os("CCS_WS_LIVE_REPORT").unwrap(),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
        }
    }
}

async fn live_turn(
    socket: &mut WebSocketStream<reqwest::Upgraded>,
    frame: Value,
) -> Result<Vec<Value>, String> {
    let operation = if frame["generate"] == false {
        "prewarm"
    } else if codex_compaction::is_compaction_request(&frame) {
        "compaction"
    } else if super::super::super::codex_native::carries_native_compaction(&frame) {
        "compaction replay"
    } else if frame.get("previous_response_id").is_some() {
        "incremental continuation"
    } else if frame.get("tool_choice").is_some() {
        "function call"
    } else {
        "generation"
    };
    socket
        .send(UpMessage::Text(frame.to_string()))
        .await
        .map_err(|_| "send failed")?;
    tokio::time::timeout(LIVE_WAIT, async {
        let mut events = Vec::new();
        loop {
            match socket.next().await {
                Some(Ok(UpMessage::Text(text))) => {
                    let event: Value =
                        serde_json::from_str(&text).map_err(|_| "non-JSON upstream event")?;
                    let status = terminal_status(&event);
                    events.push(event);
                    if let Some(status) = status {
                        if status != 200 {
                            let error = events.last().unwrap();
                            let code = error
                                .pointer("/error/code")
                                .or_else(|| error.pointer("/response/error/code"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let detail = if code == "upstream_websocket_error" {
                                error
                                    .pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("handshake failed")
                            } else {
                                "request failed"
                            };
                            return Err(format!("terminal status {status}, code {code}: {detail}"));
                        }
                        return Ok(events);
                    }
                }
                Some(Ok(UpMessage::Close(frame))) => {
                    return Err(format!(
                        "socket closed with {}",
                        frame.map(|f| u16::from(f.code)).unwrap_or(1005)
                    ))
                }
                Some(Err(_)) | None => return Err("socket ended before response.completed".into()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| format!("{operation}: test wait limit reached (600 seconds)"))?
    .map_err(|error| format!("{operation}: {error}"))
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "response.completed")
        .flat_map(|e| e["response"]["output"].as_array().into_iter().flatten())
        .flat_map(|i| i["content"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn frame(model: &str, input: Value) -> Value {
    json!({"type":"response.create","model":model,"instructions":"Follow the test request precisely. Be concise.",
        "store":false,"reasoning":{"effort":"low"},"input":input,"tools":[]})
}

async fn acceptance(
    url: &str,
    headers: &HeaderMap,
    model: &str,
    results: &mut Vec<&'static str>,
) -> Result<Vec<&'static str>, String> {
    let mut socket = live_socket(url, headers).await?;
    let mut prewarm = frame(model, json!([]));
    prewarm["generate"] = json!(false);
    live_turn(&mut socket, prewarm).await?;
    results.push("websocket_prewarm");

    let first = live_turn(
        &mut socket,
        frame(
            model,
            json!([{"role":"user","content":"Remember the exact code ORCHID-742. Reply only OK."}]),
        ),
    )
    .await?;
    let previous = first.last().unwrap()["response"]["id"]
        .as_str()
        .ok_or("missing response id")?;
    let mut next = frame(
        model,
        json!([{"role":"user","content":"What exact code did I ask you to remember? Reply only with the code."}]),
    );
    next["previous_response_id"] = json!(previous);
    let second = live_turn(&mut socket, next).await?;
    if !text_of(&second).contains("ORCHID-742") {
        return Err("incremental conversation did not retain the code".into());
    }
    results.push("incremental_multiturn_context");

    // A real encrypted reasoning item must either remain readable or expose the
    // target's rejection. A successful repair by deleting it is never accepted.
    let seed = json!({"role":"user","content":"Remember HTTP-742 and the task websocket. Compute the coefficient of x^12 in (1+x+x^2+x^3)^10 internally, without tools; return only the integer."});
    let mut reasoning_frame = frame(model, json!([seed.clone()]));
    reasoning_frame["reasoning"]["effort"] = json!("high");
    reasoning_frame["include"] = json!(["reasoning.encrypted_content"]);
    let reasoning_events = live_turn(&mut socket, reasoning_frame).await?;
    let output = reasoning_events.last().unwrap()["response"]["output"]
        .as_array()
        .ok_or("missing reasoning output")?
        .clone();
    if !output.iter().any(|item| {
        item["type"] == "reasoning"
            && item["encrypted_content"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
    }) {
        return Err("high-effort test did not produce encrypted reasoning".into());
    }
    let mut replay = vec![seed];
    replay.extend(output);
    replay
        .push(json!({"role":"user","content":"State the marker and task I gave you. Be concise."}));
    let mut fresh = live_socket(url, headers).await?;
    let own = live_turn(&mut fresh, frame(model, json!(replay))).await?;
    let text = text_of(&own);
    if !text.contains("HTTP-742") || !text.to_ascii_lowercase().contains("websocket") {
        return Err("source reasoning replay lost the test facts".into());
    }
    let opposite = if let Some(upstream) = model.strip_prefix("ccs-krill/") {
        upstream.to_owned()
    } else {
        format!("ccs-krill/{model}")
    };
    let mut cross = frame(&opposite, json!(replay));
    cross.as_object_mut().unwrap().remove("type"); // HTTP has no response.create envelope.
    cross["stream"] = json!(true);
    match live_cross_provider(url, headers, &cross).await? {
        "cross_provider_compaction_accepted_with_context" => {
            results.push("cross_provider_reasoning_accepted_unchanged")
        }
        "cross_provider_compaction_upstream_rejection_preserved" => {
            results.push("cross_provider_reasoning_upstream_rejection_preserved")
        }
        _ => return Err("unexpected reasoning continuation result".into()),
    }
    results.push("encrypted_reasoning_source_replay");

    let mut tool = frame(
        model,
        json!([{"role":"user","content":"Call echo with value 742."}]),
    );
    tool["tools"] = json!([{"type":"function","name":"echo","description":"Echo the value","parameters":{"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false},"strict":true}]);
    tool["tool_choice"] = json!({"type":"function","name":"echo"});
    let events = live_turn(&mut socket, tool.clone()).await?;
    let call = events.last().unwrap()["response"]["output"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "function_call")
        .ok_or("missing tool call")?;
    let args: Value =
        serde_json::from_str(call["arguments"].as_str().ok_or("missing tool arguments")?)
            .map_err(|_| "invalid tool arguments")?;
    if args["value"] != 742 {
        return Err("wrong tool arguments".into());
    }
    let mut output = frame(
        model,
        json!([{"type":"function_call_output","call_id":call["call_id"],"output":"742"}]),
    );
    output["tools"] = tool["tools"].clone();
    output["previous_response_id"] = events.last().unwrap()["response"]["id"].clone();
    let continued = live_turn(&mut socket, output).await?;
    if !text_of(&continued).contains("742") {
        return Err("tool output not consumed".into());
    }
    results.push("tool_call_and_output");

    let compact = live_turn(&mut socket, frame(model, json!([
        {"role":"user","content":"Our project code is ORCHID-742. The next pending task is to write a Rust WebSocket relay. Preserve both facts in the summary."},
        {"role":"assistant","content":[{"type":"output_text","text":"I will preserve project code ORCHID-742 and the pending Rust WebSocket relay task."}]},
        {"type":"compaction_trigger"}
    ]))).await?;
    let item = compact.last().unwrap()["response"]["output"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "compaction")
        .ok_or("missing compaction item")?
        .clone();
    if item["encrypted_content"].as_str().is_none_or(str::is_empty) {
        return Err("empty compaction".into());
    }
    if item["encrypted_content"]
        .as_str()
        .is_some_and(|s| s.starts_with("ccswitch-"))
    {
        return Err("native compaction was replaced by a CCS summary envelope".into());
    }
    results.push("native_compaction_without_bridge");
    let _ = socket.close(None).await;
    let mut socket = live_socket(url, headers).await?;
    let after = live_turn(&mut socket, frame(model, json!([item,
        {"role":"user","content":"State the exact project code and next pending task, concisely."}
    ]))).await?;
    let text = text_of(&after);
    if !text.contains("ORCHID-742") || !text.to_ascii_lowercase().contains("websocket") {
        return Err("compacted context was not retained".into());
    }
    results.push("native_compaction_replay_new_connection");
    let _ = socket.close(None).await;

    let mut http_body = frame(
        model,
        json!([
            {"role":"user","content":"Project code HTTP-742. Pending task: write a Rust WebSocket relay."},
            {"role":"assistant","content":[{"type":"output_text","text":"Remembered HTTP-742 and the Rust WebSocket relay task."}]},
            {"type":"compaction_trigger"}
        ]),
    );
    http_body.as_object_mut().unwrap().remove("type");
    http_body["input"][0]["content"] = json!(format!("Project code HTTP-742. Pending task: write a Rust WebSocket relay. Synthetic transport padding: {}", "Preserve the project code and pending task. ".repeat(80)));
    http_body["stream"] = json!(true);
    let compact = live_http(url, headers, &http_body, true)
        .await
        .map_err(|e| format!("HTTP compaction: {e}"))?;
    let item = compact["output"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["type"] == "compaction")
        .ok_or("HTTP native compaction missing")?;
    if item["encrypted_content"]
        .as_str()
        .is_none_or(|s| s.is_empty() || s.starts_with("ccswitch-"))
    {
        return Err("HTTP compaction did not preserve native ciphertext".into());
    }
    http_body["input"] = json!([item,{"role":"user","content":"State the exact project code and next pending task."}]);
    let after = live_http(url, headers, &http_body, false)
        .await
        .map_err(|e| format!("HTTP replay: {e}"))?;
    let as_events = vec![json!({"type":"response.completed","response":after})];
    let text = text_of(&as_events);
    if !text.contains("HTTP-742") || !text.to_ascii_lowercase().contains("websocket") {
        return Err("HTTP native compaction replay lost context".into());
    }
    results.push("http_zstd_ingress_native_compaction_and_replay");
    let opposite = if let Some(upstream) = model.strip_prefix("ccs-krill/") {
        upstream.to_owned()
    } else {
        format!("ccs-krill/{model}")
    };
    http_body["model"] = json!(opposite);
    let result = live_cross_provider(url, headers, &http_body).await?;
    results.push(result);
    Ok(results.clone())
}

async fn live_cross_provider(
    url: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<&'static str, String> {
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|_| "cross-provider client")?
        .post(url)
        .headers(headers.clone())
        .json(body)
        .timeout(LIVE_WAIT)
        .send()
        .await
        .map_err(|_| "cross-provider request failed")?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "cross-provider response body")?;
    let mut values = Vec::new();
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        values.push(value);
    } else {
        for line in String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
        {
            if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                values.push(value);
            }
        }
    }
    for value in values.iter().rev() {
        let response = if value["type"] == "response.completed" {
            &value["response"]
        } else {
            value
        };
        if response["status"] == "completed" {
            let text = text_of(&[json!({"type":"response.completed","response":response})]);
            if text.contains("HTTP-742") && text.to_ascii_lowercase().contains("websocket") {
                return Ok("cross_provider_compaction_accepted_with_context");
            }
            return Err("cross-provider continuation lost compacted context".into());
        }
        if let Some(error) = value
            .get("error")
            .or_else(|| value.pointer("/response/error"))
        {
            let code = error["code"].as_str().unwrap_or_default();
            if code == "compaction_provider_mismatch" {
                return Err("local provenance admission gate is still present".into());
            }
            let message = error["message"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if code == "invalid_encrypted_content"
                || message.contains("invalid_encrypted_content")
                || message.contains("encrypted content")
                    && message.contains("could not be verified")
                || message.contains("compaction")
                    && (message.contains("decrypt")
                        || message.contains("decode")
                        || message.contains("invalid"))
            {
                return Ok("cross_provider_compaction_upstream_rejection_preserved");
            }
        }
    }
    let errors: Vec<_> = values.iter().filter_map(|value| {
        value.get("error").or_else(||value.pointer("/response/error"))
    }).map(|error| json!({"code":error["code"],"message":error["message"].as_str().unwrap_or_default().chars().take(300).collect::<String>()})).collect();
    Err(format!(
        "Unexpected cross-provider response (HTTP {status}): {errors:?}"
    ))
}

async fn live_http(
    url: &str,
    headers: &HeaderMap,
    body: &Value,
    compressed: bool,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|_| "HTTP client")?;
    let mut request = client
        .post(url)
        .headers(headers.clone())
        .header("content-type", "application/json")
        .timeout(LIVE_WAIT);
    let bytes = serde_json::to_vec(body).unwrap();
    let bytes = if compressed {
        request = request.header("content-encoding", "zstd");
        zstd::bulk::compress(&bytes, 3).map_err(|_| "request compression")?
    } else {
        bytes
    };
    let response = request.body(bytes).send().await.map_err(|e| {
        if e.is_timeout() {
            "HTTP test wait limit reached (600 seconds)"
        } else if e.is_connect() {
            "HTTP connection failed"
        } else {
            "HTTP response request failed"
        }
    })?;
    if !response.status().is_success() {
        return Err(format!("HTTP status {}", response.status().as_u16()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "HTTP response body failed")?;
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        if value["status"] == "completed" {
            return Ok(value);
        }
    }
    for block in String::from_utf8_lossy(&bytes).split("\n\n") {
        for line in block.lines().filter_map(|l| l.strip_prefix("data:")) {
            if let Ok(event) = serde_json::from_str::<Value>(line.trim()) {
                if event["type"] == "response.completed" {
                    return Ok(event["response"].clone());
                }
                if event["type"] == "error" || event["type"] == "response.failed" {
                    return Err("HTTP Responses error event".into());
                }
            }
        }
    }
    Err("HTTP response did not complete".into())
}

#[tokio::test]
#[ignore = "Requires explicit authorization and a private CCS_WS_LIVE_FIXTURE; consumes account quota"]
#[serial_test::serial]
async fn live_official_and_krill_acceptance() {
    let fixture_path =
        PathBuf::from(std::env::var_os("CCS_WS_LIVE_FIXTURE").expect("private fixture"));
    let fixture: Value = serde_json::from_slice(&std::fs::read(fixture_path).unwrap()).unwrap();
    // Match the desktop app startup: use one shared initialized upstream client.
    crate::proxy::http_client::init(None).unwrap();
    let report_path = PathBuf::from(std::env::var_os("CCS_WS_LIVE_REPORT").expect("report path"));
    let restore = RestoreState::capture();
    let db = Arc::new(Database::memory().unwrap());
    db.save_provider("codex", &official()).unwrap();
    let krill = Provider::with_id(
        "live-krill".into(),
        "Krill".into(),
        fixture["krill_settings"].clone(),
        None,
    );
    db.save_provider("codex", &krill).unwrap();
    state::update(&DeviceStore::for_device(), |live| {
        let app = live.apps.entry("codex".into()).or_default();
        app.mode = Some(Mode::Proxy);
        app.proxy_route = Some(crate::database::CODEX_OFFICIAL_PROVIDER_ID.into());
        app.stack.enabled = true;
        app.stack.members = vec![
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.into(),
            "live-krill".into(),
        ];
        app.stack.keys.insert("krill".into(), "live-krill".into());
    })
    .unwrap();
    let proxy = ProxyServer::new(
        ProxyConfig {
            listen_port: 0,
            ..ProxyConfig::default()
        },
        db,
        None,
    );
    let info = proxy.start().await.unwrap();
    let url = format!("http://127.0.0.1:{}/v1/responses", info.port);
    let mut auth = HeaderMap::new();
    auth.insert(
        "authorization",
        HeaderValue::from_str(&format!(
            "Bearer {}",
            fixture["access_token"].as_str().unwrap()
        ))
        .unwrap(),
    );
    auth.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(fixture["account_id"].as_str().unwrap()).unwrap(),
    );
    auth.insert(
        "session_id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
    );
    auth.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    auth.insert(
        "user-agent",
        HeaderValue::from_static("codex_cli_rs/0.161.0"),
    );
    auth.insert("version", HeaderValue::from_static("0.161.0"));
    auth.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    let model = fixture["model"].as_str().unwrap();
    let models = std::env::var("CCS_WS_LIVE_MODELS").unwrap_or_else(|_| model.to_owned());
    let mut report = json!({"models":models,"native_compaction":true,"test_wait_limit_seconds":600,"results":{}});
    let diagnostic_provider = Provider::with_id(
        "live-krill".into(),
        "Krill".into(),
        fixture["krill_settings"].clone(),
        None,
    );
    let diagnostic_url =
        upstream_url(&diagnostic_provider, &"/responses".parse().unwrap()).unwrap();
    let mut diagnostic_headers =
        upstream_headers(&auth, &diagnostic_provider, false, "ignored-key").unwrap();
    diagnostic_headers.remove("sec-websocket-key");
    let started = Instant::now();
    let direct_handshake = live_socket(&diagnostic_url, &diagnostic_headers).await;
    report["direct_krill_handshake_control"] = match direct_handshake {
        Ok(mut socket) => {
            let _ = socket.close(None).await;
            json!({"passed":true})
        }
        Err(reason) => json!({"passed":false,"reason":reason}),
    };
    report["direct_krill_handshake_control"]["elapsed_ms"] = json!(started.elapsed().as_millis());
    println!(
        "Direct Krill handshake control: {}",
        report["direct_krill_handshake_control"]
    );
    let mut failed = false;
    for model in models.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        for (provider, selected) in [
            ("official", model.to_owned()),
            ("krill", format!("ccs-krill/{model}")),
        ] {
            auth.insert(
                "session_id",
                HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
            );
            let name = format!("{provider}/{model}");
            let mut completed_checks = Vec::new();
            let started = Instant::now();
            let result = acceptance(&url, &auth, &selected, &mut completed_checks).await;
            report["results"][&name] = match result {
                Ok(checks) => json!({"passed":true,"checks":checks}),
                Err(reason) => {
                    failed = true;
                    json!({"passed":false,"reason":reason,"checks_passed":completed_checks})
                }
            };
            report["results"][&name]["elapsed_ms"] = json!(started.elapsed().as_millis());
            std::fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
            println!("Live {name}: {}", report["results"][&name]);
        }
    }
    let started = Instant::now();
    let switching = async {
        let mut socket = live_socket(&url, &auth).await?;
        live_turn(&mut socket, frame(model, json!([{"role":"user","content":"Remember SWITCH-742. Reply OK."}]))).await?;
        for selected in [format!("ccs-krill/{model}"), model.to_owned()] {
            socket.send(UpMessage::Text(frame(&selected, json!([{"role":"user","content":"Remember SWITCH-742. Reply OK."}])).to_string())).await.map_err(|_| "switch send")?;
            let closed = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.map_err(|_| "switch close timeout")?;
            if !matches!(closed, Some(Ok(UpMessage::Close(Some(ref frame)))) if u16::from(frame.code) == 1012) { return Err("provider switch did not reconnect".to_string()); }
            socket = live_socket(&url, &auth).await?;
            let answer = live_turn(&mut socket, frame(&selected, json!([
                {"role":"user","content":"Remember SWITCH-742."},
                {"role":"assistant","content":[{"type":"output_text","text":"Remembered."}]},
                {"role":"user","content":"State the exact marker."}
            ]))).await?;
            if !text_of(&answer).contains("SWITCH-742") { return Err("full history replay after provider switch failed".to_string()); }
        }
        Ok::<_, String>(vec!["official_to_krill_reconnect", "krill_to_official_reconnect", "full_history_replay"])
    }.await;
    report["results"]["provider_switch"] = match switching {
        Ok(checks) => json!({"passed":true,"checks":checks}),
        Err(reason) => {
            failed = true;
            json!({"passed":false,"reason":reason})
        }
    };
    report["results"]["provider_switch"]["elapsed_ms"] = json!(started.elapsed().as_millis());
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    proxy.stop().await.unwrap();
    drop(restore);
    assert!(!failed, "Live acceptance failed; see the redacted report");
}
