//! 科搭账号：访问令牌的解析、资料核对、公开视图与去重。
//!
//! 导入方式只做「粘贴 JWT / 粘贴凭据 JSON」：微信扫码、手机验证码与
//! 本机关联需要宿主能力（人机验证、桌面凭据 helper），原生插件一期不接入。
//! 导入时现场读一次 `/api/user/self` 核对账号名（读接口，不计费）。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use super::credentials;

/// 一份科搭凭据。
#[derive(Clone, Default)]
pub struct Credentials {
    pub name: String,
    pub access_token: String,
    pub username: String,
    pub uid: String,
    pub nickname: String,
    pub expires_at: Option<i64>,
    /// 来源：`token`（粘贴令牌）或 `desktop`（本机关联）。
    pub source: String,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Self {
        Self {
            name: text(secret, &["name"]),
            access_token: text(secret, &["accessToken", "access_token", "token"]),
            username: text(secret, &["username"]),
            uid: text(secret, &["uid"]),
            nickname: text(secret, &["nickname"]),
            expires_at: secret.get("expiresAt").and_then(Value::as_i64),
            source: text(secret, &["source"]),
        }
    }

    /// 是否本机关联（续期改为重读客户端配置）。
    pub fn is_desktop(&self) -> bool {
        self.source == "desktop"
    }

