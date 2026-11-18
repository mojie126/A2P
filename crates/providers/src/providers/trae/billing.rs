//! Trae 的逐轮账单缓存（`get_session_usage`）。
//!
//! ── 为什么 SSE 的缓存不可信 ────────────────────────────────
//! 上游 `token_usage` 事件即使带缓存字段，真机实测也是 SSE 两轮全 0、
//! 账单 `cache_read_token=25344`（对齐参考实现 `docs/CHANNEL-TRAEWORK.md`
//! 2026-11-06 实测记录）——SSE 的缓存 0 不能证明未命中，权威值在账单
//! `user_usage_group_by_session.extra_info` 里。
//!
//! ── 账单键 ────────────────────────────────────────────────
//! `POST /api/v1/commercial/get_session_usage` body
//! `{session_id: <请求体主动写的 session_id>}`：上游按请求体里主动写的
//! session_id 累计商业账单，流 metadata 里上游自分配的 id 不进账单键
//! （查回全零骨架，2026-10-31 定案的根因）。本家的 session_id 由
//! `forward::session_id` 主动写入请求体，这里用同一个值查账。
//!
//! ── 累计账单必须换算成单轮差值 ────────────────────────────
//! 账单里的 `cache_read_token` 是该 session 的**累计**读数，不是本轮增量。
//! 直接并入本轮 usage 会让客户端把整个会话的缓存命中当成本轮上下文：
//! Claude Code 一侧按 `input + cache_read + cache_creation` 估算上下文，
//! 每次请求都看到越来越大的累计值，会误判上下文膨胀而反复自动压缩。
//! 因此按 session 记录上一次累计读数（插件台账），本轮只上报差值；
//! 台账读不到就不补全，宁可保持上游原样，也不把累计值写进单轮用量。
//!
//! ── 落账延迟与预算 ────────────────────────────────────────
//! 账单落库有秒级延迟（参考实现重试 3 次、1.5s/3s 退避）。参考实现是
//! 异步补账不阻塞响应，本家在收尾帧发送前同步查 —— 插件侧没有上报通道，
//! 网关按帧里的 usage 记账，收口后再查进不了账。整体预算封顶 12 秒，
//! 查不到就放弃：那一轮与今天的口径一致（缓存按上游帧原样记账），
//! 不把「没查到」伪造成命中或 0。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;

use super::credentials::Credential;
use super::headers::{self, HeaderIdentity};
use super::http;
use crate::logging;

/// 账单接口路径（对齐参考实现 `constants.EP_SESSION_USAGE`）。
const EP_SESSION_USAGE: &str = "/api/v1/commercial/get_session_usage";

/// 按 session 保存上次累计读数的台账前缀。
const LEDGER_KIND_PREFIX: &str = "traeBillCache";

/// 单次账单请求的超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// 落账延迟的重试上限（参考实现同款 3 次）。
const RETRIES: usize = 3;

/// 重试之间的退避间隔（参考实现 1.5s / 3s）。
const BACKOFFS: [Duration; RETRIES - 1] = [Duration::from_millis(1500), Duration::from_secs(3)];

/// 整体预算上限（同步查询不能拖太久，见模块头）。
const TOTAL_BUDGET: Duration = Duration::from_secs(12);

/// 账单里的缓存读数（None = 上游没给这一项，绝不补 0）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BillCache {
    pub read: Option<i64>,
    pub write: Option<i64>,
}

impl BillCache {
    /// 两项都没拿到 = 这一轮没有权威缓存读数。
    pub fn is_empty(&self) -> bool {
        self.read.is_none() && self.write.is_none()
    }
}

/// 一次账单查询的预备上下文：URL、对话头、出口代理与账单键。
pub(crate) struct BillQuery {
    url: String,
    session_id: String,
    headers: BTreeMap<String, String>,
    proxy: Option<ResolvedProxy>,
    ctx: PluginCtx,
    account: String,
}

impl BillQuery {
    /// 用当前凭据组一次查询上下文（账单键就是请求体里主动写的 session_id）。
    pub(crate) fn new(
        ctx: &PluginCtx,
        account: &str,
        base: &str,
        session_id: &str,
        credential: &Credential,
        proxy: Option<&ResolvedProxy>,
    ) -> Self {
        let identity = HeaderIdentity {
            access_token: credential.access_token.trim(),
            uid: credential.uid.trim(),
            machine_id: credential.machine_id.trim(),
            device_id: credential.device_id.trim(),
        };
        Self {
            url: format!("{base}{EP_SESSION_USAGE}"),
            session_id: session_id.trim().to_string(),
            headers: headers::solo_headers(&identity, false),
            proxy: proxy.cloned(),
            ctx: ctx.clone(),
            account: account.trim().to_string(),
        }
    }

