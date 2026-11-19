//! DeepAI Code 账号：API Key 凭证的解析、本机凭据读取、公开视图与去重。
//!
//! ── 凭证形态 ────────────────────────────────────────────────
//! 一份账号就是一个 DeepAI API Key（UUID 形态）：
//!   * 粘贴 Key（`Api-Key` 头鉴权）；
//!   * 读取本机 DeepAI Code 桌面端登录态
//!     （`~/.deepai-code/userdata/secrets/deepai-account.bin`，明文 JSON
//!     `{"apiKey":"…","username":"…","isPro":false}`）；
//!   * 粘贴整份 JSON 凭据。
//!
//! ── 机器指纹 ────────────────────────────────────────────────
//! 上游按 `(机器指纹, 账号)` 每天发放免费额度。本实现按**账号**取指纹：
//! 显式给了 `machineId` 就用它，其次读环境变量 `DEEPAI_MACHINE_ID`，
//! 都没有时从 API Key 稳定派生。这样同一 Key 反复导入指纹不变、不同账号
//! 各自独立领取（与参考实现 DeepAICode2API 的号池口径一致）。
//! 原始标识不上传，只发 sha256。

use std::path::PathBuf;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::client;
use crate::limits;

/// 一份 DeepAI Code 凭据。
#[derive(Clone, Default)]
pub struct Credentials {
    pub name: String,
    pub api_key: String,
    pub username: String,
    pub is_pro: bool,
    /// 显式的原始机器标识（可空，空时按 Key 派生）。
    pub machine_id: String,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Self {
        Self {
            name: text(secret, &["name"]),
            api_key: text(secret, &["apiKey", "api_key", "key", "token"]),
            username: text(secret, &["username", "user"]),
            is_pro: secret.get("isPro").and_then(Value::as_bool).unwrap_or(false),
            machine_id: text(secret, &["machineId", "machine_id", "machine"]),
        }
    }

    /// 上游要求的机器指纹：sha256(小写去空白原始值)。
    ///
    /// 原始值优先级：账号显式值 → 环境变量 → 按 Key 稳定派生。
    pub fn machine_fingerprint(&self) -> String {
        let raw = if !self.machine_id.trim().is_empty() {
            self.machine_id.clone()
        } else if let Ok(value) = std::env::var("DEEPAI_MACHINE_ID") {
            value
        } else {
            format!("deepai:{}", self.api_key)
        };
        fingerprint_of(&raw)
    }
}

/// sha256 十六进制（原始值小写去空白后哈希）；空串返回空串。
pub fn fingerprint_of(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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

/// 清洗粘贴的 Key：去掉请求头前缀与包裹引号。
pub fn clean_key(input: &str) -> Result<String, String> {
    let mut value = input.trim().to_string();
    if value.starts_with('{') {
        let parsed: Value = serde_json::from_str(&value)
            .map_err(|_| "这既不是有效的 API Key，也不是有效的 JSON".to_string())?;
        value = text(&parsed, &["apiKey", "api_key", "key", "token"]);
    }
    for prefix in ["Api-Key:", "api-key:", "Authorization:", "Bearer "] {
        if let Some(rest) = value.strip_prefix(prefix) {
            value = rest.trim().to_string();
        }
    }
    while value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        value = value[1..value.len() - 1].trim().to_string();
    }
    if value.is_empty() {
        return Err("请填写 DeepAI API Key".to_string());
    }
    if value.len() > limits::MAX_TOKEN_LENGTH || value.chars().any(char::is_control) {
        return Err("API Key 过长或包含非法控制字符".to_string());
    }
    Ok(value)
}

/// 本机桌面端凭据的 base 目录候选（`T3CODE_HOME` → `DEEPAI_CODE_HOME` → `~/.deepai-code`）。
fn desktop_bases() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    for name in ["T3CODE_HOME", "DEEPAI_CODE_HOME"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                bases.push(PathBuf::from(value));
            }
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        bases.push(PathBuf::from(home).join(".deepai-code"));
    }
    bases
}

/// 读取本机 DeepAI Code 桌面端登录态；找不到时返回 `None`。
pub fn read_desktop_credentials() -> Option<Credentials> {
    let secret_parts: [&[&str]; 3] = [
        &["userdata", "secrets"],
        &["secrets"],
        &["dev", "secrets"],
    ];
    for base in desktop_bases() {
        for sub in secret_parts {
            let mut dir = base.clone();
            for part in sub {
                dir = dir.join(part);
            }
            for file in ["deepai-account.bin", "deepai-account-user-managed.bin"] {
                let Ok(raw) = std::fs::read_to_string(dir.join(file)) else {
                    continue;
                };
                let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let Ok(api_key) = clean_key(&text(&parsed, &["apiKey", "api_key", "key"])) else {
                    continue;
                };
                return Some(Credentials {
                    name: text(&parsed, &["username"]),
                    api_key,
                    username: text(&parsed, &["username"]),
                    is_pro: parsed.get("isPro").and_then(Value::as_bool).unwrap_or(false),
                    machine_id: String::new(),
                });
            }
        }
    }
    None
}

