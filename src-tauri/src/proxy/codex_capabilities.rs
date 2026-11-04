//! Recent, device-local capability evidence for the existing provider chips.
//! Evidence comes only from normal routed traffic. Neither model use nor UI
//! reads send additional requests for capability detection.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Emitter;

use super::{codex_native, providers, server::ProxyState};
use crate::{
    live::engine::{sha256_hex, DeviceStore},
    provider::Provider,
};

const FILE: &str = "codex-capabilities.json";
const DAY: i64 = 24 * 60 * 60;
const WS_FRESH: i64 = 30 * 60;
const RETRY: i64 = 15 * 60;
const MAX_ENTRIES: usize = 256;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub available: bool,
    pub checked_at: i64,
    pub last_success_at: Option<i64>,
    pub detail: String,
    pub definitive: bool,
    #[serde(default)]
    failures: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub model: String,
    pub ws: Option<Evidence>,
    pub cmp: Option<Evidence>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    provider_id: String,
    config: String,
    account: String,
    at: i64,
    used_at: i64,
    capabilities: Capabilities,
}

#[derive(Default)]
struct Cache {
    path: PathBuf,
    records: HashMap<String, Entry>,
    saved_at: i64,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

impl Cache {
    fn load(&mut self) {
        let path = DeviceStore::for_device().file(FILE);
        if self.path != path {
            self.records = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            self.path = path;
        }
        let now = chrono::Utc::now().timestamp();
        self.records
            .retain(|_, entry| now.saturating_sub(entry.at) < 7 * DAY);
    }

    fn save(&mut self, changed: bool) {
        let now = chrono::Utc::now().timestamp();
        if !changed && now.saturating_sub(self.saved_at) < 60 {
            return;
        }
        if self.records.len() > MAX_ENTRIES {
            let mut oldest: Vec<_> = self
                .records
                .iter()
                .map(|(key, entry)| (key.clone(), entry.at))
                .collect();
            oldest.sort_by_key(|(_, at)| *at);
            for (key, _) in oldest.into_iter().take(self.records.len() - MAX_ENTRIES) {
                self.records.remove(&key);
            }
        }
        if let Err(error) = crate::config::write_json_file_private(&self.path, &self.records) {
            log::warn!("[Codex] Cannot save capability evidence: {error}");
        }
        self.saved_at = now;
    }
}

fn config(provider: &Provider) -> String {
    sha256_hex(
        json!([provider.settings_config, provider.category, provider.meta])
            .to_string()
            .as_bytes(),
    )
}

fn account(provider: &Provider, headers: &HeaderMap) -> String {
    if providers::is_codex_official_provider(provider) {
        sha256_hex(
            headers
                .get("chatgpt-account-id")
                .map_or(&[][..], |value| value.as_bytes()),
        )
    } else {
        String::new()
    }
}

fn key(provider: &Provider, headers: &HeaderMap, model: &str) -> String {
    sha256_hex(
        json!([
            provider.id,
            config(provider),
            account(provider, headers),
            model
        ])
        .to_string()
        .as_bytes(),
    )
}

fn entry<'a>(
    cache: &'a mut Cache,
    provider: &Provider,
    headers: &HeaderMap,
    model: &str,
) -> &'a mut Entry {
    let value = cache
        .records
        .entry(key(provider, headers, model))
        .or_insert_with(|| Entry {
            provider_id: provider.id.clone(),
            config: config(provider),
            account: account(provider, headers),
            capabilities: Capabilities {
                model: model.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        });
    value.at = chrono::Utc::now().timestamp();
    value
}

