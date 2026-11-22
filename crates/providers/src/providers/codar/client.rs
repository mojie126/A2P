//! 科搭的出站：请求头、JSON 请求与错误分类。
//!
//! 鉴权统一 `Authorization: Bearer <JWT>`；业务接口与管理接口分两个域名。
//! 上游常见形态是 `{code, message, data}` 信封，`code` 非 200 也代表业务失败。

use serde_json::Value;

use super::endpoints;
use crate::errors::GatewayError;
use crate::http;

/// 公共请求头。
pub fn headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), endpoints::USER_AGENT.to_string()),
    ];
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

/// 解包 `data`（对象 / 数组 / 布尔），否则原样返回。
pub fn data(body: &Value) -> &Value {
    let data = &body["data"];
    if data.is_object() || data.is_array() || data.is_boolean() {
        data
    } else {
        body
    }
}

/// 上游错误码。
pub fn code(value: &Value) -> String {
    let objects = [value, &value["detail"], &value["error"], &value["response"]["error"]];
    for object in objects {
        for key in ["providerCode", "code"] {
            if let Some(text) = object.get(key).and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
        }
    }
    for object in objects {
        if let Some(kind) = object.get("type").and_then(Value::as_str) {
            if !kind.is_empty() && !matches!(kind, "error" | "response.failed") {
                return kind.to_string();
            }
        }
    }
    String::new()
}

/// 上游可展示的消息。
pub fn message(value: &Value) -> String {
    for object in [&value["error"], &value["detail"], value, &value["data"]] {
        for key in ["message", "msg", "error", "detail"] {
            if let Some(text) = object.get(key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
    }
    if let Some(text) = value["detail"].as_str() {
        return text.to_string();
    }
    "上游请求失败".to_string()
}

/// 给「登录凭据仅限科搭客户端」的上游拒绝补上可执行的指引。
///
/// 上游原文是 `This login credential is restricted to Codar. Use an API key
/// for other clients.`（HTTP 403）：登录令牌只能用于科搭官方客户端，
/// 第三方客户端的对话请求要在账号凭据 JSON 里补充 `apiKey`。
pub fn with_credential_hint(message: String) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("use an api key") || lower.contains("restricted to") {
        return format!(
            "{message}（登录凭据仅限科搭官方客户端使用，请在账号凭据中补充 API Key 后重新导入）"
        );
    }
    message
}

/// 业务失败判定。
pub fn failed(value: &Value) -> bool {
    value["success"] == Value::Bool(false)
        || matches!(value["type"].as_str(), Some("error" | "response.failed"))
        || value.get("error").is_some_and(|error| !error.is_null())
        || value["response"]
            .get("error")
            .is_some_and(|error| !error.is_null())
        || numeric_code(value).is_some_and(|code| code != 200.0)
}

fn numeric_code(value: &Value) -> Option<f64> {
    value.get("code").and_then(|code| {
        code.as_f64()
            .or_else(|| code.as_str().and_then(|text| text.trim().parse().ok()))
    })
}

/// 错误类别（含凭证、限额、内容审核与其它）。
pub fn kind(status: u16, value: &Value) -> &'static str {
    let code = code(value);
    let lower = format!("{code} {}", message(value)).to_ascii_lowercase();
    let contains = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    if matches!(
        code.as_str(),
        "account_restricted" | "client_not_supported" | "codar_account_changed" | "codar_identity_mismatch"
    ) {
        "fatal"
    } else if status == 401
        || contains(&[
            "invalid_token",
            "invalid token",
            "token expired",
            "token_expired",
            "authentication_error",
            "codar_token_expired",
        ])
    {
        "token_expired"
    } else if status == 402
        || status == 429
        || contains(&[
            "quota_exceeded",
            "insufficient_quota",
            "insufficient balance",
            "余额不足",
            "积分不足",
            "rate_limit",
            "concurrent_limit",
        ])
    {
        "quota_limited"
    } else if contains(&["content_filter", "content_policy", "content_blocked", "内容审核"]) {
        "content_blocked"
    } else {
        "fatal"
    }
}