/// 现场验证 Key：调一次 `/theos`（读接口，不计费）。
async fn verify(
    credentials: &Credentials,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<(), String> {
    client::verify_key(credentials, proxy, gateway_proxy).await
}

/// 归一化一条账号。
pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let name = input.name.clone().unwrap_or_default();
    let gateway_proxy = ctx.env.proxy.clone();
    let desktop_source = matches!(input.source, proto::NormalizeSource::Desktop);
    let raw = match input.source {
        proto::NormalizeSource::Manual { payload } => {
            let fields = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(&payload);
            text(fields, &["key", "apiKey", "api_key", "token"])
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
    let is_auto = matches!(
        raw.trim().to_lowercase().as_str(),
        "" | "auto" | "read" | "读取"
    );
    let credentials = if desktop_source || is_auto {
        read_desktop_credentials().ok_or_else(|| {
            "没有找到本机 DeepAI Code 登录态（~/.deepai-code/userdata/secrets/deepai-account.bin），\
             请先在 DeepAI Code 桌面端登录，或直接粘贴 API Key"
                .to_string()
        })?
    } else {
        let api_key = clean_key(&raw)?;
        Credentials {
            api_key,
            ..Credentials::default()
        }
    };
    let credentials = Credentials {
        name: if name.is_empty() { credentials.name.clone() } else { name },
        ..credentials
    };
    verify(&credentials, input.proxy.as_ref(), &gateway_proxy).await?;
    Ok(account_of(credentials))
}

pub(super) fn account_of(credentials: Credentials) -> proto::NormalizedAccount {
    let mut secret = json!({});
    if !credentials.name.is_empty() {
        secret["name"] = json!(credentials.name.clone());
    }
    if !credentials.username.is_empty() {
        secret["username"] = json!(credentials.username.clone());
    }
    secret["isPro"] = json!(credentials.is_pro);
    secret["apiKey"] = json!(credentials.api_key.clone());
    if !credentials.machine_id.is_empty() {
        secret["machineId"] = json!(credentials.machine_id.clone());
    }
    let identity = identity_of(&credentials);
    let view = public(&secret);
    proto::NormalizedAccount {
        secret,
        public: view,
        identity,
        desktop: false,
    }
}

/// 公开视图：只露 Key 尾号，绝不带 Key 本体。
pub fn public(secret: &Value) -> Value {
    let credentials = Credentials::from_secret(secret);
    let name = if !credentials.name.is_empty() {
        credentials.name.clone()
    } else if !credentials.username.is_empty() {
        credentials.username.clone()
    } else {
        "DeepAI Code".to_string()
    };
    json!({
        "name": name,
        "username": credentials.username,
        "isPro": credentials.is_pro,
        "tokenTail": tail_of(&credentials.api_key),
        "hasCredentials": !credentials.api_key.is_empty(),
    })
}

/// 去重身份：API Key 的哈希。
pub fn identity_of(credentials: &Credentials) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(credentials.api_key.as_bytes());
    let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("deepaicode:key:{hex}")
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
    fn clean_key_strips_prefixes_and_quotes() {
        assert_eq!(clean_key(" Api-Key: abc ").unwrap(), "abc");
        assert_eq!(clean_key("\"abc\"").unwrap(), "abc");
        assert_eq!(clean_key("Bearer abc").unwrap(), "abc");
    }

    #[test]
    fn machine_fingerprint_is_stable_and_account_scoped() {
        let a = Credentials { api_key: "k1".into(), ..Credentials::default() };
        let b = Credentials { api_key: "k2".into(), ..Credentials::default() };
        assert_eq!(a.machine_fingerprint(), a.machine_fingerprint());
        assert_ne!(a.machine_fingerprint(), b.machine_fingerprint());
        assert_eq!(a.machine_fingerprint().len(), 64);
    }

    #[test]
    fn identity_is_scoped_to_key() {
        let a = Credentials { api_key: "k1".into(), ..Credentials::default() };
        let b = Credentials { api_key: "k2".into(), ..Credentials::default() };
        assert_ne!(identity_of(&a), identity_of(&b));
    }
}
