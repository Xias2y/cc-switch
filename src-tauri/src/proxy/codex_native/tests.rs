use super::*;
use crate::provider::ProviderMeta;

fn provider(base: &str) -> Provider {
    Provider::with_id(
        "native-test".into(),
        "Krill".into(),
        json!({"base_url":base,"auth":{"OPENAI_API_KEY":"test-secret"}}),
        None,
    )
}

#[test]
fn native_support_is_scoped_to_verified_hosts_models_and_explicit_overrides() {
    assert!(native_compaction(
        &provider("https://api.kbsfdxc.net"),
        "gpt-6.1-sol"
    ));
    assert!(!native_compaction(
        &provider("https://api.kbsfdxc.net"),
        "kimi-k3"
    ));
    assert!(!native_compaction(
        &provider("https://api.kbsfdxc.net.example.org"),
        "gpt-6.1-sol"
    ));
    assert!(!native_compaction(
        &provider("https://example.org"),
        "gpt-6.1-sol"
    ));
    let mut custom = provider("https://example.org");
    custom.meta = Some(ProviderMeta {
        codex_native_compaction: Some(true),
        ..Default::default()
    });
    assert!(native_compaction(&custom, "model-one"));
    assert!(
        !zstd_enabled(&custom),
        "compression and compaction are independent"
    );
}

#[test]
fn native_trigger_tools_and_format_are_preserved_and_legacy_envelopes_still_expand() {
    let native = provider("https://api.kbsfdxc.net");
    let mut request = json!({"model":"gpt-6.1-sol","input":[{"type":"compaction_trigger"}],"tool_choice":"auto","tools":[],"text":{"format":{"type":"json_object"}}});
    let before = request.clone();
    prepare(&native, &mut request);
    assert_eq!(request, before);
    request["input"] = json!([super::super::providers::codex_compaction::compaction_output_item("legacy summary"),{"type":"compaction_trigger"}]);
    prepare(&native, &mut request);
    assert_eq!(request["input"][1]["type"], "compaction_trigger");
    assert!(request["input"][0].to_string().contains("legacy summary"));
    let ordinary = provider("https://example.org");
    let mut bridge = before;
    prepare(&ordinary, &mut bridge);
    assert_eq!(bridge["tool_choice"], "none");
    assert!(!super::super::providers::codex_compaction::is_compaction_request(&bridge));
}

#[test]
#[serial_test::serial]
fn ownership_survives_reload_and_rejects_account_or_credential_changes() {
    assert!(std::env::var_os("CC_SWITCH_TEST_HOME").is_some());
    let mut source = provider("https://example.org");
    let blob = format!("native-{}", uuid::Uuid::new_v4());
    let item = json!({"type":"compaction","encrypted_content":blob});
    let input = json!({"input":[item.clone()]});
    let response = json!({"status":"completed","output":[item]});
    let headers = HeaderMap::new();
    validate(&source, &headers, &input).unwrap();
    observe(&source, &headers, &json!({}), &response);
    validate(&source, &headers, &input).unwrap();
    {
        let mut cache = owners().lock().unwrap();
        cache.path = PathBuf::new();
        cache.records.clear();
    }
    validate(&source, &headers, &input).unwrap();
    source.settings_config["auth"]["OPENAI_API_KEY"] = json!("changed-secret");
    assert!(validate(&source, &headers, &input).is_err());
    let saved = std::fs::read_to_string(DeviceStore::for_device().file(OWNER_FILE)).unwrap();
    assert!(!saved.contains(&blob));
    assert!(!saved.contains("test-secret"));
    assert!(!saved.contains("changed-secret"));
}

#[tokio::test]
#[serial_test::serial]
async fn zstd_roundtrip_small_body_and_refusal_isolation() {
    let mut source = provider("https://example.org");
    source.meta = Some(ProviderMeta {
        codex_request_zstd: Some(true),
        ..Default::default()
    });
    let url = format!("https://example.org/{}", uuid::Uuid::new_v4());
    let plain = vec![b'a'; 10_000];
    let compressed = compress(&source, &url, &plain).await.unwrap();
    assert_eq!(zstd::decode_all(compressed.as_slice()).unwrap(), plain);
    assert!(compress(&source, &url, b"small").await.is_none());
    note_zstd_refused(&source, &url);
    assert!(compress(&source, &url, &plain).await.is_none());
    source.settings_config["auth"]["OPENAI_API_KEY"] = json!("different-key");
    assert!(compress(&source, &url, &plain).await.is_some());
}

#[test]
#[serial_test::serial]
fn official_token_rotation_keeps_history_but_account_switch_does_not() {
    let mut source = provider("https://chatgpt.com/backend-api/codex");
    source.id = crate::database::CODEX_OFFICIAL_PROVIDER_ID.into();
    source.settings_config = json!({"auth":{},"config":""});
    source.category = Some("official".into());
    let mut headers = HeaderMap::new();
    headers.insert(
        "chatgpt-account-id",
        http::HeaderValue::from_static("account-one"),
    );
    headers.insert(
        "authorization",
        http::HeaderValue::from_static("Bearer token-before"),
    );
    let item =
        json!({"type":"compaction","encrypted_content":format!("native-{}",uuid::Uuid::new_v4())});
    let request = json!({"input":[item.clone()]});
    observe(
        &source,
        &headers,
        &json!({}),
        &json!({"status":"completed","output":[item]}),
    );
    headers.insert(
        "authorization",
        http::HeaderValue::from_static("Bearer token-refreshed"),
    );
    validate(&source, &headers, &request).unwrap();
    headers.insert(
        "chatgpt-account-id",
        http::HeaderValue::from_static("account-two"),
    );
    assert!(validate(&source, &headers, &request).is_err());
}
