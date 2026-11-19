//! DeepAI Code 模型目录：`GET /deepai_code/models`（需 Api-Key）+ 进程缓存。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! `{"models":[{id,name,reasoning,input[],contextWindow,maxTokens,premier,
//! theosPerRequest,api?}],"default":"glm-5.3-flash"}`
//!
//! `api = "openai-responses"` 的模型走 `/deepai_code/responses`，本插件只实现
//! Chat 通道，这类模型**不进入目录**（列出来也调不通，宁可少给）。
//!
//! ── 为什么有静态兜底 ────────────────────────────────────────
//! 目录接口要求 Api-Key，且新账号首次可能短暂不可用。没拉到目录时保留一份
//! 2026-11-08 实测快照，保证界面不空、可用模型仍能选。

use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::accounts::Credentials;
use super::client;
use super::endpoints;

/// 缓存 TTL（与各家同档）。
const TTL_MS: i64 = 10 * 60 * 1000;

struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: LazyLock<Mutex<Option<CatalogState>>> = LazyLock::new(|| Mutex::new(None));

/// 静态快照的一行：id、名称、是否推理、输入模态、上下文、输出上限、每次请求 Theos。
type FallbackRow = (&'static str, &'static str, bool, &'static [&'static str], i64, i64, i64);

/// 2026-11-08 实测快照（只保留 Chat 通道模型）。
fn fallback_models() -> Vec<Value> {
    const ROWS: &[FallbackRow] = &[
        ("glm-5.3-flash", "GLM 5.3 Flash", true, &["text", "image"], 1_048_576, 131_072, 0),
        ("deepseek-v4.1-flash", "DeepSeek V4.1 Flash", true, &["text", "image"], 1_048_576, 131_072, 0),
        ("mimo-2.6-flash", "MiMo 2.6 Flash", true, &["text", "image"], 1_048_576, 131_072, 0),
        ("solar-mini-4", "Solar Mini 4", true, &["text"], 524_288, 131_072, 0),
        ("gpt-6-luna", "GPT-6 Luna", true, &["text", "image"], 1_048_576, 128_000, 0),
        ("mimo-2.6-pro", "MiMo 2.6 Pro", true, &["text", "image"], 1_048_576, 131_072, 0),
        ("crouton-alpha", "Crouton Alpha", true, &["text", "image"], 1_048_576, 131_072, 0),
        ("glyph-cluster", "Glyph Cluster", true, &["text"], 256_000, 131_072, 0),
        ("deepai-cyber", "DeepAI Cyber", true, &["text"], 65_536, 8_192, 0),
        ("gpt-6.1-sol", "GPT-6.1 Sol", true, &["text", "image"], 1_048_576, 131_072, 10),
        ("gemini-4-argon", "Gemini 4 Argon", true, &["text", "image"], 1_048_576, 131_072, 10),
        ("glm-5.3", "GLM 5.3", true, &["text"], 1_048_576, 131_072, 3),
        ("kimi-k3", "Kimi K3", true, &["text", "image"], 1_048_576, 131_072, 3),
        ("muse-spark-1.3", "Muse Spark 1.3", true, &["text", "image"], 1_048_576, 131_072, 6),
        ("claude-opus-5-5", "Claude Opus 5.5", true, &["text", "image"], 1_000_000, 131_072, 15),
        ("muse-wonder", "Muse Wonder", true, &["text", "image"], 1_048_576, 131_072, 15),
        ("claude-fable-5-1", "Claude Fable 5.1", true, &["text", "image"], 1_000_000, 131_072, 25),
        ("gpt-6-astra", "GPT-6 Astra", true, &["text", "image"], 1_048_576, 131_072, 50),
    ];
    ROWS.iter()
        .map(|(id, name, reasoning, input, context, max, theos)| {
            let mut record = json!({
                "id": id,
                "name": name,
                "reasoning": reasoning,
                "input": input,
                "contextWindow": context,
                "maxTokens": max,
                "premier": *theos > 0,
                "theosPerRequest": theos.to_string(),
                "free": *theos == 0,
            });
            if let Some(label) = billing_label(Some(*theos as f64), *theos > 0) {
                record["credits"] = json!(label);
            }
            if *id == "gpt-6-astra" {
                record["api"] = json!("openai-responses");
            }
            record
        })
        .collect()
}

/// 计费文案：DeepAI 按请求扣 Theos，0 即免费。
///
/// 前端倍率列按 `x<数字>` 解析排序；Theos 是**每次请求的成本**而不是倍率，
/// 这里不写 `x` 前缀，界面原样展示文案、不参与倍率排序。
fn billing_label(theos: Option<f64>, premier: bool) -> Option<String> {
    match theos {
        Some(value) if value <= 0.0 => Some("免费".to_string()),
        Some(value) => Some(format!("{} Theos/次", trim_number(value))),
        None if !premier => Some("免费".to_string()),
        None => None,
    }
}

fn trim_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.00001 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

/// 走 Responses 通道的模型集合（目录刷新时更新）。
static RESPONSES_MODELS: LazyLock<Mutex<std::collections::BTreeSet<String>>> =
    LazyLock::new(|| Mutex::new(std::collections::BTreeSet::from(["gpt-6-astra".to_string()])));

