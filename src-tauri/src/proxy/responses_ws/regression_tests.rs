use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::frame::{
    coding::{Data, OpCode},
    Frame,
};

#[tokio::test]
#[serial_test::serial]
async fn ws_reasoning_rejection_reaches_http_repair_without_dropping_compaction() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut received) = mpsc::unbounded_channel::<Value>();
    let ws_tx = tx.clone();
    let error = json!({"code":"invalid_encrypted_content","message":"The encrypted content for item rs_foreign could not be verified"});
    let ws_error = error.clone();
    let router = Router::new().route("/v1/responses", get(move |ws: WebSocketUpgrade| {
        let tx = ws_tx.clone(); let error = ws_error.clone();
        async move {ws.on_upgrade(move |mut socket| async move {
            if let Some(Ok(Message::Text(text))) = socket.recv().await {
                tx.send(serde_json::from_str(&text).unwrap()).unwrap();
                socket.send(Message::Text(json!({"type":"response.failed","response":{"status":"failed","error":error}}).to_string())).await.unwrap();
                let _ = socket.recv().await;
            }
        })}
    }).post(move |Json(frame): Json<Value>| {
        let tx = tx.clone(); let error = error.clone();
        async move {
            tx.send(frame.clone()).unwrap();
            if frame["input"].as_array().unwrap().iter().any(|item| item["type"] == "reasoning") {
                (StatusCode::BAD_REQUEST, Json(json!({"error":error})))
            } else {
                (StatusCode::OK, Json(json!({"id":"resp_repaired","object":"response","status":"completed","output":[]})))
            }
        }
    }));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let compact = json!({"type":"compaction","id":"cmp_keep","encrypted_content":"gAAAA-history"});
    let body = json!({"model":"ccs-krill/model-one","input":[compact.clone(),{"type":"reasoning","id":"rs_foreign","encrypted_content":"gAAAA-reasoning","summary":[]}]});
    let mut socket = client(&url, "repair-with-compaction").await;
    let mut frame = body.clone();
    frame["type"] = json!("response.create");
    socket
        .send(UpMessage::Text(frame.to_string()))
        .await
        .unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(close, UpMessage::Close(Some(frame)) if u16::from(frame.code) == 1012));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for index in 0..3 {
        let sent = received.recv().await.unwrap();
        assert_eq!(sent["input"][0], compact);
        assert_eq!(
            sent["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning"),
            index != 2
        );
    }
    assert!(received.try_recv().is_err());
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn rejected_handshakes_preserve_error_fields_and_isolate_third_party_auth() {
    let error = json!({"type":"authentication_error","code":"key_revoked","message":"Fixture key revoked","param":"api_key","details":{"retryable":false}});
    for (status, body, expected) in [
        (
            StatusCode::UNAUTHORIZED,
            json!({"error":error.clone()}).to_string(),
            Some(error.clone()),
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":error.clone()}).to_string(),
            Some(error.clone()),
        ),
        (
            StatusCode::FORBIDDEN,
            "<html>private fixture body</html>".to_owned(),
            None,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/v1/responses",
            get(move || {
                let body = body.clone();
                async move { (status, body).into_response() }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (proxy, url, _db, _restore) = proxy(&base).await;
        let mut socket = client(&url, "handshake-error").await;
        let events = run_turn(
            &mut socket,
            json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}),
        )
        .await;
        let event = events.last().unwrap();
        assert_eq!(event["type"], "error");
        assert_eq!(
            event["status"],
            if status == StatusCode::UNAUTHORIZED {
                502
            } else {
                status.as_u16()
            }
        );
        if let Some(expected) = expected {
            assert_eq!(event["error"], expected);
        } else {
            assert_eq!(event["error"]["type"], "upstream_error");
            assert!(event["error"]["message"].as_str().unwrap().contains("403"));
            assert!(!event.to_string().contains("private fixture body"));
        }
        proxy.stop().await.unwrap();
        task.abort();
    }
    let error = ProxyError::UpstreamError {
        status: 401,
        body: Some(json!({"error":error}).to_string()),
    };
    let Message::Text(event) = upstream_error_event(&error, true) else {
        panic!("Expected text")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&event).unwrap()["status"],
        401
    );
}