    /// 把账单的累计读数换算成本轮增量。
    ///
    /// 台账按 session 保存上一次累计值；读不到台账就不补全（返回空），
    /// 避免把整个会话的缓存累计当成本轮命中。写回失败不影响本轮响应，
    /// 下一轮会重新按当时的累计值求差。
    pub(crate) async fn round_cache(&self, cumulative: &BillCache) -> BillCache {
        if self.session_id.is_empty() || cumulative.is_empty() {
            return BillCache::default();
        }
        let kind = format!("{LEDGER_KIND_PREFIX}:{}", self.session_id);
        let previous = match self.ctx.ledger_get(&self.account, &kind).await {
            Ok(value) => parse_ledger_cache(&value),
            // 没有台账就无法证明上一轮读数，保持上游原样比写累计值安全
            Err(_) => return BillCache::default(),
        };
        let round = round_delta(previous, *cumulative);
        let _ = self
            .ctx
            .ledger_put(
                &self.account,
                &kind,
                &json!({ "read": cumulative.read, "write": cumulative.write }),
            )
            .await;
        round
    }

    /// 查一轮账单的缓存计数。失败与查不到都返回空值，
    /// 调用方按「这一轮没拿到」处理，与今天的口径一致。
    pub(crate) async fn fetch_cache(&self) -> BillCache {
        if self.session_id.is_empty() {
            return BillCache::default();
        }
        let headers: Vec<(&str, String)> = self
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect();
        let body = json!({ "session_id": self.session_id });
        let deadline = Instant::now() + TOTAL_BUDGET;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                let backoff = BACKOFFS[attempt - 1];
                if Instant::now() + backoff >= deadline {
                    break;
                }
                tokio::time::sleep(backoff).await;
            }
            if Instant::now() >= deadline {
                break;
            }
            let reply =
                match http::post_json(&self.url, &body, &headers, REQUEST_TIMEOUT, self.proxy.as_ref()).await {
                    Ok(reply) => reply,
                    Err(error) => {
                        logging::verbose(
                            "[Trae]",
                            &format!("逐轮账单查询失败（第 {} 次）：{}", attempt + 1, error.message),
                        );
                        continue;
                    }
                };
            if reply.status != 200 {
                logging::verbose(
                    "[Trae]",
                    &format!("逐轮账单返回 HTTP {}，放弃缓存补全", reply.status),
                );
                return BillCache::default();
            }
            let Some(payload) = reply.json() else {
                return BillCache::default();
            };
            let cache = parse_bill_cache(&payload);
            if !cache.is_empty() {
                return cache;
            }
            // 全零骨架 = 账单还没落，退避后重试
        }
        BillCache::default()
    }
}

/// 从账单响应抽缓存计数（2026-11-06 真机形状）：
/// `user_usage_group_by_session.extra_info.{cache_read_token, cache_write_token,
/// input_token, output_token}`，`usage_group_details` 每组一份（取第一组）。
/// 全零骨架（无 input/output）返回空——免费或未结算请求的 0 与「真没缓存」
/// 不可区分，不补。
pub(crate) fn parse_bill_cache(data: &Value) -> BillCache {
    let outer = data.get("data").filter(|value| value.is_object()).unwrap_or(data);
    let Some(group) = outer
        .get("user_usage_group_by_session")
        .or_else(|| {
            outer
                .get("usage")
                .and_then(|usage| usage.get("user_usage_group_by_session"))
        })
        .filter(|value| value.is_object())
    else {
        return BillCache::default();
    };
    let details = group
        .get("usage_group_details")
        .and_then(Value::as_array)
        .filter(|details| !details.is_empty());
    let extra = if group.get("extra_info").is_some_and(Value::is_object) && details.is_some() {
        group.get("extra_info")
    } else {
        details
            .and_then(|details| details.first())
            .and_then(|first| first.get("extra_info"))
            .filter(|value| value.is_object())
    };
    let Some(extra) = extra else {
        return BillCache::default();
    };
    let input = number(extra.get("input_token"));
    let output = number(extra.get("output_token"));
    if input.is_none() && output.is_none() {
        return BillCache::default();
    }
    BillCache {
        read: number(extra.get("cache_read_token")),
        write: number(extra.get("cache_write_token")),
    }
}

/// 从台账读上一次累计读数；空台账或损坏内容返回 None。
fn parse_ledger_cache(value: &Value) -> Option<BillCache> {
    let object = value.as_object()?;
    let read = object.get("read").and_then(Value::as_i64);
    let write = object.get("write").and_then(Value::as_i64);
    if read.is_none() && write.is_none() {
        return None;
    }
    Some(BillCache { read, write })
}

/// 累计读数换算成单轮增量。
///
/// 首次没有台账时把当前累计当作本轮读数（新 session 的第一次请求两者相等）；
/// 累计回退说明账单被重置，按当前值重新起算，绝不输出负增量。
fn round_delta(previous: Option<BillCache>, current: BillCache) -> BillCache {
    let delta = |current: Option<i64>, previous: Option<i64>| {
        current.map(|now| {
            let before = previous.unwrap_or(0);
            if now >= before {
                now - before
            } else {
                now
            }
        })
    };
    BillCache {
        read: delta(current.read, previous.and_then(|cache| cache.read)),
        write: delta(current.write, previous.and_then(|cache| cache.write)),
    }
}

