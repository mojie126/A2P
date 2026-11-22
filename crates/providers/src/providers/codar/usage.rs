//! 科搭点数读数：通用点数、Agent 点数与改写点数分别展示。
//!
//! 上游口径（官方插件实测）：通用点数走 `POST /v1/gettoken`（body 带用户名），
//! Agent 点数在同一条响应或 `POST /v1/agent/balance` 里，改写点数为附加项。
//! 读不到的项留空，不填 0。
//!
//! 剩余为 0 的点数包整个不输出（与百度搭子同一口径：用完的包不在任何地方展示，
//! 含额度详情），只剩有效包参与窗口列表。

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

/// 三条点数读数翻成窗口行：剩余为 0 的整条不输出。
///
/// 0 点与「没有这一项」对用户是同一件事，列出来只会让「通用 / Agent / 改写」
/// 三行里混进一条花不出去的读数（与百度搭子 `package_windows` 同一口径）。
fn point_windows(points: &Points) -> Vec<Value> {
    let mut windows = Vec::new();
    for (id, name, value, aside) in [
        ("general", "通用点数", points.general, false),
        ("agent", "Agent 点数", points.agent, false),
        ("rewrite", "改写点数", points.rewrite, true),
    ] {
        let Some(balance) = value.filter(|balance| *balance > 0.0) else { continue };
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
    windows
}

/// 顶层「可用合计」取非附加项（通用与 Agent）的正剩余之和。
///
/// 改写点数是附加项（`aside`），不参与合计；写 0 会让组头「可用合计」
/// 与账号行余额列把这家读成「余额为零」，而实际含义只是「这一项用完了」。
fn top_balance(points: &Points) -> Option<f64> {
    let total: f64 = [points.general, points.agent]
        .into_iter()
        .flatten()
        .sum();
    (total > 0.0).then_some(total)
}

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<proto::UsageReport, String> {
    let credentials = refresh::ensure(ctx, account, false)
        .await
        .map_err(|error| error.message)?;
    let proxy = client::account_proxy(ctx, account).await;
    let points = points(&credentials.access_token, &credentials.username, proxy.as_ref(), &ctx.env.proxy).await?;
    let mut report = json!({
        "provider": PROVIDER_ID,
        "unit": "点",
        "plan": "科搭点数账户",
        "user": credentials.username,
        "windows": point_windows(&points),
        "message": "通用、Agent 与改写点数分别展示，不把活动免费期当成现金余额",
    });
    if let Some(general) = top_balance(&points) {
        report["balance"] = json!(general);
        report["totalLeft"] = json!(general);
        report["available"] = json!(general);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{point_windows, top_balance, Points};

    /// 剩余为 0 的点数包既不进窗口行，也不参与顶层合计。
    ///
    /// 实测形态（截图）：通用 500、Agent 2000、改写 0 —— 改写那一行必须在
    /// 额度详情里整个消失，而顶层「可用合计」取通用与 Agent 之和的 2500。
    #[test]
    fn zero_packs_are_omitted() {
        let points = Points { general: Some(500.0), agent: Some(2000.0), rewrite: Some(0.0) };
        let windows = point_windows(&points);
        let ids: Vec<&str> = windows.iter().filter_map(|row| row["id"].as_str()).collect();
        assert_eq!(ids, vec!["general", "agent"]);
        assert_eq!(top_balance(&points), Some(2500.0));
    }

    /// 通用点数用尽时顶层合计只剩 Agent 点数，不读成「余额为零」。
    #[test]
    fn drained_general_leaves_no_top_balance() {
        let points = Points { general: Some(0.0), agent: Some(2000.0), rewrite: None };
        let windows = point_windows(&points);
        let ids: Vec<&str> = windows.iter().filter_map(|row| row["id"].as_str()).collect();
        assert_eq!(ids, vec!["agent"]);
        assert_eq!(top_balance(&points), Some(2000.0));
    }

    /// 三条都读不到时窗口为空，顶层余额也不凭空补 0。
    #[test]
    fn all_missing_produces_empty_windows() {
        let points = Points { general: None, agent: None, rewrite: None };
        assert!(point_windows(&points).is_empty());
        assert_eq!(top_balance(&points), None);
    }

    /// 窗口行保留既有字段形状：单位「点」、改写点数属附加项（aside）。
    #[test]
    fn window_shape_stays_stable() {
        let points = Points { general: Some(1.5), agent: None, rewrite: Some(3.0) };
        let windows = point_windows(&points);
        assert_eq!(windows[0]["unit"], json!("点"));
        assert_eq!(windows[0]["display"], json!("1.50"));
        assert_eq!(windows[1]["aside"], json!(true));
    }
}
