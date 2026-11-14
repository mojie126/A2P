//! 百度搭子（DuMate）插件实现。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! 百度「搭子 DuMate」编程助手。账号登录态是网页 Cookie（`BDUSS` 核心），
//! 对话走云端网关（OpenAI Chat 兼容 SSE），余额 / 签到 / 积分走
//! `www.dumate.cn` 的网页接口。
//!
//! ── 能力 ────────────────────────────────────────────────────
//! ```text
//!   有：对话（流式 / 工具 / 视觉，OpenAI Chat 兼容）、令牌自动续期
//!       （Cookie 换约 1 小时的 Bearer 令牌）、静态模型目录、余额、每日签到
//!   无：在线模型目录（上游无目录接口，两轮实测排除）、领取类活动
//!       （成长计划任务与抽奖不在本期）
//! ```
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考实现 `wearetheone777/dumate2api`（MIT）：端点、请求头指纹、模型
//! 路由名与限额均取自其真机实测记录，详见各子模块的模块头。

pub mod accounts;
pub mod checkin;
pub mod client;
pub mod endpoints;
pub mod login;
pub mod models;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = endpoints::PROVIDER_ID;

/// 千帆原生命名空间的模型名前缀（这个前缀的模型走 `/gateway/v2`）。
const QIANFAN_PREFIX: &str = "qianfan/";

/// 百度搭子适配器。
pub struct DumateProvider;

/// 取一个现在能用的 Bearer 令牌（Cookie 换票，带进程内缓存）。
async fn ensure_token_impl(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<String, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
    let credentials = accounts::Credentials::from_secret(&stored.secret);
    if credentials.cookie.is_empty() {
        return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
    }
    match client::fetch_token(&stored.id, &credentials, None, &ctx.env.proxy, force).await {
        Ok(token) => Ok(token),
        Err(error) => Err(error.message().to_string()),
    }
}

impl Provider for DumateProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, "百度搭子")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // Cookie 换令牌可自动续期；Cookie 本体失效只能重新粘贴
                refresh: true,
                // 上游没有模型目录接口（404 / 400 两轮实测），只做静态表
                model_refresh: false,
                usage: true,
                checkin: true,
                claims: false,
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: true,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "粘贴网页 Cookie（登录 www.dumate.cn 后从浏览器复制，需包含 BDUSS）"
                    .into(),
                placeholder: "BDUSS=…; BAIDUID=…; …".into(),
                extra_fields: vec![proto::LoginField {
                    key: "name".into(),
                    label: "账号备注名（可选）".into(),
                    placeholder: "用于显示".into(),
                    optional: true,
                    multiline: false,
                }],
            }])
            .with_models(models::fallback_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: false,
                credit_expiry: true,
                usage_detail: false,
                edition: false,
                identifier: vec!["name".into()],
                expiry: Vec::new(),
                long_lived: false,
                claim: false,
                plan_channel: false,
                email_as_name: false,
                concurrency_default: None,
                default_limiters: None,
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "百度搭子每日登录积分：先查签到状态（已签不重打），再执行每日签到，\
                     并用余额快照差核对奖励是否到账"
                        .into(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            });
        meta.order = 34;
        meta
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { accounts::normalize(ctx, input).await })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::public(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    /// 动态登录方式（网关「添加账号」弹窗按它渲染）。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(login::login_methods()),
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "百度搭子不支持管理动作「{other}」"
                ))),
            }
        })
    }

    // ── 登录（受控浏览器自动抓取 + 粘贴 Cookie 两条）────────────

    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" {
                return Err("百度搭子的登录请用「打开浏览器登录（自动抓取 Cookie）」方式".into());
            }
            let index = payload.get("methodIndex").and_then(Value::as_u64);
            match index {
                // 1 = 受控浏览器自动抓取；2 = 粘贴 Cookie，不走登录流程
                Some(1) => {
                    let proxy = payload
                        .get("proxy")
                        .filter(|value| !value.is_null())
                        .and_then(|value| serde_json::from_value(value.clone()).ok());
                    login::start_browser_flow(proxy, ctx.env.proxy.clone()).await
                }
                Some(2) => Err("请直接填写网页 Cookie 并提交，无需发起登录流程".to_string()),
                _ => Err("这条登录方式不存在，请关闭弹窗后重新打开".to_string()),
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { Ok(login::poll(ctx, flow).await) })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async {
            Ok(proto::ModelList {
                models: models::fallback_models(),
                meta: proto::CatalogMeta::default(),
            })
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move { usage::query(ctx, account).await })
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move { checkin::checkin(ctx, account).await })
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move { ensure_token_impl(ctx, account, false).await })
    }

    /// 401 之后的无条件刷新：跳过缓存重新换一张票；Cookie 失效时如实报错。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move { ensure_token_impl(ctx, account, true).await })
    }

    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let stored = ctx
                .store_get(PROVIDER_ID, account)
                .await
                .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
            let credentials = accounts::Credentials::from_secret(&stored.secret);
            if credentials.cookie.is_empty() {
                return Ok(false);
            }
            Ok(client::token_expiring(&stored.id, &credentials))
        })
    }

    // ── 转发 ──────────────────────────────────────────────────

    /// 构造一次无状态请求。`qianfan/` 前缀的模型走千帆原生端点，其余走
    /// 应用链路；两条链路的模型名空间不同，不能混发。
    /// 流式请求必须带 `stream_options.include_usage`（对齐参考实现
    /// CLAUDE.md 实测记录：云端网关只在该字段存在时才补流尾 usage 帧，
    /// 漏注入的症状是请求全部 200 而 token 全记 0）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let stored = ctx
                .store_get(PROVIDER_ID, &req.account)
                .await
                .map_err(|error| format!("读取百度搭子账号失败：{error}"))?;
            let credentials = accounts::Credentials::from_secret(&stored.secret);
            if credentials.cookie.is_empty() {
                return Err("百度搭子账号缺少网页 Cookie，请重新粘贴".to_string());
            }
            let token = client::fetch_token(
                &stored.id,
                &credentials,
                req.proxy.as_ref(),
                &ctx.env.proxy,
                false,
            )
            .await
            .map_err(|error| error.message().to_string())?;
            let qianfan = req.wire_model.starts_with(QIANFAN_PREFIX);
            let path = if qianfan {
                endpoints::QIANFAN_CHAT_PATH
            } else {
                endpoints::CHAT_PATH
            };
            let mut headers = vec![
                (
                    "Authorization".to_string(),
                    format!("Bearer {token}"),
                ),
                ("X-Dumate-Client-Type".to_string(), "web".to_string()),
                ("X-Dumate-Account-Id".to_string(), "global".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ];
            if qianfan {
                headers.push((
                    "X-Appbuilder-From".to_string(),
                    "openclaw".to_string(),
                ));
            }
            let body = prepare_body(req.body.clone(), &req.wire_model, qianfan);
            Ok(ChatPlan::chat(
                format!("{}{}", endpoints::GATEWAY_BASE, path),
                headers,
                body,
            ))
        })
    }

    /// 上游错误分类。顺序要紧：401（令牌失效）先判，402（余额耗尽）与
    /// 429（限流）都进限额档但冷却时长不同——余额耗尽冷却到下一个北京
    /// 零点（参考实现把它硬冷却到下一次签到窗口），限流是分钟级窗口。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| body.get("message").and_then(Value::as_str))
            .or_else(|| body.get("msg").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let upstream_code = body
            .pointer("/error/code")
            .and_then(Value::as_i64)
            .or_else(|| body.get("code").and_then(Value::as_i64));
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 402 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: Some(next_beijing_midnight()),
                message: if message.is_empty() {
                    "百度搭子账号积分已耗尽，将在下一个自然日（北京时间 0 点）签到后恢复".into()
                } else {
                    message
                },
                upstream_code,
                status,
            };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code,
                status,
            };
        }
        let lower = message.to_ascii_lowercase();
        if lower.contains("quota")
            || lower.contains("balance")
            || lower.contains("credit")
            || message.contains("额度")
            || message.contains("余额")
            || message.contains("积分")
        {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code,
                status,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code,
        }
    }
}

