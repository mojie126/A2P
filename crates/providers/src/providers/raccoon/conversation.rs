//! 会话标识对齐官方桌面客户端 1.1.9：`sess-0-<8hex>` 按账号与首轮输入
//! 稳定派生，同一会话跨轮不变；轮次 ID 每请求生成 UUID v4（上游按它把
//! 同一会话里的多次调用串成一个「回合」）。

use std::collections::BTreeMap;

use agent2api_provider_protocol::ChatRequest;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 会话派生的命名空间种子：与其它产品的同形会话 ID 隔离，改前缀等于
/// 换一套派生结果，只允许在版本升级时动它。
const SESSION_DERIVATION_PREFIX: &str = "raccoon-session-v1\0";

/// 解出对话身份：`(会话 ID, 轮次 ID)`。
///
/// 会话 ID 的取值顺序：入站已带官方形态（`sess-<十进制>-<8 位 hex>`）就
/// 原样透传；否则按「账号 + 种子」派生。种子依次取入站会话标识的原始值、
/// 请求体里的会话字段、首条用户消息，都没有时退到空串（按账号稳定）。
pub(super) fn resolve_ids(req: &ChatRequest, token: &str, org: &str) -> (String, String) {
    let explicit = session_id(&req.headers, &req.body);
    let session = match explicit.as_deref().and_then(official_session) {
        Some(value) => value,
        None => {
            let seed = explicit
                .or_else(|| body_seed(&req.body))
                .or_else(|| first_user_seed(&req.body))
                .unwrap_or_default();
            derived_session(
                &identity_key(req, token, org),
                &seed,
            )
        }
    };
    (session, crate::ids::uuid_v4())
}

/// 派生的账号维度：账号 id 优先，令牌里的用户身份次之，最后退到令牌本身
/// —— 同一账号的会话必须落在同一个派生结果上。
fn identity_key(req: &ChatRequest, token: &str, org: &str) -> String {
    if !req.account.is_empty() {
        return req.account.clone();
    }
    let user = super::jwt::decode_jwt_claims(token)
        .map(|claims| super::jwt::extract_user_id(&claims))
        .filter(|user| !user.is_empty());
    match user {
        Some(user) => format!("{org}|{user}"),
        None => format!("{org}|{token}"),
    }
}

fn derived_session(identity: &str, seed: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(SESSION_DERIVATION_PREFIX.as_bytes());
    hash.update(identity.as_bytes());
    hash.update(b"\0");
    hash.update(seed.as_bytes());
    let digest = hash.finalize();
    let suffix: String = digest[..4].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sess-0-{suffix}")
}

/// 官方形态：`sess-<十进制>-<8 位 hex>`，计数段与十六进制段都不可缺。
fn official_session(value: &str) -> Option<String> {
    let rest = value.strip_prefix("sess-")?;
    let (counter, hex) = rest.split_once('-')?;
    let counter_ok = !counter.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit());
    let hex_ok = hex.len() == 8 && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    (counter_ok && hex_ok).then(|| value.to_string())
}

/// 请求体里的会话字段（按官方客户端下发顺序取第一个命中的）。
fn body_seed(body: &Value) -> Option<String> {
    for pointer in [
        "/conversation_id",
        "/conversationId",
        "/session_id",
        "/sessionId",
        "/prompt_cache_key",
        "/promptCacheKey",
    ] {
        if let Some(value) = body.pointer(pointer).and_then(Value::as_str).and_then(identifier) {
            return Some(value);
        }
    }
    None
}

fn first_user_seed(body: &Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|message| message.to_string())
}

/// 入站会话标识的读取面：官方形态头优先，兼容调用方的常见会话头与
/// Codex 元数据封装（这些值不合规时仅作派生种子）。
fn session_id(headers: &BTreeMap<String, String>, body: &Value) -> Option<String> {
    for name in [
        "x-raccoon-session-id",
        "x-conversation-id",
        "ah-thread-id",
        "x-opencode-session",
        "x-session-id",
        "x-session-affinity",
        "session_id",
        "session-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| identifier(value)) {
            return Some(value);
        }
    }
    if let Some(value) = codex_metadata(headers, "session_id") {
        return Some(value);
    }
    for pointer in [
        "/session_id",
        "/conversation_id",
        "/metadata/session_id",
        "/metadata/conversation_id",
    ] {
        if let Some(value) = body
            .pointer(pointer)
            .and_then(Value::as_str)
            .and_then(identifier)
        {
            return Some(value);
        }
    }
    if let Some(value) = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .and_then(identifier)
    {
        return Some(value);
    }
    let metadata = body.pointer("/metadata/user_id").and_then(Value::as_str)?.trim();
    if metadata.starts_with('{') {
        let data: Value = serde_json::from_str(metadata).ok()?;
        return data
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(identifier);
    }
    let (device, session) = metadata.strip_prefix("user_")?.split_once("_account__session_")?;
    (device.len() == 64 && device.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| identifier(session))
        .flatten()
}

fn codex_metadata(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    let raw = headers
        .get("x-codex-turn-metadata")
        .filter(|value| value.len() <= 16384)?;
    let value: Value = serde_json::from_str(raw).ok()?;
    value.get(name).and_then(Value::as_str).and_then(identifier)
}

fn identifier(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_session_shapes_pass_through_verbatim() {
        assert_eq!(
            official_session("sess-0-0123abcd").as_deref(),
            Some("sess-0-0123abcd")
        );
        assert_eq!(
            official_session("sess-17-deadbeef").as_deref(),
            Some("sess-17-deadbeef")
        );
        // 缺计数段 / hex 过长过短 / 非十六进制都不算官方形态。
        assert_eq!(official_session("sess-0123abcd"), None);
        assert_eq!(official_session("sess-0-0123abcde"), None);
        assert_eq!(official_session("sess-0-0123abcg"), None);
        assert_eq!(official_session("sess--0123abcd"), None);
    }

    #[test]
    fn derived_sessions_are_stable_per_identity_and_seed() {
        let first = derived_session("account-1", "conv-a");
        let second = derived_session("account-1", "conv-a");
        let other_seed = derived_session("account-1", "conv-b");
        let other_identity = derived_session("account-2", "conv-a");
        assert_eq!(first, second);
        assert_ne!(first, other_seed);
        assert_ne!(first, other_identity);
    }

    #[test]
    fn derived_sessions_use_the_official_shape() {
        let value = derived_session("account-1", "conv-a");
        assert!(value.starts_with("sess-0-"));
        let hex = value.trim_start_matches("sess-0-");
        assert_eq!(hex.len(), 8);
        assert!(hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(official_session(&value).as_deref(), Some(value.as_str()));
    }

    #[test]
    fn turn_ids_are_fresh_uuid_v4_per_request() {
        let first = crate::ids::uuid_v4();
        let second = crate::ids::uuid_v4();
        assert_ne!(first, second);
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(parts.iter().map(|part| part.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(first.chars().all(|ch| ch == '-' || ch.is_ascii_hexdigit()));
        assert_eq!(&first[14..15], "4");
        assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
    }
}
