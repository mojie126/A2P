//! 科搭每日签到：先查状态，未签再提交，提交后回查确认。
//!
//! 上游按滚动 24 小时窗口发放。本实现不重复提交：状态查询确认已签即返回，
//! 提交后回查仍未见已签时如实报告「结果待确认」，不自动重试。

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::{client, endpoints, refresh};

/// 签到状态。
struct Status {
    signed: bool,
}

async fn status(
    token: &str,
    username: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Status, String> {
    let (code, body) = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::SIGN_STATUS,
        Some(token),
        Some(&json!({ "username": username })),
        proxy,
        gateway_proxy,
        5_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&code) || client::failed(&body) {
        return Err(client::upstream_error(code, &body).message);
    }
    let data = client::data(&body);
    let signed = data
        .as_bool()
        .or_else(|| {
            ["isSigned", "signed", "hasSigned", "has_signed"]
                .iter()
                .find_map(|key| data.get(*key).and_then(Value::as_bool))
        })
        .ok_or_else(|| "科搭没有返回明确签到状态，未提交领取".to_string())?;
    Ok(Status { signed })
}

pub async fn claim(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let credentials = match refresh::ensure(ctx, account, false).await {
        Ok(credentials) => credentials,
        Err(error) => {
            return Ok(proto::CheckinReport {
                outcome: if error.status_code == 401 {
                    proto::CheckinOutcome::Ineligible
                } else {
                    proto::CheckinOutcome::Failed
                },
                credit: None,
                streak: None,
                message: Some(error.message),
            })
        }
    };
    let token = &credentials.access_token;
    let username = &credentials.username;
    let proxy = client::account_proxy(ctx, account).await;
    let gateway_proxy = &ctx.env.proxy;
    let before = match status(token, username, proxy.as_ref(), gateway_proxy).await {
        Ok(status) => status,
        Err(message) => {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(message),
            })
        }
    };
    if before.signed {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            streak: None,
            message: Some("科搭本轮签到已领取，没有重复提交".to_string()),
        });
    }
    let submitted = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::SIGN,
        Some(token),
        Some(&json!({ "username": username })),
        proxy.as_ref(),
        gateway_proxy,
        8_000,
    )
    .await;
    let ok = matches!(&submitted, Ok((status, body)) if (200..300).contains(status) && !client::failed(body));
    let after = status(token, username, proxy.as_ref(), gateway_proxy)
        .await
        .ok()
        .filter(|status| status.signed);
    if let Some(_after) = after {
        let credit = submitted
            .ok()
            .filter(|(status, body)| (200..300).contains(status) && !client::failed(body))
            .and_then(|(_, body)| {
                client::data(&body)
                    .get("granted")
                    .and_then(Value::as_f64)
                    .filter(|value| *value > 0.0)
                    .map(|value| format!("{value} 点"))
            });
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit,
            streak: None,
            message: Some("科搭已确认签到发放".to_string()),
        });
    }
    if ok {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            streak: None,
            message: Some("科搭签到已提交，状态回查尚未确认，请到官方页面核对".to_string()),
        });
    }
    let message = match submitted {
        Ok((status, body)) => client::upstream_error(status, &body).message,
        Err(error) => error.message,
    };
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Failed,
        credit: None,
        streak: None,
        message: Some(message),
    })
}