/// 出站请求体：改写模型名，并在流式时补 `stream_options.include_usage`。
///
/// 客户端已带 `stream_options` 时保留其余键、只把 `include_usage` 置真
/// （覆盖整个对象会悄悄丢掉用户显式设过的东西，与 ModelScope 同一口径）；
/// 非流式不带该字段，上游在响应体里直接给 usage。
fn prepare_body(mut body: Value, wire_model: &str, qianfan: bool) -> Value {
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    let model = if qianfan {
        wire_model.strip_prefix(QIANFAN_PREFIX).unwrap_or(wire_model)
    } else {
        wire_model
    };
    object.insert("model".into(), Value::String(model.to_string()));
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        let options = object
            .entry("stream_options".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !options.is_object() {
            *options = Value::Object(serde_json::Map::new());
        }
        options["include_usage"] = Value::Bool(true);
    }
    body
}

/// 下一个北京零点（毫秒时间戳），余额耗尽档的冷却终点。
fn next_beijing_midnight() -> i64 {
    use chrono::{Duration, FixedOffset};
    let offset = FixedOffset::east_opt(8 * 3600).expect("固定时区偏移合法");
    let now = chrono::DateTime::from_timestamp_millis(crate::http::now_ms())
        .unwrap_or_else(chrono::Utc::now);
    let local = now.with_timezone(&offset);
    let tomorrow = (local.date_naive() + Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .expect("零点合法");
    tomorrow
        .and_local_timezone(offset)
        .earliest()
        .map(|time| time.timestamp_millis())
        .unwrap_or_else(|| local.timestamp_millis() + Duration::hours(1).num_milliseconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streaming_body_injects_include_usage() {
        let body = prepare_body(json!({ "model": "kimi-k31", "stream": true }), "kimi-k31", false);
        assert_eq!(body["model"], "kimi-k31");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn streaming_body_keeps_client_stream_options() {
        let body = prepare_body(
            json!({ "stream": true, "stream_options": { "foo": 1 } }),
            "kimi-k31",
            false,
        );
        assert_eq!(body["stream_options"]["foo"], 1);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn non_stream_body_has_no_stream_options() {
        let body = prepare_body(json!({ "stream": false }), "qianfan/qwen3.5-35b-a3b", true);
        assert_eq!(body["model"], "qwen3.5-35b-a3b");
        assert!(body.get("stream_options").is_none());
    }
}