#[tokio::test]
#[serial_test::serial]
async fn native_compaction_rejection_is_returned_over_ws_without_http_retry() {
    let error = json!({"type":"invalid_request_error","code":"invalid_encrypted_content","param":"input[0].encrypted_content","message":"The encrypted content for item cmp_keep could not be verified"});
    let expected = error.clone();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut received) = mpsc::unbounded_channel::<Value>();
    let router=Router::new().route("/v1/responses",get(move |ws:WebSocketUpgrade| {
        let error=error.clone(); let tx=tx.clone();
        async move {ws.on_upgrade(move |mut socket| async move {
            if let Some(Ok(Message::Text(text)))=socket.recv().await {
                tx.send(serde_json::from_str(&text).unwrap()).unwrap();
                socket.send(Message::Text(json!({"type":"response.failed","response":{"status":"failed","error":error}}).to_string())).await.unwrap();
                let _=socket.recv().await;
            }
        })}
    }));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "compaction-error").await;
    let compact =
        json!({"type":"compaction","id":"cmp_keep","encrypted_content":"gAAAA-protected-history"});
    let events=run_turn(&mut socket,json!({"type":"response.create","model":"ccs-krill/model-one","input":[compact.clone(),{"type":"reasoning","id":"rs_keep","encrypted_content":"gAAAA-reasoning"}]})).await;
    assert_eq!(events.last().unwrap()["response"]["error"], expected);
    assert_eq!(received.recv().await.unwrap()["input"][0], compact);
    assert!(received.try_recv().is_err());
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn fragmented_message_larger_than_one_frame_reaches_upstream() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut received) = mpsc::unbounded_channel();
    let router=Router::new().route("/v1/responses",get(move |ws:WebSocketUpgrade| {
        let tx=tx.clone();
        async move {ws.max_message_size(MAX_REQUEST_BODY_BYTES).max_frame_size(MAX_REQUEST_BODY_BYTES).on_upgrade(move |mut socket| async move {
            if let Some(Ok(Message::Text(text)))=socket.recv().await {
                tx.send(text.len()).unwrap();
                socket.send(Message::Text(json!({"type":"response.completed","response":{"id":"resp_large","status":"completed","output":[]}}).to_string())).await.unwrap();
                let _=socket.recv().await;
            }
        })}
    }));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "fragmented-large").await;
    let text=format!("{{\"type\":\"response.create\",\"model\":\"ccs-krill/model-one\",\"input\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}","x".repeat(MAX_FRAME_BYTES+1));
    let chunks = text.as_bytes().chunks(16 * 1024 * 1024);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let opcode = if index == 0 {
            Data::Text
        } else {
            Data::Continue
        };
        socket
            .send(UpMessage::Frame(Frame::message(
                chunk.to_vec(),
                OpCode::Data(opcode),
                index + 1 == count,
            )))
            .await
            .unwrap();
    }
    let event = tokio::time::timeout(Duration::from_secs(30), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(event,UpMessage::Text(text) if serde_json::from_str::<Value>(&text).unwrap()["type"]=="response.completed")
    );
    assert!(received.recv().await.unwrap() > MAX_FRAME_BYTES);
    proxy.stop().await.unwrap();
    task.abort();
}

#[tokio::test]
#[serial_test::serial]
async fn an_announced_oversized_frame_closes_with_1009_before_allocating_payload() {
    let mock = mock(false).await;
    let (proxy, url, _db, _restore) = proxy(&mock.base).await;
    let mut socket = client(&url, "oversized-client").await;
    let mut header = vec![0x81, 0xff];
    header.extend_from_slice(&((MAX_FRAME_BYTES + 1) as u64).to_be_bytes());
    header.extend_from_slice(&[1, 2, 3, 4]);
    socket.get_mut().write_all(&header).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(event,UpMessage::Close(Some(frame)) if u16::from(frame.code)==1009));
    proxy.stop().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn an_oversized_upstream_frame_closes_both_legs_with_1009() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, mut received) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        let request = String::from_utf8(request).unwrap();
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, value)| value.trim())
            })
            .unwrap();
        let answer=format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n",derive_accept_key(key.as_bytes()));
        stream.write_all(answer.as_bytes()).await.unwrap();
        let mut upstream = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        assert!(matches!(
            upstream.next().await,
            Some(Ok(UpMessage::Text(_)))
        ));
        let mut header = vec![0x81, 0x7f];
        header.extend_from_slice(&((MAX_FRAME_BYTES + 1) as u64).to_be_bytes());
        upstream.get_mut().write_all(&header).await.unwrap();
        let closed = upstream.next().await.unwrap().unwrap();
        tx.send(matches!(closed,UpMessage::Close(Some(frame)) if u16::from(frame.code)==1009))
            .unwrap();
    });
    let (proxy, url, _db, _restore) = proxy(&base).await;
    let mut socket = client(&url, "oversized-upstream").await;
    socket
        .send(UpMessage::Text(
            json!({"type":"response.create","model":"ccs-krill/model-one","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(event,UpMessage::Close(Some(frame)) if u16::from(frame.code)==1009));
    assert!(received.recv().await.unwrap());
    proxy.stop().await.unwrap();
    task.await.unwrap();
}
