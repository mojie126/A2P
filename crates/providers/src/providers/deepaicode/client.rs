//! DeepAI Code 的出站：公共头组装、JSON 请求、错误映射与签到挑战缓存。
//!
//! ── 请求头（按参考实现）────────────────────────────────────
//! ```text
//!   Api-Key                    账号 Key（鉴权走这个头，不是 Authorization）
//!   X-DeepAI-Code-Theos        计费车道版本（默认 4，错值回 426）
//!   X-DeepAI-Code-Instance     进程级随机实例号
//!   X-DeepAI-Code-Machine      sha256(机器标识)，每日免费额度的分配键
//!   User-Agent                 必须显式带（上游拦 node 默认 UA）
//! ```
//!
//! ── 签到挑战 ────────────────────────────────────────────────
//! `/theos` 的响应（包括 403）会下发 `check_in.nonce`，下一次请求把它放进
//! `X-DeepAI-Code-Check-In` 才算完成签到。本模块按账号缓存 nonce，
//! 额度查询与签到都会顺带回传。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use serde_json::Value;

use super::accounts::Credentials;
use super::endpoints;
use crate::errors::GatewayError;
use crate::http;

/// 进程级实例号（32 位小写十六进制，进程内只生成一次）。
fn instance_id() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut bytes = [0u8; 16];
        if getrandom::getrandom(&mut bytes).is_err() {
            // 随机源不可用不阻断请求：用时间戳兜底，这个值只做客户端实例标识
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            for (index, slot) in bytes.iter_mut().enumerate() {
                *slot = ((nanos >> (8 * (index % 16))) & 0xff) as u8;
            }
        }
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    })
}

/// 计费车道版本（`DEEPAI_LANE` 可覆盖）。
pub fn lane_version() -> String {
    std::env::var("DEEPAI_LANE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| endpoints::DEFAULT_LANE.to_string())
}

/// 进程启动时刻（心跳 `up=` 使用）。
fn started_at() -> &'static Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    STARTED.get_or_init(Instant::now)
}

/// 公共请求头（JSON 接口）。
pub fn base_headers(credentials: &Credentials) -> Vec<(String, String)> {
    let mut headers = vec![
        (endpoints::HEADER_LANE.to_string(), lane_version()),
        (
            endpoints::HEADER_INSTANCE.to_string(),
            instance_id().to_string(),
        ),
        ("User-Agent".to_string(), endpoints::USER_AGENT.to_string()),
    ];
    let machine = credentials.machine_fingerprint();
    if !machine.is_empty() {
        headers.push((endpoints::HEADER_MACHINE.to_string(), machine));
    }
    if !credentials.api_key.is_empty() {
        headers.push((endpoints::HEADER_KEY.to_string(), credentials.api_key.clone()));
    }
    headers
}

/// 对话请求头：在上面基础上补内容类型与接受类型。
pub fn chat_headers(credentials: &Credentials, stream: bool) -> Vec<(String, String)> {
    let mut headers = base_headers(credentials);
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    headers.push((
        "Accept".to_string(),
        if stream { "text/event-stream" } else { "application/json" }.to_string(),
    ));
    headers
}

/// `/theos` 专用头：补签到 nonce 与心跳。
fn theos_headers(credentials: &Credentials) -> Vec<(String, String)> {
    let mut headers = base_headers(credentials);
    if let Some(nonce) = take_nonce(&credentials.api_key) {
        headers.push((endpoints::HEADER_CHECKIN.to_string(), nonce));
    }
    headers.push((
        endpoints::HEADER_BEACON.to_string(),
        format!(
            "v={};up={};pi=0",
            endpoints::CLIENT_VERSION,
            started_at().elapsed().as_secs()
        ),
    ));
    headers
}

