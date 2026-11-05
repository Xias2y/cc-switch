//! Codex-native routes, following Superba's native/synthetic compaction split,
//! compaction ownership fingerprints and per-line zstd refusal memory.
//! Reference: krill-lab/superba f78d77a, compaction.rs and transport.rs (MIT).

use super::{
    content_encoding::{decompress_body_with_limit, get_content_encoding},
    hyper_client::ProxyResponse,
    providers::{self, CodexAdapter, ProviderAdapter},
    ProxyError,
};
use crate::{
    live::engine::{sha256_hex, DeviceStore},
    provider::Provider,
};
use futures::StreamExt;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const OWNER_FILE: &str = "codex-compaction-owners.json";
const OWNER_TTL: i64 = 90 * 24 * 60 * 60;
const MAX_OWNERS: usize = 20_000;
const ZSTD_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn krill(provider: &Provider) -> bool {
    provider.name.eq_ignore_ascii_case("krill")
}

pub(crate) fn native_compaction(provider: &Provider, model: &str) -> bool {
    providers::is_codex_official_provider(provider)
        || provider
            .meta
            .as_ref()
            .and_then(|m| m.codex_native_compaction)
            .unwrap_or_else(|| krill(provider) && model.starts_with("gpt-"))
}

pub(super) fn zstd_enabled(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|m| m.codex_request_zstd)
        .unwrap_or_else(|| providers::is_codex_official_provider(provider) || krill(provider))
}

/// Protocol versions are not account identity. Native Krill routes need the
/// Codex negotiation markers that Superba preserves on native passthrough.
pub(super) fn protocol_header(provider: &Provider, name: &str) -> bool {
    (krill(provider) || native_compaction(provider, "gpt-"))
        && matches!(
            name,
            "originator"
                | "version"
                | "openai-beta"
                | "session_id"
                | "session-id"
                | "thread-id"
                | "conversation_id"
                | "x-client-request-id"
        )
}

/// Legacy CCS summary envelopes still need expansion even on native routes.
pub(crate) fn prepare(provider: &Provider, body: &mut Value) {
    let native = native_compaction(provider, body["model"].as_str().unwrap_or_default());
    super::providers::codex_compaction::prepare_third_party_request(body, !native);
}

fn compactions(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| super::providers::codex_compaction::is_compaction_item(item))
        .filter_map(|item| item["encrypted_content"].as_str())
        .filter(|s| !s.is_empty() && !s.starts_with("ccswitch-"))
        .map(|s| sha256_hex(s.as_bytes()))
        .collect()
}

pub(super) fn carries_native_compaction(request: &Value) -> bool {
    !compactions(request, "input").is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    provider: String,
    credential: String,
}

fn identity(provider: &Provider, headers: &HeaderMap) -> Identity {
    let credential = if providers::is_codex_official_provider(provider) {
        headers
            .get("chatgpt-account-id")
            .map(|v| v.as_bytes().to_vec())
            .or_else(|| {
                provider
                    .meta
                    .as_ref()?
                    .managed_account_id_for("codex_oauth")
                    .map(|id| id.into_bytes())
            })
            .unwrap_or_default()
    } else {
        let adapter = CodexAdapter::new();
        serde_json::to_vec(&json!({
            "base": if krill(provider) { Some("krill".to_owned()) } else { adapter.extract_base_url(provider).ok() },
            "key": adapter.extract_auth(provider).map(|auth| auth.api_key)
        }))
        .expect("serializable provider identity")
    };
    Identity {
        provider: provider.id.clone(),
        credential: sha256_hex(&credential),
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Owner {
    identity: Identity,
    at: i64,
}

#[derive(Default)]
struct Owners {
    path: PathBuf,
    records: HashMap<String, Owner>,
}

fn owners() -> &'static Mutex<Owners> {
    static OWNERS: OnceLock<Mutex<Owners>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(Owners::default()))
}

impl Owners {
    fn load(&mut self) {
        let path = DeviceStore::for_device().file(OWNER_FILE);
        if path != self.path {
            self.records = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            self.path = path;
        }
        let now = chrono::Utc::now().timestamp();
        self.records
            .retain(|_, owner| now.saturating_sub(owner.at) < OWNER_TTL);
    }
}

