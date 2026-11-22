//! 科搭模型目录：`GET /v1/codar/models`（精选目录，带协议与请求 schema）+ 缓存。
//!
//! 原生插件支持三种协议：`chat`（OpenAI Chat 兼容）、`anthropic`（Messages）与
//! `responses`（OpenAI Responses）。目录里协议的取值统一归一成这三个词，
//! 对话时据此选择请求构造与响应转换。

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::client;
use super::endpoints;

const TTL_MS: i64 = 10 * 60 * 1000;

struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: LazyLock<Mutex<Option<CatalogState>>> = LazyLock::new(|| Mutex::new(None));

/// 模型 id → 协议。
static MODEL_META: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(fallback_meta()));

/// 2026-11-19 实时目录快照。
///
/// `(id, 上下文, 输出上限, 支持图片, 协议, 倍率文案, 输入价, 输出价, 缓存输入价)`；
/// 价格单位为人民币元 / 百万 tokens（上游 `pricing.unit = per_million_tokens`）。
type Row = (
    &'static str,
    i64,
    i64,
    bool,
    &'static str,
    &'static str,
    f64,
    f64,
    f64,
);

fn rows() -> &'static [Row] {
    &[
        ("auto", 1_000_000, 393_216, true, "chat", "0.20x", 2.0, 8.0, 0.04),
        ("deepseek-v4.1-flash", 1_000_000, 393_216, true, "chat", "0.20x", 2.0, 8.0, 0.04),
        ("deepseek-v4.1-flash-economy", 1_000_000, 393_216, true, "chat", "0.12x", 1.2, 4.8, 0.024),
        ("deepseek-v4-pro", 1_000_000, 393_216, false, "chat", "0.95x", 9.0, 27.0, 0.3),
        ("deepseek-v4-flash", 1_000_000, 393_216, true, "chat", "0.20x", 2.0, 8.0, 0.04),
        ("gemini-4-argon", 1_048_576, 65_536, true, "chat", "0.55x", 4.0, 20.0, 0.2),
        ("gemini-4-argon-official", 1_048_576, 65_536, true, "chat", "3.28x", 24.0, 120.0, 1.2),
        ("gpt-5.6-sol", 1_050_000, 128_000, true, "chat", "0.77x", 4.0, 24.0, 0.4),
        ("gpt-6-astra", 1_050_000, 128_000, true, "chat", "1.48x", 8.0, 40.0, 0.8),
        ("gpt-6.1-sol", 1_050_000, 128_000, true, "chat", "0.30x", 1.6, 8.0, 0.16),
        ("gpt-6.2-astra", 1_050_000, 128_000, true, "chat", "0.87x", 6.4, 32.0, 0.32),
        ("gpt-6.2-sol", 1_050_000, 128_000, true, "chat", "0.30x", 1.6, 8.0, 0.16),
        ("gpt-6-sol", 1_050_000, 128_000, true, "chat", "0.30x", 1.6, 8.0, 0.16),
        ("glm-5.2", 1_000_000, 131_072, false, "chat", "2.57x", 8.0, 28.0, 2.0),
        ("glm-5.3", 1_000_000, 131_072, false, "chat", "2.57x", 8.0, 28.0, 2.0),
        ("glm-5.3-flash", 1_000_000, 131_072, true, "chat", "0.29x", 0.8, 2.8, 0.23),
        ("claude-opus-5-5", 1_000_000, 128_000, true, "anthropic", "1.37x", 10.0, 50.0, 0.5),
        ("claude-opus-5-5-kiro", 1_000_000, 128_000, true, "anthropic", "0.55x", 4.0, 20.0, 0.2),
        ("claude-sonnet-5-5", 1_000_000, 128_000, true, "anthropic", "0.93x", 5.0, 25.0, 0.5),
        ("claude-sonnet-5-5-kiro", 1_000_000, 128_000, true, "anthropic", "0.37x", 2.0, 10.0, 0.2),
    ]
}

fn fallback_meta() -> HashMap<String, String> {
    rows()
        .iter()
        .map(|(id, _, _, _, protocol, ..)| (id.to_string(), protocol.to_string()))
        .collect()
}

fn fallback_models() -> Vec<Value> {
    rows()
        .iter()
        .map(|(id, context, output, images, protocol, rate, input, out, cached)| {
            json!({
                "id": id,
                "name": id,
                "kind": "chat",
                "protocol": protocol,
                "supportsToolCall": true,
                "supportsReasoning": true,
                "supportsImages": images,
                "maxInputTokens": context,
                "maxOutputTokens": output,
                "isDefault": *id == "auto",
                "credits": rate,
                "pricing": {
                    "currency": "CNY",
                    "unit": "per_million_tokens",
                    "input": input,
                    "output": out,
                    "cached_input": cached,
                },
            })
        })
        .collect()
}

fn protocol_of(row: &Value) -> &str {
    match row["protocol"]["type"]
        .as_str()
        .or_else(|| row["protocol"].as_str())
        .unwrap_or("")
    {
        "anthropic_messages" | "anthropic" => "anthropic",
        "openai_responses" | "responses" => "responses",
        _ => "chat",
    }
}

