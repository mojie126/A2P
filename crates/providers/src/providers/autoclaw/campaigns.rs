//! AutoClaw 活动福利：只读预览、手动按项领取与兑换码兑换。
//!
//! ── 上游是什么（移植来源：官方 `wasm/autoclaw/src/campaigns.rs`）──────
//! 三类活动：
//!   - 登录活跃 / Weekly Perks：没有领取接口，由 `user-info` 触发入账，
//!     领取动作 = 上报一次活跃再读流水核对（`promotion::inspect` / `activate`）；
//!   - 客户端任务：`autoclaw-task-list` 列任务，`autoclaw-task-complete` 提交；
//!   - 新手任务：`identity-tasks` 列任务，`identity-tasks/complete` 提交。
//! 兑换码走 `autoglm-codes/redeem`。
//!
//! ── 与上游的差异（原生适配）─────────────────────────────────
//!   - wasm 的 `account::Snapshot` / `AccountLock` 换成「按账号读取凭证 +
//!     进程内异步锁」；网关台账本身按账号隔离，不再写 identity 分组；
//!   - wasm 的同步 `client` 换成异步 `send_raw`，签名头与超时沿用 `refresh`
//!     与 `balance` 的同一份实现（签名分叉就是稳定 400002）；
//!   - 401 沿用上游语义：401 表示写请求未执行，强制续期后同账号重试一次。
//!
//! ── 为什么账本必须存在 ──────────────────────────────────────
//! 任务领取与兑换都是**写请求**。上游在途结果不明时无法判断是否已到账，
//! 重复提交可能被上游判成重复领取甚至风控。账本记 `confirming` 状态：
//! 结果不明时不重放，留给用户核对流水后再次查询；`claimed` 直接返回已领取。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic，取值一律走 Option 链。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::beijing;
use crate::errors::GatewayError;
use crate::logging;

use super::balance::REQUEST_TIMEOUT_MS;
use super::credentials::{self, AutoClawCredentials};
use super::refresh;
use super::region::Region;

/// 领取台账（`claims` 子对象，30 天保留）
const CLAIM_LEDGER: &str = "autoclawCampaignClaims";
/// 兑换台账（`codes` 子对象，90 天保留）
const REDEEM_LEDGER: &str = "autoclawRedemption";

const TASK_LIST: &str = "/autoclaw-proxy/proxy/autoclaw-task-list?lang=zh-CN";
const TASK_COMPLETE: &str = "/autoclaw-proxy/proxy/autoclaw-task-complete";
const IDENTITY_TASKS: &str =
    "/agent-assetmgr/api/v1/identity-tasks?biz_app_id=autoclaw&client_version=2.2.0&os=win";
const IDENTITY_COMPLETE: &str = "/agent-assetmgr/api/v1/identity-tasks/complete";
const REDEEM_PATH: &str = "/agent-assetmgr/api/v1/autoglm-codes/redeem";
const CLIENT_VERSION: &str = "2.2.0";

const CLAIM_MAX_AGE_MS: i64 = 30 * 86_400_000;
const REDEEM_MAX_AGE_MS: i64 = 90 * 86_400_000;
const CLAIM_CAPACITY: usize = 128;
const REDEEM_CAPACITY: usize = 120;
/// 「结果待确认」的重放冷却（上游 120 秒）
const CONFIRM_COOLDOWN_MS: i64 = 120_000;

/// 领取 / 兑换串行化：同一进程内不并发提交两个写请求
static FLOW_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn flow_lock() -> &'static tokio::sync::Mutex<()> {
    FLOW_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 活动编号 → 领取动作
enum Target {
    /// 登录活跃 / Weekly Perks（日期必须等于今天）
    LoginActive(String),
    /// 客户端任务（`daily_` 前缀带日期）
    ClientTask { task_id: String, day: Option<String> },
    /// 新手任务（按分类）
    Newbie(String),
}

impl Target {
    fn parse(raw: &str) -> Option<Self> {
        let id = raw.trim();
        if let Some(day) = id.strip_prefix("login-active:") {
            return valid_day(day).then(|| Self::LoginActive(day.to_string()));
        }
        if let Some(rest) = id.strip_prefix("task:") {
            if let Some((task_id, day)) = rest.split_once(':') {
                return (valid_id(task_id) && task_id != "daily_signin" && valid_day(day))
                    .then(|| Self::ClientTask {
                        task_id: task_id.to_string(),
                        day: Some(day.to_string()),
                    });
            }
            return (valid_id(rest) && rest != "daily_signin").then(|| Self::ClientTask {
                task_id: rest.to_string(),
                day: None,
            });
        }
        if let Some(category) = id.strip_prefix("newbie:") {
            return valid_id(category).then(|| Self::Newbie(category.to_string()));
        }
        None
    }
}

fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 96
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn valid_day(text: &str) -> bool {
    text.len() == 10
        && text.as_bytes().get(4) == Some(&b'-')
        && text.as_bytes().get(7) == Some(&b'-')
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

fn text(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str) {
            let trimmed = found.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|n| n as i64)),
        Value::String(raw) => raw.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn auth_code(code: i64) -> bool {
    super::balance::is_auth_expired_code(code)
}

