//! Krill speed tiers come from its native catalog, not the official account's catalog.
//! Only capability metadata is cached; credentials are used solely for the catalog request.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::codex_config::CodexCatalogToolProfile;
use crate::live::engine::{sha256_hex, DeviceStore};
use crate::provider::Provider;
use crate::proxy::providers::codex_oauth_auth::CODEX_OAUTH_CLIENT_VERSION;
use crate::proxy::providers::{resolve_codex_catalog_tool_profile, CodexAdapter, ProviderAdapter};

const FRESH_SECONDS: i64 = 6 * 60 * 60;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct SpeedTiers {
    service_tiers: Vec<Value>,
    additional_speed_tiers: Vec<String>,
}

pub(crate) type Models = BTreeMap<String, SpeedTiers>;

#[derive(Deserialize, Serialize)]
struct Cached {
    fetched_at: i64,
    models: Models,
}

fn source(provider: &Provider) -> Option<(url::Url, String, String)> {
    if !provider.name.eq_ignore_ascii_case("krill")
        || resolve_codex_catalog_tool_profile(provider) != CodexCatalogToolProfile::NativeResponses
    {
        return None;
    }
    let url = catalog_url(&CodexAdapter.extract_base_url(provider).ok()?)?;
    let auth = CodexAdapter.extract_auth(provider)?;
    if auth.api_key.trim().is_empty() {
        return None;
    }
    let fingerprint =
        sha256_hex(format!("{}\0{}\0{CODEX_OAUTH_CLIENT_VERSION}", url, auth.api_key).as_bytes());
    Some((url, auth.api_key, fingerprint))
}