pub(super) fn observe(provider: &Provider, headers: &HeaderMap, request: &Value, response: &Value) {
    if response["status"].as_str() != Some("completed")
        && response["object"].as_str() != Some("response.compaction")
    {
        return;
    }
    super::codex_capabilities::completed(provider, headers, request, response);
    let fingerprints: Vec<_> = compactions(response, "output")
        .into_iter()
        .chain(compactions(request, "input"))
        .collect();
    if fingerprints.is_empty() {
        return;
    }
    let identity = identity(provider, headers);
    let mut owners = owners().lock().expect("compaction owner lock");
    owners.load();
    let now = chrono::Utc::now().timestamp();
    let mut changed = false;
    for fp in fingerprints {
        // A successfully imported unknown item is adopted; never overwrite its maker.
        if let std::collections::hash_map::Entry::Vacant(entry) = owners.records.entry(fp) {
            entry.insert(Owner {
                identity: identity.clone(),
                at: now,
            });
            changed = true;
        }
    }
    if changed {
        if owners.records.len() > MAX_OWNERS {
            let mut oldest: Vec<_> = owners
                .records
                .iter()
                .map(|(fp, o)| (fp.clone(), o.at))
                .collect();
            oldest.sort_by_key(|(_, at)| *at);
            for (fp, _) in oldest.into_iter().take(owners.records.len() - MAX_OWNERS) {
                owners.records.remove(&fp);
            }
        }
        if let Err(error) = crate::config::write_json_file_private(&owners.path, &owners.records) {
            log::warn!("[Codex] Cannot save compaction ownership: {error}");
        }
    }
}

/// Observe completed native items without changing any response bytes or events.
pub(super) async fn observe_http(
    response: ProxyResponse,
    provider: &Provider,
    headers: &HeaderMap,
    request: &Value,
) -> Result<ProxyResponse, ProxyError> {
    if !native_compaction(provider, request["model"].as_str().unwrap_or_default()) {
        return Ok(response);
    }
    let status = response.status();
    let response_headers = response.headers().clone();
    let encoding = get_content_encoding(&response_headers);
    if response.is_json() {
        let bytes = response
            .bytes_with_limit(super::hyper_client::MAX_RESPONSE_BODY_BYTES)
            .await?;
        // Observe decoded JSON, but keep the original bytes and headers for
        // CC Switch's existing response decoding and forwarding path.
        let decoded = encoding.as_deref().and_then(|encoding| {
            decompress_body_with_limit(
                encoding,
                &bytes,
                super::hyper_client::MAX_RESPONSE_BODY_BYTES,
            )
            .ok()
            .flatten()
        });
        if let Ok(value) = serde_json::from_slice(decoded.as_deref().unwrap_or(&bytes)) {
            observe(provider, headers, request, &value);
        }
        return Ok(ProxyResponse::buffered(status, response_headers, bytes));
    }
    if encoding.is_some() {
        return Ok(response);
    }
    let provider = provider.clone();
    let headers = headers.clone();
    // Retain compaction items only, not a copy of the potentially large input transcript.
    let input = request["input"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| super::providers::codex_compaction::is_compaction_item(i))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let request = json!({"model":request["model"],"input":input});
    let stream = response.bytes_stream();
    let stream = async_stream::stream! {
        tokio::pin!(stream);
        let mut buffer = String::new();
        let mut remainder = Vec::new();
        while let Some(chunk) = stream.next().await {
            if let Ok(bytes) = &chunk {
                super::sse::append_utf8_safe(&mut buffer, &mut remainder, bytes);
                while let Some(block) = super::sse::take_sse_block(&mut buffer) {
                    if block.contains("response.completed") {
                        for line in block.lines().filter_map(|l| l.strip_prefix("data:")) {
                            if let Ok(event) = serde_json::from_str::<Value>(line.trim()) {
                                if event["type"] == "response.completed" { observe(&provider, &headers, &request, &event["response"]); }
                            }
                        }
                    }
                }
                if buffer.len() > super::hyper_client::MAX_RESPONSE_BODY_BYTES { buffer.clear(); remainder.clear(); }
            }
            yield chunk;
        }
    };
    Ok(ProxyResponse::streamed(status, response_headers, stream))
}

fn refused() -> &'static Mutex<HashMap<String, Instant>> {
    static REFUSED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    REFUSED.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn transport_key(provider: &Provider, url: &str) -> String {
    let key = CodexAdapter::new()
        .extract_auth(provider)
        .map(|auth| auth.api_key);
    sha256_hex(json!([provider.id, url, key]).to_string().as_bytes())
}

pub(super) async fn compress(provider: &Provider, url: &str, body: &[u8]) -> Option<Vec<u8>> {
    if !zstd_enabled(provider) || body.len() < 1024 {
        return None;
    }
    let key = transport_key(provider, url);
    {
        let mut memory = refused().lock().expect("zstd refusal lock");
        memory.retain(|_, at| at.elapsed() < ZSTD_TTL);
        if memory.contains_key(&key) {
            return None;
        }
    }
    let run = |bytes: &[u8]| {
        zstd::bulk::compress(bytes, 3)
            .ok()
            .filter(|z| z.len() < bytes.len())
    };
    if body.len() < 128 * 1024 {
        run(body)
    } else {
        let bytes = body.to_vec();
        tokio::task::spawn_blocking(move || run(&bytes))
            .await
            .ok()
            .flatten()
    }
}

pub(super) fn note_zstd_refused(provider: &Provider, url: &str) {
    refused()
        .lock()
        .expect("zstd refusal lock")
        .insert(transport_key(provider, url), Instant::now());
}

#[cfg(test)]
mod tests;