pub fn snapshot<'a>(
    providers: impl Iterator<Item = &'a Provider>,
) -> HashMap<String, Capabilities> {
    let auth: Value = std::fs::read(crate::codex_config::get_codex_auth_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let current_account = sha256_hex(
        auth.pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes(),
    );
    let mut cache = cache().lock().expect("capability cache");
    cache.load();
    let now = chrono::Utc::now().timestamp();
    providers
        .map(|provider| {
            let fingerprint = config(provider);
            let mut result = cache
                .records
                .values()
                .filter(|entry| {
                    entry.provider_id == provider.id
                        && entry.config == fingerprint
                        && (!providers::is_codex_official_provider(provider)
                            || entry.account == current_account)
                })
                .max_by_key(|entry| entry.used_at)
                .map(|entry| entry.capabilities.clone())
                .unwrap_or_default();
            let fresh = |e: &Evidence, ttl| {
                now.saturating_sub(if e.available {
                    e.last_success_at.unwrap_or(e.checked_at)
                } else {
                    e.checked_at
                }) < ttl
            };
            result.ws = result.ws.filter(|e| fresh(e, WS_FRESH));
            result.cmp = result.cmp.filter(|e| fresh(e, DAY));
            (provider.id.clone(), result)
        })
        .collect()
}

fn notify(state: &ProxyState) {
    if let Some(app) = &state.app_handle {
        let _ = app.emit("codex-capabilities-changed", ());
    }
}

fn update(
    provider: &Provider,
    headers: &HeaderMap,
    model: &str,
    ws: bool,
    available: bool,
    definite: bool,
    detail: &str,
) -> bool {
    let now = chrono::Utc::now().timestamp();
    let mut cache = cache().lock().expect("capability cache");
    cache.load();
    let record = entry(&mut cache, provider, headers, model);
    let slot = if ws {
        &mut record.capabilities.ws
    } else {
        &mut record.capabilities.cmp
    };
    let previous = slot.clone().unwrap_or_default();
    let failures = if available {
        0
    } else if now.saturating_sub(previous.checked_at) >= 60 {
        previous.failures.saturating_add(1)
    } else {
        previous.failures.max(1)
    };
    let recent_success = previous
        .last_success_at
        .is_some_and(|at| now.saturating_sub(at) < RETRY);
    let available = available || (!definite && recent_success && failures < 2);
    let changed = previous.available != available || previous.detail != detail;
    *slot = Some(Evidence {
        available,
        checked_at: now,
        last_success_at: if failures == 0 {
            Some(now)
        } else {
            previous.last_success_at
        },
        detail: detail.to_owned(),
        definitive: definite,
        failures,
    });
    cache.save(changed);
    changed
}

pub(super) fn ws(
    state: &ProxyState,
    provider: &Provider,
    headers: &HeaderMap,
    model: &str,
    available: bool,
    definite: bool,
) {
    {
        let mut cache = cache().lock().expect("capability cache");
        cache.load();
        entry(&mut cache, provider, headers, model).used_at = chrono::Utc::now().timestamp_millis();
    }
    let detail = if available {
        "WebSocket verified on this route"
    } else if definite {
        "This route requires HTTP"
    } else {
        "Temporary connection failure; recent success is retained briefly"
    };
    if update(provider, headers, model, true, available, definite, detail) {
        notify(state);
    }
}

pub(super) fn completed(
    provider: &Provider,
    headers: &HeaderMap,
    request: &Value,
    response: &Value,
) {
    let model = request["model"].as_str().unwrap_or_default();
    let native = response["output"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            providers::codex_compaction::is_compaction_item(item)
                && item["encrypted_content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty() && !text.starts_with("ccswitch-"))
        })
    });
    if native && !model.is_empty() {
        update(
            provider,
            headers,
            model,
            false,
            true,
            true,
            "Native compaction and encrypted output verified",
        );
    }
}

pub(super) fn failed(
    state: &ProxyState,
    provider: &Provider,
    headers: &HeaderMap,
    model: &str,
    compaction: bool,
    error: &super::ProxyError,
) {
    let super::ProxyError::UpstreamError { status, body } = error else {
        return;
    };
    {
        let mut cache = cache().lock().expect("capability cache");
        cache.load();
        entry(&mut cache, provider, headers, model).used_at = chrono::Utc::now().timestamp_millis();
    }
    if matches!(status, 401 | 403) {
        ws(state, provider, headers, model, false, true);
        if update(
            provider,
            headers,
            model,
            false,
            false,
            true,
            "Authentication was refused on this route",
        ) {
            notify(state);
        }
    } else if compaction && *status == 400 {
        let error = body.as_deref().unwrap_or_default().to_ascii_lowercase();
        if error.contains("compaction")
            && (error.contains("unsupported")
                || error.contains("not supported")
                || error.contains("unknown"))
            && update(
                provider,
                headers,
                model,
                false,
                false,
                true,
                "Native compaction was explicitly refused on this route",
            )
        {
            notify(state);
        }
    }
}

/// Record the selected model and local configuration only. Availability is
/// observed from the user's normal requests; no additional request is sent.
pub(super) fn on_use(state: &ProxyState, provider: &Provider, headers: &HeaderMap, model: &str) {
    if !model.starts_with("gpt-")
        || !crate::mode::stack::stack_mode_now(&crate::app_config::AppType::Codex)
    {
        return;
    }
    {
        let mut cache = cache().lock().expect("capability cache");
        cache.load();
        entry(&mut cache, provider, headers, model).used_at = chrono::Utc::now().timestamp_millis();
    }
    if !codex_native::native_compaction(provider, model)
        && update(
            provider,
            headers,
            model,
            false,
            false,
            true,
            "Native compaction is disabled or this route uses a summary bridge",
        )
    {
        notify(state);
    }
}
