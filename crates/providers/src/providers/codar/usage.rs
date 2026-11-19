//! 科搭点数读数：通用点数、Agent 点数与改写点数分别展示。
//!
//! 上游口径（官方插件实测）：通用点数走 `POST /v1/gettoken`（body 带用户名），
//! Agent 点数在同一条响应或 `POST /v1/agent/balance` 里，改写点数为附加项。
//! 读不到的项留空，不填 0。

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::{client, endpoints, refresh, PROVIDER_ID};

fn amount(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = &value[*key];
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
            .filter(|number| number.is_finite() && *number >= 0.0)
    })
}

fn display(value: f64) -> String {
    if value.fract().abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

struct Points {
    general: Option<f64>,
    agent: Option<f64>,
    rewrite: Option<f64>,
}

async fn points(
    token: &str,
    username: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Points, String> {
    let primary = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::POINTS,
        Some(token),
        Some(&json!({ "username": username, "token": 0 })),
        proxy,
        gateway_proxy,
        8_000,
    )
    .await;
    let mut points = match primary {
        Ok((status, body)) if (200..300).contains(&status) && !client::failed(&body) => {
            let data = client::data(&body);
            Points {
                general: amount(data, &["token", "remaining_points", "points", "balance"]),
                agent: amount(data, &["agent_token", "agentToken", "agent_points", "agentPoints"]),
                rewrite: amount(data, &["rewrite_token", "rewriteToken", "rewrite_points"]),
            }
        }
        Ok((status, body)) => return Err(client::upstream_error(status, &body).message),
        Err(error) => return Err(error.message),
    };
    if points.agent.is_none() {
        if let Ok((status, body)) = client::request(
            "POST",
            endpoints::ACCOUNT_BASE,
            endpoints::AGENT_POINTS,
            Some(token),
            Some(&json!({ "username": username })),
            proxy,
            gateway_proxy,
            5_000,
        )
        .await
        {
            if (200..300).contains(&status) && !client::failed(&body) {
                points.agent = amount(
                    client::data(&body),
                    &[
                        "agent_balance",
                        "balance",
                        "points",
                        "agent_token",
                        "agentToken",
                        "agent_points",
                        "agentPoints",
                        "remaining_points",
                    ],
                );
            }
        }
    }
    if points.general.is_none() && points.agent.is_none() {
        return Err("科搭积分接口没有返回可读余额".to_string());
    }
    Ok(points)
}

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<proto::UsageReport, String> {
    let credentials = refresh::ensure(ctx, account, false)
        .await
        .map_err(|error| error.message)?;
    let proxy = client::account_proxy(ctx, account).await;
    let points = points(&credentials.access_token, &credentials.username, proxy.as_ref(), &ctx.env.proxy).await?;
    let mut windows = Vec::new();
    for (id, name, value, aside) in [
        ("general", "通用点数", points.general, false),
        ("agent", "Agent 点数", points.agent, false),
        ("rewrite", "改写点数", points.rewrite, true),
    ] {
        let Some(balance) = value else { continue };
        windows.push(json!({
            "id": id,
            "name": name,
            "label": name,
            "balance": balance,
            "remaining": balance,
            "display": display(balance),
            "unit": "点",
            "aside": aside,
        }));
    }
    let mut report = json!({
        "provider": PROVIDER_ID,
        "unit": "点",
        "plan": "科搭点数账户",
        "user": credentials.username,
        "windows": windows,
        "message": "通用、Agent 与改写点数分别展示，不把活动免费期当成现金余额",
    });
    if let Some(general) = points.general {
        report["balance"] = json!(general);
        report["totalLeft"] = json!(general);
        report["available"] = json!(general);
    }
    Ok(report)
}
