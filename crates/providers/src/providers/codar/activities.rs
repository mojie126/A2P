//! 科搭活动：只接入官方免费模型活动的查询与领取。
//!
//! 查询不自动领取；领取后立刻回查，结果不明时在短时间内拒绝重复提交
//! （活动领取没有幂等标记，重复提交可能重复消耗资格）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{client, endpoints, refresh};

const FREE_MODEL: &str = "codar-free-model";
const CONFIRM_GUARD_MS: i64 = 10 * 60 * 1000;

/// 领取结果待确认的账号（账号 id → 截止时刻）。
static PENDING: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn timestamp(value: &Value) -> Option<i64> {
    let number = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))?;
    Some(if number < 100_000_000_000 { number.saturating_mul(1000) } else { number })
}

async fn token_of(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    Ok(refresh::ensure(ctx, account, false).await.map_err(|error| error.message)?.access_token)
}

async fn status(
    token: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, String> {
    let (code, body) = client::request(
        "GET",
        endpoints::API_BASE,
        endpoints::FREE,
        Some(token),
        None,
        proxy,
        gateway_proxy,
        8_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&code) || client::failed(&body) {
        return Err(client::upstream_error(code, &body).message);
    }
    let data = client::data(&body);
    if !data.is_object() {
        return Err("科搭免费模型活动返回格式无效".to_string());
    }
    Ok(data.clone())
}

fn campaign(data: &Value, confirming: bool) -> Value {
    let status = match data.get("status").and_then(Value::as_str) {
        Some("available") if confirming => "confirming",
        Some("available") => "claimable",
        Some("active" | "claimed") => "claimed",
        Some("expired" | "ended") => "expired",
        _ => "ineligible",
    };
    json!({
        "campaignId": FREE_MODEL,
        "title": "科搭新用户免费模型",
        "description": "领取官方当前提供的免费模型活动；资格、支持模型和有效期以上游为准。",
        "status": status,
        "startsAt": data.get("start_at").and_then(timestamp),
        "endsAt": data.get("end_at").and_then(timestamp),
        "expiresAt": data.get("free_until").and_then(timestamp),
        "claimedAt": data.get("claimed_at").and_then(timestamp),
        "validDays": data.get("valid_days").cloned().unwrap_or(Value::Null),
    })
}

pub async fn preview(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let token = token_of(ctx, account).await?;
    let proxy = client::account_proxy(ctx, account).await;
    let data = status(&token, proxy.as_ref(), &ctx.env.proxy).await?;
    let confirming = pending(account);
    Ok(json!({
        "campaigns": [campaign(&data, confirming)],
        "notice": "此查询不会自动领取活动",
        "riskNote": "免费期结束后模型可能按积分计费，以官方活动说明为准",
        "officialUrl": "https://codar.speedai.com",
    }))
}

pub async fn claim(ctx: &PluginCtx, account: &str, id: &str) -> Result<Value, String> {
    let id = if id.trim().is_empty() { FREE_MODEL } else { id };
    if id != FREE_MODEL {
        return Ok(json!({
            "campaignId": id,
            "outcome": "inactive",
            "message": "该科搭活动尚未接入领取",
        }));
    }
    let token = token_of(ctx, account).await?;
    let proxy = client::account_proxy(ctx, account).await;
    let before = status(&token, proxy.as_ref(), &ctx.env.proxy).await?;
    if matches!(before.get("status").and_then(Value::as_str), Some("active" | "claimed")) {
        return Ok(json!({
            "campaignId": FREE_MODEL,
            "outcome": "already_claimed",
            "message": "科搭免费模型活动已领取",
            "campaign": campaign(&before, false),
        }));
    }
    if before.get("status").and_then(Value::as_str) != Some("available") {
        return Ok(json!({
            "campaignId": FREE_MODEL,
            "outcome": "inactive",
            "message": "当前账号没有可领取的科搭免费模型活动",
            "campaign": campaign(&before, false),
        }));
    }
    if pending(account) {
        return Ok(json!({
            "campaignId": FREE_MODEL,
            "outcome": "confirming",
            "message": "上次领取结果未确认，本次只回查，不重复提交",
            "campaign": campaign(&before, true),
        }));
    }
    mark_pending(account);
    let submitted = client::request(
        "POST",
        endpoints::API_BASE,
        endpoints::FREE_CLAIM,
        Some(&token),
        None,
        proxy.as_ref(),
        &ctx.env.proxy,
        8_000,
    )
    .await;
    let after = status(&token, proxy.as_ref(), &ctx.env.proxy).await.ok();
    let confirmed = after.as_ref().filter(|data| {
        matches!(data.get("status").and_then(Value::as_str), Some("active" | "claimed"))
    });
    if let Some(data) = confirmed {
        clear_pending(account);
        return Ok(json!({
            "campaignId": FREE_MODEL,
            "outcome": "claimed",
            "message": "科搭已确认免费模型活动领取成功",
            "campaign": campaign(data, false),
        }));
    }
    if let Err(error) = submitted {
        if (400..500).contains(&error.status_code) {
            clear_pending(account);
            return Ok(json!({
                "campaignId": FREE_MODEL,
                "outcome": "inactive",
                "message": error.message,
                "campaign": campaign(&before, false),
            }));
        }
    }
    Ok(json!({
        "campaignId": FREE_MODEL,
        "outcome": "confirming",
        "message": "科搭活动领取结果未确认，请稍后在官方页面核对；短时间内不会重复提交",
        "campaign": campaign(&before, true),
    }))
}

/// 活动中心清单（只读）。
pub async fn mine(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let token = token_of(ctx, account).await?;
    let proxy = client::account_proxy(ctx, account).await;
    let (code, body) = client::request(
        "GET",
        endpoints::API_BASE,
        endpoints::ACTIVITIES,
        Some(&token),
        None,
        proxy.as_ref(),
        &ctx.env.proxy,
        5_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&code) || client::failed(&body) {
        return Err(client::upstream_error(code, &body).message);
    }
    Ok(client::data(&body).clone())
}

fn pending(account: &str) -> bool {
    PENDING
        .lock()
        .ok()
        .and_then(|guard| guard.get(account).copied())
        .is_some_and(|until| until > crate::http::now_ms())
}

fn mark_pending(account: &str) {
    if let Ok(mut guard) = PENDING.lock() {
        guard.insert(account.to_string(), crate::http::now_ms() + CONFIRM_GUARD_MS);
    }
}

fn clear_pending(account: &str) {
    if let Ok(mut guard) = PENDING.lock() {
        guard.remove(account);
    }
}
