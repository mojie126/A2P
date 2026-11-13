//! 百度搭子的两套出网：网页账号面（Cookie）与模型网关（Bearer 令牌）。
//!
//! ── 请求头指纹 ──────────────────────────────────────────────
//! 网页接口要求带浏览器特征头（`X-Dumate-Client-Type: web`、`Referer`、
//! `Origin`、中文 `Accept-Language` 与浏览器 UA），缺了会被当成非浏览器
//! 来源。这里照参考实现（dumate-web.js）的原样发送。
//!
//! ── 换票与缓存 ──────────────────────────────────────────────
//! `GET /api/dumate/app/token/get` 用 Cookie 换约 1 小时的 Bearer 令牌，
//! 服务端对同一账号缓存同一张票。进程内按账号缓存，提前 5 分钟刷新；
//! `expired_at` 解析失败按 50 分钟兜底，宁可早刷不要晚刷。
//!
//! ── 业务信封 ────────────────────────────────────────────────
//! 网页接口 HTTP 200 不代表成功：业务结果看 `code`（`code:0` 才算成功），
//! `code:10001` / `10006` 是登录态失效。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use super::accounts::Credentials;
use super::endpoints;
use crate::http;

/// 参考实现使用的浏览器 UA（网页接口按它识别来源）。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// 令牌提前刷新的余量（毫秒）。
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

/// `expired_at` 解析失败时的兜底有效期。
const FALLBACK_TTL_MS: i64 = 50 * 60 * 1000;

/// 网页账号面失败的两种形态。
pub enum WebError {
    /// Cookie 已失效（让用户重新粘贴）。
    Expired(String),
    /// 其他失败（网络 / 5xx / 业务拒绝）。
    Failed(String),
}

impl WebError {
    pub fn message(&self) -> &str {
        match self {
            Self::Expired(message) | Self::Failed(message) => message,
        }
    }
}

struct CachedToken {
    token: String,
    expire_at: i64,
    /// 签发这张票时用的 Cookie 指纹（换 Cookie 后旧票作废）。
    cookie_fingerprint: String,
}

fn token_cache() -> &'static Mutex<HashMap<String, CachedToken>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_cache<T>(action: impl FnOnce(&mut HashMap<String, CachedToken>) -> T) -> T {
    match token_cache().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

fn cookie_fingerprint(cookie: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(cookie.as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// 没有账号 id 时的缓存键（Cookie 指纹）。
pub fn cache_key_of(cookie: &str) -> String {
    format!("cookie:{}", cookie_fingerprint(cookie))
}

/// 网页账号面请求共用的浏览器特征头。
fn decorate(request: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
    request
        .header("Cookie", cookie)
        .header("X-Dumate-Client-Type", "web")
        .header("Referer", format!("{}/app", endpoints::WEB_BASE))
        .header("Origin", endpoints::WEB_BASE)
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
}

/// 统一取结果体：云端有的包在 `result`，有的包在 `data`，有的直接顶层。
fn unwrap_envelope(value: Value, operation: &str) -> Result<Value, WebError> {
    let code = value.get("code").and_then(|code| {
        code.as_i64().or_else(|| {
            code.as_str().and_then(|text| text.trim().parse::<i64>().ok())
        })
    });
    if code == Some(10001) || code == Some(10006) {
        return Err(WebError::Expired(
            "百度搭子网页登录态已失效，请重新复制 Cookie".to_string(),
        ));
    }
    if let Some(code) = code.filter(|code| *code != 0) {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(WebError::Failed(if message.is_empty() {
            format!("{operation}失败（业务码 {code}）")
        } else {
            format!("{operation}失败：{message}")
        }));
    }
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("success=false");
        return Err(WebError::Failed(format!("{operation}失败：{message}")));
    }
    Ok(value
        .get("result")
        .cloned()
        .or_else(|| value.get("data").cloned())
        .unwrap_or(value))
}

async fn web_request(
    credentials: &Credentials,
    method: reqwest::Method,
    path: &str,
    operation: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, WebError> {
    let url = format!("{}{}", endpoints::WEB_BASE, path);
    let client = http::control_client(proxy, gateway_proxy);
    let request = client
        .request(method, &url)
        .timeout(std::time::Duration::from_secs(20));
    let response = decorate(request, &credentials.cookie)
        .send()
        .await
        .map_err(|error| {
            WebError::Failed(format!("{operation}网络失败：{}", http::describe(&error)))
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| {
        WebError::Failed(format!("读取{operation}响应失败：{}", http::describe(&error)))
    })?;
    if status == 401 {
        return Err(WebError::Expired(
            "百度搭子网页登录态已失效（HTTP 401），请重新复制 Cookie".to_string(),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(WebError::Failed(format!(
            "{operation}失败（HTTP {status}）"
        )));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| {
        WebError::Failed(format!("{operation}返回的不是 JSON（HTTP {status}）"))
    })?;
    unwrap_envelope(value, operation)
}

/// 账号面 JSON 请求（签到 / 余额用；代理沿用 SDK 已绑定的账号出口）。
pub async fn web_json(
    credentials: &Credentials,
    method: reqwest::Method,
    path: &str,
    operation: &str,
) -> Result<Value, WebError> {
    web_request(credentials, method, path, operation, None, "").await
}

/// Cookie 换 Bearer 令牌（带进程内缓存，`force` 跳过缓存）。
///
/// `cache_key` 是账号 id（多账号并存时各缓存各的票）。
pub async fn fetch_token(
    cache_key: &str,
    credentials: &Credentials,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
    force: bool,
) -> Result<String, WebError> {
    let fingerprint = cookie_fingerprint(&credentials.cookie);
    if !force {
        let cached = with_cache(|table| {
            table
                .get(cache_key)
                .filter(|entry| {
                    entry.cookie_fingerprint == fingerprint
                        && http::now_ms() < entry.expire_at - REFRESH_SKEW_MS
                })
                .map(|entry| entry.token.clone())
        });
        if let Some(token) = cached {
            return Ok(token);
        }
    }
    let value = web_request(
        credentials,
        reqwest::Method::GET,
        endpoints::TOKEN_GET_PATH,
        "换取访问令牌",
        proxy,
        gateway_proxy,
    )
    .await?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            WebError::Failed("换取访问令牌失败：响应里没有令牌".to_string())
        })?
        .to_string();
    let expire_at = value
        .get("expired_at")
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.timestamp_millis())
        .unwrap_or(http::now_ms() + FALLBACK_TTL_MS);
    with_cache(|table| {
        table.insert(
            cache_key.to_string(),
            CachedToken {
                token: token.clone(),
                expire_at,
                cookie_fingerprint: fingerprint,
            },
        );
    });
    Ok(token)
}

/// 缓存里的令牌是否临期（5 分钟内过期或没有缓存）。
pub fn token_expiring(cache_key: &str, credentials: &Credentials) -> bool {
    let fingerprint = cookie_fingerprint(&credentials.cookie);
    with_cache(|table| {
        match table.get(cache_key) {
            Some(entry) => {
                entry.cookie_fingerprint != fingerprint
                    || http::now_ms() >= entry.expire_at - REFRESH_SKEW_MS
            }
            None => true,
        }
    })
}