fn normalize(payload: &Value) -> Vec<Value> {
    let rows = payload["catalog"]["data"]
        .as_array()
        .or_else(|| payload["data"].as_array())
        .filter(|rows| rows.len() <= 2000)
        .cloned()
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    let mut meta = fallback_meta();
    for row in rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 256)
        else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let protocol = protocol_of(&row);
        meta.insert(id.to_string(), protocol.to_string());
        let name = ["display_name", "name"]
            .iter()
            .find_map(|key| row.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(id);
        let mut model = json!({
            "id": id,
            "name": name,
            "kind": "chat",
            "protocol": protocol,
            "isDefault": id == "auto",
        });
        if let Some(value) = row["capabilities"]["function_tools"].as_bool() {
            model["supportsToolCall"] = json!(value);
        }
        if let Some(value) = row["capabilities"]["streaming"].as_bool() {
            model["streaming"] = json!(value);
        }
        if let Some(modalities) = row["input_modalities"].as_array() {
            model["supportsImages"] = json!(modalities.iter().any(|kind| kind.as_str() == Some("image")));
        }
        if let Some(context) = amount(&row["context_window"]).filter(|value| *value > 0.0) {
            model["maxInputTokens"] = json!(context as u64);
        }
        let props = &row["request_schema"]["properties"];
        if let Some(output) = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
            .iter()
            .find_map(|key| amount(&props[*key]["maximum"]).filter(|value| *value > 0.0))
        {
            model["maxOutputTokens"] = json!(output as u64);
        }
        // 计费：优先用上游的 `rate_badge`（如 `0.20x`），其次显式的
        // `credits` / `rate` / `price` 文案；`pricing` 原样保留供明细展示。
        // 上游没给就不写，不按字段名猜单位、也不编造价格。
        let label = row
            .get("rate_badge")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 64)
            .map(str::to_string)
            .or_else(|| {
                ["credits", "rate", "price"].iter().find_map(|key| {
                    let label = row.get(*key)?;
                    label
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| label.as_f64().map(|value| value.to_string()))
                })
            });
        if let Some(text) = label.filter(|value| !value.trim().is_empty() && value.len() <= 64) {
            model["credits"] = json!(text);
        }
        if let Some(pricing) = row.get("pricing").filter(|value| value.is_object()) {
            model["pricing"] = pricing.clone();
        }
        models.push(model);
    }
    if let Ok(mut current) = MODEL_META.lock() {
        *current = meta;
    }
    models
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value| value.is_finite())
}

fn cached(fresh_only: bool) -> Option<Vec<Value>> {
    let state = STATE.lock().ok()?;
    let current = state.as_ref()?;
    if fresh_only && crate::http::now_ms() - current.fetched_at > TTL_MS {
        return None;
    }
    Some(current.models.clone())
}

fn store(models: Vec<Value>) {
    if let Ok(mut state) = STATE.lock() {
        *state = Some(CatalogState { models, fetched_at: crate::http::now_ms() });
    }
}

pub async fn list(
    token: Option<&str>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Vec<Value> {
    if let Some(models) = cached(true) {
        return models;
    }
    let Some(token) = token.filter(|token| !token.is_empty()) else {
        return cached(false).unwrap_or_else(fallback_models);
    };
    match fetch(token, proxy, gateway_proxy).await {
        Ok(models) => {
            store(models.clone());
            models
        }
        Err(_) => cached(false).unwrap_or_else(fallback_models),
    }
}

pub async fn refresh(
    token: Option<&str>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> proto::RefreshOutcome {
    let Some(token) = token.filter(|token| !token.is_empty()) else {
        return proto::RefreshOutcome::failed("科搭需要指定账号才能刷新模型目录");
    };
    match fetch(token, proxy, gateway_proxy).await {
        Ok(models) => {
            let count = models.len();
            store(models);
            proto::RefreshOutcome::refreshed(count)
        }
        Err(message) => proto::RefreshOutcome::failed(message),
    }
}

async fn fetch(
    token: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Vec<Value>, String> {
    let payload = client::checked(
        "GET",
        endpoints::API_BASE,
        endpoints::CATALOG,
        token,
        None,
        proxy,
        gateway_proxy,
        12_000,
    )
    .await
    .map_err(|error| error.message)?;
    let models = normalize(&payload);
    if models.is_empty() {
        return Err("科搭模型目录为空".to_string());
    }
    Ok(models)
}

pub fn meta() -> proto::CatalogMeta {
    match STATE.lock().ok().and_then(|state| state.as_ref().map(|current| current.fetched_at)) {
        Some(fetched_at) => proto::CatalogMeta { remote: true, refreshed_at: fetched_at },
        None => proto::CatalogMeta { remote: false, refreshed_at: 0 },
    }
}

pub fn fallback() -> Vec<Value> {
    fallback_models()
}

/// 模型的协议（`chat` / `anthropic` / `responses`）。
pub fn protocol_of_model(id: &str) -> String {
    MODEL_META
        .lock()
        .ok()
        .and_then(|meta| meta.get(id).cloned())
        .unwrap_or_else(|| "chat".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_all_protocols() {
        let payload = json!({ "data": [
            { "id": "a", "protocol": { "type": "openai_chat_completions" } },
            { "id": "claude-x", "protocol": { "type": "anthropic_messages" } },
            { "id": "r", "protocol": { "type": "openai_responses" } },
            { "id": "a" }
        ]});
        let models = normalize(&payload);
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["a", "claude-x", "r"]);
        assert_eq!(protocol_of_model("claude-x"), "anthropic");
        assert_eq!(protocol_of_model("r"), "responses");
    }

    #[test]
    fn fallback_marks_anthropic_models() {
        let models = fallback_models();
        assert!(models.iter().any(|m| m["protocol"] == "anthropic"));
    }
}