/// 把账单缓存并进用量对象：写 OpenAI 标准位（网关的缓存别名表都认）。
/// 只在拿到账单读数时写对应键，缺失项保持原样 —— 不把「没查到」伪造成命中。
pub(crate) fn merge_into_usage(usage: &mut Value, cache: &BillCache) -> bool {
    let Some(object) = usage.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if let Some(read) = cache.read {
        // 单轮缓存命中不可能超过本轮 prompt；上限防御累计值漏进单轮口径
        let read = match object.get("prompt_tokens").and_then(Value::as_i64) {
            Some(prompt) if prompt > 0 => read.min(prompt),
            _ => read,
        };
        let details = object
            .entry("prompt_tokens_details".to_string())
            .or_insert_with(|| json!({}));
        if !details.is_object() {
            *details = json!({});
        }
        details["cached_tokens"] = json!(read);
        changed = true;
    }
    if let Some(write) = cache.write {
        object.insert("cache_creation_input_tokens".to_string(), json!(write));
        changed = true;
    }
    changed
}

/// 账单字段读数：数字直取（浮点截断取整），字符串按十进制解析，其余不算数。
fn number(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|number| number as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_group_extra_info() {
        let payload = json!({
            "data": {
                "user_usage_group_by_session": {
                    "extra_info": {
                        "cache_read_token": 25344,
                        "cache_write_token": 0,
                        "input_token": 28366,
                        "output_token": 123,
                    },
                    "usage_group_details": [{}],
                }
            }
        });
        let cache = parse_bill_cache(&payload);
        assert_eq!(cache.read, Some(25344));
        assert_eq!(cache.write, Some(0));
    }

    #[test]
    fn parse_falls_back_to_first_detail() {
        let payload = json!({
            "user_usage_group_by_session": {
                "usage_group_details": [
                    { "extra_info": { "input_token": 500, "cache_read_token": "480" } }
                ],
            }
        });
        let cache = parse_bill_cache(&payload);
        assert_eq!(cache.read, Some(480));
        assert_eq!(cache.write, None);
    }

    #[test]
    fn parse_rejects_zero_skeleton() {
        let payload = json!({
            "user_usage_group_by_session": {
                "extra_info": { "cache_read_token": 0, "cache_write_token": 0 },
                "usage_group_details": [{}],
            }
        });
        assert!(parse_bill_cache(&payload).is_empty());
    }

    #[test]
    fn parse_rejects_missing_group() {
        assert!(parse_bill_cache(&json!({ "data": {} })).is_empty());
        assert!(parse_bill_cache(&json!({})).is_empty());
    }

    #[test]
    fn merge_writes_standard_positions() {
        let mut usage = json!({ "prompt_tokens": 3017, "completion_tokens": 32 });
        assert!(merge_into_usage(
            &mut usage,
            &BillCache { read: Some(2816), write: Some(0) }
        ));
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 2816);
        assert_eq!(usage["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn merge_keeps_missing_cache_absent() {
        let mut usage = json!({ "prompt_tokens": 10 });
        assert!(!merge_into_usage(&mut usage, &BillCache::default()));
        assert!(usage.get("prompt_tokens_details").is_none());
        assert!(usage.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn round_delta_turns_cumulative_reads_into_this_turn() {
        // 首次请求：台账为空，累计值就是本轮值
        assert_eq!(
            round_delta(
                None,
                BillCache {
                    read: Some(25_344),
                    write: Some(0)
                }
            ),
            BillCache {
                read: Some(25_344),
                write: Some(0)
            }
        );
        // 后续请求：只上报相对上次累计的增量
        assert_eq!(
            round_delta(
                Some(BillCache {
                    read: Some(25_344),
                    write: Some(100)
                }),
                BillCache {
                    read: Some(48_000),
                    write: Some(300)
                }
            ),
            BillCache {
                read: Some(22_656),
                write: Some(200)
            }
        );
        // 累计回退（账单重置）：按当前值重新起算，绝不输出负增量
        assert_eq!(
            round_delta(
                Some(BillCache {
                    read: Some(900_000),
                    write: Some(0)
                }),
                BillCache {
                    read: Some(12_000),
                    write: Some(0)
                }
            ),
            BillCache {
                read: Some(12_000),
                write: Some(0)
            }
        );
        // 某一项缺失时不臆造差值
        assert_eq!(
            round_delta(
                Some(BillCache {
                    read: Some(10),
                    write: Some(5)
                }),
                BillCache::default()
            ),
            BillCache::default()
        );
    }

    #[test]
    fn ledger_cache_reads_only_complete_entries() {
        assert_eq!(parse_ledger_cache(&json!(null)), None);
        assert_eq!(parse_ledger_cache(&json!({})), None);
        assert_eq!(
            parse_ledger_cache(&json!({ "read": 2_816, "write": 0 })),
            Some(BillCache {
                read: Some(2_816),
                write: Some(0)
            })
        );
    }

    #[test]
    fn merge_caps_cache_read_at_the_round_prompt() {
        let mut usage = json!({ "prompt_tokens": 100 });
        assert!(merge_into_usage(
            &mut usage,
            &BillCache {
                read: Some(500),
                write: None
            }
        ));
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 100);
    }
}
