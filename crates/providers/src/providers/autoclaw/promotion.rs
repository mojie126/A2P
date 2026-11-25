//! AutoClaw 的**活动福利到账确认**（Weekly Perks / 登录奖励这类「活跃即发」活动）。
//!
//! ── 上游是什么（从客户端产物 + 实测确认，2026-10-13 / 10-15）──────
//! 这类活动**没有领取接口**：服务端在用户「登录活跃」时自动入账。实测证据：
//!
//!   ```text
//!   2026-10-13 16:18:46  桌面端打开 → token 刷新（JWT iat 同时刻）
//!   2026-10-13 16:18:48  流水 +800「AutoClaw Weekly Perks +800 credits」
//!   2026-10-15 22:04:27  user-info 打下去（登录）
//!   2026-10-15 22:04:28  +20000「AutoClaw 用户登录奖励积分，7天有效」
//!                        +500  （同日、同 event_type，国际版是 800）
//!   ```
//!
//! 2026-10-15 逐条测过：签到本身、`refresh` 的 force 刷新、home-fetch 都**不**
//! 触发发放，只有 **`POST /userapi/v1/user-info`**（客户端启动的固定动作）会 ——
//! 服务端把「登录」的判据挂在它上面，入账流水带 `event_type: autoclaw_user_active`、
//! `grant_date`（北京日期）与 `source: campaign`。触发动作在 `checkin.rs` 的
//! `trigger_login_active`（见那边模块头）；本模块只管读积分流水回答
//! 「今天的活动福利到账了吗」。
//!
//! ── 判据：event_type 为主、activity_id 前缀兜底 ─────────────
//! 10-15 实测的两笔：20000 的 `activity_id` 是 `autoclaw_user_active_bonus_20261015`
//! （一次性大礼包），500/800 的是 `autoclaw_user_active_reward_202609`（日福利）。
//! 同一活动期内**两笔前缀不同** —— 只按单一前缀匹配会漏掉 20000 那笔。因此判据
//! 以 `metadata.event_type == "autoclaw_user_active"` 为主（两笔都带），前缀列表
//! 只作 event_type 缺失时的兜底（活动换期换后缀 / 换家族也不受影响）。
//!
//! ── 一天可能多笔：全部收回、给出合计与明细 ────────────────────
//! `granted: true` 时 `points` 是**当日全部活动入账的合计**（如 20000 + 500 =
//! 20500），`entries` 逐笔给出 amount / description / activity_id —— 签到消息用
//! 合计（「…（活动福利 +20500 已到账）」），需要拆分的调用方读 `entries`。
//!
//! ── 查询失败不影响签到结论 ──────────────────────────────────
//! 与 `balance.rs` 里「订阅失败不影响积分结果」同一取向：活动福利是附加
//! 信息，查不到（网络抖 / 活动结束）就给 `null`，绝不把它升级成签到失败。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic（取首元素用 `first()`）。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::beijing;
use crate::errors::GatewayError;

use super::balance::{userapi_get, userapi_post};
use super::credentials;
use super::region::Region;

/// 积分流水路径（只取**入账**方向：模型消费的 debit 不会挤占窗口）。
///
/// `flow_direction` 的合法值是 `income`（实测：`credit` / `in` 都是 400001）。
/// `page_size` 取 50：活动期一天可能多笔（签到 / 登录奖励 / 任务），给「今天」
/// 留足回溯余量。
const LEDGERS_PATH: &str = "/agent-assetmgr/api/v1/ledgers_std?asset_type=point&wallet_type=all&cursor=&page_size=50&flow_direction=income";

/// 流水单页条数（与 `LEDGERS_PATH` 的 `page_size` 一致；满页时无法确认今日窗口完整）
const LEDGER_PAGE_SIZE: usize = 50;

/// 「活跃即发」活动的流水事件名前缀（`metadata.event_type`）。
///
/// 2026-10-15 实测：登录奖励的每一笔（一次性大礼包与日福利）都带
/// `autoclaw_user_active`，是比 activity_id 前缀更稳的判据 —— 活动换期会换
/// activity_id 后缀，甚至换前缀家族（见模块头）。
/// `autoclaw_user_signup_day`：登录动作还会带出**新手期**的「注册 Day 1-7 奖励」
/// （event_type 形如 `autoclaw_user_signup_day_1`，实测 2026-09-22 国际版
/// +10000「New-user credits (Day 1/7)」）—— 同一个发放钩子，一并确认。
/// `autoclaw_weekly_perks`：2026-11 起的 Weekly Perks 系列（event_type 带期号）。
const ACTIVE_EVENT_TYPE_PREFIXES: [&str; 3] = [
    "autoclaw_user_active",
    "autoclaw_user_signup_day",
    "autoclaw_weekly_perks",
];

/// activity_id 前缀兜底（event_type 缺失的老数据 / 变体）。
///
/// `autoclaw_user_active_reward`：日福利（Weekly Perks，如 202609 期）；
/// `autoclaw_user_active_bonus`：一次性大礼包（如 20261015 那次 20000）；
/// `autoclaw_new_user_day_1_to_7_bonus`：新手 7 天登录礼；
/// `autoclaw_weekly_perks`：新版 Weekly Perks（activity_id 带期号）。
const ACTIVE_ACTIVITY_PREFIXES: [&str; 4] = [
    "autoclaw_user_active_reward",
    "autoclaw_user_active_bonus",
    "autoclaw_new_user_day_1_to_7_bonus",
    "autoclaw_weekly_perks",
];

