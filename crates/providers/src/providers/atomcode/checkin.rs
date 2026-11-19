//! AtomCode 的「签到」：**领取免费 CodingPlan**。
//!
//! ── 它与别家的「签到」不是一回事，先说清楚 ────────────────────
//! 其它家的签到多是「每天领一次积分」。AtomCode 的免费套餐是**一次性领取**的
//! 授权（`POST /coding-plan/claim-v2`，档位 Max / Pro / Lite），领到之后
//! 按窗口滚额度、不再需要每天领。之所以挂在签到链上，是因为：
//!   · 新账号加进来时必须领一次，否则模型目录为空、什么都调不动；
//!   · 套餐会**到期**（`codingplan_free.expires_at`），到期后要能自动续领；
//!   · 签到链本来就有「每天跑一次、逐账号、结果进台账」的调度与界面，
//!     不必再为这一家单起一个定时器。
//! 所以本实现的口径是「**按需领取**」：先探测，只有确实没有有效套餐时才领。
//!
//! ── 顺序与停规则（照官方 `setup.rs:825-918` 与本地 Go 参考 `credits.go:287-330`）──
//! 档位按 **Max → Pro → Lite** 走，**命中即停**，且三种停法各有理由：
//!
//!   | 响应 | 含义 | 动作 |
//!   | --- | --- | --- |
//!   | `duplicate: true` | 该档位（或更高档）**已经持有** | **停** —— 继续往下走会再报一次 duplicate，甚至把用户降级 |
//!   | `success: true` | 本次发放成功 | **停** |
//!   | 两者皆 false | 该档位被拒（额度 / 不在活动期 / 不符合条件） | 记下文案，试下一档 |
//!   | 传输/5xx/解析失败 | 连接坏了 | **整条级联中止** —— 换个档位重试修不好连接，只会加压 |
//!
//! ⚠️ **别只看 HTTP 200 判成功**。`duplicate: true` 是**成功语义**（已持有），
//! 把它当失败会让级联继续往下走并把用户降级。这是本模块最容易写错的一处。
//! （我先前据插件 README 说过「claim-v2 没有幂等标记、重复领取只会再回
//! success:true」—— **那句是错的**，源码里 `credits.go:149` 就有 `Duplicate bool
//! json:"duplicate"`，官方 CLI 的 `types.rs:526` 同款。）
//!
//! ── 领到之后为什么要刷目录 ──────────────────────────────────
//! `models-v2` 的 `plan_available` 是**相对被问的档位**算的 ⇒ 档位一变，
//! 可用模型集合就变。所以任何一次「真领到」或「档位变了」之后必须让目录失效，
//! 否则界面会继续用旧档位下的那份清单（表现是「领到了却看不到新模型」）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use crate::errors::GatewayError;
use crate::logging;

use agent2api_provider_sdk::PluginCtx;

use super::credentials::from_record;
use super::endpoints::REQUEST_TIMEOUT_MS;
use super::models::{PLAN_LITE, PLAN_MAX, PLAN_PRO, claim_plan, fetch_status, refresh};

/// 档位级联顺序（官方 `types.rs:48` 的 `CASCADE_ORDER`）
pub const CASCADE_ORDER: [&str; 3] = [PLAN_MAX, PLAN_PRO, PLAN_LITE];

/// 领取记忆的保留窗口（毫秒），对齐官方 `plans.rs` 的 120 秒 pending
const CLAIM_PENDING_MS: i64 = 120_000;

/// 每个账号一份的领取记忆（对齐官方 wasm/atomcode/src/plans.rs 的 `remember`）：
/// 发起级联前先落 pending（结果未知），任何结局再落结果；窗口内结果未确认时
/// 拒绝重试 —— `claim-v2` 无幂等标记，结果不明时重复提交可能重复发放。
struct ClaimMemory {
    /// 记忆有效期（毫秒时刻），过期后允许再次尝试
    until: i64,
    /// 级联结束后的结局，pending 阶段为 None
    result: Option<Value>,
}

