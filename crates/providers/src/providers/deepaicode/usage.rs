//! DeepAI Code 的额度读数：`/deepai_code/theos` + `/get_usage_summary`。
//!
//! ── 两个数据源 ──────────────────────────────────────────────
//!   * `/theos`：`{enabled, balance, granted, refresh_at, lane_version}`，
//!     `balance` / `granted` 是**字符串**（5 位小数的 Theos 数）；
//!   * `/get_usage_summary`：订阅与用量摘要
//!     `{is_pro, plan, credits_remaining, period_description, …}`。
//!
//! 主读数取 Theos 余额（对话扣的就是它），订阅积分作为明细补充。
//! 用量摘要失败不阻塞额度读数。

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::accounts::Credentials;
use super::client;
use super::PROVIDER_ID;

/// 字符串或数字 → 有限非负浮点。
fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn format_number(value: f64) -> String {
    if value.fract().abs() < 0.00001 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.5}")
    }
}

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<proto::UsageReport, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 DeepAI 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.api_key.is_empty() {
        return Err("DeepAI 账号缺少 API Key，请重新粘贴".to_string());
    }
    let proxy = client::account_proxy(ctx, account).await;
    let theos = client::theos(&credentials, proxy.as_ref(), &ctx.env.proxy, 20_000)
        .await
        .map_err(|error| error.message)?;
    let summary = client::usage_summary(&credentials, proxy.as_ref(), &ctx.env.proxy)
        .await
        .ok();

    let enabled = theos.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let balance = number(theos.get("balance"));
    let granted = number(theos.get("granted"));
    let refresh_at = theos
        .get("refresh_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut windows: Vec<Value> = Vec::new();
    if enabled {
        if let Some(balance) = balance {
            let mut window = json!({
                "name": "今日 Theos",
                "label": "今日 Theos",
                "balance": balance,
                "remaining": balance,
                "display": format_number(balance),
                "unit": "Theos",
                "aside": false,
            });
            if let Some(granted) = granted.filter(|granted| *granted > 0.0) {
                window["limit"] = json!(granted);
                window["used"] = json!((granted - balance).max(0.0));
            }
            if !refresh_at.is_empty() {
                window["runsOutAt"] = json!(refresh_at);
            }
            windows.push(window);
        }
    }
    // 剩余为 0 的订阅积分整个不输出（与百度搭子同一口径：用完的包不在任何
    // 地方展示，含额度详情），订阅积分是补充读数，免费账号常有 0 的那种
    if let Some(credits) = summary
        .as_ref()
        .and_then(|value| number(value.get("credits_remaining")))
        .filter(|credits| *credits > 0.0)
    {
        windows.push(json!({
            "name": "订阅积分",
            "label": "订阅积分",
            "balance": credits,
            "remaining": credits,
            "display": format_number(credits),
            "unit": "积分",
            "aside": true,
        }));
    }

    let is_pro = summary
        .as_ref()
        .and_then(|value| value.get("is_pro"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let plan = summary
        .as_ref()
        .and_then(|value| value.get("plan"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| if is_pro { "Pro".to_string() } else { "免费".to_string() });

    let mut report = json!({
        "provider": PROVIDER_ID,
        "unit": "Theos",
        "plan": format!("DeepAI Code（{plan}）"),
        "windows": windows,
    });
    if let Some(balance) = balance {
        report["balance"] = json!(balance);
        report["totalLeft"] = json!(balance);
        report["available"] = json!(balance);
    }
    if let Some(granted) = granted {
        report["limit"] = json!(granted);
    }
    if !refresh_at.is_empty() {
        report["until"] = json!(refresh_at);
    }
    if !enabled {
        report["error"] = json!("DeepAI 未为该账号启用每日免费额度");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{format_number, number};

    /// `credits_remaining` 是数字或 5 位小数字符串；缺失、负数与非数值一律读成 None，
    /// 不把「读不到」当成 0（那会把订阅积分误判成用完了的包）。
    #[test]
    fn credits_remaining_is_parsed_leniently() {
        assert_eq!(number(Some(&json!(12.5))), Some(12.5));
        assert_eq!(number(Some(&json!("0.00000"))), Some(0.0));
        assert_eq!(number(Some(&json!("-1.0"))), None);
        assert_eq!(number(Some(&json!("abc"))), None);
        assert_eq!(number(None), None);
    }

    /// 只有正剩余的订阅积分才进窗口行（与百度搭子 `package_windows` 同一口径）。
    #[test]
    fn zero_credits_are_filtered_out() {
        let zero = number(Some(&json!("0.00000"))).filter(|credits| *credits > 0.0);
        let positive = number(Some(&json!("25.5"))).filter(|credits| *credits > 0.0);
        assert_eq!(zero, None);
        assert_eq!(positive, Some(25.5));
    }

    /// 整数不带小数尾巴、非整数保留 5 位 —— 窗口读数与改动前逐字一致。
    #[test]
    fn format_number_keeps_existing_shape() {
        assert_eq!(format_number(72.0), "72");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(1.25), "1.25000");
    }
}
