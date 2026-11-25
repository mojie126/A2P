//! 百度搭子的每日签到：`loginBonusInfo` 查状态 → `loginBonus` 领取。
//!
//! ── 到账核对 ────────────────────────────────────────────────
//! 领取接口的响应不带奖励金额，只能用余额快照差核对（参考实现同口径）。
//! 上游还有一条特性：每日额度通常由服务端在 0 点自动发放，签到动作只是
//! 确认领取，所以「前后差值」与「接口成功」分别报告，不把两件事混为一谈。
//! 已签时不再打领取接口（幂等但重复调用没有意义）。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::accounts::Credentials;
use super::client::{self, WebError};
use super::usage;
use super::{endpoints, PROVIDER_ID};

pub async fn checkin(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.cookie.is_empty() {
        return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
    }
    let info = match client::web_json(
        &credentials,
        reqwest::Method::GET,
        endpoints::LOGIN_BONUS_INFO_PATH,
        "查询签到状态",
    )
    .await
    {
        Ok(value) => value,
        Err(WebError::Expired(message)) => return Err(message),
        Err(WebError::Failed(message)) => {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(message),
            })
        }
    };
    // 上游字段名有驼峰与下划线两种形态，两种都认（参考实现同口径）。
    let has_issued = info
        .get("hasIssued")
        .or_else(|| info.get("has_issued"))
        .and_then(|value| value.as_bool().or_else(|| value.as_i64().map(|n| n != 0)));
    if has_issued == Some(true)
    {
        let month = month_sign_count(&info);
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            // `signInDays` 是当月已签日期清单，不是连续天数；不冒充连登
            streak: None,
            message: Some(if month > 0 {
                format!("今日已签到（当月已签 {month} 天）")
            } else {
                "今日已签到".to_string()
            }),
        });
    }
    // 动作前取一次余额快照，签完再取一次，差值就是这次的奖励。
    let before = balance_left(ctx, account).await;
    if let Err(error) = client::web_json(
        &credentials,
        reqwest::Method::POST,
        endpoints::LOGIN_BONUS_PATH,
        "每日签到",
    )
    .await
    {
        return match error {
            WebError::Expired(message) => Err(message),
            WebError::Failed(message) => Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(message),
            }),
        };
    }
    let after = balance_left(ctx, account).await;
    let (credit, note) = match (before, after) {
        (Some(before), Some(after)) if after > before => {
            let delta = after - before;
            (
                Some(format!("+{}", usage::format_number(delta))),
                "每日签到完成，奖励已到账".to_string(),
            )
        }
        (Some(before), Some(after)) if (after - before).abs() < f64::EPSILON => {
            (None, "每日签到完成，未观测到新增积分（可能由服务端提前发放）".to_string())
        }
        _ => (None, "每日签到完成，余额读数不完整，无法核对奖励".to_string()),
    };
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Claimed,
        credit,
        streak: None,
        message: Some(note),
    })
}

/// 余额快照（签到前后各取一次；失败不阻断签到，只影响差值核对）。
async fn balance_left(ctx: &PluginCtx, account: &str) -> Option<f64> {
    let report = usage::query(ctx, account).await.ok()?;
    report.get("balance").and_then(|value| value.as_f64())
}

/// 当月已签天数：`signInDays` 是日期清单，按**今天**（北京时间）所在月份过滤并去重后计数
///
/// 上游名单可能带上月尾部，直接数总长会把非本月日期算进来
fn month_sign_count(info: &serde_json::Value) -> usize {
    let mut dates: Vec<String> = ["signInDays", "sign_in_days"]
        .iter()
        .find_map(|key| info.get(*key))
        .and_then(|value| value.as_array())
        .map(|days| {
            days.iter()
                .filter_map(|value| value.as_str())
                .filter(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok())
                .take(366)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    dates.sort();
    dates.dedup();
    let prefix = crate::beijing::today_key().chars().take(7).collect::<String>();
    dates.iter().filter(|date| date.starts_with(&prefix)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_sign_count_ignores_other_months_and_duplicates() {
        let prefix = crate::beijing::today_key().chars().take(7).collect::<String>();
        let list = serde_json::json!([
            format!("{prefix}-01"),
            format!("{prefix}-02"),
            format!("{prefix}-02"),
            "2000-01-31",
            "not-a-date"
        ]);
        let info = serde_json::json!({ "signInDays": list });
        assert_eq!(month_sign_count(&info), 2, "重复日期只算一次，其他月份与非法值不计入");
        assert_eq!(month_sign_count(&serde_json::json!({})), 0);
    }
}