static CLAIM_MEMORY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, ClaimMemory>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 读取当前账号的领取记忆，必要时写入 pending；返回值解释是否允许发起领取。
///
/// `Ok(None)` 表示可以发起（已写入 pending）；`Ok(Some(result))` 表示窗口内
/// 已有确认结果，直接复用；`Err` 表示窗口内结果尚未确认，应拒绝重试。
fn claim_memory_gate(account_id: &str) -> Result<Option<Value>, GatewayError> {
    let now = crate::http::now_ms();
    let mut memory = CLAIM_MEMORY
        .lock()
        .map_err(|_| GatewayError::with_status(500, "AtomCode 领取记忆锁不可用"))?;
    match memory.get(account_id) {
        Some(previous) if previous.until > now => {
            return Ok(previous.result.clone());
        }
        _ => {}
    }
    memory.insert(
        account_id.to_string(),
        ClaimMemory { until: now + CLAIM_PENDING_MS, result: None },
    );
    Ok(None)
}

/// 把级联结局写进领取记忆（成功与否都要写，清掉 pending）。
fn claim_memory_remember(account_id: &str, result: &Value) {
    let now = crate::http::now_ms();
    if let Ok(mut memory) = CLAIM_MEMORY.lock() {
        memory.insert(
            account_id.to_string(),
            ClaimMemory { until: now + CLAIM_PENDING_MS, result: Some(result.clone()) },
        );
    }
}

/// 一次 `claim-v2` 响应的判定结果
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TierVerdict {
    /// 该档位（或更高档）已持有 —— **终止级联**（照官方：继续往下会重复报或降级）
    AlreadyHeld {
        /// 上游文案
        message: String,
        /// 上游给的套餐名（可能缺）
        plan_name: String,
    },
    /// 本次发放成功 —— **终止级联**
    Granted {
        /// 上游文案
        message: String,
        /// 上游给的套餐名（可能缺）
        plan_name: String,
    },
    /// 该档位被拒 —— 记下文案，试下一档
    Refused {
        /// 上游文案
        message: String,
    },
}