/// 该模型是否走 `/responses` 通道。
pub fn is_responses_model(id: &str) -> bool {
    RESPONSES_MODELS
        .lock()
        .map(|set| set.contains(id))
        .unwrap_or_else(|poisoned| poisoned.into_inner().contains(id))
}

/// 把上游记录清洗成目录条目（丢弃无 id、重复的条目）。
///
/// `api = openai-responses` 的模型保留在目录里，并登记进
/// [`RESPONSES_MODELS`]，对话时据此走 `/responses` 通道。
fn normalize_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut responses = std::collections::BTreeSet::new();
    let mut models = Vec::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        if row.get("api").and_then(Value::as_str) == Some("openai-responses") {
            responses.insert(id.to_string());
        }
        let mut record = row.clone();
        let theos = record
            .get("theosPerRequest")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let premier = record.get("premier").and_then(Value::as_bool).unwrap_or(false);
        record["free"] = json!(if theos.is_empty() { !premier } else { theos == "0" || theos == "0.0" || theos == "0.00" });
        let theos_value = theos.trim().parse::<f64>().ok().filter(|value| value.is_finite());
        if let Some(label) = billing_label(theos_value, premier) {
            record["credits"] = json!(label);
        }
        models.push(record);
    }
    if !responses.is_empty() {
        responses.insert("gpt-6-astra".to_string());
        if let Ok(mut set) = RESPONSES_MODELS.lock() {
            *set = responses;
        }
    }
    models
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

/// 取目录：优先新鲜缓存，其次联网，失败回退旧缓存与静态快照。
pub async fn list(
    credentials: Option<&Credentials>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Vec<Value> {
    if let Some(models) = cached(true) {
        return models;
    }
    let Some(credentials) = credentials.filter(|value| !value.api_key.is_empty()) else {
        return cached(false).unwrap_or_else(fallback_models);
    };
    match fetch(credentials, proxy, gateway_proxy).await {
        Ok(models) => {
            store(models.clone());
            models
        }
        Err(_) => cached(false).unwrap_or_else(fallback_models),
    }
}

/// 真刷一次目录。
pub async fn refresh(
    credentials: Option<&Credentials>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> proto::RefreshOutcome {
    let Some(credentials) = credentials.filter(|value| !value.api_key.is_empty()) else {
        return proto::RefreshOutcome::failed("DeepAI 需要指定账号才能刷新模型目录");
    };
    match fetch(credentials, proxy, gateway_proxy).await {
        Ok(models) => {
            let count = models.len();
            store(models);
            proto::RefreshOutcome::refreshed(count)
        }
        Err(message) => proto::RefreshOutcome::failed(message),
    }
}

async fn fetch(
    credentials: &Credentials,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::MODELS_PATH);
    let headers = client::base_headers(credentials);
    let payload = client::get_json(&url, &headers, proxy, gateway_proxy, 20_000)
        .await
        .map_err(|error| error.message)?;
    let models = normalize_models(&payload);
    if models.is_empty() {
        return Err("DeepAI 模型目录为空".to_string());
    }
    Ok(models)
}

/// 目录元数据。
pub fn meta() -> proto::CatalogMeta {
    match STATE.lock().ok().and_then(|state| state.as_ref().map(|current| current.fetched_at)) {
        Some(fetched_at) => proto::CatalogMeta { remote: true, refreshed_at: fetched_at },
        None => proto::CatalogMeta { remote: false, refreshed_at: 0 },
    }
}

/// 静态兜底的原始清单（`ProviderMeta` 申报用）。
pub fn fallback() -> Vec<Value> {
    fallback_models()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_responses_models_and_drops_duplicates() {
        let payload = json!({ "models": [
            { "id": "a" },
            { "id": "a" },
            { "id": "b", "api": "openai-responses" },
            { "id": "", "name": "x" },
            { "id": "c", "theosPerRequest": "0" }
        ]});
        let models = normalize_models(&payload);
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(models[2]["free"], json!(true));
        assert!(is_responses_model("b"));
    }

    #[test]
    fn billing_label_matches_theos_semantics() {
        assert_eq!(billing_label(Some(0.0), false).as_deref(), Some("免费"));
        assert_eq!(billing_label(Some(15.0), true).as_deref(), Some("15 Theos/次"));
        assert_eq!(billing_label(Some(3.5), true).as_deref(), Some("3.50 Theos/次"));
        assert_eq!(billing_label(None, false).as_deref(), Some("免费"));
        assert_eq!(billing_label(None, true), None);
    }

    #[test]
    fn fallback_is_non_empty_and_marks_responses_model() {
        let models = fallback_models();
        assert!(models.len() >= 10);
        let responses: Vec<&str> = models
            .iter()
            .filter(|m| m.get("api").is_some())
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(responses, vec!["gpt-6-astra"]);
        assert!(is_responses_model("gpt-6-astra"));
        assert!(!is_responses_model("glm-5.3-flash"));
    }
}
