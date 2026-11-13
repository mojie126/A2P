//! 百度搭子的账号：网页 Cookie 凭证的解析、验证、公开视图与去重。
//!
//! ── 凭证载体 ────────────────────────────────────────────────
//! 登录态载体是浏览器 Cookie（核心字段 `BDUSS`，`.baidu.com` 域）。参考
//! 实测：余额与签到接口还依赖访问过百度智能云控制台（console.bce.baidu.com）
//! 后种下的会话 Cookie，缺它的表现为账号面接口 401。
//!
//! ── 采集方式 ────────────────────────────────────────────────
//! 参考实现用受控浏览器窗口登录后自动抓取；本插件一期只做「粘贴完整
//! Cookie」，添加时现场调一次换票接口验证（读接口不计费）。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use crate::limits;

/// 一份百度搭子凭据。
#[derive(Clone, Default)]
pub struct Credentials {
    pub name: String,
    /// 网页 Cookie（单行形态，必含 BDUSS）。
    pub cookie: String,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Self {
        Self {
            name: text(secret, &["name"]),
            cookie: text(secret, &["cookie"]),
        }
    }

    /// Cookie 里的 BDUSS 值（去重身份与尾号显示都用它）。
    pub fn bduss(&self) -> String {
        cookie_value(&self.cookie, "BDUSS")
    }
}

fn text(secret: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = secret.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// 从 Cookie 串里取一个字段的值（大小写按百度实际字段名匹配）。
pub fn cookie_value(cookie: &str, key: &str) -> String {
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=') {
            if name.trim() == key {
                return value.trim().to_string();
            }
        }
    }
    String::new()
}

/// 把用户粘贴的内容清洗成单行 Cookie。
///
/// 容忍三类常见粘贴形态：整段请求头（去掉 `Cookie:` 前缀）、换行分隔的
/// 多行、JSON（取 `cookie` 字段）。清洗后必须有 BDUSS 才算有效。
pub fn clean_cookie(input: &str) -> Result<String, String> {
    let mut content = input.trim().to_string();
    // JSON 形态：取 cookie 字段
    if content.starts_with('{') {
        let value: Value = serde_json::from_str(&content)
            .map_err(|_| "这不是有效的 Cookie，也不是有效的 JSON".to_string())?;
        content = text(&value, &["cookie", "Cookie"]);
    }
    // 请求头形态：去掉前缀
    for prefix in ["Cookie:", "cookie:"] {
        if let Some(rest) = content.strip_prefix(prefix) {
            content = rest.trim().to_string();
        }
    }
    // 多行粘贴：合并成单行（换行前后一般带分号或空格）
    let joined = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut cookie = joined
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if cookie.len() > limits::MAX_TOKEN_LENGTH {
        return Err("这份 Cookie 异常地长，请确认只粘贴了网页请求的 Cookie".to_string());
    }
    if cookie_value(&cookie, "BDUSS").is_empty() {
        return Err(
            "Cookie 里没有 BDUSS，请确认复制的是登录百度账号后的完整 Cookie".to_string(),
        );
    }
    // 末尾补分号不影响，保持用户原样即可；这里只统一去掉连续空格
    while cookie.contains("; ;") {
        cookie = cookie.replace("; ;", ";");
    }
    Ok(cookie)
}

/// 现场验证 Cookie：调一次换票接口（读操作，不计费）。
async fn verify(
    credentials: &Credentials,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<(), String> {
    // 验证场景没有账号 id，用 Cookie 指纹做缓存键（与正式转发不同键，
    // 互不影响；正式转发用账号 id）。
    let cache_key = client::cache_key_of(&credentials.cookie);
    match client::fetch_token(&cache_key, credentials, proxy, gateway_proxy, true).await {
        Ok(_) => Ok(()),
        Err(client::WebError::Expired(message)) => Err(message),
        Err(client::WebError::Failed(message)) => Err(message),
    }
}

/// 归一化一条账号。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let name = input.name.clone().unwrap_or_default();
    let gateway_proxy = _ctx.env.proxy.clone();
    let cookie = match input.source {
        proto::NormalizeSource::Manual { payload } => {
            let fields = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(&payload);
            let raw = text(fields, &["cookie", "token"]);
            if raw.is_empty() {
                return Err("请粘贴网页 Cookie（需包含 BDUSS）".to_string());
            }
            clean_cookie(&raw)?
        }
        proto::NormalizeSource::FileText { content, .. } => clean_cookie(&content)?,
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("百度搭子不支持导入二进制文件，请粘贴网页 Cookie".to_string())
        }
        proto::NormalizeSource::Desktop => {
            return Err(
                "百度搭子暂不支持读取桌面端登录态，请粘贴 www.dumate.cn 的网页 Cookie".to_string(),
            )
        }
    };
    let credentials = Credentials {
        name,
        cookie: cookie.clone(),
    };
    verify(&credentials, input.proxy.as_ref(), &gateway_proxy).await?;
    Ok(account_of(credentials))
}

fn account_of(credentials: Credentials) -> proto::NormalizedAccount {
    let mut secret = json!({});
    if !credentials.name.is_empty() {
        secret["name"] = json!(credentials.name.clone());
    }
    secret["cookie"] = json!(credentials.cookie.clone());
    let identity = identity_of(&credentials);
    let view = public(&secret);
    proto::NormalizedAccount {
        secret,
        public: view,
        identity,
        desktop: false,
    }
}

/// 公开视图：只露 BDUSS 尾号，绝不带 Cookie 本体。
pub fn public(secret: &Value) -> Value {
    let credentials = Credentials::from_secret(secret);
    let bduss = credentials.bduss();
    let name = if !credentials.name.is_empty() {
        credentials.name.clone()
    } else {
        "百度搭子".to_string()
    };
    json!({
        "name": name,
        "tokenTail": tail_of(&bduss),
        "hasCredentials": !credentials.cookie.is_empty(),
    })
}

/// 去重身份：BDUSS 的哈希（同一百度账号换会话 Cookie 仍是同一人）。
pub fn identity_of(credentials: &Credentials) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(credentials.bduss().as_bytes());
    let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("dumate:bduss:{hex}")
}

pub fn identity(secret: &Value) -> String {
    identity_of(&Credentials::from_secret(secret))
}

fn tail_of(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let start = chars.len().saturating_sub(6);
    chars[start..].iter().collect()
}