/// 判定一次 `claim-v2` 的响应。
///
/// **判定顺序是契约**：先看 `duplicate`（成功语义），再看 `success`。
/// 反过来写的话，上游同时给两个 false 之外的所有组合都会走错分支。
pub fn classify_tier_response(payload: &Value) -> TierVerdict {
    let message = payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let plan_name = payload
        .get("plan_name")
        .or_else(|| payload.get("planName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if payload.get("duplicate").and_then(Value::as_bool).unwrap_or(false) {
        return TierVerdict::AlreadyHeld { message, plan_name };
    }
    if payload.get("success").and_then(Value::as_bool).unwrap_or(false) {
        return TierVerdict::Granted { message, plan_name };
    }
    TierVerdict::Refused { message }
}

/// 套餐块的到期时间解析（RFC3339 优先，回落 `+08:00` 时区的本地格式）
fn parse_expiry_ms(expires_at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(expires_at).ok()
        .map(|time| time.timestamp_millis())
        .or_else(|| chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S").ok()
            .and_then(|time| chrono::FixedOffset::east_opt(8 * 3600)
                .and_then(|zone| time.and_local_timezone(zone).single()))
            .map(|time| time.timestamp_millis()))
}

/// 当前套餐是否**已经有效、不需要领取**。
///
/// 到期时间可解析时以它为准；缺失或不可解析时只接受剩余天数大于零。
pub fn active_plan_of(status: &Value) -> Option<(String, String, i64)> {
    // 旧版套餐块叫 `codingplan_free`，新版回退到 `plan`（对齐官方 `plan()`）
    let plan = status
        .get("codingplan_free")
        .filter(|value| value.is_object())
        .or_else(|| status.get("plan").filter(|value| value.is_object()))?;
    let name = plan
        .get("plan_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let expires_at = plan
        .get("expires_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let remaining = plan
        .get("remaining_days")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let expiry = parse_expiry_ms(&expires_at);
    let active = expiry.map(|expiry| expiry > crate::http::now_ms()).unwrap_or(remaining > 0);
    active.then_some((name, expires_at, remaining))
}

/// 套餐状态的三态判定（对齐官方 wasm/atomcode/src/usage.rs 的 `needs_claim`）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimNeed {
    /// 套餐有效中，无需领取
    Active,
    /// 确认未持有套餐，可以领取
    Needed,
    /// 状态读不出来 —— 不自动提交领取（官方口径「未将未知状态当成可领取」）
    Unknown,
}

/// 按套餐块判定领取需求。
///
/// 到期时间可解析时以它为准，其次剩余天数；两个套餐键**明确为 null** 视为
/// 未持有；其余形态（块缺失、字段缺、解析不出）一律未知。
pub fn claim_need_of(status: &Value) -> ClaimNeed {
    if active_plan_of(status).is_some() {
        return ClaimNeed::Active;
    }
    let plan = status
        .get("codingplan_free")
        .filter(|value| value.is_object())
        .or_else(|| status.get("plan").filter(|value| value.is_object()));
    if let Some(plan) = plan {
        let expires_at = plan
            .get("expires_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !expires_at.is_empty() {
            if let Some(until) = parse_expiry_ms(expires_at) {
                return if until <= crate::http::now_ms() {
                    ClaimNeed::Needed
                } else {
                    ClaimNeed::Active
                };
            }
        }
        if let Some(days) = plan.get("remaining_days").and_then(Value::as_i64) {
            return if days <= 0 { ClaimNeed::Needed } else { ClaimNeed::Active };
        }
        return ClaimNeed::Unknown;
    }
    if ["codingplan_free", "plan"]
        .iter()
        .any(|key| status.get(*key).is_some_and(Value::is_null))
    {
        return ClaimNeed::Needed;
    }
    ClaimNeed::Unknown
}

/// 首个展示窗口的读数文案（`已用/上限 次，N 小时后重置`）。
///
/// 与 `usage.rs` 同一口径：只读 `show_enable == 1` 的窗口、额度是**调用次数**。
/// 读不到就返回空串（签到结果里少一行，不编造）。
pub fn quota_summary(status: &Value) -> String {
    let Some(items) = status.get("rate_limit_windows").and_then(Value::as_array) else {
        return String::new();
    };
    let Some(window) = items.iter().find(|item| {
        item.get("show_enable").and_then(Value::as_i64) == Some(1)
            && item.get("call_limit").and_then(Value::as_i64).unwrap_or(0) > 0
    }) else {
        return String::new();
    };
    let limit = window.get("call_limit").and_then(Value::as_i64).unwrap_or(0);
    let used = window.get("calls_used").and_then(Value::as_i64).unwrap_or(0);
    let hours = window.get("window_hours").and_then(Value::as_i64).unwrap_or(0);
    let reset = window
        .get("reset_at_display")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut text = format!("已用 {used}/{limit} 次");
    if hours > 0 {
        text.push_str(&format!("（每 {hours} 小时滚动）"));
    }
    if !reset.is_empty() {
        text.push_str(&format!("，{reset} 重置"));
    }
    text
}

/// 按需领取：探测 → 无效才级联 → 领到后刷目录。
///
/// 返回的形状与其它家的签到实现一致（`{success, msg, ...}`），交给
/// `billing::checkin::claim_result` 翻成签到结果行。
///
/// ── 三种结局对应用户看到的三种话 ─────────────────────────────
///   · **已经有效**：`success:false` + 中性文案（`套餐有效中…无需领取`）+ 额度读数。
///     刻意**不**回 `success:true` —— 那会被日志与界面说成「签到成功」，
///     而这一天其实什么都没发生（与本仓 chatglm 对访客账号的中性处置同一口径）。
///   · **真领到**：`success:true` + 上游文案。
///   · **被拒**：`success:false` + 各档位文案汇总（例如不在活动期）。
pub async fn claim_plan_if_needed(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    // 账号记录由网关持有，插件只能走 `store.get` 回调
    let record = match crate::store::record(ctx, super::ID, account_id).await {
        Ok(record) => Some(record),
        Err(_) if account_id.is_empty() => None,
        Err(_) => {
            return Err(GatewayError::with_status(
                401,
                format!("AtomCode 账号 {account_id} 不存在（请重新添加）"),
            ));
        }
    };
    let credentials = from_record(record.as_ref())?;

    // ── 第一步：探测。已经有效就别动上游的领取接口 ────────────────
    let status = fetch_status(&credentials, REQUEST_TIMEOUT_MS).await?;
    if claim_need_of(&status) == ClaimNeed::Unknown {
        // 对齐官方：未知状态不当成可领取，避免无套餐证据时反复提交
        return Err(GatewayError::with_status(
            502,
            "AtomCode 套餐状态尚未确认，未自动提交领取，请稍后重查",
        ));
    }
    if let Some((plan_name, expires_at, remaining)) = active_plan_of(&status) {
        let quota = quota_summary(&status);
        let mut msg = String::from("套餐有效中，无需领取");
        if !plan_name.is_empty() {
            msg.push_str(&format!("（{plan_name}）"));
        }
        if remaining > 0 {
            msg.push_str(&format!("，剩余 {remaining} 天"));
        } else if !expires_at.is_empty() {
            msg.push_str(&format!("，到期 {expires_at}"));
        }
        logging::verbose("[Checkin]", &format!("AtomCode {account_id}: {msg}"));
        return Ok(json!({
            "success": false,
            "msg": msg,
            "planName": plan_name,
            "expiresAt": expires_at,
            "quota": quota,
            "needsAction": false,
        }));
    }

    // ── 领取记忆门：窗口内结果未确认时拒绝重试 ───────────────────
    if let Some(result) = claim_memory_gate(account_id)? {
        logging::verbose(
            "[Checkin]",
            &format!("AtomCode {account_id}: 领取记忆窗口内，复用上次结果"),
        );
        return Ok(result);
    }

    // ── 第二步：级联领取（Max → Pro → Lite，命中即停）─────────────
    let mut attempts: Vec<Value> = Vec::new();
    let mut last_refusal = String::new();
    for tier in CASCADE_ORDER {
        let payload = claim_plan(&credentials, tier).await.map_err(|error| {
            // 传输/HTTP 层失败：**整条级联中止**（换档位修不好连接）
            GatewayError::with_status(
                error.status_code,
                format!("AtomCode 套餐领取在 {} 档失败：{}", tier, error.message),
            )
        })?;
        // 对齐官方：响应非对象、或 success/duplicate 键存在但不是布尔 →
        // 结果无法解读，停止级联，不把异常响应当成拒绝继续换档
        if !payload.is_object()
            || ["success", "duplicate"]
                .iter()
                .any(|key| payload.get(*key).is_some_and(|value| !value.is_boolean()))
        {
            return Err(GatewayError::with_status(
                502,
                "AtomCode 套餐领取结果格式异常，未继续领取",
            ));
        }
        match classify_tier_response(&payload) {
            TierVerdict::AlreadyHeld { message, plan_name } => {
                attempts.push(json!({ "tier": tier, "outcome": "already-held", "message": message }));
                let msg = if message.is_empty() {
                    format!("已持有 {tier} 档套餐，无需重复领取")
                } else {
                    message
                };
                logging::log("[Checkin]", &format!("AtomCode {account_id}: {msg}"));
                let result = json!({
                    "success": false,
                    "msg": msg,
                    "tier": tier,
                    "planName": plan_name,
                    "attempts": attempts,
                    "needsAction": false,
                });
                claim_memory_remember(account_id, &result);
                return Ok(result);
            }
            TierVerdict::Granted { message, plan_name } => {
                attempts.push(json!({ "tier": tier, "outcome": "claimed", "message": message }));
                let msg = if message.is_empty() {
                    format!("已领取 {tier} 档免费套餐")
                } else {
                    message
                };
                // 档位变了 ⇒ 目录必须失效（`plan_available` 相对档位算）
                let outcome = refresh(&credentials, true).await;
                logging::log(
                    "[Checkin]",
                    &format!(
                        "AtomCode {account_id}: {msg}（目录已刷新：{:?}）",
                        outcome
                    ),
                );
                let result = json!({
                    "success": true,
                    "msg": msg,
                    "tier": tier,
                    "planName": plan_name,
                    "attempts": attempts,
                    "needsAction": true,
                });
                claim_memory_remember(account_id, &result);
                return Ok(result);
            }
            TierVerdict::Refused { message } => {
                // 对齐官方：success 键缺失说明响应没有明确业务结果，
                // 不能当拒绝继续换档（无幂等端点上重复提交有风险）
                if payload.get("success").and_then(Value::as_bool).is_none() {
                    return Err(GatewayError::with_status(
                        502,
                        "AtomCode 套餐领取响应缺少明确业务结果，已停止后续领取",
                    ));
                }
                attempts.push(json!({ "tier": tier, "outcome": "refused", "message": message }));
                if !message.is_empty() {
                    last_refusal = message;
                }
            }
        }
    }

    // ── 三档都被拒：如实报告（常见成因是不在活动期或名额已满）──────
    let msg = if last_refusal.is_empty() {
        "三档免费套餐都未能领取（上游未说明原因）".to_string()
    } else {
        format!("三档免费套餐都未能领取：{last_refusal}")
    };
    logging::log("[Checkin]", &format!("AtomCode {account_id}: {msg}"));
    let result = json!({
        "success": false,
        "msg": msg,
        "attempts": attempts,
        "needsAction": true,
    });
    claim_memory_remember(account_id, &result);
    Ok(result)
}
