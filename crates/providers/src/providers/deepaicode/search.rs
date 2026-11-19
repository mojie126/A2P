//! DeepAI Code 的联网搜索：`POST /deepai_code/web_search`。
//!
//! 请求体按参考实现为 `{query, num_results}`；上游结果字段没有稳定文档，
//! 这里按常见形状宽容解析（`results` / `sources` / `data` / `items`），
//! 只收带合法网页地址的条目，读不出结果时如实报「无法识别」。

use std::time::Duration;

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::{client, endpoints, PROVIDER_ID};
use crate::native_search;
use crate::errors::GatewayError;

const MAX_QUERY_CHARS: usize = 200;
const MAX_RESULTS: u64 = 10;

pub(super) async fn dispatch(
    ctx: &PluginCtx,
    account: &str,
    action: &str,
    payload: &Value,
) -> Result<Value, String> {
    if action == native_search::CAPABILITIES {
        return Ok(native_search::capabilities(MAX_QUERY_CHARS));
    }
    let (query, timeout) = native_search::input(payload, MAX_QUERY_CHARS)
        .map_err(GatewayError::into_control_message)?;
    let key = format!("search:{PROVIDER_ID}:{account}");
    tokio::time::timeout(timeout, crate::flight::run(&key, || async {
        let ctx = native_search::account_context(ctx, PROVIDER_ID, account).await?;
        search_once(&ctx, account, &query, timeout).await.map_err(GatewayError::into_control_message)
    }))
    .await
    .unwrap_or_else(|_| Err(native_search::timed_out().into_control_message()))
}

async fn search_once(
    ctx: &PluginCtx,
    account: &str,
    query: &str,
    timeout: Duration,
) -> Result<Value, GatewayError> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|_| native_search::rejected(401))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.api_key.is_empty() {
        return Err(native_search::rejected(401));
    }
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::WEB_SEARCH_PATH);
    let headers = client::base_headers(&credentials);
    let body = json!({ "query": query, "num_results": MAX_RESULTS });
    let proxy = client::account_proxy(ctx, account).await;
    let response = crate::http::control_client(proxy.as_ref(), &ctx.env.proxy)
        .post(&url)
        .timeout(timeout)
        .headers(header_map(&headers))
        .json(&body)
        .send()
        .await
        .map_err(native_search::transport)?;
    let (status, value) = native_search::read_response(response).await?;
    if !(200..300).contains(&status) {
        return Err(native_search::rejected(status));
    }
    parse_result(&value)
}

fn header_map(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// 宽容解析上游结果：只取带合法网页地址的条目。
fn parse_result(value: &Value) -> Result<Value, GatewayError> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(native_search::invalid_response());
    }
    let items = ["results", "sources", "data", "items", "web_results"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .ok_or_else(native_search::invalid_response)?;
    let normalized: Vec<Value> = items
        .iter()
        .take(50)
        .filter_map(|item| {
            let raw_url = ["url", "link", "href"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))?;
            let url = url::Url::parse(raw_url.trim()).ok()?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return None;
            }
            let title: String = ["title", "name"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .unwrap_or_else(|| url.as_str())
                .trim()
                .chars()
                .take(300)
                .collect();
            let content: String = ["snippet", "content", "description", "text"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .unwrap_or("")
                .trim()
                .chars()
                .take(2000)
                .collect();
            Some(json!({ "url": url.as_str(), "title": title, "content": content }))
        })
        .collect();
    if normalized.is_empty() {
        return Err(native_search::invalid_response());
    }
    Ok(json!({ "sources": normalized }))
}
