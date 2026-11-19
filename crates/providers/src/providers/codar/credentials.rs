//! 科搭访问令牌：JWT 格式校验、载荷解析与到期时间。
//!
//! 凭据是科搭签发的 JWT（三段式），只做格式校验与载荷读取，不验签
//! （令牌由上游签发，签名验证在服务端）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use crate::limits;

/// 清洗并校验访问令牌。
pub fn token(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let raw = if raw
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        raw.get(7..).unwrap_or("").trim()
    } else {
        raw
    };
    if raw.is_empty() {
        return Err("请填写科搭签发的完整访问令牌 JWT".to_string());
    }
    if raw.len() > limits::MAX_TOKEN_LENGTH
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err("请填写科搭签发的完整访问令牌 JWT".to_string());
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err("科搭访问令牌必须是完整 JWT，不能包含多个账号".to_string());
    }
    let payload = decode_part(parts[1])?;
    if !payload.is_object() {
        return Err("科搭令牌内容不是有效对象".to_string());
    }
    Ok(raw.to_string())
}

/// 读取 JWT 载荷。
pub fn claims(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    decode_part(part).ok()
}

/// 令牌到期时刻（毫秒，`exp` 为秒）。
pub fn expires(token: &str) -> Option<i64> {
    let claims = claims(token)?;
    let exp = claims.get("exp")?.as_i64()?;
    Some(exp.saturating_mul(1000))
}

/// 载荷里的用户名（去重身份与续期核对用）。
pub fn claim_username(token: &str) -> String {
    claims(token)
        .as_ref()
        .and_then(|claims| {
            ["username", "preferred_username", "user", "sub"]
                .iter()
                .find_map(|key| claims.get(*key).and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

fn decode_part(part: &str) -> Result<Value, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .map_err(|_| "科搭令牌格式无效".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "科搭令牌内容无效".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn make(payload: &str) -> String {
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn token_accepts_three_part_jwt() {
        let jwt = make(r#"{"username":"u1","exp":4102444800}"#);
        assert_eq!(token(&format!("Bearer {jwt}")).unwrap(), jwt);
        assert_eq!(claims(&jwt).unwrap()["username"], "u1");
        assert_eq!(claim_username(&jwt), "u1");
    }

    #[test]
    fn token_rejects_non_jwt() {
        assert!(token("abc").is_err());
        assert!(token("").is_err());
    }
}
