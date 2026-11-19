//! 百度搭子的余额：网页端积分总览（读接口，不计费）。
//!
//! ── 上游形状 ────────────────────────────────────────────────
//! `quota_overview` 返回 `{totalPoints, usedPoints, isSubscribed,
//! modelThrottleInfo:{throttled}, subscription:[…], incremental:[…]}`，
//! 每个套餐带 `{packageType, source, totalPoints, usedPoints, startDate,
//! expireDate}`。读不到的字段留空，不填 0。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::client::{self, WebError};
use super::{endpoints, PROVIDER_ID};

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .filter(|number| number.is_finite() && *number >= 0.0)
}

pub fn format_number(value: f64) -> String {
    if value.fract().abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

/// 把一组套餐记录翻成窗口行（订阅与加油包同形状）。
///
/// 剩余为 0 的积分包整个不输出（用完的包不在任何地方展示，含额度详情），
/// 只剩有效包参与窗口列表。
fn package_windows(list: Option<&Value>, label: &str) -> Vec<Value> {
    let Some(items) = list.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let total = number(item.get("totalPoints"))?;
            let used = number(item.get("usedPoints")).unwrap_or(0.0);
            let left = (total - used).max(0.0);
            if left <= 0.0 {
                return None;
            }
            let kind = item
                .get("packageType")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut window = json!({
                "name": format!("{label}·{kind}"),
                "label": format!("{label}·{kind}"),
                "balance": left,
                "remaining": left,
                "display": format_number(left),
                "unit": "积分",
                "aside": true,
            });
            if let Some(expire) = item.get("expireDate").filter(|value| !value.is_null()) {
                window["runsOutAt"] = expire.clone();
            }
            Some(window)
        })
        .collect()
}

/// 查一次余额。
pub async fn query(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.cookie.is_empty() {
        return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
    }
    let value = match client::web_json(
        &credentials,
        reqwest::Method::GET,
        endpoints::QUOTA_OVERVIEW_PATH,
        "查询积分余额",
    )
    .await
    {
        Ok(value) => value,
        Err(WebError::Expired(message)) => return Err(message),
        Err(WebError::Failed(message)) => {
            return Ok(json!({
                "provider": PROVIDER_ID,
                "plan": "百度搭子",
                "windows": [],
                "error": message,
            }))
        }
    };
    let total = number(value.get("totalPoints"));
    let used = number(value.get("usedPoints"));
    let left = match (total, used) {
        (Some(total), Some(used)) => Some((total - used).max(0.0)),
        _ => None,
    };
    let mut windows: Vec<Value> = Vec::new();
    if let Some(left) = left {
        windows.push(json!({
            "name": "总余额",
            "label": "总余额",
            "balance": left,
            "remaining": left,
            "display": format_number(left),
            "unit": "积分",
            "aside": false,
        }));
    }
    windows.extend(package_windows(value.get("subscription"), "订阅"));
    windows.extend(package_windows(value.get("incremental"), "加油包"));
    let subscribed = value.get("isSubscribed").and_then(Value::as_bool);
    let mut report = json!({
        "provider": PROVIDER_ID,
        "unit": "积分",
        "plan": "百度搭子",
        "windows": windows,
    });
    if let Some(balance) = left {
        report["balance"] = json!(balance);
        // 组头「可用合计」与账号行「余额」列的取数字段是顶层的 totalLeft /
        // available（见 ui-shared/usage-readings.ts），只写 balance 界面读不到。
        report["totalLeft"] = json!(balance);
        report["available"] = json!(balance);
    }
    if let Some(used) = used {
        report["used"] = json!(used);
    }
    if let Some(total) = total {
        report["limit"] = json!(total);
    }
    if subscribed == Some(true) {
        report["plan"] = json!("百度搭子（订阅）");
    }
    if value
        .pointer("/modelThrottleInfo/throttled")
        .and_then(Value::as_bool)
        == Some(true)
    {
        report["error"] = json!("该账号当前处于限速状态（上游 modelThrottleInfo）");
    }
    Ok(report)
}

/// 积分流水（`charge` = 收入，否则支出）。
///
/// 参数与官方一致：`startAt` / `endAt` 毫秒时间戳（缺省最近 30 天、上限一年），
/// `page`（1..1000）、`limit`（1..100）。只读接口。
pub async fn records(
    ctx: &PluginCtx,
    account: &str,
    payload: &Value,
    charge: bool,
) -> Result<Value, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.cookie.is_empty() {
        return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
    }
    let now = crate::http::now_ms();
    let start = timestamp_ms(payload.get("startAt")).unwrap_or(now - 30 * 86_400_000);
    let end = timestamp_ms(payload.get("endAt")).unwrap_or(now);
    if start >= end || end - start > 366 * 86_400_000 {
        return Err("积分明细时间范围必须在一年以内".to_string());
    }
    let page = payload
        .get("page")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 1000);
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100);
    let path = format!(
        "/api/dumate/points/records/{}?startAt={}&endAt={}&page={page}&limit={limit}",
        if charge { "charge" } else { "usage" },
        start / 1000,
        end / 1000
    );
    web_get(&credentials, &path, "查询积分明细").await
}

/// 成长任务只读清单（不提交完成、不领取）。
pub async fn activity_tasks(
    ctx: &PluginCtx,
    account: &str,
) -> Result<Value, String> {
    web_get_for_account(ctx, account, endpoints::TASKS, "查询成长任务").await
}

/// 抽奖状态只读（不抽奖、不兑换）。
pub async fn activity_draw_status(
    ctx: &PluginCtx,
    account: &str,
) -> Result<Value, String> {
    web_get_for_account(ctx, account, endpoints::DRAW_STATUS, "查询抽奖状态").await
}

async fn web_get_for_account(
    ctx: &PluginCtx,
    account: &str,
    path: &str,
    operation: &str,
) -> Result<Value, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.cookie.is_empty() {
        return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
    }
    web_get(&credentials, path, operation).await
}

async fn web_get(
    credentials: &Credentials,
    path: &str,
    operation: &str,
) -> Result<Value, String> {
    match client::web_json(credentials, reqwest::Method::GET, path, operation).await {
        Ok(value) => Ok(value),
        Err(WebError::Expired(message)) | Err(WebError::Failed(message)) => Err(message),
    }
}

/// 毫秒时间戳或可解析的时间文本 → 毫秒。
fn timestamp_ms(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    let number = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())?;
    Some(if number < 100_000_000_000 {
        number.saturating_mul(1000)
    } else {
        number
    })
    .filter(|value| *value > 0)
}