/// 签到 nonce 缓存（按 API Key 的哈希）。
static NONCES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn nonce_key(api_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(api_key.as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// 取出并移除本账号待回传的 nonce（回传一次即失效）。
fn take_nonce(api_key: &str) -> Option<String> {
    let mut table = NONCES.lock().ok()?;
    table.remove(&nonce_key(api_key))
}

/// 记录响应里的签到挑战 nonce。
pub fn remember_nonce(api_key: &str, body: &Value) {
    let Some(nonce) = body
        .pointer("/check_in/nonce")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    if let Ok(mut table) = NONCES.lock() {
        table.insert(nonce_key(api_key), nonce.to_string());
    }
}

/// 清空本账号的 nonce（命中当日签到限流后不再回传）。
pub fn clear_nonce(api_key: &str) {
    if let Ok(mut table) = NONCES.lock() {
        table.remove(&nonce_key(api_key));
    }
}

/// 上游错误体里的错误码。
pub fn error_code(body: &Value) -> String {
    body.pointer("/error/code")
        .and_then(Value::as_str)
        .or_else(|| body.get("code").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 上游错误体里可展示的消息。
pub fn error_message(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .or_else(|| body.pointer("/error").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 把上游失败映射成网关可见的错误。
///
/// 关键映射：`409 session_busy` 是容量拒绝，转 429 交给网关换号；
/// `426` 是车道版本不受理；`403 theos_exhausted` 是当日额度用完。
pub fn upstream_error(status: u16, body: &Value) -> GatewayError {
    let code = error_code(body);
    let message = error_message(body);
    let detail = if message.is_empty() {
        format!("DeepAI 上游返回 HTTP {status}")
    } else {
        format!("DeepAI 上游返回 HTTP {status}：{message}")
    };
    match code.as_str() {
        endpoints::CODE_SESSION_BUSY => {
            return GatewayError::with_status(429, "DeepAI 上游容量繁忙，稍后重试或换号");
        }
        endpoints::CODE_LANE_UNSUPPORTED => {
            return GatewayError::with_status(
                426,
                "DeepAI 车道版本不受理，请通过环境变量 DEEPAI_LANE 指定新车道",
            );
        }
        endpoints::CODE_EMAIL_UNVERIFIED => {
            return GatewayError::with_status(403, "DeepAI 账号邮箱未验证，请在官网完成验证");
        }
        endpoints::CODE_SOCIAL_LOGIN => {
            return GatewayError::with_status(
                403,
                "该 DeepAI 账号需网页社交登录，API Key 不可用，请重新登录",
            );
        }
        endpoints::CODE_CHECKIN_REFUSED => {
            return GatewayError::with_status(
                403,
                "DeepAI 当日签到次数超限，明日自动恢复（对话不受影响）",
            );
        }
        endpoints::CODE_THEOS_EXHAUSTED => {
            return GatewayError::with_status(403, {
                if message.is_empty() {
                    "DeepAI 账号当日额度已用完，明日刷新后恢复".to_string()
                } else {
                    message
                }
            });
        }
        _ => {}
    }
    match status {
        409 => GatewayError::with_status(429, "DeepAI 上游容量繁忙，稍后重试或换号"),
        401 => GatewayError::with_status(401, "DeepAI 登录态已失效，请重新粘贴 API Key"),
        426 => GatewayError::with_status(
            426,
            "DeepAI 车道版本不受理，请通过环境变量 DEEPAI_LANE 指定新车道",
        ),
        _ => GatewayError::with_status(status as i32, detail),
    }
}

/// 发一次 GET 并返回 JSON（失败按 [`upstream_error`] 映射）。
///
/// 出口优先级与各家一致：账号代理 → 网关代理；两者都没有才直连。
pub async fn get_json(
    url: &str,
    headers: &[(String, String)],
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
    timeout_ms: u64,
) -> Result<Value, GatewayError> {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    let client = http::control_client(proxy, gateway_proxy);
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .headers(map)
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(502, format!("请求 DeepAI 上游失败：{}", http::describe(&error)))
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(upstream_error(status, &body));
    }
    Ok(body)
}

/// 查 `/theos`（额度与签到通道），顺带缓存签到挑战。
pub async fn theos(
    credentials: &Credentials,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
    timeout_ms: u64,
) -> Result<Value, GatewayError> {
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::THEOS_PATH);
    let headers = theos_headers(credentials);
    let body = get_json(&url, &headers, proxy, gateway_proxy, timeout_ms).await?;
    remember_nonce(&credentials.api_key, &body);
    Ok(body)
}

/// 验证 Key：能拿到 `/theos` 即有效；401/403 明确报错，其余失败也如实报。
pub async fn verify_key(
    credentials: &Credentials,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<(), String> {
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::THEOS_PATH);
    let headers = base_headers(credentials);
    match get_json(&url, &headers, proxy, gateway_proxy, 20_000).await {
        Ok(_) => Ok(()),
        Err(error) => Err(error.message),
    }
}

/// 查 `/get_usage_summary`（订阅与用量摘要）。
pub async fn usage_summary(
    credentials: &Credentials,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::USAGE_SUMMARY_PATH);
    let headers = base_headers(credentials);
    get_json(&url, &headers, proxy, gateway_proxy, 15_000).await
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