fn is_auth_error(error: &GatewayError) -> bool {
    error.status_code == 401 || error.upstream_code.is_some_and(auth_code)
}

// ─── 凭证与刷新 ──────────────────────────────────────────────

async fn load(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<AutoClawCredentials, GatewayError> {
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
        return Err(GatewayError::with_status(401, "该账号没有可用凭证，无法查询活动"));
    }
    Ok(credentials)
}

/// 入口凭证：临期时先续期，续期失败保留旧凭证（交给操作本身报 401）
async fn prepare(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<AutoClawCredentials, GatewayError> {
    let credentials = load(ctx, region, account_id).await?;
    match refresh::ensure_fresh(&credentials).await {
        Ok(next) => {
            super::persist_refresh(ctx, &credentials, &next).await?;
            Ok(next)
        }
        Err(_) => Ok(credentials),
    }
}

/// 强制续期（操作遇 401 时调用一次）
async fn force_refresh(
    ctx: &PluginCtx,
    credentials: &AutoClawCredentials,
) -> Result<AutoClawCredentials, GatewayError> {
    let next = refresh::refresh(credentials, true).await?;
    super::persist_refresh(ctx, credentials, &next).await?;
    Ok(next)
}

// ─── 原始请求（保留状态码与 body，供「已拒绝 / 待确认」判定）────

struct RawReply {
    status: u16,
    body: Value,
}

async fn call(
    credentials: &AutoClawCredentials,
    method: &str,
    path: &str,
    body: Option<&Value>,
    what: &str,
) -> Result<RawReply, GatewayError> {
    let url = format!(
        "{}{path}",
        credentials::userapi_base_url(credentials.region)
    );
    let headers = refresh::signed_auth_headers(&credentials.token);
    let response = crate::http::send_raw(
        method,
        &url,
        body,
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, format!("{what}超时"))
        } else {
            GatewayError::with_status(502, format!("{what}失败: {error}"))
        }
    })?;
    Ok(RawReply {
        status: response.status,
        body: response.payload.clone().unwrap_or(Value::Null),
    })
}

fn business_code(body: &Value) -> Option<i64> {
    body.get("code").and_then(Value::as_i64)
}

fn has_error(body: &Value) -> bool {
    body.get("success").and_then(Value::as_bool) == Some(false)
        || body
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty())
}

fn detail_message(body: &Value) -> String {
    let mut message = text(body, &["message", "msg", "error"]);
    if message.is_empty() {
        if let Some(data) = body.get("data") {
            message = text(data, &["message", "msg", "error"]);
        }
    }
    if message.is_empty() {
        "上游拒绝请求".to_string()
    } else {
        message
    }
}

fn reject(reply: &RawReply, what: &str) -> GatewayError {
    let code = business_code(&reply.body);
    if reply.status == 401 || code.is_some_and(auth_code) {
        return GatewayError::with_status(401, format!("登录态已过期，无法{what}"));
    }
    let mut error = GatewayError::with_status(
        if (400..500).contains(&reply.status) {
            i32::from(reply.status)
        } else {
            502
        },
        format!("{what}失败：{}", detail_message(&reply.body)),
    );
    if let Some(code) = code {
        error = error.upstream_code(code);
    }
    error
}

fn accepted(reply: &RawReply) -> bool {
    (200..300).contains(&reply.status)
        && business_code(&reply.body).is_none_or(|code| code == 0)
        && !has_error(&reply.body)
}

/// 业务码非零且非鉴权码 → 视为确定性失败（不再重试）
fn business_failed(reply: &RawReply) -> bool {
    business_code(&reply.body).is_some_and(|code| code != 0 && !auth_code(code))
}

// ─── 台账 ────────────────────────────────────────────────────

async fn load_ledger(
    ctx: &PluginCtx,
    account_id: &str,
    kind: &str,
    bucket: &str,
    max_age_ms: i64,
) -> Result<Value, GatewayError> {
    if account_id.trim().is_empty() {
        return Err(GatewayError::with_status(400, "请指定 AutoClaw 账号"));
    }
    let stored = ctx.ledger_get(account_id, kind).await.unwrap_or(Value::Null);
    if !stored.is_null() && !stored.is_object() {
        return Err(GatewayError::with_status(409, "AutoClaw 活动台账格式异常"));
    }
    let mut root = if stored.is_object() {
        stored
    } else {
        json!({ bucket: {} })
    };
    if !root.get(bucket).is_some_and(Value::is_object) {
        root[bucket] = json!({});
    }
    let now = logging::now_ms();
    if let Some(map) = root.get_mut(bucket).and_then(Value::as_object_mut) {
        map.retain(|_, row| {
            row.get("at")
                .and_then(Value::as_i64)
                .is_some_and(|at| now.saturating_sub(at) < max_age_ms)
        });
    }
    Ok(root)
}

async fn save_ledger(
    ctx: &PluginCtx,
    account_id: &str,
    kind: &str,
    value: &Value,
) -> Result<(), GatewayError> {
    if serde_json::to_string(value).map(|text| text.len()).unwrap_or(0) > 60 * 1024 {
        return Err(GatewayError::with_status(409, "AutoClaw 活动台账容量已满"));
    }
    ctx.ledger_put(account_id, kind, value)
        .await
        .map_err(|error| GatewayError::with_status(409, format!("AutoClaw 活动台账写入失败：{error}")))
}