fn catalog_url(base: &str) -> Option<url::Url> {
    let mut url = url::Url::parse(base).ok()?;
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/responses").unwrap_or(path);
    let path = if path.ends_with("/backend-api/codex") {
        format!("{path}/models")
    } else {
        format!(
            "{}/backend-api/codex/models",
            path.strip_suffix("/v1").unwrap_or(path)
        )
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}

fn cache_path(fingerprint: &str) -> std::path::PathBuf {
    DeviceStore::for_device().file(&format!("krill-models/{fingerprint}.json"))
}

fn read_cache(fingerprint: &str) -> Option<Cached> {
    serde_json::from_slice(&std::fs::read(cache_path(fingerprint)).ok()?).ok()
}

pub(crate) fn cached(provider: &Provider) -> Option<Models> {
    let (_, _, fingerprint) = source(provider)?;
    read_cache(&fingerprint).map(|entry| entry.models)
}

/// Called before the client-file write lock. A failed refresh retains only this
/// endpoint/key's previous metadata; another account's tiers are never substituted.
pub(crate) async fn rows(provider: &Provider) -> Option<Models> {
    let (url, key, fingerprint) = source(provider)?;
    let cached = read_cache(&fingerprint);
    let now = chrono::Utc::now().timestamp();
    if let Some(entry) = &cached {
        if (0..FRESH_SECONDS).contains(&(now - entry.fetched_at)) {
            return Some(entry.models.clone());
        }
    }
    match fetch(url, &key).await {
        Ok(models) => {
            let entry = Cached {
                fetched_at: now,
                models: models.clone(),
            };
            if let Err(error) = crate::config::write_json_file(&cache_path(&fingerprint), &entry) {
                log::warn!("Could not cache Krill model speed tiers: {error}");
            }
            Some(models)
        }
        Err(error) => {
            log::warn!("Could not refresh Krill model speed tiers: {error}");
            cached.map(|entry| entry.models)
        }
    }
}

async fn fetch(url: url::Url, key: &str) -> Result<Models, &'static str> {
    let mut response = crate::proxy::http_client::get_for_url(url.as_str())
        .get(url)
        .bearer_auth(key)
        .query(&[("client_version", CODEX_OAUTH_CLIENT_VERSION)])
        .header("originator", "codex_cli_rs")
        .header("version", CODEX_OAUTH_CLIENT_VERSION)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| "catalog request failed")?
        .error_for_status()
        .map_err(|_| "catalog request rejected")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "catalog read failed")? {
        if bytes.len() + chunk.len() > MAX_CATALOG_BYTES {
            return Err("catalog exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice(&bytes).map_err(|_| "invalid catalog JSON")?;
    parse(&body).ok_or("native model catalog missing")
}

fn parse(body: &Value) -> Option<Models> {
    let rows = body.get("models")?.as_array()?;
    let mut models = Models::new();
    for row in rows {
        let Some(model) = row.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let service_tiers = row
            .get("service_tiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|tier| tier.get("id").and_then(Value::as_str).is_some())
            .cloned()
            .collect();
        let additional_speed_tiers = row
            .get("additional_speed_tiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        models.insert(
            model.to_owned(),
            SpeedTiers {
                service_tiers,
                additional_speed_tiers,
            },
        );
    }
    Some(models)
}

/// Overlay speed metadata only, after CCS has generated tools, history markers,
/// windows and namespaced model names. Exact matches prevent alias inheritance.
pub(crate) fn apply(catalog: &mut Value, models: &Models, stack_key: Option<&str>) {
    let Some(entries) = catalog.get_mut("models").and_then(Value::as_array_mut) else {
        return;
    };
    let prefix = stack_key.map(|key| format!("ccs-{key}/"));
    for entry in entries {
        let Some(slug) = entry.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let model = match &prefix {
            Some(prefix) => slug.strip_prefix(prefix),
            None => Some(slug),
        };
        let Some(tiers) = model.and_then(|model| models.get(model)) else {
            continue;
        };
        entry["service_tiers"] = json!(tiers.service_tiers);
        entry["additional_speed_tiers"] = json!(tiers.additional_speed_tiers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, base: &str, key: &str) -> Provider {
        let mut provider = Provider::with_id(
            "speed-test".into(),
            name.into(),
            json!({"base_url": base, "auth": {"OPENAI_API_KEY": key}}),
            None,
        );
        provider.meta = Some(crate::provider::ProviderMeta {
            api_format: Some("openai_responses".into()),
            ..Default::default()
        });
        provider
    }

    #[test]
    fn native_catalog_urls_preserve_gateway_prefixes() {
        for (base, path) in [
            ("https://example.test", "/backend-api/codex/models"),
            ("https://example.test/v1/", "/backend-api/codex/models"),
            (
                "https://example.test/v1/responses",
                "/backend-api/codex/models",
            ),
            (
                "https://example.test/backend-api/codex",
                "/backend-api/codex/models",
            ),
            (
                "https://example.test/gateway/v1",
                "/gateway/backend-api/codex/models",
            ),
        ] {
            assert_eq!(catalog_url(base).unwrap().path(), path);
        }
    }

    #[test]
    fn only_native_krill_uses_endpoint_and_credential_scoped_tiers() {
        let first = provider("Krill", "https://example.test", "first-key");
        let fingerprint = source(&first).unwrap().2;
        assert_ne!(
            fingerprint,
            source(&provider("KRILL", "https://example.test", "second-key"))
                .unwrap()
                .2
        );
        assert_ne!(
            fingerprint,
            source(&provider("krill", "https://other.test", "first-key"))
                .unwrap()
                .2
        );
        assert!(source(&provider(
            "krill-backup",
            "https://example.test",
            "first-key"
        ))
        .is_none());
        let mut converted = first;
        converted.meta.as_mut().unwrap().api_format = Some("openai_chat".into());
        assert!(source(&converted).is_none());
    }

    #[test]
    fn speed_tiers_follow_exact_upstream_models_without_changing_native_context() {
        let models = parse(&json!({"models": [
            {"slug":"gpt-6-astra", "service_tiers":[{"id":"priority","name":"Fast"},{"id":"ultrafast","name":"Ultrafast"}], "additional_speed_tiers":["fast"]},
            {"slug":"gpt-6.1-terra", "service_tiers":[{"id":"priority","name":"Fast"}]},
            {"slug":"standard-only", "service_tiers":[]}
        ]})).unwrap();
        let mut catalog = json!({"models": [
            {"slug":"gpt-6-astra", "service_tiers":[{"id":"official-only"}]},
            {"slug":"ccs-krill/gpt-6-astra", "service_tiers":[], "comp_hash":"native", "tool_mode":"native"},
            {"slug":"ccs-krill/gpt-6.1-terra", "service_tiers":[]},
            {"slug":"ccs-krill/gpt-6-astra-alias", "service_tiers":[]},
            {"slug":"ccs-other/gpt-6-astra", "service_tiers":[]},
            {"slug":"ccs-krill/standard-only", "service_tiers":[{"id":"ultrafast"}]}
        ]});
        apply(&mut catalog, &models, Some("krill"));
        let rows = catalog["models"].as_array().unwrap();
        assert_eq!(rows[0]["service_tiers"][0]["id"], "official-only");
        assert_eq!(rows[1]["service_tiers"][1]["id"], "ultrafast");
        assert_eq!(rows[1]["additional_speed_tiers"], json!(["fast"]));
        assert_eq!(rows[1]["comp_hash"], "native");
        assert_eq!(rows[1]["tool_mode"], "native");
        assert_eq!(rows[2]["service_tiers"].as_array().unwrap().len(), 1);
        for row in &rows[3..] {
            assert_eq!(row["service_tiers"], json!([]));
        }
        assert!(parse(&json!({"data":[{"id":"gpt-6-astra"}]})).is_none());
        apply(&mut catalog, &models, None);
        assert_eq!(catalog["models"][0]["service_tiers"][1]["id"], "ultrafast");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_catalog_is_cached_and_published_by_the_real_stack_planner() {
        use crate::services::provider::codex_direct::{self, Owner, Prepared, Target};
        use axum::{routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };

        assert!(std::env::var_os("CC_SWITCH_TEST_HOME").is_some());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let reject = Arc::new(AtomicBool::new(false));
        let count = calls.clone();
        let refuse = reject.clone();
        let router = Router::new().route("/backend-api/codex/models", get(move |headers: axum::http::HeaderMap| {
            let calls = count.clone();
            let reject = refuse.clone();
            async move {
                calls.fetch_add(1, Ordering::Relaxed);
                assert_eq!(headers["originator"], "codex_cli_rs");
                if reject.load(Ordering::Relaxed) {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
                }
                assert_eq!(headers["authorization"], "Bearer catalog-key");
                (axum::http::StatusCode::OK, Json(json!({"models":[
                    {"slug":"gpt-6-astra", "service_tiers":[{"id":"priority","name":"Fast"},{"id":"ultrafast","name":"Ultrafast"}], "additional_speed_tiers":["fast"]},
                    {"slug":"gpt-6.1-terra", "service_tiers":[{"id":"priority","name":"Fast"}]}
                ]})))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut krill = provider("Krill", &base, "catalog-key");
        krill.settings_config["config"] = json!(format!("model = \"gpt-6-astra\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"{base}\"\nwire_api = \"responses\"\n"));
        krill.settings_config["modelCatalog"] =
            json!({"models":[{"model":"gpt-6-astra"},{"model":"gpt-6.1-terra"}]});
        let fetched = rows(&krill).await.unwrap();
        assert_eq!(rows(&krill).await.unwrap(), fetched);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let mut route = krill.clone();
        route.id = "unrelated".into();
        route.name = "Other".into();
        let db = crate::database::Database::memory().unwrap();
        db.save_provider("codex", &route).unwrap();
        db.save_provider("codex", &krill).unwrap();
        let members = [crate::mode::stack::Member {
            provider: krill.clone(),
            key: "krill".into(),
            route: false,
            model_ids: vec![],
        }];
        let target = Target::Proxy {
            route: &route,
            base_url: "http://127.0.0.1:15721/v1",
            stack: &members,
        };
        let mut prepared = Prepared::default();
        codex_direct::prepare_model_rows(&db, &Owner::None, &target, &mut prepared)
            .await
            .unwrap();
        let catalog = codex_direct::plan(&db, &Owner::None, &target, &prepared)
            .unwrap()
            .catalog_for_test();
        let entries = catalog["models"].as_array().unwrap();
        let astra = entries
            .iter()
            .find(|row| row["slug"] == "ccs-krill/gpt-6-astra")
            .unwrap();
        assert_eq!(
            astra["service_tiers"],
            json!(fetched["gpt-6-astra"].service_tiers)
        );
        let terra = entries
            .iter()
            .find(|row| row["slug"] == "ccs-krill/gpt-6.1-terra")
            .unwrap();
        assert_eq!(terra["service_tiers"].as_array().unwrap().len(), 1);
        let other = entries
            .iter()
            .find(|row| row["slug"] == "gpt-6-astra")
            .unwrap();
        assert_eq!(other["service_tiers"], json!([]));

        let fingerprint = source(&krill).unwrap().2;
        let stale = Cached {
            fetched_at: chrono::Utc::now().timestamp() - FRESH_SECONDS - 1,
            models: fetched.clone(),
        };
        crate::config::write_json_file(&cache_path(&fingerprint), &stale).unwrap();
        reject.store(true, Ordering::Relaxed);
        assert_eq!(rows(&krill).await.unwrap(), fetched);
        assert!(rows(&provider("Krill", &base, "another-account"))
            .await
            .is_none());
        server.abort();
    }
}
