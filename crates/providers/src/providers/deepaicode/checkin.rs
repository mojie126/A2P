//! DeepAI Code 的每日签到：调用 `/theos` 触发并锁定当日免费额度。
//!
//! ── 上游机制 ────────────────────────────────────────────────
//! 免费 Theos 按 `(机器指纹, 账号)` 每天发放一次，`/theos` 同时是签到通道：
//! 响应（包括 403）会下发 `check_in.nonce`，下一次请求回传 `X-DeepAI-Code-Check-In`
//! 才算完成。上游对「当天反复重启签到」有防刷保护，命中时回 `403 check_in_refused`，
//! 此时不应继续重试。
//!
//! ── 本实现的判定 ────────────────────────────────────────────
//! 记下每个账号上一次的 `refresh_at`：调用后 `refresh_at` 变化且 `granted > 0`
//! 记为「刚领到」，否则记为「今日已领 / 暂无额度」。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::accounts::Credentials;
use super::client;
use super::PROVIDER_ID;

/// 上一次看到的 `refresh_at`（按 API Key 哈希）。
static LAST_REFRESH: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn state_key(api_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(api_key.as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

pub async fn claim(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 DeepAI 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.api_key.is_empty() {
        return Err("DeepAI 账号缺少 API Key，请重新粘贴".to_string());
    }
    let proxy = client::account_proxy(ctx, account).await;
    let body = match client::theos(&credentials, proxy.as_ref(), &ctx.env.proxy, 20_000).await {
        Ok(body) => body,
        Err(error) => {
            // 403 明确是终态（当日限流 / 邮箱未验证 / 需社交登录），不安排重试
            let terminal = matches!(error.status_code, 401 | 403);
            if error.status_code == 403 {
                client::clear_nonce(&credentials.api_key);
                return Ok(proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Inactive,
                    credit: None,
                    streak: None,
                    message: Some(error.message),
                });
            }
            return Ok(proto::CheckinReport {
                outcome: if terminal {
                    proto::CheckinOutcome::Ineligible
                } else {
                    proto::CheckinOutcome::Failed
                },
                credit: None,
                streak: None,
                message: Some(error.message),
            });
        }
    };
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let granted = number(body.get("granted")).unwrap_or(0.0);
    let refresh_at = body
        .get("refresh_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let key = state_key(&credentials.api_key);
    let previous = LAST_REFRESH.lock().ok().and_then(|mut table| {
        let previous = table.get(&key).cloned();
        if !refresh_at.is_empty() {
            table.insert(key.clone(), refresh_at.clone());
        }
        previous
    });
    if !enabled {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Ineligible,
            credit: None,
            streak: None,
            message: Some("DeepAI 未为该账号启用每日免费额度".to_string()),
        });
    }
    let changed = previous.as_deref() != Some(refresh_at.as_str());
    if granted > 0.0 && (changed || previous.is_none()) {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: Some(format!("{granted} Theos")),
            streak: None,
            message: Some(if refresh_at.is_empty() {
                "DeepAI 今日免费额度已到账".to_string()
            } else {
                format!("DeepAI 今日免费额度已到账，下次刷新 {refresh_at}")
            }),
        });
    }
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Done,
        credit: Some(format!("{granted} Theos")),
        streak: None,
        message: Some(if refresh_at.is_empty() {
            "DeepAI 今日额度已领取或已用完".to_string()
        } else {
            format!("DeepAI 今日额度已领取或已用完，下次刷新 {refresh_at}")
        }),
    })
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .filter(|number| number.is_finite() && *number >= 0.0)
}