    /// 是否需要提前续期（窗口 48 小时）。
    pub fn expiring(&self) -> bool {
        credentials::expires(&self.access_token)
            .or(self.expires_at)
            .is_some_and(|expires| expires - crate::http::now_ms() <= super::endpoints::REFRESH_BEFORE_MS)
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

/// 从任意粘贴内容里抽出访问令牌（纯文本、JSON、嵌套 `credentialJson` 都认）。
pub fn extract_token(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("请填写科搭签发的完整访问令牌 JWT".to_string());
    }
    if raw.len() > 256 * 1024 {
        return Err("科搭凭据文本超过 256 KiB 限制".to_string());
    }
    if raw.starts_with('{') || raw.starts_with('[') {
        let parsed: Value = serde_json::from_str(raw).map_err(|_| "科搭凭据 JSON 格式无效".to_string())?;
        return extract_from_json(&parsed, 0);
    }
    credentials::token(raw)
}

fn extract_from_json(value: &Value, depth: usize) -> Result<String, String> {
    if depth > 8 {
        return Err("科搭凭据 JSON 嵌套过深".to_string());
    }
    if let Some(raw) = value.as_str() {
        return extract_token(raw);
    }
    if !value.is_object() {
        return Err("请导入单个科搭账号，不要导入整个账号池".to_string());
    }
    if value.get("accounts").is_some() {
        return Err("请导入单个科搭账号，不要导入整个账号池".to_string());
    }
    if let Some(raw) = value.get("credentialJson").and_then(Value::as_str) {
        return extract_token(raw);
    }
    for object in [value, &value["secret"], &value["provider"]["speedai"]] {
        for key in ["accessToken", "access_token", "token"] {
            if let Some(raw) = object.get(key).and_then(Value::as_str) {
                let token = credentials::token(raw)?;
                if !token.is_empty() {
                    return Ok(token);
                }
            }
        }
    }
    Err("没有在凭据里找到科搭访问令牌".to_string())
}

/// 归一化一条账号。
pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let name = input.name.clone().unwrap_or_default();
    let desktop_source = matches!(input.source, proto::NormalizeSource::Desktop);
    let raw = match input.source {
        proto::NormalizeSource::Manual { payload } => {
            let fields = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(&payload);
            text(fields, &["key", "token", "accessToken", "apiKey"])
        }
        proto::NormalizeSource::FileText { content, .. } => content,
        proto::NormalizeSource::FileBase64 { content, .. } => {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine;
            let bytes = STANDARD
                .decode(content.trim())
                .map_err(|_| "凭据文件不是有效的 Base64".to_string())?;
            String::from_utf8(bytes).map_err(|_| "凭据文件不是有效的文本".to_string())?
        }
        proto::NormalizeSource::Desktop => String::new(),
    };
    let (token, source) = if desktop_source {
        // 本机关联：只读官方客户端当前登录令牌，不写回客户端文件
        (super::desktop::read()?.token, "desktop".to_string())
    } else if let Some((phone, code)) = sms_login_input(&raw) {
        // 短信登录：两段式（先通过管理动作发送验证码，再用「手机号:验证码」导入）
        (
            complete_sms_login(&phone, &code, input.proxy.as_ref(), &ctx.env.proxy).await?,
            "token".to_string(),
        )
    } else {
        (extract_token(&raw)?, "token".to_string())
    };
    if credentials::expires(&token).is_some_and(|expires| expires <= crate::http::now_ms()) {
        return Err("科搭登录凭据已过期，请重新登录".to_string());
    }
    let profile = client::profile(&token, input.proxy.as_ref(), &ctx.env.proxy)
        .await
        .map_err(|error| error.message)?;
    let data = client::data(&profile);
    let username = ["username", "user_name"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "科搭没有返回有效账号名，未保存凭据".to_string())?
        .to_string();
    let claim_user = credentials::claim_username(&token);
    if !claim_user.is_empty() && claim_user != username {
        return Err("科搭令牌与账号资料不一致，请重新登录".to_string());
    }
    let nickname = ["display_name", "nickname"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let uid = data
        .get("uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let credentials = Credentials {
        name: if name.trim().is_empty() {
            if nickname.is_empty() { username.clone() } else { nickname.clone() }
        } else {
            name
        },
        expires_at: credentials::expires(&token),
        access_token: token,
        username,
        uid,
        nickname,
        source,
    };
    Ok(account_of(credentials))
}

/// 识别短信登录输入：`sms:<手机号>:<验证码>` 或 JSON `{phone, code}`。
///
/// 面板的交互式短信流程需要网关的原生交互能力（当前只对 WASM 插件开放），
/// 原生入口因此提供「先发送验证码、再按此格式导入」的两段式路径。
pub fn sms_login_input(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("sms:") {
        let (phone, code) = rest.split_once(':')?;
        let (phone, code) = (phone.trim(), code.trim());
        if !phone.is_empty() && !code.is_empty() {
            return Some((phone.to_string(), code.to_string()));
        }
        return None;
    }
    if !raw.starts_with('{') {
        return None;
    }
    let value: Value = serde_json::from_str(raw).ok()?;
    let phone = ["phone", "mobile", "username"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let code = ["code", "smsCode", "sms_code", "password"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some((phone.to_string(), code.to_string()))
}

/// 用手机号与验证码换令牌（`/v1/login`）。
pub async fn complete_sms_login(
    phone: &str,
    code: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<String, String> {
    let (status, body) = client::request(
        "POST",
        super::endpoints::ACCOUNT_BASE,
        "/v1/login",
        None,
        Some(&json!({ "username": phone, "password": code })),
        proxy,
        gateway_proxy,
        12_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&status) || client::failed(&body) {
        return Err(client::upstream_error(status, &body).message);
    }
    let data = client::data(&body);
    let token = ["token", "accessToken"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .or_else(|| {
            ["token", "accessToken"]
                .iter()
                .find_map(|key| body.get(*key).and_then(Value::as_str))
        })
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        return Err("科搭短信登录没有返回访问令牌".to_string());
    }
    credentials::token(token)
}

pub(super) fn account_of(credentials: Credentials) -> proto::NormalizedAccount {
    let mut secret = json!({});
    if !credentials.name.is_empty() {
        secret["name"] = json!(credentials.name.clone());
    }
    if !credentials.username.is_empty() {
        secret["username"] = json!(credentials.username.clone());
    }
    if !credentials.uid.is_empty() {
        secret["uid"] = json!(credentials.uid.clone());
    }
    if !credentials.nickname.is_empty() {
        secret["nickname"] = json!(credentials.nickname.clone());
    }
    secret["accessToken"] = json!(credentials.access_token.clone());
    secret["expiresAt"] = credentials.expires_at.map_or(Value::Null, |value| json!(value));
    secret["source"] = json!(if credentials.source.is_empty() { "token" } else { &credentials.source });
    let identity = identity_of(&credentials);
    let view = public(&secret);
    proto::NormalizedAccount {
        secret,
        public: view,
        identity,
        desktop: false,
    }
}

/// 公开视图：只露令牌尾号，绝不带令牌本体。
pub fn public(secret: &Value) -> Value {
    let credentials = Credentials::from_secret(secret);
    let name = if !credentials.name.is_empty() {
        credentials.name.clone()
    } else if !credentials.username.is_empty() {
        credentials.username.clone()
    } else {
        "科搭账号".to_string()
    };
    let expires = credentials::expires(&credentials.access_token).or(credentials.expires_at);
    let mut view = json!({
        "name": name,
        "uid": credentials.uid,
        "username": credentials.username,
        "nickname": credentials.nickname,
        "tokenTail": tail_of(&credentials.access_token),
        "hasAccessToken": !credentials.access_token.is_empty(),
        "canRefresh": true,
        "desktop": credentials.is_desktop(),
        "source": if credentials.source.is_empty() { "token" } else { credentials.source.as_str() },
        "available": expires.is_none_or(|expires| expires > crate::http::now_ms()),
    });
    if let Some(expires) = expires {
        view["expiresAt"] = json!(expires);
    }
    view
}

/// 去重身份：账号名哈希。
pub fn identity_of(credentials: &Credentials) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(credentials.username.as_bytes());
    let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("codar-{hex}")
}

pub fn identity(secret: &Value) -> String {
    identity_of(&Credentials::from_secret(secret))
}

fn tail_of(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let start = chars.len().saturating_sub(6);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_accepts_plain_and_json() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let token = format!("h.{}.s", URL_SAFE_NO_PAD.encode(r#"{"username":"u"}"#));
        assert_eq!(extract_token(&token).unwrap(), token);
        let json = format!(r#"{{"accessToken":"{token}"}}"#);
        assert_eq!(extract_token(&json).unwrap(), token);
    }

    #[test]
    fn extract_rejects_pool() {
        assert!(extract_token(r#"{"accounts":[]}"#).is_err());
    }
}
