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
fn native_support_is_scoped_to_provider_name_models_and_explicit_overrides() {
    let mut named = provider("https://relay.example.org/v1");
    for name in ["krill", "Krill", "KRILL", "kRiLl"] {
        named.name = name.into();
        assert!(native_compaction(&named, "gpt-6.1-sol"));
        assert!(zstd_enabled(&named));
    }
    assert!(!native_compaction(&named, "kimi-k3"));
    let mut custom = provider("https://api.kbsfdxc.net");
    for name in ["Other", "my-krill", "krill-backup"] {
        custom.name = name.into();
        assert!(!native_compaction(&custom, "gpt-6.1-sol"));
        assert!(!zstd_enabled(&custom));
    }
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
fn strict_native_trigger_tools_format_and_legacy_envelopes_are_preserved() {
    let native = provider("https://api.kbsfdxc.net");
    let mut request = json!({"model":"gpt-6.1-sol","input":[{"type":"compaction_trigger"}],"tool_choice":"auto","tools":[],"text":{"format":{"type":"json_object"}}});
    let before = request.clone();
    prepare(&native, &mut request);
    assert_eq!(request, before);
    request["input"] = json!([super::super::providers::codex_compaction::compaction_output_item("legacy summary"),{"type":"compaction_trigger"}]);
    let legacy = request.clone();
    prepare(&native, &mut request);
    assert_eq!(
        request, legacy,
        "strict routes never expand or drop old state"
    );
    let mut ordinary = provider("https://example.org");
    ordinary.name = "Other".into();
    let mut bridge = before;
    prepare(&ordinary, &mut bridge);
    assert_eq!(bridge["tool_choice"], "none");
    assert!(!super::super::providers::codex_compaction::is_compaction_request(&bridge));
}

#[test]
#[serial_test::serial]
fn ownership_survives_reload_without_treating_a_new_reader_as_the_maker() {
    assert!(std::env::var_os("CC_SWITCH_TEST_HOME").is_some());
    let mut source = provider("https://example.org");
    let blob = format!("native-{}", uuid::Uuid::new_v4());
    let item = json!({"type":"compaction","encrypted_content":blob});
    let input = json!({"input":[item.clone()]});
    let response = json!({"status":"completed","output":[item]});
    let headers = HeaderMap::new();
    observe(&source, &headers, &json!({}), &response);
    let maker = identity(&source, &headers);
    {
        let mut cache = owners().lock().unwrap();
        cache.path = PathBuf::new();
        cache.records.clear();
        cache.load();
        assert_eq!(
            cache.records[&compactions(&input, "input")[0]].identity,
            maker
        );
    }
    source.id = "another-provider".into();
    source.settings_config["auth"]["OPENAI_API_KEY"] = json!("changed-secret");
    observe(
        &source,
        &headers,
        &input,
        &json!({"status":"completed","output":[]}),
    );
    assert_eq!(
        owners().lock().unwrap().records[&compactions(&input, "input")[0]].identity,
        maker
    );
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
fn official_identity_ignores_token_rotation_and_records_account_changes_only() {
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
    let same_account = identity(&source, &headers);
    headers.insert(
        "chatgpt-account-id",
        http::HeaderValue::from_static("account-two"),
    );
    assert_ne!(identity(&source, &headers), same_account);
    let fingerprint = &compactions(&request, "input")[0];
    assert_eq!(
        owners().lock().unwrap().records[fingerprint].identity,
        same_account
    );
}

#[test]
fn strict_native_history_preserves_nested_payloads_and_rejects_context_overrides() {
    let native = provider("https://relay.example.org/v1");
    let request = json!({
        "model":"gpt-6.1-sol", "_ccs_internal":"local-only",
        "input":[
            {"type":"reasoning","id":"foreign-rs","summary":[],"encrypted_content":"cipher-rs"},
            {"type":"reasoning","id":"rs_plain","summary":[],"content":[]},
            {"type":"compaction","id":"cmp_keep","encrypted_content":"cipher-cmp"},
            {"type":"function_call","id":"foreign-fc","call_id":"call_keep","name":"echo","arguments":"{\"_value\":7}"},
            {"type":"function_call_output","call_id":"call_keep","output":[{"type":"encrypted_content","encrypted_content":"cipher-tool","_format":"keep"}]}
        ],
        "tools":[{"type":"function","name":"echo","parameters":{"type":"object","properties":{"_value":{"type":"integer"}}}}]
    });
    assert!(strict_passthrough(&native, &request));
    let filtered = filter_strict_request(request.clone());
    assert!(filtered.get("_ccs_internal").is_none());
    assert_eq!(filtered["input"], request["input"]);
    assert_eq!(filtered["tools"], request["tools"]);
    assert!(validate_strict_context(&request, &filtered).is_ok());
    let mut replaced = filtered.clone();
    replaced["input"] = json!([]);
    assert!(validate_strict_context(&request, &replaced).is_err());
    let mut ordinary = native.clone();
    ordinary.name = "Other".into();
    assert!(!strict_passthrough(&ordinary, &request));
    ordinary.name = "Krill".into();
    ordinary.meta = Some(ProviderMeta {
        api_format: Some("openai_chat".into()),
        ..Default::default()
    });
    assert!(
        !strict_passthrough(&ordinary, &request),
        "conversion routes retain their old behavior"
    );
}