fn claims_of(ledger: &Value) -> Value {
    ledger.get("claims").cloned().unwrap_or_else(|| json!({}))
}

fn set_claim_state(
    ledger: &mut Value,
    id: &str,
    status: &str,
    day: Option<&str>,
) -> Result<(), GatewayError> {
    if !ledger.get("claims").is_some_and(Value::is_object) {
        ledger["claims"] = json!({});
    }
    let too_many = ledger
        .get("claims")
        .and_then(Value::as_object)
        .is_some_and(|map| map.len() >= CLAIM_CAPACITY && !map.contains_key(id));
    if too_many {
        return Err(GatewayError::with_status(409, "AutoClaw 活动记录过多，请稍后重试"));
    }
    let mut entry = json!({ "status": status, "at": logging::now_ms() });
    if let Some(day) = day {
        entry["day"] = json!(day);
    }
    ledger["claims"][id] = entry;
    Ok(())
}

fn clear_claim_state(ledger: &mut Value, id: &str) -> bool {
    ledger
        .get_mut("claims")
        .and_then(Value::as_object_mut)
        .and_then(|map| map.remove(id))
        .is_some()
}

fn claim_status(ledger: &Value, id: &str) -> String {
    ledger
        .get("claims")
        .and_then(|claims| claims.get(id))
        .and_then(|entry| entry.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn claim_at(ledger: &Value, id: &str) -> i64 {
    ledger
        .get("claims")
        .and_then(|claims| claims.get(id))
        .and_then(|entry| entry.get("at"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

// ─── 活动行 ──────────────────────────────────────────────────

fn campaign_row(
    id: String,
    title: String,
    title_en: String,
    description: String,
    description_en: String,
    amount: Option<i64>,
    status: &str,
) -> Value {
    json!({
        "campaignId": id,
        "title": title,
        "titleEn": title_en,
        "description": description,
        "descriptionEn": description_en,
        "amount": amount,
        "unit": "积分",
        "status": status,
        "startsAt": null,
        "endsAt": null,
        "claimedAt": null,
        "expiresAt": null,
        "validDays": null
    })
}

fn build_login_campaign(report: &Value, today: &str, saved_status: &str) -> Value {
    let granted = report.get("granted").and_then(Value::as_bool) == Some(true);
    let confirming = saved_status == "confirming";
    let incomplete = report.get("incomplete").and_then(Value::as_bool) == Some(true);
    let status = if granted {
        "claimed"
    } else if confirming {
        "confirming"
    } else if incomplete {
        "ineligible"
    } else {
        "claimable"
    };
    let latest_hint = if granted {
        String::new()
    } else {
        let latest = report.get("latest").unwrap_or(&Value::Null);
        let date = text(latest, &["grantDate"]);
        if date.is_empty() {
            String::new()
        } else {
            match integer(latest.get("points")) {
                Some(points) if points > 0 => {
                    format!(" 最近活动入账：{date} +{points} 积分。")
                }
                _ => format!(" 最近活动入账日期：{date}。"),
            }
        }
    };
    let description = if granted {
        let detail = text(report, &["description"]);
        if detail.is_empty() {
            format!("今日（{today}）登录活跃 / Weekly Perks 福利已记录入账。")
        } else {
            format!("今日已入账：{detail}")
        }
    } else if incomplete {
        "今日积分收入记录超过单页上限，暂无法确认是否已到账。".to_string()
    } else {
        format!(
            "手动触发桌面活跃上报以核对当前周期登录福利或 Weekly Perks 奖励（实际是否发放以官方账户流水为准）。{latest_hint}"
        )
    };
    campaign_row(
        format!("login-active:{today}"),
        "登录活跃 / Weekly Perks 福利".to_string(),
        "Login Active & Weekly Perks".to_string(),
        description,
        "Report desktop client activity to check and claim active login or Weekly Perks credits based on upstream ledger.".to_string(),
        if granted { integer(report.get("points")) } else { None },
        status,
    )
}

fn client_task_id(task: &Value) -> String {
    text(task, &["task_id"])
}

fn client_task_done(task: &Value) -> bool {
    task.get("completed").and_then(Value::as_bool) == Some(true)
        || matches!(
            text(task, &["status"]).to_ascii_lowercase().as_str(),
            "completed" | "claimed" | "done"
        )
}

fn client_task_triggerable(task: &Value, task_id: &str) -> bool {
    task.get("client_triggered").and_then(Value::as_bool) == Some(true)
        || task_id == "daily_inspiration_center"
}

fn client_task_campaign_id(task_id: &str, today: &str) -> String {
    if task_id.starts_with("daily_") {
        format!("task:{task_id}:{today}")
    } else {
        format!("task:{task_id}")
    }
}

fn map_client_task(task: &Value, today: &str, ledger: &Value) -> Option<Value> {
    if task.get("display").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let task_id = client_task_id(task);
    if !valid_id(&task_id) || task_id == "daily_signin" {
        return None;
    }
    let campaign_id = client_task_campaign_id(&task_id, today);
    let done = client_task_done(task);
    let triggerable = client_task_triggerable(task, &task_id);
    let status = if done {
        "claimed"
    } else if claim_status(ledger, &campaign_id) == "confirming" {
        "confirming"
    } else if triggerable {
        "claimable"
    } else {
        "ineligible"
    };
    let title = {
        let raw = text(task, &["title", "name"]);
        if raw.is_empty() {
            task_id.clone()
        } else {
            raw
        }
    };
    let raw_description = text(task, &["status_description", "point_description", "description"]);
    let description = if status == "ineligible" {
        let hint = if task_id == "upgrade_pc_app" {
            "需在官方 AutoClaw 桌面客户端升级至最新版本，无法由网关代领。"
        } else {
            "需在官方客户端完成对应动作后才可领取，插件不伪造上报。"
        };
        if raw_description.is_empty() {
            hint.to_string()
        } else {
            format!("{raw_description}（{hint}）")
        }
    } else if raw_description.is_empty() {
        "官方客户端积分任务。".to_string()
    } else {
        raw_description
    };
    Some(campaign_row(
        campaign_id,
        title.clone(),
        title,
        description,
        "Official AutoClaw client credit task.".to_string(),
        integer(task.get("reward_points")),
        status,
    ))
}

fn newbie_category(task: &Value) -> String {
    text(task, &["task_category", "category", "id"])
}

fn newbie_status(task: &Value) -> String {
    text(task, &["status"]).to_ascii_lowercase()
}

fn map_newbie_task(task: &Value, ledger: &Value) -> Option<Value> {
    let category = newbie_category(task);
    if !valid_id(&category) {
        return None;
    }
    let campaign_id = format!("newbie:{category}");
    let raw_status = newbie_status(task);
    let status = if matches!(raw_status.as_str(), "completed" | "claimed" | "done") {
        "claimed"
    } else if claim_status(ledger, &campaign_id) == "confirming" {
        "confirming"
    } else if raw_status == "claim" {
        "claimable"
    } else {
        "ineligible"
    };
    let title = {
        let raw = text(task, &["title", "task_name", "name"]);
        if raw.is_empty() {
            format!("新手任务 · {category}")
        } else {
            raw
        }
    };
    let raw_description = text(task, &["description", "desc", "status_description"]);
    let description = if status == "ineligible" {
        if raw_description.is_empty() {
            "需先完成对应新手引导任务条件后才可领取奖励，插件不代做任务。".to_string()
        } else {
            format!("{raw_description}（需先满足条件后才可领取）")
        }
    } else if raw_description.is_empty() {
        "新手任务条件已达成，可点击领取积分。".to_string()
    } else {
        raw_description
    };
    Some(campaign_row(
        campaign_id,
        title.clone(),
        title,
        description,
        "Official AutoClaw newbie reward task.".to_string(),
        integer(task.get("reward_points")),
        status,
    ))
}

// ─── 查询 ────────────────────────────────────────────────────

async fn fetch_client_tasks(credentials: &AutoClawCredentials) -> Result<Vec<Value>, GatewayError> {
    let reply = call(credentials, "GET", TASK_LIST, None, "查询任务列表").await?;
    if !accepted(&reply) {
        return Err(reject(&reply, "查询任务列表"));
    }
    Ok(reply
        .body
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

async fn fetch_newbie_tasks(
    credentials: &AutoClawCredentials,
) -> Result<(bool, Vec<Value>), GatewayError> {
    let reply = call(credentials, "GET", IDENTITY_TASKS, None, "查询新手任务").await?;
    if !accepted(&reply) {
        return Err(reject(&reply, "查询新手任务"));
    }
    let data = reply.body.get("data").cloned().unwrap_or(Value::Null);
    let all_done = data
        .get("all_tasks_completed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tasks = data
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((all_done, tasks))
}

async fn preview_once(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
    credentials: &AutoClawCredentials,
) -> Result<Value, GatewayError> {
    let today = beijing::today_key();
    let ledger = load_ledger(ctx, account_id, CLAIM_LEDGER, "claims", CLAIM_MAX_AGE_MS).await?;
    let claims = claims_of(&ledger);
    let mut campaigns = Vec::new();
    let mut notices = Vec::new();
    let mut first_error: Option<GatewayError> = None;
    let mut any_ok = false;

    match super::promotion::inspect(ctx, region, account_id).await {
        Ok(report) => {
            any_ok = true;
            let status = claim_status(&ledger, &format!("login-active:{today}"));
            campaigns.push(build_login_campaign(&report, &today, &status));
        }
        Err(error) if is_auth_error(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("登录活跃福利查询失败：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    match fetch_client_tasks(credentials).await {
        Ok(tasks) => {
            any_ok = true;
            for task in &tasks {
                if let Some(row) = map_client_task(task, &today, &claims) {
                    campaigns.push(row);
                }
            }
        }
        Err(error) if is_auth_error(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("客户端任务查询失败：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    match fetch_newbie_tasks(credentials).await {
        Ok((all_done, tasks)) => {
            any_ok = true;
            let has_claimable = tasks.iter().any(|task| newbie_status(task) == "claim");
            if !all_done || has_claimable {
                for task in &tasks {
                    if let Some(row) = map_newbie_task(task, &claims) {
                        campaigns.push(row);
                    }
                }
            }
        }
        Err(error) if is_auth_error(&error) => return Err(error),
        Err(error) => {
            notices.push(format!("新手任务查询提示：{}", error.message));
            first_error.get_or_insert(error);
        }
    }

    if !any_ok {
        return Err(first_error.unwrap_or_else(|| {
            GatewayError::with_status(502, "AutoClaw 活动列表查询失败")
        }));
    }
    Ok(json!({
        "campaigns": campaigns,
        "riskNote": null,
        "notice": if notices.is_empty() { Value::Null } else { json!(notices.join("；")) },
        "redemption": { "supported": true },
    }))
}

pub async fn preview(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let credentials = prepare(ctx, region, account_id).await?;
    match preview_once(ctx, region, account_id, &credentials).await {
        Err(error) if is_auth_error(&error) => {
            let fresh = force_refresh(ctx, &credentials).await?;
            preview_once(ctx, region, account_id, &fresh).await
        }
        other => other,
    }
}

// ─── 领取 ────────────────────────────────────────────────────

fn outcome_json(id: &str, outcome: &str, campaign: Option<Value>, message: &str) -> Value {
    json!({
        "campaignId": id,
        "outcome": outcome,
        "campaign": campaign,
        "message": message,
    })
}

pub async fn claim(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
    campaign: &str,
) -> Result<Value, GatewayError> {
    let id = campaign.trim();
    if Target::parse(id).is_none() {
        return Ok(outcome_json(id, "inactive", None, "不支持的 AutoClaw 活动编号"));
    }
    let credentials = prepare(ctx, region, account_id).await?;
    let guard = flow_lock().lock().await;
    let today = beijing::today_key();
    let mut ledger = load_ledger(ctx, account_id, CLAIM_LEDGER, "claims", CLAIM_MAX_AGE_MS).await?;
    let result = claim_locked(&credentials, id, &today, &mut ledger).await;
    // 台账状态（claimed / confirming / 清除）必须随结果落盘，否则冷却与去重失效
    save_ledger(ctx, account_id, CLAIM_LEDGER, &ledger).await?;
    match result {
        Err(error) if is_auth_error(&error) => {
            drop(guard);
            let fresh = force_refresh(ctx, &credentials).await?;
            let guard = flow_lock().lock().await;
            let mut ledger =
                load_ledger(ctx, account_id, CLAIM_LEDGER, "claims", CLAIM_MAX_AGE_MS).await?;
            let result = claim_locked(&fresh, id, &today, &mut ledger).await;
            save_ledger(ctx, account_id, CLAIM_LEDGER, &ledger).await?;
            drop(guard);
            result
        }
        other => other,
    }
}

async fn claim_locked(
    credentials: &AutoClawCredentials,
    id: &str,
    today: &str,
    ledger: &mut Value,
) -> Result<Value, GatewayError> {
    match Target::parse(id) {
        Some(Target::LoginActive(day)) => {
            if day != today {
                return Ok(outcome_json(id, "inactive", None, "该登录活动日期已过期，请刷新列表"));
            }
            claim_login_active(credentials, id, today, ledger).await
        }
        Some(Target::ClientTask { task_id, day }) => {
            if day.as_deref().is_some_and(|d| d != today) {
                return Ok(outcome_json(id, "inactive", None, "该每日任务日期已过期，请刷新列表"));
            }
            claim_client_task(credentials, id, &task_id, today, ledger).await
        }
        Some(Target::Newbie(category)) => {
            claim_newbie_task(credentials, id, &category, ledger).await
        }
        None => Ok(outcome_json(id, "inactive", None, "活动标识无效")),
    }
}

async fn claim_login_active(
    credentials: &AutoClawCredentials,
    id: &str,
    today: &str,
    ledger: &mut Value,
) -> Result<Value, GatewayError> {
    let before = super::promotion::inspect_credentials(credentials).await?;
    if before.get("granted").and_then(Value::as_bool) == Some(true) {
        set_claim_state(ledger, id, "claimed", Some(today))?;
        let row = build_login_campaign(&before, today, "claimed");
        return Ok(outcome_json(
            id,
            "already_claimed",
            Some(row),
            "今日登录活跃 / Weekly Perks 福利已到账",
        ));
    }
    if before.get("incomplete").and_then(Value::as_bool) == Some(true) {
        let row = build_login_campaign(&before, today, &claim_status(ledger, id));
        return Ok(outcome_json(
            id,
            "confirming",
            Some(row),
            "今日收入流水超过单页上限，暂无法确认是否已到账，已暂停重发上报",
        ));
    }
    let recent = claim_status(ledger, id) == "confirming"
        && logging::now_ms().saturating_sub(claim_at(ledger, id)) < CONFIRM_COOLDOWN_MS;
    if recent {
        let row = build_login_campaign(&before, today, "confirming");
        return Ok(outcome_json(
            id,
            "confirming",
            Some(row),
            "上次活跃上报仍在确认冷却期，请稍后再试或先核对流水",
        ));
    }
    set_claim_state(ledger, id, "confirming", Some(today))?;

    match super::promotion::activate_credentials(credentials).await {
        Ok(()) => {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let after = super::promotion::inspect_credentials(credentials).await?;
            if after.get("granted").and_then(Value::as_bool) == Some(true) {
                set_claim_state(ledger, id, "claimed", Some(today))?;
                let row = build_login_campaign(&after, today, "claimed");
                return Ok(outcome_json(
                    id,
                    "claimed",
                    Some(row),
                    "已触发客户端活跃并核对今日福利到账",
                ));
            }
            if after.get("incomplete").and_then(Value::as_bool) == Some(true) {
                let row = build_login_campaign(&after, today, "confirming");
                return Ok(outcome_json(
                    id,
                    "confirming",
                    Some(row),
                    "已上报活跃状态，但今日收入流水未完整，请稍后核对",
                ));
            }
            clear_claim_state(ledger, id);
            let mut row = build_login_campaign(&after, today, "");
            row["status"] = json!("ineligible");
            row["description"] = json!(
                "已上报客户端活跃状态，官方今日暂未发放新奖励（可能本周已领过、不在活动期或暂不符合条件）。"
            );
            Ok(outcome_json(
                id,
                "ineligible",
                Some(row),
                "已完成活跃上报，今日暂无可发放的新福利",
            ))
        }
        Err(error) if is_auth_error(&error) => {
            clear_claim_state(ledger, id);
            Err(error)
        }
        Err(error) => {
            let row = build_login_campaign(&before, today, &claim_status(ledger, id));
            Ok(outcome_json(
                id,
                "confirming",
                Some(row),
                &format!("活跃上报未确认完成（{}），请稍后刷新核对流水", error.message),
            ))
        }
    }
}

/// 客户端任务领取
async fn claim_client_task(
    credentials: &AutoClawCredentials,
    id: &str,
    task_id: &str,
    today: &str,
    ledger: &mut Value,
) -> Result<Value, GatewayError> {
    let tasks = fetch_client_tasks(credentials).await?;
    let Some(task) = tasks
        .iter()
        .find(|task| client_task_id(task) == task_id && task.get("display").and_then(Value::as_bool) != Some(false))
    else {
        return Ok(outcome_json(id, "inactive", None, "当前客户端任务列表未开启此任务"));
    };
    if client_task_done(task) {
        set_claim_state(ledger, id, "claimed", Some(today))?;
        let row = map_client_task(task, today, ledger);
        return Ok(outcome_json(id, "already_claimed", row, "该任务今日已完成领取"));
    }
    if !client_task_triggerable(task, task_id) {
        let row = map_client_task(task, today, ledger);
        return Ok(outcome_json(
            id,
            "ineligible",
            row,
            "该任务需在官方客户端完成实际操作后才可领取",
        ));
    }
    if claim_status(ledger, id) == "confirming" {
        let row = map_client_task(task, today, ledger);
        return Ok(outcome_json(
            id,
            "confirming",
            row,
            "上次提交结果待确认，已重查任务列表仍未完成，暂不重复提交",
        ));
    }
    set_claim_state(ledger, id, "confirming", Some(today))?;

    let body = json!({ "task_id": task_id });
    let reply = call(credentials, "POST", TASK_COMPLETE, Some(&body), "客户端任务领取").await;
    handle_task_reply(
        id,
        reply,
        credentials,
        ledger,
        Verify::ClientTask {
            task_id: task_id.to_string(),
            today: today.to_string(),
        },
    )
    .await
}

/// 新手任务领取
async fn claim_newbie_task(
    credentials: &AutoClawCredentials,
    id: &str,
    category: &str,
    ledger: &mut Value,
) -> Result<Value, GatewayError> {
    let (_, tasks) = fetch_newbie_tasks(credentials).await?;
    let Some(task) = tasks.iter().find(|task| newbie_category(task) == category) else {
        return Ok(outcome_json(id, "inactive", None, "当前账号新手任务列表未找到该项"));
    };
    let raw_status = newbie_status(task);
    if matches!(raw_status.as_str(), "completed" | "claimed" | "done") {
        set_claim_state(ledger, id, "claimed", None)?;
        let row = map_newbie_task(task, ledger);
        return Ok(outcome_json(id, "already_claimed", row, "该新手任务奖励已领取"));
    }
    if raw_status != "claim" {
        let row = map_newbie_task(task, ledger);
        return Ok(outcome_json(id, "ineligible", row, "该新手任务尚不满足领取条件"));
    }
    if claim_status(ledger, id) == "confirming" {
        let row = map_newbie_task(task, ledger);
        return Ok(outcome_json(
            id,
            "confirming",
            row,
            "上次领取结果待确认，已重查新手任务仍可领取，暂不重复提交",
        ));
    }
    set_claim_state(ledger, id, "confirming", None)?;

    let body = json!({
        "biz_app_id": "autoclaw",
        "client_version": CLIENT_VERSION,
        "os": "win",
        "source_id": "autoclaw",
        "device_id": credentials.device_id,
        "task_category": category,
    });
    let reply = call(credentials, "POST", IDENTITY_COMPLETE, Some(&body), "新手任务领取").await;
    handle_task_reply(
        id,
        reply,
        credentials,
        ledger,
        Verify::Newbie {
            category: category.to_string(),
        },
    )
    .await
}

enum Verify {
    ClientTask { task_id: String, today: String },
    Newbie { category: String },
}

/// 写请求结果 → 统一领取结果（先重查任务状态，再下结论）
async fn handle_task_reply(
    id: &str,
    reply: Result<RawReply, GatewayError>,
    credentials: &AutoClawCredentials,
    ledger: &mut Value,
    verify: Verify,
) -> Result<Value, GatewayError> {
    let mut rejected: Option<String> = None;
    let mut already = false;
    match reply {
        Ok(res) => {
            if res.status == 401 || business_code(&res.body).is_some_and(auth_code) {
                clear_claim_state(ledger, id);
                return Err(reject(&res, "领取任务"));
            }
            if accepted(&res) {
                already = res
                    .body
                    .get("data")
                    .and_then(|data| data.get("already_completed"))
                    .and_then(Value::as_bool)
                    == Some(true);
            } else if ((400..500).contains(&res.status) && !matches!(res.status, 408 | 429))
                || business_failed(&res)
            {
                rejected = Some(format!("官方拒绝领取：{}", detail_message(&res.body)));
            }
        }
        Err(error) if is_auth_error(&error) => {
            clear_claim_state(ledger, id);
            return Err(error);
        }
        Err(_) => {}
    }

    let mut verified = verify_state(credentials, &verify, ledger).await;
    if let Ok((false, _)) = &verified {
        if rejected.is_none() {
            tokio::time::sleep(Duration::from_millis(800)).await;
            verified = verify_state(credentials, &verify, ledger).await;
        }
    }
    if let Ok((true, view)) = verified {
        set_claim_state(ledger, id, "claimed", None)?;
        let outcome = if already { "already_claimed" } else { "claimed" };
        return Ok(outcome_json(
            id,
            outcome,
            view,
            if already { "该奖励此前已领取" } else { "奖励领取成功" },
        ));
    }
    if let Some(reason) = rejected {
        clear_claim_state(ledger, id);
        return Ok(outcome_json(id, "failed", None, &reason));
    }
    Ok(outcome_json(
        id,
        "confirming",
        None,
        "领取请求结果待确认，请稍后刷新核对状态",
    ))
}

async fn verify_state(
    credentials: &AutoClawCredentials,
    verify: &Verify,
    ledger: &Value,
) -> Result<(bool, Option<Value>), GatewayError> {
    match verify {
        Verify::ClientTask { task_id, today } => {
            let tasks = fetch_client_tasks(credentials).await?;
            let current = tasks.iter().find(|task| client_task_id(task).as_str() == task_id);
            let done = current.is_some_and(client_task_done);
            Ok((
                done,
                current.and_then(|task| map_client_task(task, today, ledger)),
            ))
        }
        Verify::Newbie { category } => {
            let (_, tasks) = fetch_newbie_tasks(credentials).await?;
            let current = tasks.iter().find(|task| newbie_category(task).as_str() == category);
            let done = current.is_some_and(|task| {
                matches!(newbie_status(task).as_str(), "completed" | "claimed" | "done")
            });
            Ok((done, current.and_then(|task| map_newbie_task(task, ledger))))
        }
    }
}

// ─── 兑换码 ──────────────────────────────────────────────────

pub async fn redeem(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
    payload: &Value,
) -> Result<Value, GatewayError> {
    let code = payload.get("code").and_then(Value::as_str).unwrap_or("").trim();
    if code.is_empty() || code.len() > 64 || code.chars().any(char::is_control) {
        return Err(GatewayError::with_status(400, "请填写有效的 AutoClaw 兑换码"));
    }
    let credentials = prepare(ctx, region, account_id).await?;
    let _guard = flow_lock().lock().await;
    let result = redeem_locked(ctx, account_id, &credentials, code).await;
    match result {
        Err(error) if is_auth_error(&error) => {
            drop(_guard);
            let fresh = force_refresh(ctx, &credentials).await?;
            let _guard = flow_lock().lock().await;
            redeem_locked(ctx, account_id, &fresh, code).await
        }
        other => other,
    }
}

async fn redeem_locked(
    ctx: &PluginCtx,
    account_id: &str,
    credentials: &AutoClawCredentials,
    code: &str,
) -> Result<Value, GatewayError> {
    let mut ledger =
        load_ledger(ctx, account_id, REDEEM_LEDGER, "codes", REDEEM_MAX_AGE_MS).await?;
    let state = ledger
        .get("codes")
        .and_then(|codes| codes.get(code))
        .and_then(|entry| entry.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match state {
        "succeeded" => {
            return Ok(json!({ "outcome": "already_claimed", "message": "该兑换码已在当前账号兑换过" }))
        }
        "confirming" => {
            return Ok(json!({ "outcome": "confirming", "message": "上次兑换结果待确认，请先核对官方积分明细；不会重复提交" }))
        }
        _ => {}
    }
    let too_many = ledger
        .get("codes")
        .and_then(Value::as_object)
        .is_some_and(|map| map.len() >= REDEEM_CAPACITY);
    if too_many {
        return Err(GatewayError::with_status(409, "AutoClaw 兑换记录过多，请稍后清理后再试"));
    }
    if !ledger.get("codes").is_some_and(Value::is_object) {
        ledger["codes"] = json!({});
    }
    ledger["codes"][code] = json!({ "state": "confirming", "at": logging::now_ms() });
    save_ledger(ctx, account_id, REDEEM_LEDGER, &ledger).await?;

    let body = json!({ "biz_app_id": "autoclaw", "code": code });
    match call(credentials, "POST", REDEEM_PATH, Some(&body), "兑换码兑换").await {
        Ok(res) => {
            let biz = business_code(&res.body);
            if res.status == 401 || biz.is_some_and(auth_code) {
                if let Some(map) = ledger.get_mut("codes").and_then(Value::as_object_mut) {
                    map.remove(code);
                }
                save_ledger(ctx, account_id, REDEEM_LEDGER, &ledger).await?;
                return Err(reject(&res, "兑换码兑换"));
            }
            if accepted(&res) {
                ledger["codes"][code] = json!({ "state": "succeeded", "at": logging::now_ms() });
                save_ledger(ctx, account_id, REDEEM_LEDGER, &ledger).await?;
                return Ok(json!({ "outcome": "claimed", "message": "AutoClaw 兑换码兑换成功" }));
            }
            if ((400..500).contains(&res.status) && !matches!(res.status, 408 | 429))
                || business_failed(&res)
            {
                if let Some(map) = ledger.get_mut("codes").and_then(Value::as_object_mut) {
                    map.remove(code);
                }
                save_ledger(ctx, account_id, REDEEM_LEDGER, &ledger).await?;
                let message = match biz {
                    Some(650_101) => "该兑换码已使用".to_string(),
                    Some(650_102) => "兑换码已过期".to_string(),
                    _ => format!("AutoClaw 兑换码兑换失败：{}", detail_message(&res.body)),
                };
                return Ok(json!({ "outcome": "failed", "message": message }));
            }
            Ok(json!({ "outcome": "confirming", "message": "兑换结果待确认，请核对官方积分明细；不会重复提交" }))
        }
        Err(error) if is_auth_error(&error) => {
            if let Some(map) = ledger.get_mut("codes").and_then(Value::as_object_mut) {
                map.remove(code);
            }
            save_ledger(ctx, account_id, REDEEM_LEDGER, &ledger).await?;
            Err(error)
        }
        Err(_) => Ok(json!({ "outcome": "confirming", "message": "兑换请求未获确认结果，请核对官方积分明细；不会重复提交" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets_and_rejects_daily_signin() {
        assert!(matches!(
            Target::parse("login-active:2026-11-23"),
            Some(Target::LoginActive(day)) if day == "2026-11-23"
        ));
        assert!(Target::parse("login-active:bad-date").is_none());
        assert!(matches!(
            Target::parse("task:daily_inspiration_center:2026-11-23"),
            Some(Target::ClientTask { task_id, day: Some(day) })
                if task_id == "daily_inspiration_center" && day == "2026-11-23"
        ));
        assert!(Target::parse("task:daily_signin:2026-11-23").is_none());
        assert!(Target::parse("task:daily_signin").is_none());
        assert!(matches!(Target::parse("newbie:bind_im"), Some(Target::Newbie(c)) if c == "bind_im"));
        assert!(Target::parse("newbie:bad:id").is_none());
    }

    #[test]
    fn maps_client_and_newbie_campaigns_honestly() {
        let ledger = json!({ "claims": { "task:upgrade_pc_app": { "status": "confirming" } } });
        let inspiration = map_client_task(
            &json!({ "task_id": "daily_inspiration_center", "title": "每日浏览灵感中心",
                "reward_points": 50, "status": "pending" }),
            "2026-11-23",
            &ledger,
        )
        .expect("client task");
        assert_eq!(inspiration["status"], "claimable");
        assert_eq!(inspiration["amount"], 50);
        let locked = map_client_task(
            &json!({ "task_id": "upgrade_pc_app", "title": "升级客户端", "reward_points": 100,
                "status": "pending", "client_triggered": false }),
            "2026-11-23",
            &ledger,
        )
        .expect("locked task");
        assert_eq!(locked["status"], "confirming");

        let newbie = map_newbie_task(
            &json!({ "task_category": "first_chat", "title": "首次对话",
                "reward_points": 200, "status": "claim" }),
            &ledger,
        )
        .expect("newbie");
        assert_eq!(newbie["status"], "claimable");
        let todo = map_newbie_task(
            &json!({ "task_category": "bind_im", "title": "绑定微信",
                "reward_points": 200, "status": "todo" }),
            &ledger,
        )
        .expect("newbie todo");
        assert_eq!(todo["status"], "ineligible");
    }

    #[test]
    fn builds_login_campaign_from_report() {
        let login = build_login_campaign(
            &json!({ "granted": false, "incomplete": false,
                "latest": { "grantDate": "2026-11-16", "points": 4000 } }),
            "2026-11-23",
            "",
        );
        assert_eq!(login["status"], "claimable");
        assert!(login["description"]
            .as_str()
            .is_some_and(|text| text.contains("2026-11-16 +4000")));
    }
}