/// 把业务失败映射成带状态的网关错误。
pub fn upstream_error(status: u16, value: &Value) -> GatewayError {
    let business = numeric_code(value).filter(|code| code.fract() == 0.0 && (400.0..600.0).contains(code));
    let class = kind(
        if status < 400 {
            business.map(|code| code as u16).unwrap_or(status)
        } else {
            status
        },
        value,
    );
    let effective = if status >= 400 {
        status
    } else {
        business.map(|code| code as u16).unwrap_or(match class {
            "token_expired" => 401,
            "quota_limited" => 429,
            "content_blocked" => 403,
            _ => 502,
        })
    };
    GatewayError::with_status(effective as i32, format!("科搭上游返回 {effective}：{}", message(value)))
}

/// 发一次请求并返回 `(状态码, JSON)`。
///
/// 出口优先级与各家一致：账号代理 → 网关代理；两者都没有才直连。
pub async fn request(
    method: &str,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
    timeout_ms: u64,
) -> Result<(u16, Value), GatewayError> {
    let url = format!("{base}{path}");
    let client = http::control_client(proxy, gateway_proxy);
    let mut builder = match method {
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        "PATCH" => client.patch(&url),
        _ => client.get(&url),
    };
    builder = builder.timeout(std::time::Duration::from_millis(timeout_ms));
    for (name, value) in headers(token) {
        // 有 body 时由 `.json()` 写入 Content-Type，避免重复同名头
        if body.is_some() && name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(body) = body {
        builder = builder.json(body);
    }
    let response = builder.send().await.map_err(|error| {
        GatewayError::with_status(502, format!("请求科搭上游失败：{}", http::describe(&error)))
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, payload))
}

/// 带校验的请求：非 2xx 或业务失败都算错。
pub async fn checked(
    method: &str,
    base: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
    timeout_ms: u64,
) -> Result<Value, GatewayError> {
    let (status, payload) = request(method, base, path, Some(token), body, proxy, gateway_proxy, timeout_ms).await?;
    if !(200..300).contains(&status) || failed(&payload) {
        return Err(upstream_error(status, &payload));
    }
    Ok(payload)
}

/// 读账号资料。
///
/// ── 主机为什么是业务域名而不是账号域名 ──────────────────────
/// 参考实现把 `/api/user/self` 放在账号域名（`api.kuaipaper.com`），但
/// 2026-11-19 用真实令牌实测：账号域名对该路径返回 HTTP 404
/// `{"detail":"Not Found"}`，业务域名（`api.claw.speedai.com`）返回 200 与账号资料。
/// 导入与扫码后的资料核对都必须走业务域名。
pub async fn profile(
    token: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, GatewayError> {
    checked("GET", endpoints::API_BASE, endpoints::PROFILE, token, None, proxy, gateway_proxy, 12_000).await
}

/// 续期令牌。
pub async fn refresh(
    token: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<String, GatewayError> {
    let payload = checked("POST", endpoints::ACCOUNT_BASE, endpoints::REFRESH, token, None, proxy, gateway_proxy, 12_000).await?;
    let data = data(&payload);
    let candidate = ["token", "accessToken"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .or_else(|| {
            ["token", "accessToken"]
                .iter()
                .find_map(|key| payload.get(*key).and_then(Value::as_str))
        })
        .unwrap_or("")
        .trim();
    if candidate.is_empty() {
        return Err(GatewayError::with_status(502, "科搭续期没有返回有效令牌"));
    }
    Ok(candidate.to_string())
}

/// 账号级出网代理（取账号记录的 `proxy` 字段，由网关预先解析）。
pub async fn account_proxy(
    ctx: &agent2api_provider_sdk::PluginCtx,
    account: &str,
) -> Option<agent2api_provider_protocol::WireProxy> {
    let record = crate::store::record(ctx, super::PROVIDER_ID, account).await.ok()?;
    match crate::proxy_util::resolve_account_proxy(record.get("proxy")) {
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Some(proxy),
        _ => None,
    }
}