/// 活动流水的完整读数（登录福利 / Weekly Perks 共用）。
///
/// 返回 `{granted, points, description, grantDate, entries, incomplete, latest}`：
/// 今日匹配到入账时给合计与明细，否则给 `latest`（最近一笔活动入账）与
/// `incomplete`（今日流水窗口被单页上限截断，无法确认是否已到账）。
/// 判据是北京日期键（`core::beijing`，与签到链路同一个「今天」口径）。
pub async fn inspect(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = super::balance::record_for(region, ctx, account_id).await?;
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            404,
            format!(
                "AutoClaw {}账号 {account_id} 不存在或不属于该地区",
                region.label()
            ),
        ));
    }
    let credentials = credentials::snapshot_for(record.as_ref(), region)?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法查询活动福利"));
    }
    inspect_credentials(&credentials).await
}

/// 与 [`inspect`] 同一读数，凭证由调用方提供（活动领取要求复核用同一份凭证）
pub async fn inspect_credentials(
    credentials: &credentials::AutoClawCredentials,
) -> Result<Value, GatewayError> {
    let payload = userapi_get(credentials, LEDGERS_PATH, "活动福利查询").await?;
    let today = beijing::today_key();
    let entries: &[Value] = payload
        .get("data")
        .and_then(|data| data.get("entries"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let matched: Vec<&Value> = entries.iter().filter(|entry| is_reward(entry)).collect();
    let latest = matched.first().map(|entry| entry_detail(entry)).unwrap_or(Value::Null);
    let found: Vec<&Value> = matched
        .into_iter()
        .filter(|entry| grant_date(entry) == today)
        .collect();
    let incomplete = found.is_empty()
        && entries.len() >= LEDGER_PAGE_SIZE
        && entries
            .last()
            .map(|entry| grant_date(entry))
            .is_none_or(|date| date >= today.as_str());
    if found.is_empty() {
        return Ok(json!({
            "granted": false,
            "grantDate": today,
            "incomplete": incomplete,
            "latest": latest,
        }));
    }
    let mut total: i64 = 0;
    let mut known = !incomplete && entries.len() < LEDGER_PAGE_SIZE;
    let detail: Vec<Value> = found
        .iter()
        .map(|entry| {
            let points = entry.get("amount").and_then(Value::as_i64);
            match points.and_then(|points| total.checked_add(points)) {
                Some(sum) => total = sum,
                None => known = false,
            }
            entry_detail(entry)
        })
        .collect();
    Ok(json!({
        "granted": true,
        "points": if known { json!(total) } else { Value::Null },
        "description": found
            .first()
            .and_then(|entry| entry.get("description"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        "grantDate": today,
        "entries": detail,
        "incomplete": false,
        "latest": latest,
    }))
}

/// 今天的活动福利到账状态（签到链路用；未到账不因窗口不完整而报错）。
pub async fn today_reward(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let report = inspect(ctx, region, account_id).await?;
    if report.get("granted").and_then(Value::as_bool) == Some(true) {
        return Ok(report);
    }
    Ok(json!({
        "granted": false,
        "grantDate": report.get("grantDate").cloned().unwrap_or(json!(beijing::today_key())),
    }))
}

/// 触发一次「登录活跃」（客户端启动同款 user-info），供活动领取复核使用
/// 触发一次「登录活跃」（客户端启动同款 user-info），供活动领取复核使用
pub async fn activate_credentials(
    credentials: &credentials::AutoClawCredentials,
) -> Result<(), GatewayError> {
    let body = json!({ "source_id": "autoclaw", "device_id": credentials.device_id });
    userapi_post(credentials, super::checkin::USER_INFO_PATH, &body, "触发登录活跃").await?;
    Ok(())
}

/// 一笔活动入账的明细（`entries` / `latest` 的元素）。
fn entry_detail(entry: &Value) -> Value {
    let metadata = entry.get("metadata");
    json!({
        "points": entry.get("amount").and_then(Value::as_i64),
        "grantDate": grant_date(entry),
        "description": entry.get("description").and_then(Value::as_str).unwrap_or(""),
        "activityId": metadata
            .and_then(|metadata| metadata.get("activity_id"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        "eventType": metadata
            .and_then(|metadata| metadata.get("event_type"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    })
}

/// 一笔流水的入账日期（`metadata.grant_date`，缺失给空串）
fn grant_date(entry: &Value) -> &str {
    entry
        .get("metadata")
        .and_then(|metadata| metadata.get("grant_date"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// 这条流水是不是活动福利入账。
///
/// 判据见模块头：`event_type` 命中活动前缀（含 Weekly Perks），
/// 缺失时退回 `activity_id` 前缀兜底。
fn is_reward(entry: &Value) -> bool {
    let metadata = match entry.get("metadata") {
        Some(metadata) => metadata,
        None => return false,
    };
    let event_type = metadata.get("event_type").and_then(Value::as_str).unwrap_or("");
    if ACTIVE_EVENT_TYPE_PREFIXES
        .iter()
        .any(|prefix| event_type.starts_with(prefix))
        || event_type.contains("weekly_perks")
    {
        return true;
    }
    let activity_id = metadata.get("activity_id").and_then(Value::as_str).unwrap_or("");
    ACTIVE_ACTIVITY_PREFIXES
        .iter()
        .any(|prefix| activity_id.starts_with(prefix))
        || activity_id.contains("weekly_perks")
}
